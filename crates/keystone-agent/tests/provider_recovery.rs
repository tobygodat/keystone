//! Crash recovery against scripted provider stores: the Claude reader on synthetic
//! store files, Codex history through the scripted app-server, and the reconcile
//! step on what they return. Nothing here writes to a provider store, and the
//! scripted providers are checked to receive no turn, resume or approval.

use keystone_agent::recovery::{self, Limit, Outcome, Snapshot, TurnEvidence};
use keystone_agent::{
    ApprovalState, ClaudeConnection, CodexConnection, Event, Item, ItemKind, ItemStatus,
    LaunchProfile, TurnStatus,
};
use keystone_core::ModelChoice;
use keystone_core::{ApprovalMode, Provider};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const SESSION: &str = "0b1c2d3e-0000-4000-8000-000000000001";
const CWD: &str = "/work/project";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/recovery")
        .join(name)
}

/// A config dir holding the fixture store as `session` in a project folder named
/// differently from Claude's own escaping, which the reader must not rely on.
fn claude_root(extra: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("projects/any-folder-name");
    std::fs::create_dir_all(&folder).unwrap();
    let mut text = std::fs::read_to_string(fixture("claude-2.1.300-store.jsonl")).unwrap();
    text.push_str(extra);
    std::fs::write(folder.join(format!("{SESSION}.jsonl")), text).unwrap();
    root
}

fn hash_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn the_claude_reader_maps_the_store_like_a_live_session_and_skips_what_it_must() {
    let root = claude_root("");
    let before = hash_tree(root.path());
    let snapshot = recovery::claude::read(root.path(), SESSION, Path::new(CWD)).unwrap();
    assert_eq!(hash_tree(root.path()), before, "reading never writes");
    assert!(snapshot.limits.is_empty(), "{:?}", snapshot.limits);
    let ids: Vec<Vec<&str>> = snapshot
        .turns
        .iter()
        .map(|t| t.items.iter().map(|i| i.id.as_str()).collect())
        .collect();
    // Block indexes count the skipped thinking block, as a live session does.
    assert_eq!(
        ids,
        [
            vec!["u1", "msg_A:1", "toolu_1", "msg_B:0"],
            vec!["u2", "msg_C:0", "toolu_2"]
        ]
    );
    let first = &snapshot.turns[0].items;
    assert_eq!(first[0].text, "List the files");
    assert_eq!(
        first[2].kind,
        ItemKind::Command {
            command: "ls".into(),
            exit_code: Some(0)
        }
    );
    assert_eq!(first[2].status, ItemStatus::Completed);
    assert_eq!(first[2].text, "a.txt\nb.txt");
    let last = &snapshot.turns[1].items[2];
    assert_eq!(last.status, ItemStatus::Interrupted, "no result recorded");
    assert!(snapshot.turns.iter().all(|t| t.end.is_none()));
    assert!(
        !snapshot
            .turns
            .iter()
            .flat_map(|t| &t.items)
            .any(|i| i.text.contains("subagent") || i.text.contains("hidden"))
    );
}

#[test]
fn a_torn_or_malformed_claude_store_keeps_its_complete_records_and_says_what_was_lost() {
    let torn = claude_root(r#"{"parentUuid":"a6","type":"user","message":{"ro"#);
    let snapshot = recovery::claude::read(torn.path(), SESSION, Path::new(CWD)).unwrap();
    assert_eq!(snapshot.limits, [Limit::TornTail]);
    assert_eq!(snapshot.turns.len(), 2);
    let malformed = claude_root("not json\n");
    let snapshot = recovery::claude::read(malformed.path(), SESSION, Path::new(CWD)).unwrap();
    assert_eq!(snapshot.limits, [Limit::Malformed { count: 1 }]);
}

#[test]
fn a_missing_unsafe_or_foreign_claude_store_is_never_guessed_at() {
    let root = claude_root("");
    assert_eq!(
        recovery::claude::read(root.path(), "another-session", Path::new(CWD)),
        Err(recovery::claude::NOT_FOUND.into())
    );
    assert!(recovery::claude::read(root.path(), "../projects/x", Path::new(CWD)).is_err());
    assert!(recovery::claude::read(&root.path().join("absent"), SESSION, Path::new(CWD)).is_err());
    let elsewhere = recovery::claude::read(root.path(), SESSION, Path::new("/other")).unwrap();
    assert_eq!(elsewhere.limits, [Limit::IdentityMismatch]);
    assert!(elsewhere.turns.is_empty());
}

#[cfg(unix)]
#[test]
fn links_out_of_the_config_dir_are_not_followed() {
    let outside = claude_root("");
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("projects/real")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("projects/any-folder-name"),
        root.path().join("projects/escape"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        outside
            .path()
            .join(format!("projects/any-folder-name/{SESSION}.jsonl")),
        root.path().join(format!("projects/real/{SESSION}.jsonl")),
    )
    .unwrap();
    assert_eq!(
        recovery::claude::read(root.path(), SESSION, Path::new(CWD)),
        Err(recovery::claude::NOT_FOUND.into())
    );
}

#[test]
fn a_branched_claude_store_is_a_conflict() {
    let branch = format!(
        r#"{{"parentUuid":"a5","isSidechain":false,"cwd":"{CWD}","sessionId":"{SESSION}","type":"assistant","message":{{"id":"msg_D","content":[{{"type":"text","text":"Other branch"}}]}},"uuid":"a7"}}"#
    );
    let root = claude_root(&format!("{branch}\n"));
    let snapshot = recovery::claude::read(root.path(), SESSION, Path::new(CWD)).unwrap();
    assert_eq!(snapshot.limits, [Limit::Branched]);
    let batch = recovery::reconcile(&[user("u1", "List the files")], true, &snapshot).unwrap();
    assert!(batch.operations.is_empty());
    assert_eq!(batch.report.outcome, Outcome::Conflict);
}

fn user(id: &str, text: &str) -> Item {
    Item {
        id: id.into(),
        kind: ItemKind::UserMessage,
        text: text.into(),
        status: ItemStatus::Completed,
    }
}

#[test]
fn a_claude_turn_killed_mid_text_recovers_the_stored_blocks_but_not_its_outcome() {
    let root = claude_root("");
    let snapshot = recovery::claude::read(root.path(), SESSION, Path::new(CWD)).unwrap();
    // Keystone saved the first turn, then the second prompt before it was echoed.
    let neutral = [
        user("u1", "List the files"),
        Item {
            id: "msg_A:1".into(),
            kind: ItemKind::AgentMessage,
            text: "I'll list them.".into(),
            status: ItemStatus::Completed,
        },
        user("local:1-2", "Delete b.txt"),
    ];
    let batch = recovery::reconcile(&neutral, true, &snapshot).unwrap();
    let inserted: Vec<&str> = batch
        .operations
        .iter()
        .filter_map(|op| match op {
            recovery::Operation::Insert { item, .. } => Some(item.id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(inserted, ["msg_C:0", "toolu_2"]);
    assert!(
        matches!(&batch.operations[0], recovery::Operation::Replace { expected, item }
        if expected.id == "local:1-2" && item.id == "u2")
    );
    assert_eq!(batch.report.turn, TurnEvidence::Unknown);
    assert_eq!(batch.report.outcome, Outcome::Partial);
    assert!(
        batch
            .report
            .limits
            .contains(&Limit::EarlierHistoryDiffers { count: 2 })
    );
    assert!(
        batch
            .report
            .limits
            .contains(&Limit::UnknownOutcome { count: 1 })
    );
}

fn wake() -> (keystone_agent::Wake, Receiver<()>) {
    let (tx, rx) = channel();
    (
        Arc::new(move || {
            let _ = tx.send(());
        }),
        rx,
    )
}

fn until(
    drain: impl Fn() -> Vec<(u64, Event)>,
    wake: &Receiver<()>,
    done: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut events = Vec::new();
    loop {
        let batch = drain();
        let finished = batch.iter().any(|(_, event)| done(event));
        events.extend(batch.into_iter().map(|(_, event)| event));
        if finished {
            return events;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(wake.recv_timeout(left).is_ok(), "timed out; got {events:?}");
    }
}

/// The scripted app-server run with extra environment, through a wrapper script.
fn wrapper(dir: &Path, env: &[(&str, String)]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("codex-wrapper");
    let exports: String = env
        .iter()
        .map(|(name, value)| format!("export {name}='{value}'\n"))
        .collect();
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n{exports}exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_keystone-fake-codex")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn codex_home(thread: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let threads = home.path().join("keystone-fake-threads");
    std::fs::create_dir_all(&threads).unwrap();
    let page: Value =
        serde_json::from_slice(&std::fs::read(fixture("codex-0.163-turns-list.json")).unwrap())
            .unwrap();
    let mut turns = page["data"].as_array().unwrap().clone();
    turns.reverse();
    std::fs::write(
        threads.join(format!("{thread}.json")),
        json!({"turns": turns}).to_string(),
    )
    .unwrap();
    home
}

fn read_codex(
    env: &[(&str, String)],
    home: &Path,
    thread: &str,
) -> (Result<Snapshot, String>, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("methods");
    let mut env = env.to_vec();
    env.push(("KEYSTONE_FAKE_CODEX_METHOD_LOG", log.display().to_string()));
    let program = wrapper(dir.path(), &env);
    let (wake, rx) = wake();
    let codex = CodexConnection::with_profile(
        Some(program),
        "0.0.0-test",
        LaunchProfile::account(Provider::Codex, "Work", home),
        wake,
    );
    codex.read_history(4, thread.into(), 7);
    let events = until(
        || codex.drain(),
        &rx,
        |e| matches!(e, Event::HistoryRead { .. }),
    );
    let result = events
        .into_iter()
        .find_map(|e| match e {
            Event::HistoryRead { ticket: 7, result } => Some(result),
            _ => None,
        })
        .unwrap();
    drop(codex);
    let methods = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    (result, methods)
}

#[test]
fn codex_history_is_read_from_the_accounts_app_server_without_resuming_anything() {
    let home = codex_home("thread-a");
    let before = hash_tree(home.path());
    let (result, methods) = read_codex(&[], home.path(), "thread-a");
    let snapshot = result.unwrap();
    assert_eq!(hash_tree(home.path()), before, "reading never writes");
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "thread/turns/list",
            "account/rateLimits/read"
        ]
    );
    assert_eq!(snapshot.turns.len(), 2);
    assert_eq!(snapshot.turns[0].end, Some(TurnStatus::Completed));
    assert_eq!(snapshot.turns[1].items[2].status, ItemStatus::Interrupted);

    // An app-server that cannot list turns pages the newest items instead: the same
    // items, as one turn whose end is unknown, never the whole thread.
    let (result, methods) = read_codex(
        &[("KEYSTONE_FAKE_CODEX_NO_TURNS_LIST", "1".into())],
        home.path(),
        "thread-a",
    );
    // Codex wraps each item with its turn's ID, so the turns survive; their ends do not.
    let items = result.unwrap();
    assert!(items.limits.is_empty(), "{:?}", items.limits);
    assert_eq!(items.turns.len(), snapshot.turns.len());
    for (read, listed) in items.turns.iter().zip(&snapshot.turns) {
        assert_eq!(read.items, listed.items);
        assert_eq!(read.end, None);
    }
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "thread/turns/list",
            "account/rateLimits/read",
            "thread/items/list"
        ]
    );

    // A thread this account does not have is an error, never empty history.
    let (result, methods) = read_codex(&[], home.path(), "thread-b");
    assert!(result.unwrap_err().contains("thread not found"));
    assert!(
        !methods
            .iter()
            .any(|m| m.starts_with("turn/") || m == "thread/resume" || m == "thread/start")
    );
}

#[test]
fn a_codex_turn_killed_mid_text_with_the_provider_ahead_recovers_its_answer() {
    let home = tempfile::tempdir().unwrap();
    let threads = home.path().join("keystone-fake-threads");
    std::fs::create_dir_all(&threads).unwrap();
    std::fs::write(
        threads.join("thread-c.json"),
        json!({"turns": [{"id": "turn-1", "status": "completed", "itemsView": "full", "items": [
            {"type": "userMessage", "id": "user-1", "content": [{"type": "text", "text": "hello"}]},
            {"type": "agentMessage", "id": "msg-1", "text": "Hello there, all done."}
        ]}]})
        .to_string(),
    )
    .unwrap();
    let (result, _) = read_codex(&[], home.path(), "thread-c");
    let neutral = [user("user-1", "hello")];
    let batch = recovery::reconcile(&neutral, true, &result.unwrap()).unwrap();
    assert_eq!(batch.report.outcome, Outcome::Recovered);
    assert_eq!(
        batch.report.turn,
        TurnEvidence::Recorded(TurnStatus::Completed)
    );
    assert_eq!(batch.report.messages, 1);
}

const PROVIDER: ApprovalState = ApprovalState {
    mode: ApprovalMode::Provider,
    overridden: false,
};

#[test]
fn a_provider_that_exits_mid_turn_asks_for_recovery_and_its_store_is_ahead() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("claude-home");
    std::fs::create_dir_all(&config).unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let claude = PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-claude"));
    let program = dir.path().join("claude-wrapper");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nexport KEYSTONE_FAKE_CLAUDE_STORE=1 KEYSTONE_FAKE_CLAUDE_DELAY_MS=5\nexec '{}' \"$@\"\n",
            claude.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (wake, rx) = wake();
    let connection = ClaudeConnection::with_profile(
        Some(program),
        LaunchProfile::account(Provider::Claude, "Work", &config),
        wake,
    );
    connection
        .send(
            1,
            cwd.to_str().unwrap(),
            None,
            &ModelChoice::default(),
            PROVIDER,
            "please crash".into(),
        )
        .unwrap();
    let events = until(
        || connection.drain(),
        &rx,
        |e| matches!(e, Event::TurnCompleted { .. }),
    );
    let at = |want: fn(&Event) -> bool| events.iter().position(want).unwrap();
    assert!(
        at(|e| matches!(e, Event::RecoveryNeeded))
            < at(|e| matches!(
                e,
                Event::TurnCompleted {
                    status: TurnStatus::Failed,
                    ..
                }
            ))
    );
    let thread = events
        .iter()
        .find_map(|e| match e {
            Event::ThreadReady { thread_id } => Some(thread_id.clone()),
            _ => None,
        })
        .unwrap();
    let partial = events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::ItemStarted(item) if item.kind == ItemKind::AgentMessage => {
                Some(item.id.clone())
            }
            _ => None,
        })
        .unwrap();
    let snapshot = recovery::claude::read(&config, &thread, &cwd).unwrap();
    let stored = snapshot.turns[0]
        .items
        .iter()
        .find(|item| item.id == partial)
        .expect("the account's store has the reply the stream lost");
    assert!(stored.text.contains("You wrote: please crash"));
    drop(connection);
}

/// Import reads Codex's own list and history through the account's app-server: the
/// folder's threads with their turn counts, then a thread's newest turns. Nothing is
/// resumed, started or written.
#[test]
fn codex_import_lists_the_folders_threads_and_reads_the_newest_turns_without_resuming() {
    let home = tempfile::tempdir().unwrap();
    let threads = home.path().join("keystone-fake-threads");
    std::fs::create_dir_all(&threads).unwrap();
    let turn = |n: usize| {
        json!({"id": format!("turn-{n}"), "status": "completed", "itemsView": "full", "items": [
            {"type": "userMessage", "id": format!("u{n}"), "content": [{"type": "text", "text": format!("ask {n}")}]},
            {"type": "agentMessage", "id": format!("a{n}"), "text": format!("answer {n}")}
        ]})
    };
    let store = |id: &str, cwd: &str, updated: u64, name: Value, count: usize| {
        let thread = json!({"name": name, "preview": format!("first of {id}"), "cwd": cwd,
            "createdAt": 1_700_000_000, "updatedAt": updated});
        let turns: Vec<Value> = (0..count).map(turn).collect();
        std::fs::write(
            threads.join(format!("{id}.json")),
            json!({"thread": thread, "turns": turns}).to_string(),
        )
        .unwrap();
    };
    store("thread-old", CWD, 1_700_000_100, json!("Named thread"), 2);
    store("thread-long", CWD, 1_700_000_200, Value::Null, 60);
    store("thread-elsewhere", "/other", 1_700_000_300, Value::Null, 1);
    store("thread-empty", CWD, 1_700_000_400, json!("Nothing yet"), 0);
    let before = hash_tree(home.path());

    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("methods");
    let program = wrapper(
        dir.path(),
        &[("KEYSTONE_FAKE_CODEX_METHOD_LOG", log.display().to_string())],
    );
    let (wake, rx) = wake();
    let codex = CodexConnection::with_profile(
        Some(program),
        "0.0.0-test",
        LaunchProfile::account(Provider::Codex, "Work", home.path()),
        wake,
    );
    codex.list_threads(CWD, 9);
    let events = until(
        || codex.drain(),
        &rx,
        |e| matches!(e, Event::ThreadsListed { .. }),
    );
    let listed = events
        .into_iter()
        .find_map(|e| match e {
            Event::ThreadsListed { ticket: 9, result } => Some(result),
            _ => None,
        })
        .unwrap()
        .unwrap();
    let shown: Vec<_> = listed
        .iter()
        .map(|l| (l.id.as_str(), l.title.as_str(), l.turns, l.updated))
        .collect();
    assert_eq!(
        shown,
        [
            (
                "thread-long",
                "first of thread-long",
                Some(60),
                Some(1_700_000_200_000)
            ),
            (
                "thread-old",
                "Named thread",
                Some(2),
                Some(1_700_000_100_000)
            ),
        ]
    );

    codex.read_import("thread-long".into(), 10);
    let events = until(
        || codex.drain(),
        &rx,
        |e| matches!(e, Event::HistoryRead { .. }),
    );
    let snapshot = events
        .into_iter()
        .find_map(|e| match e {
            Event::HistoryRead { ticket: 10, result } => Some(result),
            _ => None,
        })
        .unwrap()
        .unwrap();
    drop(codex);
    assert_eq!(snapshot.turns.len(), keystone_agent::import::TURNS);
    assert_eq!(snapshot.turns[0].items[0].text, "ask 10");
    assert!(snapshot.limits.contains(&Limit::OlderNotRead));
    assert_eq!(hash_tree(home.path()), before, "importing never writes");
    let methods = std::fs::read_to_string(&log).unwrap();
    assert!(
        methods
            .lines()
            .all(|m| !["thread/resume", "thread/start", "turn/start"].contains(&m)),
        "{methods}"
    );
    assert!(methods.lines().any(|m| m == "thread/list"), "{methods}");
}

/// A Codex without turn pagination still lists the folder's threads, without their
/// counts, and an import reads its newest items instead.
#[test]
fn codex_import_without_turn_pagination_lists_threads_uncounted_and_reads_their_items() {
    let home = tempfile::tempdir().unwrap();
    let threads = home.path().join("keystone-fake-threads");
    std::fs::create_dir_all(&threads).unwrap();
    let stored = json!({
        "thread": {"name": "Older Codex", "preview": "ask", "cwd": CWD,
            "createdAt": 1_700_000_000, "updatedAt": 1_700_000_000},
        "turns": [{"id": "turn-1", "status": "completed", "itemsView": "full", "items": [
            {"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "ask"}]},
            {"type": "agentMessage", "id": "a1", "text": "answer"}
        ]}]
    });
    std::fs::write(threads.join("thread-1.json"), stored.to_string()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let program = wrapper(
        dir.path(),
        &[("KEYSTONE_FAKE_CODEX_NO_TURNS_LIST", "1".to_owned())],
    );
    let (wake, rx) = wake();
    let codex = CodexConnection::with_profile(
        Some(program),
        "0.0.0-test",
        LaunchProfile::account(Provider::Codex, "Work", home.path()),
        wake,
    );
    codex.list_threads(CWD, 1);
    let listed = until(
        || codex.drain(),
        &rx,
        |e| matches!(e, Event::ThreadsListed { .. }),
    )
    .into_iter()
    .find_map(|e| match e {
        Event::ThreadsListed { ticket: 1, result } => Some(result),
        _ => None,
    })
    .unwrap()
    .unwrap();
    let shown: Vec<_> = listed.iter().map(|l| (l.id.as_str(), l.turns)).collect();
    assert_eq!(shown, [("thread-1", None)]);

    codex.read_import("thread-1".into(), 2);
    let snapshot = until(
        || codex.drain(),
        &rx,
        |e| matches!(e, Event::HistoryRead { .. }),
    )
    .into_iter()
    .find_map(|e| match e {
        Event::HistoryRead { ticket: 2, result } => Some(result),
        _ => None,
    })
    .unwrap()
    .unwrap();
    drop(codex);
    let texts: Vec<&str> = snapshot
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .map(|item| item.text.as_str())
        .collect();
    assert_eq!(texts, ["ask", "answer"]);
    assert!(keystone_agent::import::log(&snapshot).is_ok());
}
