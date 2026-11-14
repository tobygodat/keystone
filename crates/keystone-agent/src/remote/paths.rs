//! Where `keystone-host` keeps its socket, lock and journal, and the check that only
//! this user can reach them.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

pub(crate) struct Paths {
    /// Holds `host.sock` and `host.lock`. Only this user may open it.
    pub run: PathBuf,
    /// Holds the journal and the daemon's log.
    pub state: PathBuf,
}

impl Paths {
    /// `KEYSTONE_HOST_DIR` holds everything when set, for tests. Otherwise the socket is
    /// under `$XDG_RUNTIME_DIR/keystone-host` or `~/.keystone-host/run`, and the journal
    /// under `$XDG_STATE_HOME/keystone-host` (`~/.local/state/keystone-host`), or
    /// `~/Library/Application Support/Keystone Host` on macOS.
    pub fn from_env() -> Result<Self, String> {
        let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        if let Some(dir) = var("KEYSTONE_HOST_DIR") {
            let dir = PathBuf::from(dir);
            return Ok(Self {
                run: dir.join("run"),
                state: dir,
            });
        }
        let home = var("HOME")
            .map(PathBuf::from)
            .ok_or("keystone-host needs HOME or KEYSTONE_HOST_DIR to find its folders")?;
        let run = match var("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir).join("keystone-host"),
            None => home.join(".keystone-host/run"),
        };
        let state = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Keystone Host")
        } else {
            match var("XDG_STATE_HOME") {
                Some(dir) => PathBuf::from(dir).join("keystone-host"),
                None => home.join(".local/state/keystone-host"),
            }
        };
        Ok(Self { run, state })
    }

    pub fn socket(&self) -> PathBuf {
        self.run.join("host.sock")
    }

    pub fn lock(&self) -> PathBuf {
        self.run.join("host.lock")
    }

    pub fn journal(&self) -> PathBuf {
        self.state.join("journal.sqlite3")
    }

    pub fn log(&self) -> PathBuf {
        self.state.join("host.log")
    }

    /// The images the app sent, by hash.
    pub fn attachments(&self) -> PathBuf {
        self.state.join("attachments")
    }
}

/// Creates `dir` with mode 0700 if it is missing, then refuses it unless it is a real
/// directory this user owns and no other user can open.
pub(crate) fn private_dir(dir: &Path) -> Result<(), String> {
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("Could not create {}: {e}", dir.display()))?,
        Err(error) => return Err(format!("Could not read {}: {error}", dir.display())),
        Ok(_) => {}
    }
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // SAFETY: geteuid only reads.
    let me = unsafe { libc::geteuid() };
    if !meta.is_dir() {
        Err(format!("{} is not a directory", dir.display()))
    } else if meta.uid() != me {
        Err(format!("{} belongs to another user", dir.display()))
    } else if meta.mode() & 0o077 != 0 {
        Err(format!(
            "{} can be opened by other users (mode {:o}); it must be 0700",
            dir.display(),
            meta.mode() & 0o777
        ))
    } else {
        Ok(())
    }
}
