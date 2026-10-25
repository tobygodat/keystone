use crate::host::{Exit, Process};
use crate::profile::LaunchProfile;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

#[derive(Clone, Copy)]
pub(crate) enum Provider {
    Claude,
    Codex,
}

struct Policy {
    name: &'static str,
    minimum_supported: (u32, u32, u32),
    highest_tested: (u32, u32, u32),
    incompatible_through: (u32, u32, u32),
    missing: &'static str,
    /// The first version checked to host Keystone's `show_ui` tool.
    widgets_from: (u32, u32, u32),
}

impl Provider {
    fn policy(self) -> Policy {
        match self {
            Self::Claude => Policy {
                name: "Claude Code",
                minimum_supported: (2, 1, 300),
                highest_tested: (2, 1, 301),
                incompatible_through: (0, 2, 9),
                missing: "stream-json input and the stdio control protocol",
                widgets_from: (2, 1, 300),
            },
            Self::Codex => Policy {
                name: "Codex",
                minimum_supported: (0, 162, 1),
                highest_tested: (0, 163, 0),
                incompatible_through: (0, 20, 0),
                missing: "the app-server command",
                widgets_from: (0, 163, 0),
            },
        }
    }
}

pub(crate) struct Compatibility {
    pub notice: Option<String>,
    pub cancel_queued: bool,
    /// Whether to offer the `show_ui` tool. A version Keystone cannot read gets none.
    pub widgets: bool,
}

#[derive(Default)]
pub(crate) struct Notices {
    active: HashMap<u64, Option<String>>,
    seen: HashSet<String>,
}

impl Notices {
    pub fn update(&mut self, session: u64, message: Option<String>) -> Option<Option<String>> {
        let previous = self.active.get(&session).cloned().flatten();
        if previous == message {
            return None;
        }
        let shown = message.filter(|message| self.seen.insert(message.clone()));
        self.active.insert(session, shown.clone());
        (previous != shown).then_some(shown)
    }
}

fn parse(provider: Provider, output: &str) -> Option<((u32, u32, u32), bool)> {
    let token = match provider {
        Provider::Claude => output.strip_suffix(" (Claude Code)").unwrap_or(output),
        Provider::Codex => output.strip_prefix("codex-cli ").unwrap_or(output),
    };
    let (base, suffix) = token.split_once(['-', '+']).unwrap_or((token, ""));
    if !suffix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
    {
        return None;
    }
    let mut parts = base.split('.');
    let mut number = || {
        let part = parts.next()?;
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        part.parse::<u32>().ok()
    };
    let version = (number()?, number()?, number()?);
    (parts.next().is_none()).then_some((version, token == base))
}

fn assess(provider: Provider, output: &str, success: bool) -> Result<Compatibility, String> {
    let policy = provider.policy();
    let format_version = |(major, minor, patch)| format!("{major}.{minor}.{patch}");
    let minimum = format_version(policy.minimum_supported);
    let maximum = format_version(policy.highest_tested);
    let tested_range = format!("{minimum}–{maximum}");
    let parsed = success.then(|| parse(provider, output)).flatten();
    if let Some((version, _)) = parsed
        && version <= policy.incompatible_through
    {
        return Err(format!(
            "{} version \"{output}\" is incompatible: it lacks {}. Install a tested CLI version ({tested_range}).",
            policy.name, policy.missing
        ));
    }
    let tested = parsed.is_some_and(|(version, release)| {
        release && version >= policy.minimum_supported && version <= policy.highest_tested
    });
    Ok(Compatibility {
        notice: (!tested).then(|| {
            format!(
                "{} \"{output}\" {}. Supported baseline: {minimum}; tested range: {tested_range}.",
                policy.name,
                match parsed {
                    Some((version, true)) if version > policy.highest_tested =>
                        "is newer than tested",
                    Some(_) => "is untested",
                    None => "could not be verified",
                }
            )
        }),
        cancel_queued: parsed.is_some_and(|(version, _)| version >= policy.minimum_supported),
        widgets: parsed.is_some_and(|(version, _)| version >= policy.widgets_from),
    })
}

pub(crate) fn detect(
    provider: Provider,
    program: &Path,
    cwd: &Path,
    profile: &LaunchProfile,
) -> Result<Compatibility, String> {
    let output = Arc::new(Mutex::new(String::new()));
    let captured = output.clone();
    let (tx, rx) = mpsc::channel();
    let process = Process::spawn(
        program,
        &["--version"],
        cwd,
        profile,
        move |line| {
            let mut text = captured.lock().unwrap();
            if text.len() < 512 {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.extend(
                    String::from_utf8_lossy(line)
                        .chars()
                        .take(160)
                        .map(|c| if c.is_control() { ' ' } else { c }),
                );
            }
        },
        move |exit: Exit| {
            let _ = tx.send(exit);
        },
    )
    .map_err(|e| {
        format!(
            "Could not start {} at {}. Version check failed: {e}",
            provider.policy().name,
            program.display()
        )
    })?;
    let exit = rx.recv_timeout(Duration::from_secs(2));
    drop(process);
    let text = output.lock().unwrap().trim().to_owned();
    let success = exit.as_ref().is_ok_and(|exit| exit.code == Some(0));
    let text = if text.is_empty() {
        match exit {
            Ok(exit) if !exit.stderr_tail.is_empty() => exit
                .stderr_tail
                .chars()
                .filter(|c| !c.is_control())
                .take(160)
                .collect(),
            Ok(_) => "unavailable (empty --version output)".into(),
            Err(_) => "unavailable (--version timed out)".into(),
        }
    } else {
        text
    };
    assess(provider, &text, success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_are_once_per_version_per_connection_run_and_clear_when_changed() {
        let mut notices = Notices::default();
        assert_eq!(
            notices.update(1, Some("newer".into())),
            Some(Some("newer".into()))
        );
        assert_eq!(notices.update(1, Some("newer".into())), None);
        assert_eq!(notices.update(2, Some("newer".into())), None);
        assert_eq!(notices.update(1, None), Some(None));
        assert_eq!(notices.update(1, Some("newer".into())), None);
        assert_eq!(
            notices.update(2, Some("another".into())),
            Some(Some("another".into()))
        );
    }

    #[test]
    fn newer_versions_keep_the_baseline_until_the_handshake_overrides_it() {
        assert!(
            assess(Provider::Claude, "2.1.302 (Claude Code)", true)
                .unwrap()
                .cancel_queued
        );
        assert!(
            assess(Provider::Codex, "codex-cli 0.163.1", true)
                .unwrap()
                .notice
                .is_some()
        );
        assert!(
            assess(Provider::Claude, "2.1.301 (Claude Code)", true)
                .unwrap()
                .notice
                .is_none()
        );
        assert!(
            assess(Provider::Codex, "codex-cli 0.163.0", true)
                .unwrap()
                .notice
                .is_none()
        );
    }

    #[test]
    fn only_recognized_release_output_is_tested() {
        for (provider, tested) in [
            (Provider::Claude, "2.1.300 (Claude Code)"),
            (Provider::Codex, "codex-cli 0.162.1"),
        ] {
            assert!(assess(provider, tested, true).unwrap().notice.is_none());
            assert!(assess(provider, tested, false).unwrap().notice.is_some());
            for output in [
                "",
                "garbage",
                "2.1.300-beta.1",
                "0.162.1-beta.1",
                "prefix 2.1.300",
                "2.1.300.1",
                "2.1.300-!",
                "2.1.300\n0.162.1",
            ] {
                assert!(assess(provider, output, true).unwrap().notice.is_some());
            }
        }
    }
}
