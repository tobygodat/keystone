//! Forking a conversation at the second of three turns, through the full stack against
//! the scripted providers in store mode: each turn names its fork point, the fork
//! opens with the provider's own fork of the source through that turn, and the source
//! is left as it was.

use keystone_agent::{
    ApprovalState, ClaudeConnection, CodexConnection, Connection, Event, LaunchProfile, TurnStatus,
};
use keystone_core::ModelChoice;
use keystone_core::{ApprovalMode, ForkPoint, Provider};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const PROVIDER: ApprovalState = ApprovalState {
    mode: ApprovalMode::Provider,
    overridden: false,
};

fn wake() -> (keystone_agent::Wake, Receiver<()>) {
    let (tx, rx) = channel();
    (
        Arc::new(move || {
            let _ = tx.send(());
        }),
        rx,
    )
}

/// Events until `session`'s turn ends, keeping every session's.
fn turn(conn: &dyn Connection, wake: &Receiver<()>, session: u64) -> Vec<(u64, Event)> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut events = Vec::new();
    loop {
        let batch = conn.drain();
        let finished = batch
            .iter()
            .any(|(s, e)| *s == session && matches!(e, Event::TurnCompleted { .. }));
        events.extend(batch);
        if finished {
            return events;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(wake.recv_timeout(left).is_ok(), "timed out; got {events:?}");
    }
}

fn fork_point(events: &[(u64, Event)], session: u64) -> ForkPoint {
    let points: Vec<_> = events
        .iter()
        .filter_map(|(s, e)| match e {
            Event::ForkPoint(point) if *s == session => Some(point.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(points.len(), 1, "{events:?}");
    points[0].clone()
}

fn thread(events: &[(u64, Event)], session: u64) -> Option<String> {
    events.iter().find_map(|(s, e)| match e {
        Event::ThreadReady { thread_id } if *s == session => Some(thread_id.clone()),
        _ => None,
    })
}

fn wrapper(dir: &Path, program: &str, env: &[(&str, String)]) -> PathBuf {
    let script = dir.join("wrapper");
    let exports: String = env
        .iter()
        .map(|(name, value)| format!("export {name}='{value}'\n"))
        .collect();
    keystone_core::test_support::write_executable(
        &script,
        format!("#!/bin/sh\n{exports}exec '{program}' \"$@\"\n"),
    );
    script
}

fn records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn uuids(records: &[Value]) -> Vec<String> {
    records
        .iter()
        .map(|r| r["uuid"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_claude_fork_resumes_the_source_at_the_turn_s_last_record_and_leaves_it_unchanged() {
    // Canonical, like the folders Keystone records: macOS's temporary folder sits
    // behind a symlink.
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let config = dir.path().join("claude-home");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let argv = dir.path().join("argv");
    std::fs::write(&argv, "").unwrap();
    let program = wrapper(
        dir.path(),
        env!("CARGO_BIN_EXE_keystone-fake-claude"),
        &[
            ("KEYSTONE_FAKE_CLAUDE_STORE", "1".into()),
            ("KEYSTONE_FAKE_CLAUDE_DELAY_MS", "2".into()),
            ("KEYSTONE_FAKE_CLAUDE_ARGV_LOG", argv.display().to_string()),
        ],
    );
    let (wake, rx) = wake();
    let conn = ClaudeConnection::with_profile(
        Some(program),
        LaunchProfile::account(Provider::Claude, "Work", &config),
        wake,
    );
    let cwd_text = cwd.to_str().unwrap();
    let mut points = Vec::new();
    let mut source = None;
    for text in ["first", "second", "third"] {
        conn.send(
            1,
            cwd_text,
            None,
            &ModelChoice::default(),
            PROVIDER,
            text.into(),
        )
        .unwrap();
        let events = turn(&conn, &rx, 1);
        source = source.or(thread(&events, 1));
        points.push(fork_point(&events, 1));
    }
    let source = source.unwrap();
    assert!(points.iter().all(|p| p.thread == source), "{points:?}");
    let folder = cwd_text.replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    let store = config.join("projects").join(folder);
    let source_file = store.join(format!("{source}.jsonl"));
    let before = std::fs::read(&source_file).unwrap();
    let source_records = records(&source_file);
    let cut = source_records
        .iter()
        .position(|r| r["uuid"].as_str() == Some(&points[1].at))
        .expect("the point names a stored record");
    assert!(
        source_records[cut]["message"]["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("You wrote: second")),
        "the second turn's point is its reply: {}",
        source_records[cut]
    );

    conn.set_fork(2, points[1].clone());
    conn.send(
        2,
        cwd_text,
        None,
        &ModelChoice::default(),
        PROVIDER,
        "fork".into(),
    )
    .unwrap();
    let events = turn(&conn, &rx, 2);
    let fork = thread(&events, 2).unwrap();
    assert_ne!(fork, source);
    assert!(events.iter().all(|(s, _)| *s == 2), "{events:?}");
    let launches: Vec<Vec<String>> = std::fs::read_to_string(&argv)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let forked = launches.last().unwrap();
    assert_eq!(
        forked[forked.len() - 5..],
        [
            "--resume",
            source.as_str(),
            "--fork-session",
            "--resume-session-at",
            points[1].at.as_str()
        ]
    );
    assert_eq!(fork_point(&events, 2).thread, fork);

    // The fork holds the source's records through the second turn, then its own.
    let fork_records = records(&store.join(format!("{fork}.jsonl")));
    assert_eq!(uuids(&fork_records[..=cut]), uuids(&source_records[..=cut]));
    assert!(fork_records.len() > cut + 1);
    assert_eq!(std::fs::read(&source_file).unwrap(), before);

    // Later messages go to the fork's own process, not a new fork.
    conn.send(
        2,
        cwd_text,
        None,
        &ModelChoice::default(),
        PROVIDER,
        "again".into(),
    )
    .unwrap();
    turn(&conn, &rx, 2);
    let launched = std::fs::read_to_string(&argv).unwrap().lines().count();
    assert_eq!(launched, launches.len());
    assert_eq!(std::fs::read(&source_file).unwrap(), before);
}

#[test]
fn a_claude_fork_of_a_conversation_claude_lacks_fails_without_starting_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("claude-home");
    std::fs::create_dir_all(&config).unwrap();
    let program = wrapper(
        dir.path(),
        env!("CARGO_BIN_EXE_keystone-fake-claude"),
        &[("KEYSTONE_FAKE_CLAUDE_STORE", "1".into())],
    );
    let (wake, rx) = wake();
    let conn = ClaudeConnection::with_profile(
        Some(program),
        LaunchProfile::account(Provider::Claude, "Work", &config),
        wake,
    );
    conn.set_fork(
        2,
        ForkPoint {
            thread: "gone".into(),
            at: "entry".into(),
        },
    );
    conn.send(
        2,
        dir.path().to_str().unwrap(),
        None,
        &ModelChoice::default(),
        PROVIDER,
        "fork".into(),
    )
    .unwrap();
    let events = turn(&conn, &rx, 2);
    let message = events
        .iter()
        .find_map(|(_, e)| match e {
            Event::TurnCompleted {
                status: TurnStatus::Failed,
                error: Some(message),
                ..
            } => Some(message.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        message.starts_with(
            "Could not fork this Claude Code conversation: No conversation found with session ID: gone"
        ),
        "{message}"
    );
    assert!(thread(&events, 2).is_none(), "{events:?}");
}

#[test]
fn a_codex_fork_keeps_the_source_s_turns_through_its_point_and_leaves_it_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    let methods = dir.path().join("methods");
    let program = wrapper(
        dir.path(),
        env!("CARGO_BIN_EXE_keystone-fake-codex"),
        &[
            ("KEYSTONE_FAKE_CODEX_STORE", "1".into()),
            ("KEYSTONE_FAKE_CODEX_DELAY_MS", "2".into()),
            (
                "KEYSTONE_FAKE_CODEX_METHOD_LOG",
                methods.display().to_string(),
            ),
        ],
    );
    let (wake, rx) = wake();
    let conn = CodexConnection::with_profile(
        Some(program),
        "0.0.0-test",
        LaunchProfile::account(Provider::Codex, "Work", &home),
        wake,
    );
    let cwd = dir.path().to_str().unwrap();
    let mut points = Vec::new();
    let mut source = None;
    for text in ["first", "second", "third"] {
        conn.send(1, cwd, None, &ModelChoice::default(), PROVIDER, text.into());
        let events = turn(&conn, &rx, 1);
        source = source.or(thread(&events, 1));
        points.push(fork_point(&events, 1));
    }
    let source = source.unwrap();
    let stored = |thread: &str| -> Vec<String> {
        let path = home
            .join("keystone-fake-threads")
            .join(format!("{thread}.json"));
        let stored: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        stored["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let source_turns = stored(&source);
    assert_eq!(
        source_turns,
        points.iter().map(|p| p.at.clone()).collect::<Vec<_>>()
    );

    conn.set_fork(2, points[1].clone());
    conn.send(
        2,
        cwd,
        None,
        &ModelChoice::default(),
        PROVIDER,
        "fork".into(),
    );
    let events = turn(&conn, &rx, 2);
    let fork = thread(&events, 2).unwrap();
    assert_ne!(fork, source);
    let point = fork_point(&events, 2);
    assert_eq!(point.thread, fork);
    assert_eq!(
        stored(&fork),
        [
            source_turns[0].clone(),
            source_turns[1].clone(),
            point.at.clone()
        ]
    );
    assert_eq!(stored(&source), source_turns);
    let methods = std::fs::read_to_string(&methods).unwrap();
    assert_eq!(methods.lines().filter(|m| *m == "thread/fork").count(), 1);

    // The fork's own thread is the one its later turns run in.
    conn.send(
        2,
        cwd,
        None,
        &ModelChoice::default(),
        PROVIDER,
        "again".into(),
    );
    let events = turn(&conn, &rx, 2);
    assert_eq!(fork_point(&events, 2).thread, fork);
    assert_eq!(stored(&fork).len(), 4);
    assert_eq!(stored(&source), source_turns);
}
