//! What differs between Linux and macOS outside GPUI: where the workspace lives, the
//! environment a Finder launch lacks, and the macOS menu bar.

use std::path::PathBuf;

/// The folder Keystone keeps its workspace in: `$XDG_DATA_HOME/keystone` or
/// `~/.local/share/keystone` on Linux, `~/Library/Application Support/Keystone` on macOS.
pub(crate) fn data_dir() -> Result<PathBuf, String> {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("Set KEYSTONE_STATE_PATH or HOME to select a state location")
    };
    if cfg!(target_os = "macos") {
        return Ok(home()?.join("Library/Application Support/Keystone"));
    }
    match std::env::var_os("XDG_DATA_HOME") {
        Some(base) => Ok(PathBuf::from(base).join("keystone")),
        None => Ok(home()?.join(".local/share/keystone")),
    }
}

/// An app started from Finder, the Dock or `open` gets launchd's environment: a short
/// `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`), none of the variables the user's shell
/// exports, and `/` as its folder. So `claude`, `codex`, `git`, `gh` and what they run,
/// such as `node`, would be missing, and a provider could read another config folder
/// or account than it does from a terminal. This adopts the login shell's environment
/// and starts in the home folder. A launch from a terminal, which sets `TERM`, already
/// has both. Call before any other thread starts.
#[cfg(target_os = "macos")]
pub(crate) fn adopt_login_environment() {
    if std::env::var_os("TERM").is_some() {
        return;
    }
    if std::env::current_dir().is_ok_and(|dir| dir == std::path::Path::new("/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        let _ = std::env::set_current_dir(home);
    }
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
    let Some(variables) = login_environment(&shell) else {
        eprintln!("Could not read the environment of {}", shell.display());
        return;
    };
    for (name, value) in variables {
        if !SHELL_SESSION.iter().any(|session| name == *session) {
            // SAFETY: called first in main, before any other thread exists.
            unsafe { std::env::set_var(name, value) };
        }
    }
}

/// Variables that describe the shell that printed the environment, not the user's.
#[cfg(target_os = "macos")]
const SHELL_SESSION: [&str; 5] = ["PWD", "OLDPWD", "SHLVL", "_", "TERM"];

/// The environment an interactive login `shell` sets up, or `None` when it fails or
/// takes longer than five seconds. Whatever the shell's startup files leave running in
/// its process group is killed; a job that holds its output open cannot stall this.
#[cfg(any(target_os = "macos", test))]
fn login_environment(
    shell: &std::ffi::OsStr,
) -> Option<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c"])
        .arg(format!(
            "printf '%s' {ENV_START}; /usr/bin/env -0; printf '%s' {ENV_END}"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // A session of its own, with no terminal, as under Finder: an interactive shell in
    // a background group of a terminal's session stops when it touches the terminal.
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (chunks, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0; 16 * 1024];
        while let Ok(read @ 1..) = stdout.read(&mut chunk) {
            if chunks.send(chunk[..read].to_vec()).is_err() {
                return;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    let variables = loop {
        if let Some(variables) = parse_login_environment(&output) {
            break Some(variables);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || output.len() > 1 << 22 {
            break None;
        }
        match received.recv_timeout(left) {
            Ok(chunk) => output.extend(chunk),
            Err(_) => break parse_login_environment(&output),
        }
    };
    // SAFETY: the shell is unreaped, so its session's process group ID is still its own.
    unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
    let _ = child.wait();
    variables
}

/// Frame the environment in the shell's output, so whatever its startup and logout
/// files print around it is ignored.
#[cfg(any(target_os = "macos", test))]
const ENV_START: &str = "__KEYSTONE_ENV_START__";
#[cfg(any(target_os = "macos", test))]
const ENV_END: &str = "__KEYSTONE_ENV_END__";

/// The `NAME=value` entries `env -0` printed between the markers, once both arrived.
#[cfg(any(target_os = "macos", test))]
fn parse_login_environment(output: &[u8]) -> Option<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    use std::os::unix::ffi::OsStrExt;
    let find = |haystack: &[u8], needle: &str| {
        haystack
            .windows(needle.len())
            .rposition(|window| window == needle.as_bytes())
    };
    let end = find(output, ENV_END)?;
    let start = find(&output[..end], ENV_START)? + ENV_START.len();
    Some(
        output[start..end]
            .split(|byte| *byte == 0)
            .filter_map(|entry| {
                let split = entry.iter().position(|byte| *byte == b'=')?;
                (split > 0).then(|| {
                    (
                        std::ffi::OsStr::from_bytes(&entry[..split]).to_owned(),
                        std::ffi::OsStr::from_bytes(&entry[split + 1..]).to_owned(),
                    )
                })
            })
            .collect(),
    )
}

#[cfg(target_os = "macos")]
mod menus {
    use crate::{OpenSettings, Quit};
    use gpui::{App, KeyBinding, Menu, MenuItem, SystemMenuType};

    gpui::actions!(keystone, [Hide, HideOthers, ShowAll, Minimize, Zoom]);

    /// The app menu and Window menu every Mac app has. Copy, paste and the rest stay
    /// on their shortcuts, which text fields and the transcript bind to their own
    /// actions.
    pub(crate) fn install(cx: &mut App) {
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.on_action(|_: &Minimize, cx| {
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, _| window.minimize_window());
            }
        });
        cx.on_action(|_: &Zoom, cx| {
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, _| window.zoom_window());
            }
        });
        cx.bind_keys([
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("cmd-alt-h", HideOthers, None),
            KeyBinding::new("cmd-m", Minimize, None),
        ]);
        cx.set_menus([
            Menu::new("Keystone").items([
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide Keystone", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit Keystone", Quit),
            ]),
            Menu::new("Window").items([
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
            ]),
        ]);
    }
}

/// Installs the macOS menu bar. Linux has none.
pub(crate) fn install_menus(cx: &mut gpui::App) {
    #[cfg(target_os = "macos")]
    menus::install(cx);
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_shell_that_never_stops_printing_is_given_up_at_the_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join("shell");
        std::fs::write(
            &shell,
            "#!/bin/sh
while :; do echo noise; done
",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        assert!(super::login_environment(shell.as_os_str()).is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(7));
    }

    use super::*;

    #[test]
    fn the_login_environment_is_read_between_the_markers() {
        let output = b"Welcome!\n__KEYSTONE_ENV_START__PATH=/opt/homebrew/bin:/usr/bin\0EMPTY=\0\
            MULTI=a\nb=c\0__KEYSTONE_ENV_END__logout noise";
        let names: Vec<(String, String)> = parse_login_environment(output)
            .unwrap()
            .into_iter()
            .map(|(name, value)| (name.into_string().unwrap(), value.into_string().unwrap()))
            .collect();
        assert_eq!(
            names,
            [
                ("PATH".into(), "/opt/homebrew/bin:/usr/bin".into()),
                ("EMPTY".into(), "".into()),
                ("MULTI".into(), "a\nb=c".into()),
            ]
        );
        assert_eq!(
            parse_login_environment(b"__KEYSTONE_ENV_START__PATH=/usr/bin\0"),
            None,
            "nothing is taken until the end marker arrives"
        );
        assert_eq!(parse_login_environment(b"no markers"), None);
    }
}
