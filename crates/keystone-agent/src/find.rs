//! Finding provider programs. Apps launched from a desktop menu often get a shorter
//! `PATH` than a terminal, so common install directories are searched too.

use std::env;
use std::path::{Path, PathBuf};

/// Directories searched after `PATH`, relative to the home directory or absolute.
const EXTRA_DIRS: [&str; 8] = [
    ".local/bin",
    ".local/share/mise/shims",
    ".cargo/bin",
    ".bun/bin",
    ".npm-global/bin",
    "/usr/local/bin",
    "/opt/homebrew/bin",
    "/usr/bin",
];

/// The first executable named `name` on `PATH` or in a common install directory.
pub fn program(name: &str) -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let path_dirs = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let extra = EXTRA_DIRS.iter().filter_map(|dir| {
        if dir.starts_with('/') {
            Some(PathBuf::from(dir))
        } else {
            home.as_ref().map(|home| home.join(dir))
        }
    });
    path_dirs
        .into_iter()
        .chain(extra)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}
