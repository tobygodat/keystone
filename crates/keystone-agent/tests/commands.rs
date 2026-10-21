//! The commands and skills each provider account lists for a folder, through the real
//! process host and the scripted providers, with fixture config homes.

use keystone_agent::{
    ClaudeConnection, CodexConnection, CommandSource, Commands, Connection, Event, LaunchProfile,
    SlashCommand,
};
use keystone_core::Provider;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

fn wake() -> (keystone_agent::Wake, Receiver<()>) {
    let (tx, rx) = channel();
    (
        Arc::new(move || {
            let _ = tx.send(());
        }),
        rx,
    )
}

/// The first commands answer, after any other events.
fn commands(drain: impl Fn() -> Vec<(u64, Event)>, wake: &Receiver<()>) -> Commands {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        for (session, event) in drain() {
            if let Event::Commands(commands) = event {
                assert_eq!(session, 1);
                return commands;
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(wake.recv_timeout(left).is_ok(), "no commands arrived");
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn listed(commands: Commands) -> Vec<(String, String, String, CommandSource)> {
    let Commands::Ready(commands) = commands else {
        panic!("{commands:?}");
    };
    commands
        .into_iter()
        .map(|c: SlashCommand| (c.invocation, c.description, c.argument_hint, c.source))
        .collect()
}

fn row(
    invocation: &str,
    description: &str,
    hint: &str,
    source: CommandSource,
) -> (String, String, String, CommandSource) {
    (invocation.into(), description.into(), hint.into(), source)
}

/// The account's own commands and skills and the folder's are listed with their
/// sources, from the catalog probe's one `initialize` answer.
#[test]
fn a_claude_account_lists_its_commands_and_skills_and_the_folders() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("claude-work");
    let project = root.path().join("project");
    write(
        &home.join("commands/fixture-review.md"),
        "---\ndescription: Fixture user command\nargument-hint: [file]\n---\nReview $ARGUMENTS\n",
    );
    write(
        &home.join("skills/fixture-skill/SKILL.md"),
        "---\nname: fixture-skill\ndescription: Fixture user skill\n---\nSkill.\n",
    );
    write(
        &project.join(".claude/commands/fixture-project.md"),
        "---\ndescription: Fixture project command\nargument-hint: <ticket>\n---\nProject\n",
    );
    let (wake, rx) = wake();
    let connection = ClaudeConnection::with_profile(
        Some(PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-claude"))),
        LaunchProfile::account(Provider::Claude, "Work", &home),
        wake,
    );
    connection.request_catalog(1, project.to_str().unwrap());
    assert_eq!(
        listed(commands(|| connection.drain(), &rx)),
        [
            row(
                "/fixture-skill",
                "Fixture user skill",
                "",
                CommandSource::Skill
            ),
            row(
                "/fixture-review",
                "Fixture user command",
                "file",
                CommandSource::User
            ),
            row(
                "/fixture-project",
                "Fixture project command",
                "<ticket>",
                CommandSource::Project
            ),
            row(
                "/compact",
                "Free up context by summarizing the conversation so far",
                "<optional custom summarization instructions>",
                CommandSource::BuiltIn
            ),
            row(
                "/context",
                "Show current context usage",
                "",
                CommandSource::BuiltIn
            ),
        ]
    );
}

/// Codex lists the account's skills and the folder's through `skills/list`, asked
/// with the catalog.
#[test]
fn a_codex_account_lists_its_skills_and_the_folders() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex-work");
    let project = root.path().join("project");
    write(
        &home.join("skills/fixture-codex-skill/SKILL.md"),
        "---\nname: fixture-codex-skill\ndescription: Fixture Codex user skill\n---\nSkill.\n",
    );
    write(
        &project.join(".agents/skills/fixture-repo-skill/SKILL.md"),
        "---\nname: fixture-repo-skill\ndescription: Fixture repo skill\n---\nSkill.\n",
    );
    let (wake, rx) = wake();
    let connection = CodexConnection::with_profile(
        Some(PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-codex"))),
        "test",
        LaunchProfile::account(Provider::Codex, "Work", &home),
        wake,
    );
    Connection::request_catalog(&connection, 1, project.to_str().unwrap());
    assert_eq!(
        listed(commands(|| connection.drain(), &rx)),
        [
            row(
                "$fixture-codex-skill",
                "Fixture Codex user skill",
                "",
                CommandSource::Skill
            ),
            row(
                "$fixture-repo-skill",
                "Fixture repo skill",
                "",
                CommandSource::Skill
            ),
            row(
                "$fake-system-skill",
                "Scripted system skill",
                "",
                CommandSource::BuiltIn
            ),
        ]
    );
}

/// A Codex that cannot start fails the commands as it fails the catalog.
#[test]
fn a_codex_that_cannot_start_fails_its_commands() {
    let (wake, rx) = wake();
    let connection = CodexConnection::with_profile(
        Some(PathBuf::from("/nonexistent/codex")),
        "test",
        LaunchProfile::inherited(),
        wake,
    );
    connection.request_commands(1, "/tmp");
    assert!(matches!(
        commands(|| connection.drain(), &rx),
        Commands::Failed(message) if message.contains("/nonexistent/codex")
    ));
}
