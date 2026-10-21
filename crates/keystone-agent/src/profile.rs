//! How a provider CLI is launched for one account.
//!
//! An account other than the default runs its CLI with a config home of its own:
//! `CLAUDE_CONFIG_DIR` for Claude Code, `CODEX_HOME` for Codex. `HOME` is never
//! changed, so the user's shell, git and SSH setup stay as they are. Credentials an
//! inherited environment variable would supply are removed, since they would
//! override the account's own sign-in. Keystone never reads, writes or passes
//! credentials itself.

use keystone_core::Provider;
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
