use crate::{MachineId, Provider, RemoteFolder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Names one provider account. Every provider has an implicit [`AccountId::DEFAULT`]
/// account, which runs the CLI with the environment Keystone inherited. Other IDs are
/// slugs Keystone made from the account's label, unique across providers.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    pub const DEFAULT: &'static str = "default";

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A provider account the user added: the CLI runs with `home` as its own config
/// home (`CLAUDE_CONFIG_DIR` or `CODEX_HOME`), so its sign-in, settings, skills and
/// transcripts stay apart from every other account's. Keystone never reads or writes
/// the credentials inside it. An account on another machine is a login there: its
/// `home` is a folder on that machine, which its host makes and reads, so it has no
/// `Path` form here.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Account {
    id: AccountId,
    provider: Provider,
    label: String,
    /// Absolute, with `~` already expanded. Fixed once the account exists, so a
    /// session's transcripts never move to another home.
    home: PathBuf,
    /// The machine the account is a login on, `None` for this one. Fixed once the
    /// account exists, like its home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    machine: Option<MachineId>,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

impl Account {
    pub(crate) fn new(id: AccountId, provider: Provider, label: String, home: PathBuf) -> Self {
        Self {
            id,
            provider,
            label,
            home,
            machine: None,
            unknown: Map::new(),
        }
    }

    pub(crate) fn remote(
        id: AccountId,
        provider: Provider,
        label: String,
        machine: MachineId,
        home: &RemoteFolder,
    ) -> Self {
        Self {
            id,
            provider,
            label,
            home: PathBuf::from(home.wire()),
            machine: Some(machine),
            unknown: Map::new(),
        }
    }

    pub fn id(&self) -> &AccountId {
        &self.id
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// The account's config home on this machine, or `None` for a login on another,
    /// whose home names nothing here.
    pub fn home(&self) -> Option<&Path> {
        self.machine.is_none().then_some(self.home.as_path())
    }

    /// The machine the account is a login on, `None` for this one.
    pub fn machine(&self) -> Option<&MachineId> {
        self.machine.as_ref()
    }

    /// For a login on another machine, that machine and its home there.
    pub fn remote_home(&self) -> Option<(&MachineId, RemoteFolder)> {
        let machine = self.machine.as_ref()?;
        let home = RemoteFolder::new(&self.home.to_string_lossy()).ok()?;
        Some((machine, home))
    }

    pub(crate) fn set_label(&mut self, label: String) {
        self.label = label;
    }

    /// Moves a login on another machine to `home` there. The store allows it only
    /// while no session has sent with it.
    pub(crate) fn set_remote_home(&mut self, home: &RemoteFolder) {
        if self.machine.is_some() {
            self.home = PathBuf::from(home.wire());
        }
    }
}

/// `home` made absolute: a leading `~` becomes `user_home`, since child processes
/// never expand it. `..` is kept, since only the filesystem knows where it leads when
/// an earlier component is a symlink. Refuses a relative path, which would depend on
/// Keystone's working directory, and one that is lexically the filesystem root.
pub fn expand_home(home: &str, user_home: Option<&Path>) -> Result<PathBuf, String> {
    let home = home.trim();
    if home.is_empty() {
        return Err("Choose a folder.".into());
    }
    let path = match home.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            let user_home = user_home.ok_or("Home folder unknown; write the full path.")?;
            user_home.join(rest.trim_start_matches('/'))
        }
        Some(_) => return Err("Write the full path.".into()),
        None => PathBuf::from(home),
    };
    if !path.is_absolute() {
        return Err("Use a full path, starting with `/` or `~`.".into());
    }
    if lexical(&path).parent().is_none() {
        return Err("The account folder cannot be the filesystem root.".into());
    }
    Ok(path)
}

/// `path` with `.` and `..` resolved by text alone, which is right only where no
/// component is a symlink.
pub fn lexical(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            component => normal.push(component),
        }
    }
    normal
}

/// Whether two homes would share files: the same folder, or one inside the other. On
/// macOS, whose volumes usually ignore case, they are compared case-folded, so
/// `~/.CODEX` is `~/.codex` there; that refuses more, never less.
pub fn homes_overlap(a: &Path, b: &Path) -> bool {
    homes_overlap_folding(a, b, cfg!(target_os = "macos"))
}

/// [`homes_overlap`], case-folded when `fold` is set.
pub fn homes_overlap_folding(a: &Path, b: &Path, fold: bool) -> bool {
    if fold {
        let folded = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
        let (a, b) = (folded(a), folded(b));
        return a.starts_with(&b) || b.starts_with(&a);
    }
    a.starts_with(b) || b.starts_with(a)
}

/// [`homes_overlap`] for two folders on another machine, by their text alone, since
/// only that machine can resolve them, and case-folded, since it may be a Mac.
pub fn remote_homes_overlap(a: &RemoteFolder, b: &RemoteFolder) -> bool {
    let (a, b) = (a.wire().to_lowercase(), b.wire().to_lowercase());
    let within = |inner: &str, outer: &str| {
        inner == outer
            || outer == "/"
            || inner
                .strip_prefix(outer)
                .is_some_and(|r| r.starts_with('/'))
    };
    within(&a, &b) || within(&b, &a)
}

/// A slug for `label` that no account in `taken` uses and that is never
/// [`AccountId::DEFAULT`].
pub(crate) fn slug(provider: Provider, label: &str, taken: &[AccountId]) -> AccountId {
    let mut base = String::new();
    for c in label.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            base.push(c);
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
        if base.len() >= 32 {
            break;
        }
    }
    let base = base.trim_end_matches('-');
    let prefix = match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    let base = if base.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}-{base}")
    };
    (1..)
        .map(|n| match n {
            1 => base.clone(),
            n => format!("{base}-{n}"),
        })
        .map(AccountId)
        .find(|id| {
            id.as_str() != AccountId::DEFAULT
                && id.as_str() != format!("{prefix}-{}", AccountId::DEFAULT)
                && !taken.contains(id)
        })
        .expect("an unused slug exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homes_expand_tilde_resolve_dots_and_refuse_relative_paths() {
        let user = Path::new("/home/dev");
        assert_eq!(
            expand_home("~/.claude-work", Some(user)).unwrap(),
            Path::new("/home/dev/.claude-work")
        );
        assert_eq!(expand_home("~", Some(user)).unwrap(), user);
        assert_eq!(
            expand_home(" /a/./b/../c ", None).unwrap(),
            Path::new("/a/b/../c")
        );
        assert_eq!(lexical(Path::new("/a/./b/../c")), Path::new("/a/c"));
        assert!(expand_home("relative/dir", Some(user)).is_err());
        assert!(expand_home("~other/dir", Some(user)).is_err());
        assert!(expand_home("~/x", None).is_err());
        assert!(expand_home("/", None).is_err());
        assert!(expand_home("/a/..", None).is_err());
        assert!(expand_home("", None).is_err());
    }

    #[test]
    fn a_case_insensitive_volume_compares_homes_case_folded() {
        let default = Path::new("/Users/dev/.codex");
        let shouting = Path::new("/Users/Dev/.CODEX/work");
        assert!(homes_overlap_folding(shouting, default, true));
        assert!(homes_overlap_folding(
            default,
            Path::new("/USERS/DEV/.CODEX"),
            true
        ));
        assert!(
            !homes_overlap_folding(shouting, default, false),
            "case counts elsewhere"
        );
        assert!(
            !homes_overlap_folding(Path::new("/Users/dev/.codex-work"), default, true),
            "a shared prefix is still not inside"
        );
        assert_eq!(
            homes_overlap(shouting, default),
            cfg!(target_os = "macos"),
            "folded on macOS only"
        );
    }

    #[test]
    fn overlap_covers_equal_and_nested_homes_but_not_siblings_with_a_shared_prefix() {
        assert!(homes_overlap(
            Path::new("/h/.claude"),
            Path::new("/h/.claude")
        ));
        assert!(homes_overlap(
            Path::new("/h/.claude/work"),
            Path::new("/h/.claude")
        ));
        assert!(homes_overlap(
            Path::new("/h/.claude"),
            Path::new("/h/.claude/work")
        ));
        assert!(!homes_overlap(
            Path::new("/h/.claude-work"),
            Path::new("/h/.claude")
        ));
    }

    #[test]
    fn slugs_are_prefixed_unique_and_never_default() {
        let taken = vec![AccountId::new("claude-work")];
        assert_eq!(
            slug(Provider::Claude, "Work", &taken).as_str(),
            "claude-work-2"
        );
        assert_eq!(
            slug(Provider::Codex, "  Max #2 ", &[]).as_str(),
            "codex-max-2"
        );
        assert_eq!(slug(Provider::Claude, "✨", &[]).as_str(), "claude");
    }
}
