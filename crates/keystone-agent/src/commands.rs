//! The commands and skills a provider account offers in one folder, for the composer's
//! `/` menu. Claude lists them in its `initialize` answer; Codex lists skills through
//! `skills/list`. What each CLI lists, and what it leaves to its own terminal, is
//! recorded in `docs/research/slash-commands-probe.md`.

use keystone_core::Provider;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

/// Where a command comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandSource {
    BuiltIn,
    User,
    Project,
    Plugin,
    Skill,
}

/// One command or skill, as the composer's menu lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    /// What a message starts with to run it: `/name` for Claude, `$name` for a Codex
    /// skill.
    pub invocation: String,
    pub description: String,
    /// What it takes after its name, as the provider describes it; empty when it
    /// says nothing.
    pub argument_hint: String,
    pub source: CommandSource,
    /// It works only in the provider's own interactive terminal, so Keystone never
    /// sends it.
    pub terminal_only: bool,
}

impl SlashCommand {
    /// The name without its `/` or `$`.
    pub fn name(&self) -> &str {
        &self.invocation[1..]
    }
}

/// The commands an account lists for a folder, or why it could not say.
#[derive(Clone, Debug, PartialEq)]
pub enum Commands {
    Ready(Vec<SlashCommand>),
    Failed(String),
}

/// Built-in commands that need the provider's terminal UI, with what they do. Claude
/// Code 2.1.301 marks these `requires: {ink: true}` and leaves them out of its
/// `initialize` list; Codex's slash commands all belong to its TUI, and these are the
/// common ones. The descriptions follow the CLIs' own.
const CLAUDE_TERMINAL_ONLY: [(&str, &str); 15] = [
    ("login", "Sign in with your Anthropic account"),
    ("logout", "Sign out from your Anthropic account"),
    ("resume", "Resume a previous conversation"),
    ("permissions", "Manage allow and deny tool permission rules"),
    (
        "status",
        "Show Claude Code status including version, model, account, API connectivity, and tool statuses",
    ),
    ("theme", "Change the theme"),
    ("hooks", "View hook configurations for tool events"),
    ("memory", "Edit CLAUDE.md files and memory settings"),
    ("plugin", "Manage Claude Code plugins"),
    ("add-dir", "Add a new working directory"),
    (
        "export",
        "Export the current conversation to a file or clipboard",
    ),
    ("help", "Show help and available commands"),
    ("ide", "Manage IDE integrations and show status"),
    ("terminal-setup", "Set up terminal key bindings"),
    ("statusline", "Set up the status line"),
];

const CODEX_TERMINAL_ONLY: [(&str, &str); 11] = [
    ("model", "choose what model and reasoning effort to use"),
    ("permissions", "choose what Codex is allowed to do"),
    ("review", "review my current changes and find issues"),
    ("new", "start a new chat during a conversation"),
    ("resume", "resume a saved chat"),
    ("fork", "fork the current chat"),
    (
        "init",
        "create an AGENTS.md file with instructions for Codex",
    ),
    (
        "compact",
        "summarize conversation to prevent hitting the context limit",
    ),
    ("diff", "show git diff (including untracked files)"),
    (
        "status",
        "show current session configuration and token usage",
    ),
    (
        "mcp",
        "list MCP tools; use /mcp verbose or /mcp login <name>",
    ),
];

/// The provider's built-in commands that only its own terminal runs.
pub fn terminal_only(provider: Provider) -> Vec<SlashCommand> {
    let list: &[(&str, &str)] = match provider {
        Provider::Claude => &CLAUDE_TERMINAL_ONLY,
        Provider::Codex => &CODEX_TERMINAL_ONLY,
    };
    list.iter()
        .map(|(name, description)| SlashCommand {
            invocation: format!("/{name}"),
            description: (*description).to_owned(),
            argument_hint: String::new(),
            source: CommandSource::BuiltIn,
            terminal_only: true,
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeCommand {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    argument_hint: String,
    #[serde(default)]
    builtin: bool,
}

/// The commands in Claude's `initialize` answer `commands`. Claude marks a built-in
/// with `builtin` and names any other command's source at the end of its
/// description, as `(user)`, `(project)` or `(plugin…)`. A user or project command is
/// a skill when `home/skills/<name>/SKILL.md` or `cwd/.claude/skills/<name>/SKILL.md`
/// exists. Names starting with `__` are Claude's own internals and are left out.
pub fn claude(commands: &Value, home: Option<&Path>, cwd: &Path) -> Vec<SlashCommand> {
    let Ok(commands) = Vec::<ClaudeCommand>::deserialize(commands) else {
        return Vec::new();
    };
    commands
        .into_iter()
        .filter(|c| !c.name.starts_with("__") && !c.name.is_empty())
        .map(|c| {
            let (description, source) = if c.builtin {
                (c.description, CommandSource::BuiltIn)
            } else {
                claude_source(c.description)
            };
            let skill = |root: Option<&Path>| {
                root.is_some_and(|root| {
                    root.join("skills").join(&c.name).join("SKILL.md").is_file()
                })
            };
            let source = match source {
                CommandSource::User if skill(home) => CommandSource::Skill,
                CommandSource::Project if skill(Some(&cwd.join(".claude"))) => CommandSource::Skill,
                source => source,
            };
            SlashCommand {
                invocation: format!("/{}", c.name),
                description,
                argument_hint: c.argument_hint,
                source,
                terminal_only: false,
            }
        })
        .collect()
}

/// Splits Claude's source suffix off a description. One it does not recognise stays
/// in the description, and the command counts as the user's.
fn claude_source(description: String) -> (String, CommandSource) {
    let trimmed = description.trim_end();
    if let Some(open) = trimmed.rfind(" (")
        && trimmed.ends_with(')')
    {
        let source = match &trimmed[open + 2..trimmed.len() - 1] {
            "user" => Some(CommandSource::User),
            "project" => Some(CommandSource::Project),
            tag if tag.starts_with("plugin") => Some(CommandSource::Plugin),
            _ => None,
        };
        if let Some(source) = source {
            return (trimmed[..open].to_owned(), source);
        }
    }
    (description, CommandSource::User)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexSkill {
    name: String,
    #[serde(default)]
    description: String,
    short_description: Option<String>,
    interface: Option<CodexSkillInterface>,
    #[serde(default)]
    scope: String,
    enabled: Option<bool>,
    plugin_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexSkillInterface {
    short_description: Option<String>,
}

/// The enabled skills in Codex's answer to `skills/list`, invoked as `$name`. Codex's
/// own system skills count as built-in and a plugin's as the plugin's.
pub fn codex(result: &Value) -> Vec<SlashCommand> {
    let entries = result["data"].as_array().into_iter().flatten();
    let mut skills: Vec<SlashCommand> = Vec::new();
    for skill in entries.flat_map(|entry| entry["skills"].as_array().into_iter().flatten()) {
        let Ok(skill) = CodexSkill::deserialize(skill) else {
            continue;
        };
        let invocation = format!("${}", skill.name);
        if skill.enabled == Some(false) || skills.iter().any(|s| s.invocation == invocation) {
            continue;
        }
        let description = skill
            .short_description
            .or_else(|| skill.interface.and_then(|i| i.short_description))
            .unwrap_or(skill.description);
        skills.push(SlashCommand {
            invocation,
            description,
            argument_hint: String::new(),
            source: match (skill.plugin_id, skill.scope.as_str()) {
                (Some(_), _) => CommandSource::Plugin,
                (None, "system") => CommandSource::BuiltIn,
                _ => CommandSource::Skill,
            },
            terminal_only: false,
        });
    }
    skills
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(
        invocation: &str,
        description: &str,
        hint: &str,
        source: CommandSource,
    ) -> SlashCommand {
        SlashCommand {
            invocation: invocation.into(),
            description: description.into(),
            argument_hint: hint.into(),
            source,
            terminal_only: false,
        }
    }

    /// The fixture entries Claude Code 2.1.301 listed in the probe, with a skill on
    /// disk for each of the two skills.
    #[test]
    fn claude_sources_come_from_the_suffix_and_skills_from_disk() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        for skill in [
            home.path().join("skills/fixture-skill"),
            cwd.path().join(".claude/skills/fixture-project-skill"),
        ] {
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "---\nname: x\n---\n").unwrap();
        }
        let listed = json!([
            {"name": "fixture-skill", "description": "Fixture user skill (user)", "argumentHint": ""},
            {"name": "fixture-project-skill", "description": "Fixture project skill (project)", "argumentHint": ""},
            {"name": "fixture-ns:deep", "description": "Fixture namespaced user command (user)", "argumentHint": ""},
            {"name": "fixture-project", "description": "Fixture project command (project)", "argumentHint": "<ticket>"},
            {"name": "lint", "description": "Lint it (plugin:tools)"},
            {"name": "odd", "description": "Kept as is (managed)"},
            {"name": "compact", "description": "Free up context by summarizing the conversation so far", "argumentHint": "<optional custom summarization instructions>", "builtin": true},
            {"name": "__remote-workflow", "description": "Internal", "argumentHint": "", "builtin": true},
        ]);
        assert_eq!(
            claude(&listed, Some(home.path()), cwd.path()),
            vec![
                command(
                    "/fixture-skill",
                    "Fixture user skill",
                    "",
                    CommandSource::Skill
                ),
                command(
                    "/fixture-project-skill",
                    "Fixture project skill",
                    "",
                    CommandSource::Skill
                ),
                command(
                    "/fixture-ns:deep",
                    "Fixture namespaced user command",
                    "",
                    CommandSource::User
                ),
                command(
                    "/fixture-project",
                    "Fixture project command",
                    "<ticket>",
                    CommandSource::Project
                ),
                command("/lint", "Lint it", "", CommandSource::Plugin),
                command("/odd", "Kept as is (managed)", "", CommandSource::User),
                command(
                    "/compact",
                    "Free up context by summarizing the conversation so far",
                    "<optional custom summarization instructions>",
                    CommandSource::BuiltIn
                ),
            ]
        );
        assert_eq!(claude(&json!("not a list"), None, cwd.path()), vec![]);
    }

    /// The shapes Codex 0.163.0 answered in the probe.
    #[test]
    fn codex_lists_enabled_skills_once_with_their_short_descriptions() {
        let answer = json!({"data": [{"cwd": "/p", "skills": [
            {"name": "fixture-codex-skill", "description": "Fixture Codex user skill", "path": "/h/skills/fixture-codex-skill/SKILL.md", "scope": "user", "enabled": true, "pluginId": null},
            {"name": "imagegen", "description": "Long text", "interface": {"shortDescription": "Generate or edit images"}, "scope": "system", "enabled": true},
            {"name": "docs", "description": "Long", "shortDescription": "Short", "scope": "system", "enabled": true},
            {"name": "off", "description": "Disabled", "scope": "repo", "enabled": false},
            {"name": "plugged", "description": "From a plugin", "scope": "user", "pluginId": "p@m"},
            {"name": "fixture-codex-skill", "description": "Again", "scope": "repo"},
        ]}]});
        assert_eq!(
            codex(&answer),
            vec![
                command(
                    "$fixture-codex-skill",
                    "Fixture Codex user skill",
                    "",
                    CommandSource::Skill
                ),
                command(
                    "$imagegen",
                    "Generate or edit images",
                    "",
                    CommandSource::BuiltIn
                ),
                command("$docs", "Short", "", CommandSource::BuiltIn),
                command("$plugged", "From a plugin", "", CommandSource::Plugin),
            ]
        );
        assert_eq!(codex(&json!({})), vec![]);
    }

    #[test]
    fn terminal_only_commands_are_built_in_and_never_sent() {
        for provider in [Provider::Claude, Provider::Codex] {
            let list = terminal_only(provider);
            assert!(
                list.iter()
                    .all(|c| c.terminal_only && c.source == CommandSource::BuiltIn)
            );
            assert!(list.iter().any(|c| c.invocation == "/status"));
        }
        assert_eq!(terminal_only(Provider::Claude)[0].name(), "login");
    }
}
