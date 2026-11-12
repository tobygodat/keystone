//! Machines the user linked, and the folders on them. A session on another machine
//! names its machine and a [`RemoteFolder`], which has no [`std::path::Path`] form, so
//! no local filesystem call can be handed one. See `docs/design/remote.md`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// Names one linked machine. A slug Keystone made from its SSH destination, unique in
/// the workspace.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MachineId(String);

impl MachineId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A machine the user linked: what they typed to reach it over SSH. It holds no
/// secret, since SSH does all the signing in.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Machine {
    id: MachineId,
    label: String,
    destination: String,
    /// Whether Quit ends this machine's sessions. Off, Quit leaves them running.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    end_on_quit: bool,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

impl Machine {
    pub(crate) fn new(id: MachineId, destination: String) -> Self {
        Self {
            id,
            label: destination.clone(),
            destination,
            end_on_quit: false,
            unknown: Map::new(),
        }
    }

    pub fn id(&self) -> &MachineId {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn destination(&self) -> &str {
        &self.destination
    }

    pub fn end_on_quit(&self) -> bool {
        self.end_on_quit
    }

    pub(crate) fn set_end_on_quit(&mut self, end: bool) {
        self.end_on_quit = end;
    }
}

/// Why `destination` can't be an SSH destination: empty, an option, or holding
/// whitespace or a control character. `ssh` never sees one of those.
pub fn check_destination(destination: &str) -> Result<(), String> {
    if destination.is_empty() {
        return Err("Type an SSH destination, such as a Host from ~/.ssh/config.".into());
    }
    if destination.starts_with('-')
        || destination
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("{destination:?} isn't an SSH destination."));
    }
    Ok(())
}

/// A folder on another machine, as the user typed it: an absolute POSIX path that is
/// never resolved here, since it names nothing on this machine. It has no `Path` form.
/// [`RemoteFolder::wire`] is the text the host is sent.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RemoteFolder(String);

impl RemoteFolder {
    /// Refuses a relative path, which would depend on the host's working directory,
    /// and one with a NUL or a line break. Trailing slashes are dropped.
    pub fn new(folder: &str) -> Result<Self, String> {
        let folder = folder.trim();
        if !folder.starts_with('/') {
            return Err("Type the folder's full path on that machine, starting with /.".into());
        }
        if folder.chars().any(char::is_control) {
            return Err("A folder path can't hold a control character.".into());
        }
        let trimmed = folder.trim_end_matches('/');
        Ok(Self(if trimmed.is_empty() {
            "/".into()
        } else {
            trimmed.to_owned()
        }))
    }

    /// The path as the host reads it.
    pub fn wire(&self) -> &str {
        &self.0
    }

    /// The folder's own name, or `/` for the root.
    pub fn name(&self) -> &str {
        self.0
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("/")
    }

    pub(crate) fn from_saved(saved: String) -> Self {
        Self(saved)
    }
}

impl fmt::Display for RemoteFolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A slug for `destination` that no machine in `taken` uses: the host part, without a
/// user or a port, in lowercase letters, digits and dashes.
pub(crate) fn slug(destination: &str, taken: &[MachineId]) -> MachineId {
    let host = destination
        .rsplit('@')
        .next()
        .unwrap_or(destination)
        .split(':')
        .next()
        .unwrap_or_default();
    let mut base = String::new();
    for c in host.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            base.push(c);
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
        if base.len() >= 32 {
            break;
        }
    }
    let base = match base.trim_end_matches('-') {
        "" => "machine".to_owned(),
        base => base.to_owned(),
    };
    (1..)
        .map(|n| match n {
            1 => base.clone(),
            n => format!("{base}-{n}"),
        })
        .map(MachineId)
        .find(|id| !taken.contains(id))
        .expect("an unused slug exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations_refuse_options_whitespace_and_empty_text() {
        assert!(check_destination("dev@desk").is_ok());
        assert!(check_destination("desk.tail1234.ts.net").is_ok());
        assert!(check_destination("").is_err());
        assert!(check_destination("-oProxyCommand=x").is_err());
        assert!(check_destination("a b").is_err());
        assert!(check_destination("a\nb").is_err());
    }

    #[test]
    fn remote_folders_are_absolute_and_never_resolved() {
        let folder = RemoteFolder::new(" /home/dev/src/app/ ").unwrap();
        assert_eq!(folder.wire(), "/home/dev/src/app");
        assert_eq!(folder.name(), "app");
        assert_eq!(RemoteFolder::new("/").unwrap().wire(), "/");
        assert_eq!(
            RemoteFolder::new("/no/such/../folder").unwrap().wire(),
            "/no/such/../folder"
        );
        assert!(RemoteFolder::new("src/app").is_err());
        assert!(RemoteFolder::new("~/src").is_err());
        assert!(RemoteFolder::new("/a\nb").is_err());
    }

    #[test]
    fn slugs_name_the_host_and_stay_unique() {
        assert_eq!(slug("dev@Desk.local:22", &[]).as_str(), "desk-local");
        let taken = vec![MachineId::new("desk")];
        assert_eq!(slug("desk", &taken).as_str(), "desk-2");
        assert_eq!(slug("@@", &[]).as_str(), "machine");
    }
}
