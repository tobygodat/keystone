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

/// The provider CLIs a test never searches for: on a developer's machine the ones found
/// are the user's real `claude` and `codex`, with their logins and config.
const PROVIDERS: [&str; 2] = ["claude", "codex"];

/// Turns off this process's search for `claude` and `codex`, so a test that would start
/// the user's real CLI stops instead (TOB-363). Only test setup calls it:
/// `keystone_core::test_support::built_binary`, which every test that runs the scripted
/// providers uses, and the desktop's test builds. The app never does.
pub fn refuse_path_search() {
    keystone_core::provider_search::refuse();
}

/// The first executable named `name` on `PATH` or in a common install directory.
///
/// This crate's unit tests, and any process whose test setup called
/// [`refuse_path_search`], never look for a provider CLI. They stop the whole process
/// instead, because the search usually runs on a connection's own thread, where a panic
/// would only end that thread and the test could still pass.
pub fn program(name: &str) -> Option<PathBuf> {
    if PROVIDERS.contains(&name) && (cfg!(test) || keystone_core::provider_search::refused()) {
        refuse(name);
    }
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

/// Stops a test process that was about to search `PATH` for the provider `name`. The
/// message goes straight to stderr, past the test harness's output capture, which a
/// process that aborts never prints.
fn refuse(name: &str) -> ! {
    use std::io::Write;
    let thread = std::thread::current();
    let message = format!(
        "\nA test tried to find `{name}` on PATH, which would start the user's real {name} \
         (TOB-363). Name a scripted provider before anything asks for one: in \
         keystone-desktop, `AppState::test_programs` from `test_support`; elsewhere, the \
         path of keystone-fake-claude or keystone-fake-codex.\n\
         Thread: {}\n{}\n",
        thread.name().unwrap_or("<unnamed>"),
        std::backtrace::Backtrace::force_capture()
    );
    let _ = std::io::stderr().lock().write_all(message.as_bytes());
    std::process::abort();
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
