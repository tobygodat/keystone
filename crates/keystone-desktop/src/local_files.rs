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

use keystone_core::{Attachment, SessionContext};
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
    /// The files of a session working in `context`, or `None` when it works on another
    /// machine or in a folder this version can't read.
    pub(crate) fn of(context: SessionContext<'_>) -> Option<Self> {
        match context {
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
        let folder = keystone_core::WorkingDirectory::new(&std::env::temp_dir()).unwrap();
        let recorded = LocalFiles::of(SessionContext::Recorded {
            directory: &folder,
            origin: keystone_core::ContextOrigin::Created,
        })
        .unwrap();
        assert_eq!(recorded.root(), Some(folder.path()));
        let machine = keystone_core::MachineId::new("desk");
        let remote = keystone_core::RemoteFolder::new("/srv/app").unwrap();
        assert_eq!(
            LocalFiles::of(SessionContext::Remote {
                machine: &machine,
                folder: &remote,
            }),
            None
        );
        assert_eq!(LocalFiles::of(SessionContext::Unsupported), None);
        let legacy = LocalFiles::of(SessionContext::LegacyMissing).unwrap();
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
