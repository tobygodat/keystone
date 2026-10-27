//! How a provider CLI is launched for one account.
//!
//! An account other than the default runs its CLI with a config home of its own:
//! `CLAUDE_CONFIG_DIR` for Claude Code, `CODEX_HOME` for Codex. `HOME` is never
//! changed, so the user's shell, git and SSH setup stay as they are. Credentials an
//! inherited environment variable would supply are removed, since they would
//! override the account's own sign-in. Keystone never reads, writes or passes
//! credentials itself.

use keystone_core::Provider;
use keystone_core::integrations::Location;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Inherited variables that sign Claude Code in regardless of its config dir.
const CLAUDE_CREDENTIALS: [&str; 3] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];
/// Inherited variables that route Claude Code to a cloud provider's credentials
/// instead of the signed-in account.
const CLAUDE_CLOUD_ROUTING: [&str; 3] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];
/// Inherited variables that sign Codex in regardless of its home.
const CODEX_CREDENTIALS: [&str; 2] = ["OPENAI_API_KEY", "CODEX_API_KEY"];
/// Everything removed for a Claude account.
const CLAUDE_REMOVED: [&str; 6] = [
    CLAUDE_CREDENTIALS[0],
    CLAUDE_CREDENTIALS[1],
    CLAUDE_CREDENTIALS[2],
    CLAUDE_CLOUD_ROUTING[0],
    CLAUDE_CLOUD_ROUTING[1],
    CLAUDE_CLOUD_ROUTING[2],
];

/// The environment every process Keystone starts for one account gets: the CLI
/// itself, its version check, and its model and permission probes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LaunchProfile {
    /// Variables to set, or with `None` to remove, over the inherited environment.
    env: Vec<(&'static str, Option<OsString>)>,
    /// How to sign this account in, for a signed-out error.
    sign_in: Option<String>,
}

impl LaunchProfile {
    /// The default account: the environment Keystone inherited, unchanged.
    pub fn inherited() -> Self {
        Self::default()
    }

    /// An account whose CLI uses `home`, which must be absolute, as its config home.
    pub fn account(provider: Provider, label: &str, home: &Path) -> Self {
        let (variable, credentials): (_, &[&str]) = match provider {
            Provider::Claude => ("CLAUDE_CONFIG_DIR", &CLAUDE_REMOVED),
            Provider::Codex => ("CODEX_HOME", &CODEX_CREDENTIALS),
        };
        let mut env = vec![(variable, Some(home.as_os_str().to_owned()))];
        env.extend(credentials.iter().map(|name| (*name, None)));
        Self {
            env,
            sign_in: Some(format!(
                "The account \u{201c}{label}\u{201d} uses {}. Sign it in from a terminal with `{}`",
                home.display(),
                sign_in_command(provider, Some(home))
            )),
        }
    }

    /// The variables this profile sets or removes.
    pub fn env(&self) -> &[(&'static str, Option<OsString>)] {
        &self.env
    }

    /// The error for a provider CLI that is not installed, ending with how to sign
    /// this account in once it is.
    pub fn not_installed(&self, provider: Provider) -> String {
        let (name, cli) = match provider {
            Provider::Claude => ("Claude Code", "claude"),
            Provider::Codex => ("Codex", "codex"),
        };
        match &self.sign_in {
            Some(sign_in) => format!("{name} is not installed. Install the {cli} CLI. {sign_in}."),
            None => format!(
                "{name} is not installed. Install the {cli} CLI and sign in with `{}`.",
                sign_in_command(provider, None)
            ),
        }
    }

    /// How to sign this account in, or `None` for the default account.
    pub fn sign_in(&self) -> Option<&str> {
        self.sign_in.as_deref()
    }

    pub(crate) fn apply(&self, command: &mut Command) {
        for (name, value) in &self.env {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
    }
}

/// The command a user runs in a terminal to sign an account in: with `home`, the one
/// for that account's config home; without, the default account's.
pub fn sign_in_command(provider: Provider, home: Option<&Path>) -> String {
    let (variable, command) = match provider {
        Provider::Claude => ("CLAUDE_CONFIG_DIR", "claude auth login"),
        Provider::Codex => ("CODEX_HOME", "codex login"),
    };
    match home {
        Some(home) => format!(
            "{variable}={} {command}",
            shell_quote(&home.to_string_lossy())
        ),
        None => command.to_owned(),
    }
}

/// The sign-in command for the default account, naming its home when Keystone
/// inherited `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, so a fresh terminal signs in the
/// same folder.
pub fn default_sign_in_command(provider: Provider) -> String {
    let variable = match provider {
        Provider::Claude => "CLAUDE_CONFIG_DIR",
        Provider::Codex => "CODEX_HOME",
    };
    let inherited = std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    sign_in_command(provider, inherited.as_deref())
}

/// The command that opens the provider's own terminal UI in `folder` on the account
/// whose home is `home`, without the inherited credentials [`LaunchProfile::account`]
/// removes, so they cannot sign it in as someone else. The default account (`None`)
/// keeps the terminal's environment and names its home only when Keystone inherited
/// `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, so a fresh terminal uses the same one.
pub fn terminal_command(provider: Provider, home: Option<&Path>, folder: &Path) -> String {
    let (variable, program, removed): (_, _, &[&str]) = match provider {
        Provider::Claude => ("CLAUDE_CONFIG_DIR", "claude", &CLAUDE_REMOVED),
        Provider::Codex => ("CODEX_HOME", "codex", &CODEX_CREDENTIALS),
    };
    let named = |home: &Path| {
        format!(
            "{variable}={} {program}",
            shell_quote(&home.to_string_lossy())
        )
    };
    let open = match home {
        Some(home) => {
            let unset: String = removed.iter().map(|name| format!("-u {name} ")).collect();
            format!("env {unset}{}", named(home))
        }
        None => std::env::var_os(variable)
            .filter(|value| !value.is_empty())
            .map_or_else(|| program.to_owned(), |home| named(Path::new(&home))),
    };
    format!("cd {} && {open}", shell_quote(&folder.to_string_lossy()))
}

/// The home the provider's default account uses: the inherited `CLAUDE_CONFIG_DIR`
/// or `CODEX_HOME`, else `~/.claude` or `~/.codex`. No other account may overlap it.
pub fn default_home(provider: Provider) -> Option<PathBuf> {
    let (variable, folder) = match provider {
        Provider::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
        Provider::Codex => ("CODEX_HOME", ".codex"),
    };
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(folder)))
}

/// The variables that decide where a provider keeps an account's files. Captured once
/// from Keystone's environment, so a test can name fixture folders instead.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigEnv {
    pub claude_config_dir: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

impl ConfigEnv {
    /// `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `HOME` as Keystone inherited them.
    pub fn inherited() -> Self {
        let var = |name| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
            codex_home: var("CODEX_HOME"),
            home: var("HOME"),
        }
    }

    /// Where the account whose home is `home` keeps its files; `None` as the home is
    /// the default account, which uses the inherited folder. Claude keeps
    /// `.claude.json` in its config dir when `CLAUDE_CONFIG_DIR` is set, else in `~`.
    /// `None` when the default account's folder is unknown.
    pub fn location(&self, provider: Provider, home: Option<&Path>) -> Option<Location> {
        match provider {
            Provider::Claude => {
                let (config_dir, global_file) = match home.or(self.claude_config_dir.as_deref()) {
                    Some(dir) => (dir.to_owned(), dir.join(".claude.json")),
                    None => {
                        let user = self.home.as_deref()?;
                        (user.join(".claude"), user.join(".claude.json"))
                    }
                };
                Some(Location::Claude {
                    config_dir,
                    global_file,
                })
            }
            Provider::Codex => {
                let home = match home.or(self.codex_home.as_deref()) {
                    Some(home) => home.to_owned(),
                    None => self.home.as_deref()?.join(".codex"),
                };
                Some(Location::Codex {
                    home,
                    user_home: self.home.clone(),
                })
            }
        }
    }
}

/// Runs the provider's own CLI for a config change, such as `claude mcp add-json`, as
/// the account `profile` describes. `program` is the CLI to run, else the one found
/// on `PATH`. An error is the CLI's last line of output, or why it did not run.
pub fn run_config_cli(
    provider: Provider,
    program: Option<&Path>,
    profile: &LaunchProfile,
    cli: &keystone_core::integrations::edit::Cli,
) -> Result<(), String> {
    let name = match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    let program = match program {
        Some(program) => program.to_owned(),
        None => crate::find::program(name).ok_or_else(|| profile.not_installed(provider))?,
    };
    let mut command = Command::new(program);
    profile.apply(&mut command);
    let mut child = command
        .args(&cli.args)
        .current_dir(&cli.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("{name} did not start: {error}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{name} did not finish in a minute."));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let text = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .map(str::to_owned)
    };
    Err(text(&output.stderr)
        .or_else(|| text(&output.stdout))
        .unwrap_or_else(|| format!("{name} failed ({})", output.status)))
}

/// `text` as one shell word.
fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@%,".contains(c))
    {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_locations_follow_each_providers_own_rules() {
        let env = ConfigEnv {
            claude_config_dir: None,
            codex_home: None,
            home: Some("/h".into()),
        };
        assert_eq!(
            env.location(Provider::Claude, None),
            Some(Location::Claude {
                config_dir: "/h/.claude".into(),
                global_file: "/h/.claude.json".into(),
            })
        );
        assert_eq!(
            env.location(Provider::Claude, Some(Path::new("/w"))),
            Some(Location::Claude {
                config_dir: "/w".into(),
                global_file: "/w/.claude.json".into(),
            })
        );
        assert_eq!(
            env.location(Provider::Codex, None),
            Some(Location::Codex {
                home: "/h/.codex".into(),
                user_home: Some("/h".into()),
            })
        );
        let env = ConfigEnv {
            claude_config_dir: Some("/c".into()),
            codex_home: Some("/x".into()),
            home: None,
        };
        assert_eq!(
            env.location(Provider::Claude, None),
            Some(Location::Claude {
                config_dir: "/c".into(),
                global_file: "/c/.claude.json".into(),
            })
        );
        assert_eq!(
            env.location(Provider::Codex, Some(Path::new("/y"))),
            Some(Location::Codex {
                home: "/y".into(),
                user_home: None,
            })
        );
        assert_eq!(ConfigEnv::default().location(Provider::Codex, None), None);
        assert_eq!(ConfigEnv::default().location(Provider::Claude, None), None);
    }

    #[test]
    fn an_account_sets_only_its_own_home_and_drops_inherited_credentials() {
        let claude = LaunchProfile::account(Provider::Claude, "Work", Path::new("/h/.claude-work"));
        assert_eq!(
            claude.env(),
            [
                ("CLAUDE_CONFIG_DIR", Some("/h/.claude-work".into())),
                ("ANTHROPIC_API_KEY", None),
                ("ANTHROPIC_AUTH_TOKEN", None),
                ("CLAUDE_CODE_OAUTH_TOKEN", None),
                ("CLAUDE_CODE_USE_BEDROCK", None),
                ("CLAUDE_CODE_USE_VERTEX", None),
                ("CLAUDE_CODE_USE_FOUNDRY", None),
            ]
        );
        assert!(!claude.env().iter().any(|(name, _)| *name == "HOME"));
        let codex = LaunchProfile::account(Provider::Codex, "Work", Path::new("/h/codex work"));
        assert_eq!(codex.env()[0], ("CODEX_HOME", Some("/h/codex work".into())));
        assert_eq!(
            codex.sign_in(),
            Some(
                "The account \u{201c}Work\u{201d} uses /h/codex work. Sign it in from a terminal with `CODEX_HOME='/h/codex work' codex login`"
            )
        );
        assert!(LaunchProfile::inherited().env().is_empty());
        assert_eq!(LaunchProfile::inherited().sign_in(), None);
    }

    #[test]
    fn sign_in_commands_quote_the_home_as_one_shell_word() {
        assert_eq!(
            sign_in_command(Provider::Claude, Some(Path::new("/h/.claude-work"))),
            "CLAUDE_CONFIG_DIR=/h/.claude-work claude auth login"
        );
        assert_eq!(
            sign_in_command(Provider::Claude, Some(Path::new("/h/it's $HOME"))),
            r"CLAUDE_CONFIG_DIR='/h/it'\''s $HOME' claude auth login"
        );
        assert_eq!(sign_in_command(Provider::Codex, None), "codex login");
    }

    #[test]
    fn terminal_commands_open_the_cli_in_the_folder_on_the_account() {
        assert_eq!(
            terminal_command(
                Provider::Claude,
                Some(Path::new("/h/.claude-work")),
                Path::new("/src/it's here")
            ),
            concat!(
                r"cd '/src/it'\''s here' && env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN ",
                "-u CLAUDE_CODE_OAUTH_TOKEN -u CLAUDE_CODE_USE_BEDROCK -u CLAUDE_CODE_USE_VERTEX ",
                "-u CLAUDE_CODE_USE_FOUNDRY CLAUDE_CONFIG_DIR=/h/.claude-work claude"
            )
        );
        assert_eq!(
            terminal_command(
                Provider::Codex,
                Some(Path::new("/h/codex")),
                Path::new("/src")
            ),
            "cd /src && env -u OPENAI_API_KEY -u CODEX_API_KEY CODEX_HOME=/h/codex codex"
        );
    }

    #[test]
    fn a_missing_cli_names_how_to_sign_in_the_account_it_was_for() {
        assert_eq!(
            LaunchProfile::inherited().not_installed(Provider::Codex),
            "Codex is not installed. Install the codex CLI and sign in with `codex login`."
        );
        let work = LaunchProfile::account(Provider::Codex, "Work", Path::new("/h/.codex-work"));
        assert_eq!(
            work.not_installed(Provider::Codex),
            "Codex is not installed. Install the codex CLI. The account \u{201c}Work\u{201d} uses /h/.codex-work. Sign it in from a terminal with `CODEX_HOME=/h/.codex-work codex login`."
        );
    }
}
