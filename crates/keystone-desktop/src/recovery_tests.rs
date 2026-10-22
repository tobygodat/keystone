//! Crash and partial-transcript recovery through the app runtime, against scripted
//! provider stores: Keystone killed mid-turn, a provider that exits while Keystone
//! runs, a pending approval, a provider ahead with a completed answer, a missing or
//! torn store, a storage failure then repair, and restarts.

use super::*;
use crate::conversation::Recovery;
use keystone_agent::recovery::{Limit, Outcome, TurnEvidence};
use serde_json::{Value, json};

fn user(id: &str, text: &str) -> Item {
    Item {
        id: id.into(),
        kind: ItemKind::UserMessage,
        text: text.into(),
        status: ItemStatus::Completed,
    }
}

/// The log Keystone leaves when it is killed mid-turn: the message shown at once, the
/// turn's start and the provider's echo, then nothing.
fn killed_turn(sessions: &Path, session: u64, echo: &str, prompt: &str) {
    let mut log = SessionLog::open(sessions, session).unwrap();
    for entry in [
        Entry::Item {
            item: user("local:1-0", prompt),
            seq: Some(0),
            run: Some(1),
        },
        Entry::TurnStarted,
        Entry::Item {
            item: user(echo, prompt),
            seq: Some(0),
            run: Some(1),
        },
    ] {
        log.append(&entry).unwrap();
    }
    log.sync().unwrap();
}

/// Writes a Claude session store under `root`, chaining the records.
fn claude_store(root: &Path, thread: &str, cwd: &Path, records: &[Value]) -> PathBuf {
    let folder = root.join("projects/scripted");
    std::fs::create_dir_all(&folder).unwrap();
    let mut parent = Value::Null;
    let mut text = String::new();
    for (n, record) in records.iter().enumerate() {
        let mut record = record.clone();
        let uuid = record["uuid"]
            .as_str()
            .map_or_else(|| format!("record-{n}"), str::to_owned);
        record["uuid"] = json!(uuid);
        record["parentUuid"] = parent;
        record["sessionId"] = json!(thread);
        record["cwd"] = json!(cwd.to_str().unwrap());
        record["isSidechain"] = json!(false);
        text.push_str(&format!("{record}\n"));
        parent = json!(uuid);
    }
    let path = folder.join(format!("{thread}.jsonl"));
    std::fs::write(&path, text).unwrap();
    path
}

fn prompt(uuid: &str, text: &str) -> Value {
    json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": text}})
}

fn assistant(id: &str, block: Value) -> Value {
    json!({"type": "assistant", "message": {"id": id, "role": "assistant", "content": [block]}})
}

/// A workspace with one Claude session on an added account, its saved thread, and
/// its folder.
fn claude_workspace(dir: &Path) -> (Store, PathBuf, PathBuf) {
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    let directory = WorkingDirectory::new(dir).unwrap();
    let home = dir.join("claude-work");
    std::fs::create_dir_all(&home).unwrap();
    let work = store
        .add_account(Provider::Claude, "Work", home.clone(), &[])
        .unwrap()
        .value;
    let id = store.create(Provider::Claude, &directory, 1).unwrap().value;
    assert_eq!(id, 1);
    store.set_account(id, Some(work)).unwrap();
    store
        .set_provider_thread(id, "claude-thread".into())
        .unwrap();
    (store, home, directory.path().to_owned())
}

fn start(store: Store, cx: &mut gpui::TestAppContext) -> Entity<AppState> {
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    state.update(cx, |state, _| state.ensure_history(1));
    state
}

fn reopen(dir: &Path, cx: &mut gpui::TestAppContext) -> Entity<AppState> {
    start(Store::open(dir.join("workspace.json")).unwrap(), cx)
}

fn settled(state: &AppState) -> bool {
    state.conversations.get(&1).is_some_and(|c| {
        !c.is_loading()
            && !matches!(
                c.recovery,
                Some(Recovery::Checking | Recovery::Saving { .. })
            )
    })
}

fn report(rows: &[Row]) -> Vec<keystone_agent::recovery::RecoveryReport> {
    rows.iter()
        .filter_map(|row| match row {
            Row::Recovery(report) => Some(report.clone()),
            _ => None,
        })
        .collect()
}

fn batches(sessions: &Path) -> usize {
    keystone_agent::log::read(sessions, 1)
        .unwrap()
        .iter()
        .filter(|entry| matches!(entry, Entry::Recovery { .. }))
        .count()
}

fn draft(text: &str) -> ComposerDraft {
    ComposerDraft {
        session: 1,
        text: text.into(),
        attachments: Vec::new(),
    }
}

#[gpui::test]
fn a_claude_turn_killed_mid_text_recovers_from_its_accounts_store_and_holds_send(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "u1", "hello");
    claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant(
                "msg_A",
                json!({"type": "text", "text": "Hello from the work account."}),
            ),
        ],
    );
    // The default account's store has another reply for the same ID; it must not be read.
    let default_home = dir.join("claude-default");
    claude_store(
        &default_home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant("msg_X", json!({"type": "text", "text": "Wrong account."})),
        ],
    );
    let before = std::fs::read(home.join("projects/scripted/claude-thread.jsonl")).unwrap();
    let state = cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.test_claude_home = Some(default_home.clone());
        state
    });
    cx.executor().allow_parking();
    state.update(cx, |state, _| state.ensure_history(1));
    wait_until(cx, &state, settled);
    state.update(cx, |state, cx| {
        let conversation = &state.conversations[&1];
        let texts: Vec<&str> = conversation
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) => Some(item.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["hello", "Hello from the work account."]);
        let reports = report(&conversation.rows);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].turn, TurnEvidence::Unknown);
        assert_eq!(reports[0].outcome, Outcome::Partial);
        assert!(
            !conversation
                .rows
                .iter()
                .any(|row| matches!(row, Row::TurnEnded { .. })),
            "the report replaces the closed-mid-turn marker"
        );
        assert!(matches!(conversation.recovery, Some(Recovery::Review(_))));
        assert_eq!(conversation.reported_model, None);
        // Send waits for the acknowledgement and starts nothing.
        assert!(!state.send_message(draft("next"), cx));
        assert!(state.claude.is_empty() && state.codex.is_empty());
        state.continue_after_recovery(1, cx);
        assert_eq!(state.conversations[&1].recovery, None);
    });
    assert_eq!(
        std::fs::read(home.join("projects/scripted/claude-thread.jsonl")).unwrap(),
        before,
        "the provider store is never written"
    );
    assert_eq!(batches(&sessions), 1);
    drop(state);

    // Two restarts restore the same rows from the one saved batch and check nothing
    // again. The acknowledgement holds only until a message is sent, so it returns.
    for _ in 0..2 {
        let state = reopen(dir, cx);
        wait_until(cx, &state, |state| {
            state.conversations.get(&1).is_some_and(|c| !c.is_loading())
        });
        state.read_with(cx, |state, _| {
            let conversation = &state.conversations[&1];
            assert_eq!(report(&conversation.rows).len(), 1);
            assert!(matches!(conversation.recovery, Some(Recovery::Review(_))));
            assert!(state.recoveries.is_empty(), "no new check");
        });
        assert_eq!(batches(&sessions), 1);
    }
}

#[gpui::test]
fn a_pending_approval_at_the_kill_expires_and_its_tool_has_no_recorded_outcome(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    killed_turn(&dir.join("sessions"), 1, "u1", "please run");
    claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "please run"),
            assistant(
                "msg_A",
                json!({"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "rm -rf build"}}),
            ),
        ],
    );
    let state = start(store, cx);
    wait_until(cx, &state, settled);
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert!(
            conversation.approvals.is_empty(),
            "no stale approval is restored"
        );
        assert!(conversation.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.id == "toolu_1" && item.status == ItemStatus::Interrupted)));
        let reports = report(&conversation.rows);
        assert!(
            reports[0]
                .limits
                .contains(&Limit::UnknownOutcome { count: 1 })
        );
        assert!(matches!(conversation.recovery, Some(Recovery::Review(_))));
    });
}

#[gpui::test]
fn a_missing_store_is_unavailable_saves_nothing_and_continue_sends_nothing(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, _, _) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "u1", "hello");
    let state = start(store, cx);
    wait_until(cx, &state, settled);
    state.update(cx, |state, cx| {
        let conversation = &state.conversations[&1];
        assert!(
            matches!(&conversation.recovery, Some(Recovery::Unavailable(reason))
            if reason.contains("no saved conversation"))
        );
        assert!(
            conversation
                .rows
                .iter()
                .any(|row| matches!(row, Row::TurnEnded { .. })),
            "still shown as closed mid-turn"
        );
        assert!(!state.send_message(draft("next"), cx));
        state.continue_after_recovery(1, cx);
        assert_eq!(state.conversations[&1].recovery, None);
        assert!(state.claude.is_empty(), "continuing sends nothing");
    });
    assert_eq!(batches(&sessions), 0);
    drop(state);
    // Nothing was saved, so the next start checks again.
    let state = reopen(dir, cx);
    wait_until(cx, &state, settled);
    state.read_with(cx, |state, _| {
        assert!(matches!(
            state.conversations[&1].recovery,
            Some(Recovery::Unavailable(_))
        ))
    });
}

#[gpui::test]
fn a_torn_store_recovers_its_complete_records_and_says_the_rest_was_cut_off(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    killed_turn(&dir.join("sessions"), 1, "u1", "hello");
    let path = claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant("msg_A", json!({"type": "text", "text": "First block."})),
        ],
    );
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(r#"{"type":"assistant","message":{"id":"msg_A","content":[{"type":"te"#);
    std::fs::write(&path, text).unwrap();
    let state = start(store, cx);
    wait_until(cx, &state, settled);
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert!(conversation.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.text == "First block.")));
        let reports = report(&conversation.rows);
        assert!(reports[0].limits.contains(&Limit::TornTail));
        assert_eq!(reports[0].outcome, Outcome::Partial);
    });
}

#[gpui::test]
fn a_recovery_that_cannot_be_saved_holds_send_until_storage_is_repaired(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "u1", "hello");
    claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant("msg_A", json!({"type": "text", "text": "Recovered reply."})),
        ],
    );
    let path = store.path().to_owned();
    let writer = crate::tests::lock_database(&path);
    let state = start(store, cx);
    wait_until(cx, &state, |state| {
        matches!(
            state
                .conversations
                .get(&1)
                .and_then(|c| c.recovery.as_ref()),
            Some(Recovery::Saving { error: Some(_), .. })
        )
    });
    state.update(cx, |state, cx| {
        assert!(!state.send_message(draft("next"), cx));
        // Continue cannot skip an unsaved recovery.
        state.continue_after_recovery(1, cx);
        assert!(matches!(
            state.conversations[&1].recovery,
            Some(Recovery::Saving { .. })
        ));
    });
    assert_eq!(batches(&sessions), 0);
    drop(writer);
    state.update(cx, |state, cx| state.retry_recovery(1, cx));
    wait_until(cx, &state, |state| {
        matches!(
            state
                .conversations
                .get(&1)
                .and_then(|c| c.recovery.as_ref()),
            Some(Recovery::Review(_))
        )
    });
    assert_eq!(batches(&sessions), 1);
    drop(state);
    let state = reopen(dir, cx);
    wait_until(cx, &state, |state| {
        state.conversations.get(&1).is_some_and(|c| !c.is_loading())
    });
    state.read_with(cx, |state, _| {
        assert!(state.conversations[&1].rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.text == "Recovered reply.")));
    });
    assert_eq!(batches(&sessions), 1);
}

#[gpui::test]
fn retry_saves_a_rejected_recovery_once_the_log_has_room(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "u1", "hello");
    claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant(
                "msg_A",
                json!({"type": "text", "text": "A recovered reply."}),
            ),
        ],
    );
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    state.update(cx, |state, cx| {
        let logs = Logs::with_database(state.store().database(), state.test_wake().clone())
            .with_budget(3000);
        state.test_set_logs(logs);
        state.ensure_history(2);
        state.flush_logs(cx).unwrap();
    });
    // A locked store retains another session's writes until the shared budget is full.
    let writer = crate::tests::lock_database(&dir.join("workspace.sqlite3"));
    state.update(cx, |state, cx| {
        for n in 0..40 {
            state
                .test_conversation(2)
                .push_local(&n.to_string().repeat(40));
            state.commit(2);
        }
        state.publish(cx);
        state.ensure_history(1);
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        let recovery = state.read_with(cx, |state, _| {
            state.conversations.get(&1).and_then(|c| c.recovery.clone())
        });
        if matches!(recovery, Some(Recovery::Unsaved(_))) {
            break;
        }
        assert!(Instant::now() < deadline, "{recovery:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(writer);
    state.update(cx, |state, cx| state.flush_logs(cx).unwrap());
    state.update(cx, |state, cx| state.retry_recovery(1, cx));
    wait_until(cx, &state, |state| {
        !matches!(
            state.conversations[&1].recovery,
            Some(Recovery::Unsaved(_) | Recovery::Saving { .. })
        )
    });
    assert_eq!(batches(&sessions), 1);
    let saved = keystone_agent::log::read(&sessions, 1).unwrap();
    assert!(saved.iter().any(|entry| matches!(entry,
        Entry::Recovery { batch } if format!("{:?}", batch.operations).contains("A recovered reply."))));
}

#[gpui::test]
fn a_recovery_the_log_budget_rejects_says_it_is_not_saved_and_continue_goes_on(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "u1", "hello");
    claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant(
                "msg_A",
                json!({"type": "text", "text": "A recovered reply too large for the log. ".repeat(20)}),
            ),
        ],
    );
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    state.update(cx, |state, _| {
        let logs = Logs::with_database(state.store().database(), state.test_wake().clone())
            .with_budget(600);
        state.test_set_logs(logs);
        state.ensure_history(1);
    });
    wait_until(cx, &state, |state| {
        matches!(
            state
                .conversations
                .get(&1)
                .and_then(|c| c.recovery.as_ref()),
            Some(Recovery::Unsaved(_))
        )
    });
    assert_eq!(batches(&sessions), 0);
    // Still no room: Retry holds the batch again and never confirms it.
    state.update(cx, |state, cx| state.retry_recovery(1, cx));
    assert!(matches!(
        state.read_with(cx, |state, _| state.conversations[&1].recovery.clone()),
        Some(Recovery::Unsaved(_))
    ));
    assert_eq!(batches(&sessions), 0);
    state.update(cx, |state, cx| {
        assert!(state.conversations[&1].rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.text.starts_with("A recovered reply"))));
        assert!(state.log_losses().next().is_some());
        state.continue_after_recovery(1, cx);
        assert_eq!(state.conversations[&1].recovery, None);
    });
}

/// A scripted provider that records its store and keeps Keystone's environment.
fn store_wrapper(dir: &Path, name: &str) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("keystone-fake-{name}"));
    let environment = json!({
        "KEYSTONE_FAKE_CODEX_DELAY_MS": "5",
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS": "5",
        "KEYSTONE_FAKE_CLAUDE_STORE": "1",
        "KEYSTONE_FAKE_CODEX_STORE": "1",
        "KEYSTONE_FAKE_CODEX_METHOD_LOG": dir.join("codex-methods").to_str().unwrap(),
        "KEYSTONE_FAKE_CLAUDE_LOG_DIR": dir.to_str().unwrap(),
    });
    let script = dir.join(format!("{name}-store-wrapper"));
    std::fs::write(
        &script,
        format!(
            "#!/usr/bin/python3\nimport os, sys\nos.environ.update({environment})\nbinary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A workspace with one session of `provider` on an added account whose home exists.
fn account_workspace(dir: &Path, provider: Provider) -> (Store, PathBuf) {
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    let directory = WorkingDirectory::new(dir).unwrap();
    let home = dir.join("account-home");
    std::fs::create_dir_all(&home).unwrap();
    let work = store
        .add_account(provider, "Work", home.clone(), &[])
        .unwrap()
        .value;
    let id = store.create(provider, &directory, 1).unwrap().value;
    store.set_account(id, Some(work)).unwrap();
    (store, home)
}

#[gpui::test]
fn a_provider_that_exits_while_keystone_runs_has_its_reply_completed_and_its_outcome_held(
    cx: &mut gpui::TestAppContext,
) {
    for provider in [Provider::Claude, Provider::Codex] {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let (store, _) = account_workspace(dir, provider);
        let state = start(store, cx);
        state.update(cx, |state, _| {
            state.test_programs = Some((store_wrapper(dir, "codex"), store_wrapper(dir, "claude")));
        });
        wait_until(cx, &state, |state| {
            state.conversations.get(&1).is_some_and(|c| !c.is_loading())
        });
        state.update(cx, |state, cx| {
            assert!(state.send_message(draft("please crash"), cx))
        });
        wait_until(cx, &state, |state| {
            state
                .conversations
                .get(&1)
                .is_some_and(|c| !c.running && !report(&c.rows).is_empty() && settled(state))
        });
        state.update(cx, |state, cx| {
            let conversation = &state.conversations[&1];
            let reply = conversation
                .rows
                .iter()
                .find_map(|row| match row {
                    Row::Item(item) if item.kind == ItemKind::AgentMessage => Some(item),
                    _ => None,
                })
                .unwrap();
            assert!(
                reply.text.contains("You wrote: please crash") && reply.text.len() > 40,
                "{provider:?}: {reply:?}"
            );
            assert_eq!(reply.status, ItemStatus::Completed);
            let failed = conversation
                .rows
                .iter()
                .position(|row| {
                    matches!(
                        row,
                        Row::TurnEnded {
                            status: TurnStatus::Failed,
                            ..
                        }
                    )
                })
                .unwrap();
            let shown = conversation
                .rows
                .iter()
                .position(|row| matches!(row, Row::Recovery(_)))
                .unwrap();
            assert!(failed < shown, "the report follows the failed turn");
            let reports = report(&conversation.rows);
            assert_eq!(reports[0].extended, 1);
            // The provider died mid-turn: what it saved is shown, and unless it recorded
            // how the turn ended, the outcome stays unknown and the next Send waits.
            assert_ne!(reports[0].turn, TurnEvidence::Unaffected, "{provider:?}");
            assert_eq!(
                matches!(conversation.recovery, Some(Recovery::Review(_))),
                reports[0].needs_acknowledgement()
            );
            assert_eq!(conversation.reported_model, None);
            if provider == Provider::Claude {
                assert_eq!(reports[0].turn, TurnEvidence::Unknown);
                assert!(!state.send_message(draft("next"), cx));
            }
            state.save(Vec::new(), Vec::new(), cx).unwrap();
        });
        let restored = restored(&dir.join("sessions"), 1);
        state.read_with(cx, |state, _| {
            assert_eq!(restored, state.conversations[&1].rows)
        });
        if provider == Provider::Codex {
            let methods = std::fs::read_to_string(dir.join("codex-methods")).unwrap();
            let methods: Vec<&str> = methods.lines().collect();
            assert_eq!(
                methods.iter().filter(|m| **m == "turn/start").count(),
                1,
                "recovery never starts a turn: {methods:?}"
            );
            assert!(methods.contains(&"thread/turns/list"));
            assert!(!methods.contains(&"thread/resume"));
        }
    }
}

/// Keystone is killed after its provider exited mid-turn but before the check saved
/// anything: the next start checks again rather than trusting the closed turn.
#[gpui::test]
fn a_provider_exit_whose_check_never_saved_is_checked_on_the_next_start(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    let mut log = SessionLog::open(&sessions, 1).unwrap();
    for entry in [
        Entry::Item {
            item: user("u1", "hello"),
            seq: Some(0),
            run: Some(1),
        },
        Entry::TurnStarted,
        Entry::ProviderExited,
        Entry::TurnEnded {
            status: TurnStatus::Failed,
            error: Some("Claude Code stopped with exit code 3".into()),
            seq: Some(1),
            run: Some(1),
        },
    ] {
        log.append(&entry).unwrap();
    }
    log.sync().unwrap();
    drop(log);
    claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant(
                "msg_A",
                json!({"type": "text", "text": "Saved before it died."}),
            ),
        ],
    );
    let state = start(store, cx);
    wait_until(cx, &state, settled);
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert!(conversation.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.text == "Saved before it died.")));
        assert_eq!(report(&conversation.rows)[0].turn, TurnEvidence::Unknown);
        assert!(matches!(conversation.recovery, Some(Recovery::Review(_))));
    });
    assert_eq!(batches(&sessions), 1);
}

#[gpui::test]
fn a_codex_turn_killed_with_the_provider_ahead_shows_its_completion_without_a_gate(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (mut store, home) = account_workspace(dir, Provider::Codex);
    store.set_provider_thread(1, "codex-thread".into()).unwrap();
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "user-1", "hello");
    std::fs::create_dir_all(home.join("keystone-fake-threads")).unwrap();
    std::fs::write(
        home.join("keystone-fake-threads/codex-thread.json"),
        json!({"turns": [{"id": "turn-1", "status": "completed", "itemsView": "full", "items": [
            {"type": "userMessage", "id": "user-1", "content": [{"type": "text", "text": "hello"}]},
            {"type": "agentMessage", "id": "msg-1", "text": "Finished while Keystone was away."}
        ]}]})
        .to_string(),
    )
    .unwrap();
    cx.executor().allow_parking();
    let state = cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.test_programs = Some((store_wrapper(dir, "codex"), store_wrapper(dir, "claude")));
        state.ensure_history(1);
        state
    });
    wait_until(cx, &state, |state| {
        settled(state) && state.conversations[&1].recovery.is_none()
    });
    state.update(cx, |state, cx| {
        let conversation = &state.conversations[&1];
        assert!(conversation.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.text == "Finished while Keystone was away.")));
        let reports = report(&conversation.rows);
        assert_eq!(
            reports[0].turn,
            TurnEvidence::Recorded(TurnStatus::Completed)
        );
        assert_eq!(reports[0].outcome, Outcome::Recovered);
        assert!(state.test_codex_accounts().iter().all(Option::is_some));
        // Nothing waits: the next message may go, and only then does a turn start.
        assert!(state.send_message(draft("next"), cx));
    });
    wait_until(cx, &state, |state| completed(state, 1));
    let methods = std::fs::read_to_string(dir.join("codex-methods")).unwrap();
    let methods: Vec<&str> = methods.lines().collect();
    let read = methods
        .iter()
        .position(|m| *m == "thread/turns/list")
        .unwrap();
    let resume = methods.iter().position(|m| *m == "thread/resume").unwrap();
    assert!(
        read < resume,
        "read first, resume only for the explicit Send: {methods:?}"
    );
    assert_eq!(batches(&sessions), 1);
}

/// A Codex session on an added account, killed mid-turn, whose account store holds
/// `turns`; `None` leaves the store without the thread.
fn codex_killed(
    dir: &Path,
    turns: Option<Value>,
    cx: &mut gpui::TestAppContext,
) -> (Entity<AppState>, PathBuf) {
    let (mut store, home) = account_workspace(dir, Provider::Codex);
    store.set_provider_thread(1, "codex-thread".into()).unwrap();
    killed_turn(&dir.join("sessions"), 1, "user-1", "hello");
    let threads = home.join("keystone-fake-threads");
    std::fs::create_dir_all(&threads).unwrap();
    if let Some(turns) = turns {
        std::fs::write(
            threads.join("codex-thread.json"),
            json!({ "turns": turns }).to_string(),
        )
        .unwrap();
    }
    cx.executor().allow_parking();
    let state = cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.test_programs = Some((store_wrapper(dir, "codex"), store_wrapper(dir, "claude")));
        state.ensure_history(1);
        state
    });
    wait_until(cx, &state, settled);
    (state, threads)
}

fn codex_methods(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("codex-methods"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn read_only(methods: &[String]) {
    for written in ["turn/start", "turn/steer", "thread/resume", "thread/start"] {
        assert!(
            !methods.iter().any(|m| m == written),
            "recovery sent {written}: {methods:?}"
        );
    }
    assert!(
        methods.iter().any(|m| m == "thread/turns/list"),
        "{methods:?}"
    );
}

#[gpui::test]
fn a_codex_turn_killed_mid_text_and_mid_approval_recovers_read_only_across_restarts(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (state, threads) = codex_killed(
        dir,
        Some(
            json!([{"id": "turn-1", "status": "inProgress", "itemsView": "full", "items": [
                {"type": "userMessage", "id": "user-1", "content": [{"type": "text", "text": "hello"}]},
                {"type": "agentMessage", "id": "msg-1", "text": "Half of the reply"},
                {"type": "commandExecution", "id": "exec-1", "command": "cargo test",
                 "status": "inProgress", "aggregatedOutput": null, "exitCode": null}
            ]}]),
        ),
        cx,
    );
    let before = std::fs::read(threads.join("codex-thread.json")).unwrap();
    state.update(cx, |state, cx| {
        let conversation = &state.conversations[&1];
        assert!(conversation.approvals.is_empty());
        assert!(conversation.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.text == "Half of the reply")));
        assert!(conversation.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.id == "exec-1" && item.status == ItemStatus::Interrupted)));
        let reports = report(&conversation.rows);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, Outcome::Partial);
        assert_eq!(reports[0].turn, TurnEvidence::Unknown);
        assert_eq!(conversation.reported_model, None);
        assert!(!state.send_message(draft("next"), cx));
        state.continue_after_recovery(1, cx);
    });
    read_only(&codex_methods(dir));
    assert_eq!(
        std::fs::read(threads.join("codex-thread.json")).unwrap(),
        before
    );
    drop(state);
    for _ in 0..2 {
        std::fs::remove_file(dir.join("codex-methods")).ok();
        let state = cx.new(|cx| {
            let mut state = AppState::new(Store::open(dir.join("workspace.json")).unwrap(), cx);
            state.test_programs = Some((store_wrapper(dir, "codex"), store_wrapper(dir, "claude")));
            state.ensure_history(1);
            state
        });
        wait_until(cx, &state, |state| {
            state.conversations.get(&1).is_some_and(|c| !c.is_loading())
        });
        state.read_with(cx, |state, _| {
            assert_eq!(report(&state.conversations[&1].rows).len(), 1);
            assert!(state.recoveries.is_empty());
        });
        assert!(
            !std::fs::read_to_string(dir.join("codex-methods"))
                .unwrap_or_default()
                .contains("thread/turns/list"),
            "a saved recovery is not read again"
        );
    }
    assert_eq!(batches(&dir.join("sessions")), 1);
}

#[gpui::test]
fn a_codex_thread_missing_from_its_accounts_store_is_unavailable_and_saves_nothing(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (state, _) = codex_killed(dir, None, cx);
    state.update(cx, |state, cx| {
        let conversation = &state.conversations[&1];
        assert!(
            matches!(&conversation.recovery, Some(Recovery::Unavailable(reason))
                if reason.contains("thread not found")),
            "{:?}",
            conversation.recovery
        );
        assert!(
            conversation
                .rows
                .iter()
                .any(|row| matches!(row, Row::TurnEnded { .. }))
        );
        assert!(!state.send_message(draft("next"), cx));
    });
    read_only(&codex_methods(dir));
    assert_eq!(batches(&dir.join("sessions")), 0);
}

/// A message sent before an interrupted conversation's history arrives would resume
/// the provider before recovery could check it, so Send waits for the history.
#[gpui::test]
fn send_waits_for_a_started_conversations_history_before_any_recovery_check(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "u1", "hello");
    claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant("msg_A", json!({"type": "text", "text": "Partly there"})),
        ],
    );
    cx.executor().allow_parking();
    let gate = HistoryGate::default();
    let state = cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.logs = Logs::with_reader(sessions.clone(), state.wake.clone(), gate.reader());
        state.ensure_history(1);
        state
    });
    state.update(cx, |state, cx| {
        assert!(state.conversations[&1].is_loading());
        assert!(!state.send_message(draft("too soon"), cx));
        assert_eq!(state.conversations[&1].recovery, Some(Recovery::Loading));
        assert!(state.claude.is_empty(), "nothing reached Claude");
    });
    gate.release();
    wait_until(cx, &state, settled);
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert!(matches!(conversation.recovery, Some(Recovery::Review(_))));
        assert!(!conversation.running);
    });
}

/// Retry on a partial recovery checks again. With nothing new it saves nothing and
/// keeps the report waiting; it never adds a second report.
#[gpui::test]
fn retrying_a_partial_recovery_that_finds_nothing_new_saves_nothing(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "u1", "hello");
    let path = claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant("msg_A", json!({"type": "text", "text": "First block."})),
        ],
    );
    let state = start(store, cx);
    wait_until(cx, &state, settled);
    state.update(cx, |state, cx| state.retry_recovery(1, cx));
    wait_until(cx, &state, settled);
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert_eq!(report(&conversation.rows).len(), 1);
        assert!(matches!(conversation.recovery, Some(Recovery::Review(_))));
    });
    assert_eq!(batches(&sessions), 1);

    // The provider saved more since: Retry adds it, and the hold stays until Continue.
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!(
        "{}\n",
        json!({"type": "assistant", "uuid": "late", "parentUuid": "record-1",
                "sessionId": "claude-thread", "cwd": cwd.to_str().unwrap(), "isSidechain": false,
                "message": {"id": "msg_B", "role": "assistant",
                    "content": [{"type": "text", "text": "Saved later."}]}})
    ));
    std::fs::write(&path, text).unwrap();
    state.update(cx, |state, cx| state.retry_recovery(1, cx));
    wait_until(cx, &state, settled);
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert!(conversation.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.text == "Saved later.")));
        assert!(matches!(conversation.recovery, Some(Recovery::Review(_))));
        // The second check still treats the turn as left open, as the first found it.
        let reports = report(&conversation.rows);
        assert!(
            reports.iter().all(|r| r.turn == TurnEvidence::Unknown),
            "{reports:?}"
        );
    });
    assert_eq!(batches(&sessions), 2);
}

/// A history that never arrives does not hold Send for good: the wait turns into an
/// unavailable check the user can continue past, and then the message goes.
#[gpui::test]
fn a_history_that_never_loads_lets_the_user_continue_and_send(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, _, _) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "u1", "hello");
    cx.executor().allow_parking();
    let gate = HistoryGate::default();
    let state = cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.test_programs = Some((store_wrapper(dir, "codex"), store_wrapper(dir, "claude")));
        state.logs = Logs::with_reader(sessions.clone(), state.wake.clone(), gate.reader());
        state.ensure_history(1);
        state
    });
    state.update(cx, |state, cx| {
        assert!(!state.send_message(draft("hi"), cx))
    });
    cx.executor().advance_clock(RECOVERY_DEADLINE);
    cx.run_until_parked();
    state.update(cx, |state, cx| {
        assert!(matches!(
            &state.conversations[&1].recovery,
            Some(Recovery::Unavailable(reason)) if reason.contains("could not read")
        ));
        state.continue_after_recovery(1, cx);
        assert!(state.conversations[&1].is_loading());
        assert!(
            state.send_message(draft("hi"), cx),
            "sent once the user chose to"
        );
    });
    drop(gate);
}

/// Retry after the wait for history timed out never checks against rows that have not
/// arrived: it waits for them again, and the check that follows keeps the prompt.
#[gpui::test]
fn retrying_before_the_history_arrives_waits_for_it_and_checks_against_it(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, home, cwd) = claude_workspace(dir);
    let sessions = dir.join("sessions");
    killed_turn(&sessions, 1, "u1", "hello");
    claude_store(
        &home,
        "claude-thread",
        &cwd,
        &[
            prompt("u1", "hello"),
            assistant("msg_A", json!({"type": "text", "text": "Partly there"})),
        ],
    );
    cx.executor().allow_parking();
    let gate = HistoryGate::default();
    let state = cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.logs = Logs::with_reader(sessions.clone(), state.wake.clone(), gate.reader());
        state.ensure_history(1);
        state
    });
    state.update(cx, |state, cx| {
        assert!(!state.send_message(draft("hi"), cx))
    });
    cx.executor().advance_clock(RECOVERY_DEADLINE);
    cx.run_until_parked();
    state.update(cx, |state, cx| {
        assert!(matches!(
            state.conversations[&1].recovery,
            Some(Recovery::Unavailable(_))
        ));
        state.retry_recovery(1, cx);
        assert_eq!(state.conversations[&1].recovery, Some(Recovery::Loading));
    });
    cx.run_until_parked();
    assert_eq!(batches(&sessions), 0, "nothing was checked against no rows");
    gate.release();
    wait_until(cx, &state, settled);
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert!(matches!(conversation.recovery, Some(Recovery::Review(_))));
        assert_eq!(report(&conversation.rows).len(), 1);
        assert!(conversation.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.text == "hello")));
        assert!(conversation.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.text == "Partly there")));
    });
    assert_eq!(batches(&sessions), 1);
}

/// When a started conversation's history cannot be read, Keystone cannot tell whether a
/// turn was left open, so the next message waits for the user.
#[gpui::test]
fn an_unreadable_history_holds_send_until_the_user_continues(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (store, _, _) = claude_workspace(dir);
    cx.executor().allow_parking();
    let state = cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.logs = Logs::with_reader(
            dir.join("sessions"),
            state.wake.clone(),
            Arc::new(|_, _| Err(std::io::Error::other("disk gone"))),
        );
        state.ensure_history(1);
        state
    });
    wait_until(cx, &state, |state| {
        state.conversations.get(&1).is_some_and(|c| !c.is_loading())
    });
    state.update(cx, |state, cx| {
        assert!(matches!(
            state.conversations[&1].recovery,
            Some(Recovery::Unavailable(_))
        ));
        assert!(!state.send_message(draft("hi"), cx));
        state.continue_after_recovery(1, cx);
        assert_eq!(state.conversations[&1].recovery, None);
    });
}
