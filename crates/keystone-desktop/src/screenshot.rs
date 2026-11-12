//! Screenshot capture into the composer. The user picks a region with the platform's
//! own tool, and the image is attached to the draft like a pasted one: `slurp` and
//! `grim` on Wayland compositors with the wlroots layer-shell and screencopy protocols,
//! such as Sway and Hyprland, and `screencapture -i` on macOS. Debug builds run
//! `KEYSTONE_TEST_SCREENSHOT_COMMAND` instead, with the file to write as its last
//! argument.

use keystone_core::Attachment;
use keystone_core::attachment::Attachments;
use std::fmt::Display;
use std::path::Path;
use std::process::{Command, Output};

gpui::actions!(keystone, [CaptureScreenshot]);

/// The file a screenshot is written to, and the name it is attached as.
const NAME: &str = "Screenshot.png";

/// How this machine captures a region the user picks.
#[derive(Debug, PartialEq)]
enum Tool {
    /// `slurp` prints the picked region and `grim` captures it.
    #[cfg(not(target_os = "macos"))]
    Region {
        slurp: std::path::PathBuf,
        grim: std::path::PathBuf,
    },
    /// A program that lets the user pick, then writes the file named by its last argument.
    Program(Vec<String>),
}

#[derive(Debug, PartialEq)]
enum Shot {
    Taken,
    Cancelled,
    Failed(String),
}

/// `KEYSTONE_TEST_SCREENSHOT_COMMAND` as shell words, in debug builds only: run instead
/// of the platform's tool by [`take`].
pub(crate) fn test_command() -> Option<Vec<String>> {
    #[cfg(debug_assertions)]
    if let Some(words) = std::env::var("KEYSTONE_TEST_SCREENSHOT_COMMAND")
        .ok()
        .and_then(|command| shlex::split(&command))
        .filter(|words| !words.is_empty())
    {
        return Some(words);
    }
    None
}

/// Lets the user pick a region and copies it into `attachments`. Blocks until they do.
/// `None` means they cancelled; an error says what to install or what went wrong.
/// `command`, when given, is run in place of the platform's tool.
pub(crate) fn take(
    attachments: &Attachments,
    command: Option<Vec<String>>,
) -> Option<Result<Attachment, String>> {
    let tool = match command.map_or_else(platform_tool, |words| Ok(Tool::Program(words))) {
        Ok(tool) => tool,
        Err(needed) => return Some(Err(needed.into())),
    };
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => return Some(Err(failed(error))),
    };
    let out = dir.path().join(NAME);
    match tool.capture(&out) {
        Shot::Taken => Some(match attachments.import_file(&out) {
            Ok(Some(image)) => Ok(image),
            Ok(None) => Err("Screenshot failed.".into()),
            Err(reason) => Err(reason),
        }),
        Shot::Cancelled => None,
        Shot::Failed(reason) => Some(Err(reason)),
    }
}

fn failed(detail: impl Display) -> String {
    format!("Screenshot failed. {detail}")
}

#[cfg(target_os = "macos")]
fn platform_tool() -> Result<Tool, &'static str> {
    Ok(Tool::Program(
        ["/usr/sbin/screencapture", "-i", "-x"]
            .map(String::from)
            .into(),
    ))
}

#[cfg(not(target_os = "macos"))]
fn platform_tool() -> Result<Tool, &'static str> {
    let desktop = std::env::var_os("WAYLAND_DISPLAY")
        .map(|_| std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default());
    region(desktop.as_deref(), std::env::var_os("PATH").as_deref())
}

/// `slurp` and `grim` from `path`, or what is missing. `desktop` is
/// `XDG_CURRENT_DESKTOP` in a Wayland session and `None` outside one; GNOME and KDE
/// lack the wlroots protocols both tools need.
#[cfg(not(target_os = "macos"))]
fn region(desktop: Option<&str>, path: Option<&std::ffi::OsStr>) -> Result<Tool, &'static str> {
    let unsupported = desktop.is_none_or(|desktop| {
        desktop
            .split(':')
            .any(|name| name.eq_ignore_ascii_case("GNOME") || name.eq_ignore_ascii_case("KDE"))
    });
    if unsupported {
        return Err("Screenshots need a compositor like Sway or Hyprland.");
    }
    let find = |name: &str| {
        std::env::split_paths(path?)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    };
    match (find("slurp"), find("grim")) {
        (Some(slurp), Some(grim)) => Ok(Tool::Region { slurp, grim }),
        _ => Err("Install grim and slurp."),
    }
}

impl Tool {
    fn capture(&self, out: &Path) -> Shot {
        let output = match self {
            Tool::Program(words) => Command::new(&words[0]).args(&words[1..]).arg(out).output(),
            #[cfg(not(target_os = "macos"))]
            Tool::Region { slurp, grim } => match Command::new(slurp).output() {
                Ok(picked) if picked.status.success() => Command::new(grim)
                    .arg("-g")
                    .arg(String::from_utf8_lossy(&picked.stdout).trim())
                    .arg(out)
                    .output(),
                picked => picked,
            },
        };
        match output {
            Ok(output) => shot(&output, out),
            Err(error) => Shot::Failed(failed(error)),
        }
    }
}

/// A file written by a tool that succeeded is taken. Otherwise the user cancelled when
/// the tool exited cleanly without a word (`screencapture` on Escape) or said so
/// (`slurp` exits 1 with "selection cancelled"); anything else failed, in the tool's
/// own last words. `screencapture` without Screen Recording permission exits cleanly
/// with "could not create image from rect".
fn shot(output: &Output, out: &Path) -> Shot {
    let written = out.metadata().is_ok_and(|file| file.len() > 0);
    if written && output.status.success() {
        return Shot::Taken;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default();
    if said.contains("cancel") || (output.status.success() && said.is_empty()) {
        return Shot::Cancelled;
    }
    Shot::Failed(match said {
        "" => "Screenshot failed.".into(),
        said if said.contains("could not create image") => {
            "Check Keystone's Screen Recording permission in System Settings.".into()
        }
        said => failed(said),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        keystone_core::test_support::write_executable(&path, format!("#!/bin/sh\n{body}\n"));
        path
    }

    fn program(dir: &Path, body: &str) -> Tool {
        Tool::Program(vec![script(dir, "capture", body).to_str().unwrap().into()])
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn grim_and_slurp_are_found_on_path_in_a_wayland_session_or_the_notice_says_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, empty) = (dir.path().join("bin"), dir.path().join("empty"));
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths([&empty, &bin]).unwrap();
        let missing = Err("Install grim and slurp.");
        let hyprland = Some("Hyprland");
        assert_eq!(region(hyprland, Some(&path)), missing);
        assert_eq!(region(hyprland, None), missing);
        let slurp = script(&bin, "slurp", "");
        assert_eq!(
            region(hyprland, Some(&path)),
            missing,
            "grim is still missing"
        );
        let grim = script(&bin, "grim", "");
        let found = Ok(Tool::Region { slurp, grim });
        assert_eq!(region(hyprland, Some(&path)), found);
        assert_eq!(region(Some("sway"), Some(&path)), found);
        assert_eq!(region(Some(""), Some(&path)), found);
        for desktop in [None, Some("ubuntu:GNOME"), Some("KDE")] {
            assert_eq!(
                region(desktop, Some(&path)),
                Err("Screenshots need a compositor like Sway or Hyprland."),
                "{desktop:?}"
            );
        }
    }

    #[test]
    fn a_written_file_is_taken_silence_or_cancel_is_cancelled_and_errors_are_quoted() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join(NAME);
        let cases = [
            ("printf png > \"$1\"", Shot::Taken),
            ("exit 0", Shot::Cancelled),
            (": > \"$1\"", Shot::Cancelled),
            ("echo 'selection cancelled' >&2; exit 1", Shot::Cancelled),
            ("exit 1", Shot::Failed("Screenshot failed.".into())),
            (
                "printf png > \"$1\"; echo 'error writing file' >&2; exit 1",
                Shot::Failed("Screenshot failed. error writing file".into()),
            ),
            (
                "echo 'could not create image from rect' >&2",
                Shot::Failed(
                    "Check Keystone's Screen Recording permission in System Settings.".into(),
                ),
            ),
        ];
        for (body, expected) in cases {
            let _ = std::fs::remove_file(&out);
            assert_eq!(
                (program(dir.path(), body)).capture(&out),
                expected,
                "{body}"
            );
        }
        let gone = Tool::Program(vec![dir.path().join("gone").to_str().unwrap().into()]);
        assert!(
            matches!(gone.capture(&out), Shot::Failed(reason) if reason.starts_with("Screenshot failed. No such file"))
        );
    }

    /// `grim` captures the region `slurp` printed; a cancelled pick never runs `grim`.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn grim_captures_the_region_slurp_picked() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join(NAME);
        let log = dir.path().join("grim.log");
        let grim = script(
            dir.path(),
            "grim",
            &format!("echo \"$@\" > {}; printf png > \"$3\"", log.display()),
        );
        let picked = Tool::Region {
            slurp: script(dir.path(), "slurp", "echo '10,20 300x200'"),
            grim: grim.clone(),
        };
        assert_eq!(picked.capture(&out), Shot::Taken);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap().trim(),
            format!("-g 10,20 300x200 {}", out.display())
        );
        std::fs::remove_file(&out).unwrap();
        std::fs::remove_file(&log).unwrap();
        let cancelled = Tool::Region {
            slurp: script(
                dir.path(),
                "slurp",
                "echo 'selection cancelled' >&2; exit 1",
            ),
            grim,
        };
        assert_eq!(cancelled.capture(&out), Shot::Cancelled);
        assert!(!log.exists(), "grim ran after a cancelled pick");
    }

    /// A region picked but not captured says why, in grim's words.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_failed_grim_after_a_pick_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let unsupported = Tool::Region {
            slurp: script(dir.path(), "slurp", "echo '0,0 10x10'"),
            grim: script(
                dir.path(),
                "grim",
                "echo \"compositor doesn't support wlr-screencopy-unstable-v1\" >&2; exit 1",
            ),
        };
        assert_eq!(
            unsupported.capture(&dir.path().join(NAME)),
            Shot::Failed(
                "Screenshot failed. compositor doesn't support wlr-screencopy-unstable-v1".into()
            )
        );
    }
}
