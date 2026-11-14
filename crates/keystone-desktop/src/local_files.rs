//! Reading this machine's files on behalf of a session.
//!
//! Local paths reach Keystone from two sources. A session's folder is one, and
//! [`crate::presentation::SessionPlace`] gates it. The other is strings in a
//! transcript: file changes, tool output, Markdown images and links, and mentions. A
//! session on another machine names paths there, and the same path here can hold a
//! different file. So a transcript string becomes a file here only through
//! [`LocalFiles`], which only a session working on this machine has, and every
//! function that reads or opens such a file takes the [`LocalPath`] it makes. Neither
//! type has a public constructor that takes a bare path.

use keystone_core::{Attachment, Session, SessionContext};
use std::path::{Path, PathBuf};

/// Leave to read this machine's files for one session. [`LocalFiles::of`] makes it,
/// and only for a session that works here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalFiles {
    /// The session's folder. A session from before folders were recorded has none, and
    /// only absolute paths name its files.
    root: Option<PathBuf>,
}

impl LocalFiles {
    /// The files of `session`, or `None` when it works on another machine or in a
    /// folder this version can't read. It takes the session itself, not a context a
    /// caller could make up, so only a session saved as working here gets a handle.
    pub(crate) fn of(session: &Session) -> Option<Self> {
        match session.context() {
            SessionContext::Recorded { directory, .. } => Some(Self {
                root: Some(directory.path().to_owned()),
            }),
            SessionContext::LegacyMissing => Some(Self { root: None }),
            SessionContext::Remote { .. } | SessionContext::Unsupported => None,
        }
    }

    /// The session's folder, when it has one.
    pub(crate) fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The file a transcript names as `path`: under the session's folder, or as given
    /// when it is absolute and the session has no folder.
    pub(crate) fn resolve(&self, path: &Path) -> Option<LocalPath> {
        let path = match &self.root {
            Some(root) => root.join(path),
            None => Some(path.to_owned()).filter(|path| path.is_absolute())?,
        };
        #[cfg(test)]
        tests::RESOLVED.lock().unwrap().push(path.clone());
        Some(LocalPath(path))
    }

    /// `path`, a file the user picked on this machine, such as one dropped on the
    /// composer, relative to the session's folder when it is relative.
    pub(crate) fn picked(&self, path: PathBuf) -> LocalPath {
        LocalPath(match &self.root {
            Some(root) if path.is_relative() => root.join(path),
            _ => path,
        })
    }
}

/// A file on this machine that a local session named, the user picked, or Keystone
/// keeps itself. Only [`LocalFiles`] and [`LocalPath::kept`] make one.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct LocalPath(PathBuf);

impl LocalPath {
    /// `path`, a file the user picked on this machine by its full path, for a session
    /// that has no files here: one on another machine, to which an image picked here
    /// goes by its bytes. A relative path names nothing here for it, so it comes back.
    pub(crate) fn picked_here(path: PathBuf) -> Result<Self, PathBuf> {
        if path.is_absolute() {
            Ok(Self(path))
        } else {
            Err(path)
        }
    }

    /// Keystone's own copy of an attached image, under `dir`, the attachments folder.
    pub(crate) fn kept(attachment: &Attachment, dir: &Path) -> Self {
        Self(attachment.path(dir))
    }

    /// The same file by its canonical path, or `None` when it can't be found.
    pub(crate) fn canonical(&self) -> Option<Self> {
        self.0.canonicalize().ok().map(Self)
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    /// Any path, for unit tests of code that takes a [`LocalPath`].
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        Self(path)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Every path a transcript string was resolved to, so a test can check that no
    /// session on another machine reached one.
    pub(crate) static RESOLVED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    #[test]
    fn only_a_session_on_this_machine_has_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = keystone_core::Store::open(dir.path().join("workspace.json")).unwrap();
        let folder = keystone_core::WorkingDirectory::new(dir.path()).unwrap();
        let here = store
            .create(keystone_core::Provider::Claude, &folder, 1)
            .unwrap()
            .value;
        let desk = store.link_machine("desk", "client".into()).unwrap().value;
        let remote = keystone_core::RemoteFolder::new("/srv/app").unwrap();
        let there = store
            .create_remote_showing(&desk, &remote, 2, keystone_core::WindowId::FIRST, None)
            .unwrap()
            .value;
        let session = |id| {
            store
                .workspace()
                .sessions()
                .iter()
                .find(|session| session.id() == id)
                .unwrap()
                .clone()
        };
        let recorded = LocalFiles::of(&session(here)).unwrap();
        assert_eq!(recorded.root(), Some(folder.path()));
        assert_eq!(LocalFiles::of(&session(there)), None);

        // Saved by an older build, or by a newer one, as a launch reads them.
        let saved = |context: Option<serde_json::Value>| -> Session {
            let mut value = serde_json::to_value(session(here)).unwrap();
            match context {
                Some(context) => value["execution_context"] = context,
                None => drop(value.as_object_mut().unwrap().remove("execution_context")),
            }
            serde_json::from_value(value).unwrap()
        };
        let unsupported = saved(Some(serde_json::json!({ "version": 99 })));
        assert!(matches!(unsupported.context(), SessionContext::Unsupported));
        assert_eq!(LocalFiles::of(&unsupported), None);
        let legacy = saved(None);
        assert!(matches!(legacy.context(), SessionContext::LegacyMissing));
        let legacy = LocalFiles::of(&legacy).unwrap();
        assert_eq!(legacy.resolve(Path::new("shot.png")), None);
        assert_eq!(
            legacy
                .resolve(Path::new("/tmp/shot.png"))
                .unwrap()
                .as_path(),
            Path::new("/tmp/shot.png")
        );
    }
}
