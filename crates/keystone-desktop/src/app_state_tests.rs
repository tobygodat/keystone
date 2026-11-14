use super::*;
use keystone_agent::log::{Entry, Reader, SessionLog};
use keystone_agent::{Item, ItemStatus};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A choice of `model` alone.
fn chosen(model: &str) -> ModelChoice {
    ModelChoice {
        model: Some(model.into()),
        ..ModelChoice::default()
    }
}

fn runtime(dir: &Path, cx: &mut gpui::TestAppContext) -> Entity<AppState> {
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    let directory = WorkingDirectory::new(dir).unwrap();
    for provider in [Provider::Codex, Provider::Claude] {
        store.create(provider, &directory, 1).unwrap();
    }
    cx.executor().allow_parking();
    cx.new(|cx| AppState::new(store, cx))
}

fn wrapper(dir: &Path, provider: Provider, delay_ms: u64, descendants: bool) -> PathBuf {
    let name = match provider {
        Provider::Codex => "codex",
        Provider::Claude => "claude",
    };
    let binary = keystone_core::test_support::built_binary(&format!("keystone-fake-{name}"));
    let environment = serde_json::json!({
        "PATH": "/usr/bin:/bin",
        "KEYSTONE_FAKE_CODEX_DELAY_MS": delay_ms.to_string(),
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS": delay_ms.to_string(),
        "KEYSTONE_FAKE_CLAUDE_LOG_DIR": dir.to_str().unwrap(),
        "KEYSTONE_FAKE_CODEX_RELEASE": dir.join("codex-release").to_str().unwrap(),
    });
    let tracking = if descendants {
        format!(
            "if '--version' not in sys.argv[1:]:\n    child = subprocess.Popen(['/bin/sleep', '60'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n    with open({}, 'a') as log:\n        log.write(f'{{os.getpid()}} {{child.pid}}\\n')\n",
            serde_json::to_string(&dir.join(format!("{name}.pids"))).unwrap()
        )
    } else {
        String::new()
    };
    let script = dir.join(format!("{name}-wrapper"));
    keystone_core::test_support::write_executable(
        &script,
        format!(
            "#!/usr/bin/python3\nimport os, subprocess, sys\nos.environ.clear()\nos.environ.update({environment})\n{tracking}binary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    );
    script
}

fn connect(
    state: &Entity<AppState>,
    dir: &Path,
    delay_ms: u64,
    descendants: bool,
    cx: &mut gpui::TestAppContext,
) {
    let codex = wrapper(dir, Provider::Codex, delay_ms, descendants);
    let claude = wrapper(dir, Provider::Claude, delay_ms, descendants);
    state.update(cx, |state, _| {
        state.codex.insert(
            None,
            CodexConnection::new(Some(codex), "test", state.wake.clone()),
        );
        state.claude.insert(
            None,
            ClaudeConnection::new(Some(claude), state.wake.clone()),
        );
    });
}

#[track_caller]
fn wait_until(
    cx: &mut gpui::TestAppContext,
    state: &Entity<AppState>,
    done: impl Fn(&AppState) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if state.read_with(cx, |state, _| done(state)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "runtime did not progress: {:?}",
            state.read_with(cx, |state, _| (
                state.error.clone(),
                state
                    .conversations
                    .iter()
                    .map(|(id, c)| (
                        *id,
                        c.is_loading(),
                        c.running,
                        c.rows.clone(),
                        c.approvals.clone(),
                        c.error.clone()
                    ))
                    .collect::<Vec<_>>()
            ))
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Default)]
struct HistoryGate(Arc<(Mutex<bool>, Condvar)>);

impl HistoryGate {
    fn reader(&self) -> Reader {
        let gate = self.0.clone();
        Arc::new(move |dir, session| {
            let (released, _) = gate
                .1
                .wait_timeout_while(
                    gate.0.lock().unwrap(),
                    Duration::from_secs(15),
                    |released| !*released,
                )
                .unwrap();
            if !*released {
                return Err(std::io::Error::other("test history gate was not released"));
            }
            keystone_agent::log::read(dir, session)
        })
    }

    fn release(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

impl Drop for HistoryGate {
    fn drop(&mut self) {
        self.release();
    }
}

fn restored(sessions: &Path, session: u64) -> Vec<Row> {
    let mut conversation = Conversation::loading();
    conversation.restore(Ok(keystone_agent::log::read(sessions, session).unwrap()));
    conversation.rows
}

fn wait_to_quit(state: &Entity<AppState>, cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        cx.set_global(crate::DesktopApp {
            state: state.clone(),
            windows: Vec::new(),
            quick: None,
        })
    });
    state.update(cx, |state, _| state.quit_when_idle = true);
}

fn completed(state: &AppState, session: u64) -> bool {
    state.conversations.get(&session).is_some_and(|conversation| {
        !conversation.is_loading()
            && !conversation.running
            && conversation.rows.iter().any(|row| matches!(row,
                Row::Item(item) if item.kind == ItemKind::AgentMessage && item.status == ItemStatus::Completed
            ))
    })
}

#[gpui::test]
fn runtime_logs_share_the_store_database_for_custom_paths_and_memory(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    cx.executor().allow_parking();
    for mut store in [
        Store::open(dir.path().join("custom.json")).unwrap(),
        Store::in_memory().unwrap(),
    ] {
        store
            .create(
                Provider::Codex,
                &WorkingDirectory::new(dir.path()).unwrap(),
                1,
            )
            .unwrap();
        let database = store.database();
        let state = cx.new(|cx| AppState::new(store, cx));
        state.update(cx, |state, cx| {
            state.ensure_history(1);
            state.flush_logs(cx).unwrap();
            state
                .conversations
                .get_mut(&1)
                .unwrap()
                .push_local("shared log");
            state.commit(1);
            state.flush_logs(cx).unwrap();
        });
        let entries = database.read_logs(1).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            serde_json::from_str::<Entry>(&entries[0]).unwrap(),
            Entry::Item { item, .. } if item.text == "shared log"
        ));
    }
    assert!(!dir.path().join("workspace.sqlite3").exists());
}

#[gpui::test]
fn global_runtime_loads_sends_answers_and_logs_both_providers_without_a_view(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    for session in [1, 2] {
        let mut log = SessionLog::open(&sessions, session).unwrap();
        log.append(&Entry::Item {
            item: Item {
                id: "history".into(),
                kind: ItemKind::AgentMessage,
                text: format!("Saved history {session}"),
                status: ItemStatus::Completed,
            },
            seq: None,
            run: None,
        })
        .unwrap();
        log.sync().unwrap();
    }
    let weak = {
        let state = runtime(dir.path(), cx);
        connect(&state, dir.path(), 2, false, cx);
        cx.update(|cx| {
            cx.set_global(DesktopApp {
                state: state.clone(),
                windows: Vec::new(),
                quick: None,
            })
        });
        state.downgrade()
    };
    cx.run_until_parked();
    let state = weak
        .upgrade()
        .expect("the app global retains the runtime without a view");
    assert!(cx.windows().is_empty());
    state.update(cx, |state, cx| {
        state.request_catalog(1, Provider::Codex, cx);
        state.request_catalog(2, Provider::Claude, cx);
    });
    wait_until(cx, &state, |state| {
        [(1, Provider::Codex), (2, Provider::Claude)]
            .into_iter()
            .all(|(id, provider)| {
                matches!(
                    state
                        .catalogs
                        .choices(&state.catalog_key(id, provider).unwrap()),
                    Choices::Ready(..)
                )
            })
    });
    let gate = HistoryGate::default();
    state.update(cx, |state, cx| {
        state.logs = Logs::with_reader(sessions.clone(), state.wake.clone(), gate.reader());
        for (id, model, text) in [
            (1, "fake-fast", "please run codex"),
            (2, "fake-haiku", "please run claude"),
        ] {
            state.set_choice(id, chosen(model), cx).unwrap();
            state.set_approval_mode(id, ApprovalMode::Ask, cx).unwrap();
            assert!(state.send_message(
                ComposerDraft {
                    session: id,
                    text: text.into(),
                    attachments: Vec::new(),
                },
                cx
            ));
        }
    });
    wait_until(cx, &state, |state| {
        [1, 2].into_iter().all(|id| {
            state
                .conversations
                .get(&id)
                .is_some_and(|c| c.awaiting_approval() && c.reported_model.is_some())
        })
    });
    state.read_with(cx, |state, _| {
        for (id, model) in [(1, "fake-fast"), (2, "fake-haiku")] {
            let conversation = &state.conversations[&id];
            assert!(conversation.is_loading());
            assert!(conversation.running);
            assert!(conversation.rows.is_empty());
            assert!(conversation.approvals.is_empty());
            assert_eq!(conversation.reported_model.as_deref(), Some(model));
            let saved = state.session(id).unwrap();
            assert!(saved.started());
            assert_eq!(saved.model(), Some(model));
            assert_eq!(saved.approval_mode(), ApprovalMode::Ask);
            assert!(saved.approval_overridden());
            assert!(saved.provider_thread().is_some());
        }
    });
    gate.release();
    wait_until(cx, &state, |state| {
        [1, 2].into_iter().all(|id| {
            state
                .conversations
                .get(&id)
                .is_some_and(|c| !c.is_loading() && c.approvals.len() == 1)
        })
    });
    state.update(cx, |state, cx| {
        for id in [1, 2] {
            assert!(matches!(&state.conversations[&id].rows[0], Row::Item(item) if item.text == format!("Saved history {id}")));
            let request = state.conversations[&id].approvals[0].request.clone();
            state.answer(id, request, true, cx);
        }
    });
    wait_until(cx, &state, |state| {
        completed(state, 1) && completed(state, 2)
    });
    state.update(cx, |state, cx| {
        state.save(Vec::new(), Vec::new(), Vec::new(), cx).unwrap()
    });
    state.read_with(cx, |state, _| {
        assert!(state.error.is_none());
        for (id, prompt, model) in [(1, "please run codex", "fake-fast"), (2, "please run claude", "fake-haiku")] {
            let conversation = &state.conversations[&id];
            assert!(conversation.approvals.is_empty());
            assert!(conversation.error.is_none());
            assert!(conversation.save_error.is_none());
            assert_eq!(restored(&sessions, id), conversation.rows);
            assert_eq!(conversation.rows.iter().filter(|row| matches!(row,
                Row::Item(item) if item.kind == ItemKind::UserMessage && item.text == prompt
            )).count(), 1, "the local message and provider echo share one row");
            let reply = conversation.rows.iter().rposition(|row| matches!(row,
                Row::Item(item) if item.kind == ItemKind::AgentMessage && item.text.contains(prompt) && item.text.contains(model)
            )).unwrap();
            let decision = conversation.rows.iter().position(|row| matches!(row,
                Row::Decision { allowed: true, .. }
            )).unwrap();
            assert!(decision > 0 && decision < reply);
            let entries = keystone_agent::log::read(&sessions, id).unwrap();
            assert_eq!(entries.iter().filter(|entry| matches!(entry, Entry::TurnStarted)).count(), 1);
            assert_eq!(entries.iter().filter(|entry| matches!(entry, Entry::Approval { allowed: true, .. })).count(), 1);
            assert!(matches!(entries.last(), Some(Entry::TurnEnded { status: TurnStatus::Completed, .. })));
        }
    });
    let saved = Store::open(dir.path().join("workspace.json")).unwrap();
    for (session, model) in saved
        .workspace()
        .sessions()
        .iter()
        .zip(["fake-fast", "fake-haiku"])
    {
        assert!(session.started() && session.approval_overridden());
        assert_eq!(session.model(), Some(model));
        assert_eq!(session.approval_mode(), ApprovalMode::Ask);
        assert!(session.draft().is_empty());
        assert!(session.provider_thread().is_some());
    }
    assert!(cx.windows().is_empty());
    drop(state);
    cx.update(|cx| drop(cx.remove_global::<DesktopApp>()));
    cx.run_until_parked();
    assert!(
        weak.upgrade().is_none(),
        "runtime wake tasks do not retain their owner"
    );
}

#[gpui::test]
fn dropping_an_attached_view_during_streaming_keeps_both_turns_alive(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    connect(&state, dir.path(), 20, false, cx);
    cx.update(|cx| {
        keystone_ui::init(cx);
        cx.set_global(DesktopApp {
            state: state.clone(),
            windows: Vec::new(),
            quick: None,
        });
    });
    let window = cx.add_window(|window, cx| Keystone::new(state.clone(), false, window, cx));
    let view = window.root(cx).unwrap();
    let weak_view = view.downgrade();
    cx.update(|cx| cx.global_mut::<DesktopApp>().windows = vec![weak_view.clone()]);
    state.update(cx, |state, cx| {
        for id in [1, 2] {
            assert!(state.send_message(
                ComposerDraft {
                    session: id,
                    text: "long reply without a view until stopped".into(),
                    attachments: Vec::new(),
                },
                cx
            ));
        }
    });
    wait_until(cx, &state, |state| {
        [1, 2].into_iter().all(|id| {
        state.conversations.get(&id).is_some_and(|c| c.running && c.rows.iter().any(|row| matches!(row,
            Row::Item(item) if item.kind == ItemKind::AgentMessage && item.status == ItemStatus::InProgress && !item.text.is_empty()
        )))
    })
    });
    let pids = state.read_with(cx, |state, _| {
        [
            state.codex[&None].pid().unwrap(),
            state.claude[&None].pid(2).unwrap(),
        ]
    });
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    drop(view);
    cx.run_until_parked();
    assert!(
        weak_view.upgrade().is_none(),
        "subscriptions and global registration retain no view"
    );
    assert!(cx.windows().is_empty());
    for pid in pids {
        assert!(alive(pid), "provider {pid} stopped with its view");
    }
    state.read_with(cx, |state, _| {
        assert_eq!(state.phase, Phase::Running);
        assert!(state.conversations[&1].running && state.conversations[&2].running);
    });
    for (id, last) in [(1, "`fake-default`"), (2, "word200")] {
        wait_until(cx, &state, |state| {
            state.conversations[&id].rows.iter().any(|row| matches!(row,
                Row::Item(item) if item.kind == ItemKind::AgentMessage && item.text.ends_with(last)
            ))
        });
        state.update(cx, |state, cx| state.stop_turn(id, cx));
    }
    wait_until(cx, &state, |state| {
        [1, 2].into_iter().all(|id| {
            let conversation = &state.conversations[&id];
            !conversation.running
                && matches!(
                    conversation.rows.last(),
                    Some(Row::TurnEnded {
                        status: TurnStatus::Interrupted,
                        ..
                    })
                )
        })
    });
    state.update(cx, |state, cx| {
        state.save(Vec::new(), Vec::new(), Vec::new(), cx).unwrap()
    });
    state.read_with(cx, |state, _| {
        for id in [1, 2] {
            assert_eq!(
                restored(&dir.path().join("sessions"), id),
                state.conversations[&id].rows
            );
        }
    });
    state.update(cx, |state, cx| state.teardown(cx));
    for pid in pids {
        gone(pid);
    }
}

fn alive(pid: u32) -> bool {
    !matches!(keystone_agent::host::process_state(pid), None | Some('Z'))
}

#[track_caller]
fn gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "owned process {pid} did not exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn tracked(dir: &Path, provider: &str) -> Vec<(u32, u32)> {
    std::fs::read_to_string(dir.join(format!("{provider}.pids")))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (parent, child) = line.split_once(' ')?;
            Some((parent.parse().ok()?, child.parse().ok()?))
        })
        .collect()
}

fn owned_processes(
    state: &Entity<AppState>,
    dir: &Path,
    cx: &mut gpui::TestAppContext,
) -> Vec<u32> {
    connect(state, dir, 2, true, cx);
    state.update(cx, |state, cx| {
        for id in [1, 2] {
            assert!(state.send_message(
                ComposerDraft {
                    session: id,
                    text: "please run it".into(),
                    attachments: Vec::new(),
                },
                cx
            ));
        }
    });
    wait_until(cx, state, |state| {
        [1, 2].into_iter().all(|id| {
            state
                .conversations
                .get(&id)
                .is_some_and(|c| c.running && c.approvals.len() == 1)
        })
    });
    let probe_directory = dir.join("probe");
    std::fs::create_dir(&probe_directory).unwrap();
    std::fs::write(
        probe_directory.join(".fake-claude-ignore-initialize"),
        "all",
    )
    .unwrap();
    std::fs::write(probe_directory.join(".fake-claude-pids"), "").unwrap();
    state.update(cx, |state, _| {
        state.claude[&None].request_catalog(2, probe_directory.to_str().unwrap());
    });
    wait_until(cx, state, |_| {
        tracked(dir, "codex").len() == 1
            && tracked(dir, "claude").len() == 2
            && std::fs::read_to_string(probe_directory.join(".fake-claude-pids")).is_ok_and(
                |pids| pids.trim().parse::<u32>().ok() == Some(tracked(dir, "claude")[1].0),
            )
    });
    state.read_with(cx, |state, _| {
        assert_eq!(
            tracked(dir, "codex")[0].0,
            state.codex[&None].pid().unwrap()
        );
        assert_eq!(
            tracked(dir, "claude")[0].0,
            state.claude[&None].pid(2).unwrap()
        );
    });
    let pids: Vec<_> = tracked(dir, "codex")
        .into_iter()
        .chain(tracked(dir, "claude"))
        .flat_map(|(parent, child)| [parent, child])
        .collect();
    assert_eq!(
        pids.len(),
        6,
        "two providers and one probe, each with a descendant"
    );
    assert!(pids.iter().all(|pid| alive(*pid)));
    pids
}

struct Unrelated(std::process::Child);

impl Unrelated {
    fn new() -> Self {
        Self(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        )
    }
}

impl Drop for Unrelated {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[gpui::test]
fn explicit_teardown_ends_only_owned_providers_descendants_and_catalog_probe(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let unrelated = Unrelated::new();
    let pids = owned_processes(&state, dir.path(), cx);
    state.update(cx, |state, cx| {
        state.save(Vec::new(), Vec::new(), Vec::new(), cx).unwrap();
        state.teardown(cx);
        assert_eq!(state.phase, Phase::Exiting);
        assert!(state.codex.is_empty() && state.claude.is_empty());
        for id in [1, 2] {
            assert!(!state.send_message(
                ComposerDraft {
                    session: id,
                    text: "must not start".into(),
                    attachments: Vec::new(),
                },
                cx
            ));
        }
    });
    for pid in pids {
        gone(pid);
    }
    assert!(
        alive(unrelated.0.id()),
        "teardown touched an unrelated child"
    );
    cx.run_until_parked();
    assert_eq!(tracked(dir.path(), "codex").len(), 1);
    assert_eq!(tracked(dir.path(), "claude").len(), 2);
    for id in [1, 2] {
        assert!(matches!(
            restored(&dir.path().join("sessions"), id).last(),
            Some(Row::TurnEnded {
                status: TurnStatus::Interrupted,
                ..
            })
        ));
        assert!(
            !keystone_agent::log::read(&dir.path().join("sessions"), id)
                .unwrap()
                .iter()
                .any(|entry| matches!(
                    entry,
                    Entry::TurnEnded {
                        status: TurnStatus::Completed,
                        ..
                    }
                ))
        );
    }
}

#[gpui::test]
fn dropping_the_global_owner_ends_only_owned_processes_without_a_view(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let unrelated = Unrelated::new();
    let pids = owned_processes(&state, dir.path(), cx);
    let weak = state.downgrade();
    cx.update(|cx| {
        cx.set_global(DesktopApp {
            state: state.clone(),
            windows: Vec::new(),
            quick: None,
        })
    });
    drop(state);
    cx.run_until_parked();
    assert!(
        weak.upgrade().is_some(),
        "the app retains work without a window"
    );
    assert!(pids.iter().all(|pid| alive(*pid)));
    assert!(cx.windows().is_empty());
    cx.update(|cx| drop(cx.remove_global::<DesktopApp>()));
    cx.run_until_parked();
    assert!(
        weak.upgrade().is_none(),
        "wake tasks do not create an owner cycle"
    );
    for pid in pids {
        gone(pid);
    }
    assert!(
        alive(unrelated.0.id()),
        "owner Drop touched an unrelated child"
    );
    assert_eq!(tracked(dir.path(), "codex").len(), 1);
    assert_eq!(tracked(dir.path(), "claude").len(), 2);
}

#[gpui::test]
fn recovered_provider_metadata_invalidates_the_cached_sidebar(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let capture = seen.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&state, move |_, changed: &RuntimeChanged, _| {
            capture.borrow_mut().push(changed.sidebar)
        })
    });
    state.update(cx, |state, cx| {
        state.ensure_history(1);
        state.flush_logs(cx).unwrap();
    });
    let writer = crate::tests::lock_database(&dir.path().join("workspace.sqlite3"));
    state.update(cx, |state, cx| {
        let _ = state
            .store
            .record_message(1, "Recovered sidebar title", now_ms());
        state.publish(cx);
    });
    crate::tests::settle(&state, cx);
    state.read_with(cx, |state, _| assert!(state.error.is_some()));
    drop(writer);
    seen.borrow_mut().clear();
    state.update(cx, |state, cx| {
        state.receive(
            Vec::new(),
            vec![(
                1,
                Event::ThreadReady {
                    thread_id: "recovered-thread".into(),
                },
            )],
            cx,
        )
    });
    crate::tests::settle(&state, cx);
    state.read_with(cx, |state, _| {
        assert!(state.error.is_none());
        assert_eq!(state.session(1).unwrap().title(), "Recovered sidebar title");
    });
    assert!(
        seen.borrow().iter().any(|sidebar| *sidebar),
        "metadata replay changed the saved title but never invalidated the cached sidebar"
    );
}

/// A scripted provider that keeps the environment Keystone gave it, so its env receipt
/// shows the account home it ran under.
fn account_wrapper(dir: &Path, name: &str) -> PathBuf {
    let binary = keystone_core::test_support::built_binary(&format!("keystone-fake-{name}"));
    let environment = serde_json::json!({
        "KEYSTONE_FAKE_ENV_RECEIPT": dir.join("receipts.jsonl").to_str().unwrap(),
        "KEYSTONE_FAKE_CODEX_DELAY_MS": "5",
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS": "5",
        "KEYSTONE_FAKE_CLAUDE_LOG_DIR": dir.to_str().unwrap(),
    });
    let script = dir.join(format!("{name}-account-wrapper"));
    keystone_core::test_support::write_executable(
        &script,
        format!(
            "#!/usr/bin/python3\nimport os, sys\nos.environ.update({environment})\nbinary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    );
    script
}

fn receipts(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("receipts.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[gpui::test]
fn each_session_runs_on_its_own_accounts_home_and_codex_accounts_get_one_server_each(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    let directory = WorkingDirectory::new(dir).unwrap();
    let (claude_home, codex_home) = (dir.join("claude-work"), dir.join("codex-work"));
    std::fs::create_dir(&codex_home).unwrap();
    let claude_work = store
        .add_account(Provider::Claude, "Work", claude_home.clone(), &[])
        .unwrap()
        .value;
    let codex_work = store
        .add_account(Provider::Codex, "Work", codex_home.clone(), &[])
        .unwrap()
        .value;
    for (provider, account) in [
        (Provider::Claude, None),
        (Provider::Claude, Some(&claude_work)),
        (Provider::Codex, None),
        (Provider::Codex, Some(&codex_work)),
        (Provider::Codex, Some(&codex_work)),
    ] {
        let id = store.create(provider, &directory, 1).unwrap().value;
        store.set_account(id, account.cloned()).unwrap();
    }
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    state.update(cx, |state, cx| {
        state.test_programs = Some((
            account_wrapper(dir, "codex"),
            account_wrapper(dir, "claude"),
        ));
        for id in 1..=5 {
            assert!(state.send_message(
                ComposerDraft {
                    session: id,
                    text: format!("hello {id}"),
                    attachments: Vec::new(),
                },
                cx
            ));
        }
    });
    wait_until(cx, &state, |state| (1..=5).all(|id| completed(state, id)));
    let inherited = |name| std::env::var(name).ok();
    state.read_with(cx, |state, _| {
        assert_eq!(
            state.test_claude_accounts(),
            [None, Some(claude_work.clone())]
        );
        assert_eq!(
            state.test_codex_accounts(),
            [None, Some(codex_work.clone())]
        );
        let receipts = receipts(dir);
        let home_of = |provider: &str, pid: u32| {
            let receipt = receipts
                .iter()
                .find(|r| r["provider"] == provider && r["pid"] == pid)
                .unwrap_or_else(|| panic!("no receipt for {provider} {pid}: {receipts:#?}"));
            let variable = if provider == "claude" {
                "CLAUDE_CONFIG_DIR"
            } else {
                "CODEX_HOME"
            };
            receipt[variable].as_str().map(str::to_owned)
        };
        let claude_default = state.claude[&None].pid(1).unwrap();
        let claude_on_work = state.claude[&Some(claude_work.clone())].pid(2).unwrap();
        assert_eq!(
            home_of("claude", claude_default),
            inherited("CLAUDE_CONFIG_DIR")
        );
        assert_eq!(
            home_of("claude", claude_on_work).as_deref(),
            claude_home.to_str()
        );
        let codex_default = state.codex[&None].pid().unwrap();
        let codex_on_work = state.codex[&Some(codex_work.clone())].pid().unwrap();
        assert_ne!(codex_default, codex_on_work);
        assert_eq!(home_of("codex", codex_default), inherited("CODEX_HOME"));
        assert_eq!(
            home_of("codex", codex_on_work).as_deref(),
            codex_home.to_str()
        );
        let servers = |home: Option<&str>| {
            receipts
                .iter()
                .filter(|r| {
                    r["provider"] == "codex"
                        && r["argv"] == serde_json::json!(["app-server"])
                        && r["CODEX_HOME"].as_str() == home
                })
                .count()
        };
        assert_eq!(
            servers(codex_home.to_str()),
            1,
            "sessions 4 and 5 share one"
        );
        assert!(
            receipts
                .iter()
                .filter(|r| r["CLAUDE_CONFIG_DIR"].as_str() == claude_home.to_str()
                    || r["CODEX_HOME"].as_str() == codex_home.to_str())
                .all(|r| r["HOME"].as_str() == inherited("HOME").as_deref()),
            "HOME is never changed"
        );
        for id in 1..=5 {
            assert!(state.session(id).unwrap().started());
        }
    });
}

#[gpui::test]
fn catalogs_are_asked_per_account_and_never_shared(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    let directory = WorkingDirectory::new(dir).unwrap();
    let home = dir.join("claude-work");
    let work = store
        .add_account(Provider::Claude, "Work", home.clone(), &[])
        .unwrap()
        .value;
    for account in [None, Some(work.clone())] {
        let id = store.create(Provider::Claude, &directory, 1).unwrap().value;
        store.set_account(id, account).unwrap();
    }
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    state.update(cx, |state, cx| {
        state.test_programs = Some((
            account_wrapper(dir, "codex"),
            account_wrapper(dir, "claude"),
        ));
        state.request_catalog(1, Provider::Claude, cx);
    });
    let ready = |state: &AppState, id: u64| {
        matches!(
            state
                .catalogs
                .choices(&state.catalog_key(id, Provider::Claude).unwrap()),
            Choices::Ready(..)
        )
    };
    wait_until(cx, &state, |state| ready(state, 1));
    state.read_with(cx, |state, _| {
        assert!(state.catalog_key(1, Provider::Claude) != state.catalog_key(2, Provider::Claude));
        assert!(
            !ready(state, 2),
            "the default account's catalog is not the work account's"
        );
    });
    state.update(cx, |state, cx| {
        state.request_catalog(2, Provider::Claude, cx)
    });
    wait_until(cx, &state, |state| ready(state, 2));
    let probes: Vec<Option<String>> = receipts(dir)
        .iter()
        .filter(|r| r["argv"].to_string().contains("stream-json"))
        .map(|r| r["CLAUDE_CONFIG_DIR"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(probes.len(), 2, "{probes:?}");
    assert!(probes.contains(&home.to_str().map(str::to_owned)));
    assert!(probes.contains(&std::env::var("CLAUDE_CONFIG_DIR").ok()));
}

#[gpui::test]
fn adding_an_account_creates_a_private_folder_and_refuses_shared_homes(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let folder = dir.path().join("accounts/claude-work");
    let added = state
        .update(cx, |state, cx| {
            state.add_account(Provider::Claude, "Work", folder.to_str().unwrap(), cx)
        })
        .unwrap()
        .value;
    let metadata = std::fs::metadata(&folder).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    let inside = folder.join("nested");
    let refused = state
        .update(cx, |state, cx| {
            state.add_account(Provider::Codex, "Inside", inside.to_str().unwrap(), cx)
        })
        .unwrap_err();
    assert!(refused.contains("already used"), "{refused}");
    assert!(!inside.exists(), "a refused account leaves no new folder");
    assert!(
        state
            .update(cx, |state, cx| state.add_account(
                Provider::Codex,
                "Rel",
                "relative",
                cx
            ))
            .is_err()
    );
    state.update(cx, |state, cx| {
        state
            .set_provider_account(2, Provider::Claude, Some(added.clone()), cx)
            .unwrap();
        assert!(state.remove_account(&added, cx).is_err());
        state
            .set_provider_account(2, Provider::Claude, None, cx)
            .unwrap();
        state.remove_account(&added, cx).unwrap();
    });
    assert!(folder.is_dir(), "removing an account keeps its folder");
}

#[gpui::test]
fn a_symlinked_or_refused_folder_is_checked_before_anything_is_created(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let real = dir.path().join("claude-work");
    state
        .update(cx, |state, cx| {
            state.add_account(Provider::Claude, "Work", real.to_str().unwrap(), cx)
        })
        .unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let refused = state
        .update(cx, |state, cx| {
            state.add_account(Provider::Codex, "Alias", alias.to_str().unwrap(), cx)
        })
        .unwrap_err();
    assert!(refused.contains("already"), "{refused}");
    let through = alias.join("nested");
    let refused = state
        .update(cx, |state, cx| {
            state.add_account(Provider::Codex, "Through", through.to_str().unwrap(), cx)
        })
        .unwrap_err();
    assert!(refused.contains("already"), "{refused}");
    assert!(
        !real.join("nested").exists(),
        "nothing is created inside another account's folder"
    );
    let linked = dir.path().join("linked");
    std::fs::create_dir(dir.path().join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), &linked).unwrap();
    let added = state
        .update(cx, |state, cx| {
            state.add_account(Provider::Codex, "Linked", linked.to_str().unwrap(), cx)
        })
        .unwrap()
        .value;
    state.read_with(cx, |state, _| {
        let account = state.store().workspace().account(&added).unwrap();
        assert_eq!(
            account.home(),
            Some(
                std::fs::canonicalize(dir.path().join("elsewhere"))
                    .unwrap()
                    .as_path()
            ),
            "the real folder is saved"
        );
    });

    let nested = dir.path().join("new/a/b");
    assert!(
        state
            .update(cx, |state, cx| state.add_account(
                Provider::Claude,
                " ",
                nested.to_str().unwrap(),
                cx
            ))
            .is_err()
    );
    assert!(
        !dir.path().join("new").exists(),
        "a refused name creates nothing"
    );
}

#[gpui::test]
fn an_account_on_the_inherited_default_home_never_sends(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let default = keystone_agent::default_home(Provider::Claude).unwrap();
    state.update(cx, |state, cx| {
        let clash = state
            .store
            .add_account(Provider::Claude, "Clash", default.clone(), &[])
            .unwrap()
            .value;
        state
            .set_provider_account(2, Provider::Claude, Some(clash), cx)
            .unwrap();
        assert!(!state.send_message(
            ComposerDraft {
                session: 2,
                text: "hello".into(),
                attachments: Vec::new(),
            },
            cx
        ));
    });
    state.read_with(cx, |state, _| {
        let error = state.conversations[&2].error.clone().unwrap_or_default();
        assert!(error.contains("overlaps a default home"), "{error}");
        assert!(state.claude.is_empty(), "no process was started");
    });
}

#[gpui::test]
fn a_parent_step_after_a_symlink_follows_the_filesystem(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let base = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(base.join("real/child")).unwrap();
    std::os::unix::fs::symlink(base.join("real/child"), base.join("alias")).unwrap();
    let typed = format!("{}/alias/../work", base.display());
    let added = state
        .update(cx, |state, cx| {
            state.add_account(Provider::Claude, "Work", &typed, cx)
        })
        .unwrap()
        .value;
    state.read_with(cx, |state, _| {
        assert_eq!(
            state.store().workspace().account(&added).unwrap().home(),
            Some(base.join("real/work").as_path()),
            "the folder the filesystem reaches, not the text"
        );
    });
    assert!(base.join("real/work").is_dir());
    assert!(!base.join("work").exists());
}

#[gpui::test]
fn an_account_folder_swapped_for_a_link_to_another_never_sends(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let (work, personal) = (dir.path().join("work"), dir.path().join("personal"));
    let ids: Vec<AccountId> = state.update(cx, |state, cx| {
        [("Work", &work), ("Personal", &personal)]
            .map(|(label, home)| {
                state
                    .add_account(Provider::Claude, label, home.to_str().unwrap(), cx)
                    .unwrap()
                    .value
            })
            .into()
    });
    std::fs::remove_dir(&work).unwrap();
    std::os::unix::fs::symlink(&personal, &work).unwrap();
    state.update(cx, |state, cx| {
        state
            .set_provider_account(2, Provider::Claude, Some(ids[0].clone()), cx)
            .unwrap();
        assert!(!state.send_message(
            ComposerDraft {
                session: 2,
                text: "hello".into(),
                attachments: Vec::new(),
            },
            cx
        ));
    });
    state.read_with(cx, |state, _| {
        let error = state.conversations[&2].error.clone().unwrap_or_default();
        assert!(error.contains("overlaps the account"), "{error}");
        assert!(state.claude.is_empty(), "no process was started");
    });
}

fn send_text(state: &Entity<AppState>, text: &str, cx: &mut gpui::TestAppContext) {
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: text.into(),
                attachments: Vec::new(),
            },
            cx
        ));
    });
}

fn replied(state: &AppState, text: &str) -> bool {
    let reply = format!("You wrote: {text}");
    state.conversations[&1].rows.iter().any(|row| {
        matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage && item.text.contains(&reply))
    })
}

fn streaming(state: &AppState) -> bool {
    state.conversations[&1]
        .rows
        .iter()
        .any(|row| matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage))
}

#[gpui::test]
fn a_follow_up_still_queued_when_the_turn_ends_runs_before_keystone_quits(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    connect(&state, dir.path(), 20, false, cx);
    send_text(&state, "hello until released", cx);
    wait_until(cx, &state, streaming);
    let (open, gate) = std::sync::mpsc::channel::<()>();
    state.read_with(cx, |state, _| {
        state.codex[&None].run_in_order(
            1,
            Box::new(move || {
                let _ = gate.recv();
            }),
        )
    });
    wait_to_quit(&state, cx);
    send_text(&state, "again", cx);
    std::fs::write(dir.path().join("codex-release"), "").unwrap();
    wait_until(cx, &state, |state| !state.conversations[&1].running);
    assert_eq!(
        state.read_with(cx, |state, _| state.phase),
        Phase::Running,
        "the follow-up the connection still holds keeps Keystone open"
    );
    drop(open);
    wait_until(cx, &state, |state| state.phase == Phase::Exiting);
    state.read_with(cx, |state, _| {
        assert!(
            replied(state, "again"),
            "the follow-up ran before Keystone quit"
        );
        assert!(completed(state, 1));
    });
}

#[gpui::test]
fn a_steer_codex_rejects_after_the_turn_ends_runs_before_keystone_quits(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    connect(&state, dir.path(), 20, false, cx);
    send_text(&state, "end on steer until released", cx);
    wait_until(cx, &state, streaming);
    wait_to_quit(&state, cx);
    send_text(&state, "late", cx);
    wait_until(cx, &state, |state| !state.conversations[&1].running);
    assert_eq!(
        state.read_with(cx, |state, _| state.phase),
        Phase::Running,
        "the steer Codex has not answered keeps Keystone open"
    );
    std::fs::write(dir.path().join("codex-release"), "").unwrap();
    wait_until(cx, &state, |state| state.phase == Phase::Exiting);
    state.read_with(cx, |state, _| {
        assert!(
            replied(state, "late"),
            "the steer's turn ran before Keystone quit"
        );
        assert!(completed(state, 1));
    });
}

#[path = "recovery_tests.rs"]
mod recovery;

#[path = "idle_tests.rs"]
mod idle;

#[path = "delivery_tests.rs"]
mod delivery;

#[path = "scheduled_tests.rs"]
mod scheduled;

#[gpui::test]
fn an_account_never_shows_usage_left_under_its_id(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
    let mut usage = AccountUsage::default();
    usage.record(
        vec![LimitWindow {
            id: "five_hour".into(),
            label: "5-hour".into(),
            used_percent: 90.0,
            resets_at: None,
        }],
        1,
    );
    // A save that landed after its account was removed.
    let id = store
        .add_account(Provider::Claude, "Work", dir.path().join("old"), &[])
        .unwrap()
        .value;
    store.remove_account(&id).unwrap();
    store
        .database()
        .record_usage(&format!("claude/{id}"), &usage)
        .unwrap();
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    let folder = dir.path().join("new");
    let again = state
        .update(cx, |state, cx| {
            state.add_account(Provider::Claude, "Work", folder.to_str().unwrap(), cx)
        })
        .unwrap()
        .value;
    assert_eq!(again, id);
    state.read_with(cx, |state, _| {
        assert_eq!(state.usage(Provider::Claude, Some(&again)), None);
    });

    // A save still queued when its account goes is dropped.
    state.update(cx, |state, cx| {
        state.usage_saves.lock().unwrap().1 = true;
        state.record_usage(
            Provider::Claude,
            Some(&again),
            usage.readings.iter().map(|r| r.window.clone()).collect(),
            cx,
        );
        state.remove_account(&again, cx).unwrap();
        assert!(state.usage_saves.lock().unwrap().0.is_empty());
    });
}

/// An import read nobody waits on any more leaves nothing behind when it finishes.
#[gpui::test]
fn an_abandoned_import_read_keeps_no_answer(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("claude-home");
    let folder = home.join("projects/p");
    std::fs::create_dir_all(&folder).unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let record = serde_json::json!({"type": "user", "uuid": "u1", "parentUuid": null,
        "cwd": cwd, "sessionId": "s", "message": {"role": "user", "content": "hi"}});
    std::fs::write(folder.join("s.jsonl"), format!("{record}\n")).unwrap();
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| state.test_claude_home = Some(home));

    let (left, kept) = state.update(cx, |state, _| {
        let left = state.read_native(Provider::Claude, None, &cwd, "s".into());
        state.abandon_native(left);
        (
            left,
            state.read_native(Provider::Claude, None, &cwd, "s".into()),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let answer = loop {
        if let Some(answer) = state.update(cx, |state, _| state.take_native(kept)) {
            break answer;
        }
        assert!(Instant::now() < deadline, "the read answers");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(matches!(answer, Native::Read(Ok(_))));
    std::thread::sleep(Duration::from_millis(100));
    state.update(cx, |state, _| {
        assert!(state.take_native(left).is_none());
        assert!(state.native.lock().unwrap().is_empty());
    });
}

fn git(folder: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Each turn of a session in a repository gets a checkpoint, and restoring one puts
/// the files back while no message to a session in that repository goes.
#[gpui::test]
fn turns_get_checkpoints_and_a_restore_holds_messages_to_the_repository(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "start\n").unwrap();
    git(&repo, &["add", "--all"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-q",
            "-m",
            "start",
        ],
    );
    let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
    let directory = WorkingDirectory::new(&repo).unwrap();
    for provider in [Provider::Codex, Provider::Claude] {
        store.create(provider, &directory, 1).unwrap();
    }
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    connect(&state, dir.path(), 0, false, cx);
    state.update(cx, |state, _| state.test_checkpoints = true);
    let send = |state: &mut AppState, session: u64, text: &str, cx: &mut Context<AppState>| {
        state.send_message(
            ComposerDraft {
                session,
                text: text.into(),
                attachments: Vec::new(),
            },
            cx,
        )
    };
    let saved = |state: &AppState, turns: usize| match state.checkpoints(2) {
        Some(checkpoints::State::Saved(saved)) => saved.turns.len() == turns,
        _ => false,
    };
    for (turn, text) in [
        (1, "write a.txt: one"),
        (2, "write a.txt: two; write b.txt: new"),
    ] {
        state.update(cx, |state, cx| assert!(send(state, 2, text, cx)));
        wait_until(cx, &state, |state| {
            completed(state, 2) && saved(state, turn)
        });
    }
    state.read_with(cx, |state, _| match state.checkpoints(2) {
        Some(checkpoints::State::Saved(saved)) => {
            assert_eq!(
                saved.turns.values().copied().collect::<Vec<_>>(),
                [Some(1), Some(2)]
            )
        }
        other => panic!("{other:?}"),
    });

    let restoring = state.update(cx, |state, cx| {
        let restoring = state.restore_checkpoint(2, 1, cx).unwrap();
        assert!(
            state.restoring(1),
            "the other session shares the repository"
        );
        assert!(!send(state, 1, "too soon", cx));
        assert_eq!(
            state.conversations[&1].error.as_deref(),
            Some(checkpoints::RESTORING)
        );
        restoring
    });
    let restored = finish(restoring, true, &state, cx).unwrap();
    assert!(restored.skipped.is_empty());
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!repo.join("b.txt").exists());
    state.read_with(cx, |state, _| assert!(!state.restoring(1)));
    let refs = git(
        &repo,
        &["for-each-ref", "--format=%(refname)", "refs/keystone"],
    );
    let store = state.read_with(cx, |state, _| {
        state.checkpoint_store.session(2).unwrap().store
    });
    assert_eq!(
        refs.lines().collect::<Vec<_>>(),
        ["1", "2", "before-restore"]
            .map(|name| format!("refs/keystone/checkpoints/{store}/2/{name}"))
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), " M a.txt\n");
}

/// Waits for a restore to end, keeping its files once they are in place when `keep`.
fn finish(
    restoring: checkpoints::Restoring,
    keep: bool,
    state: &Entity<AppState>,
    cx: &mut gpui::TestAppContext,
) -> keystone_agent::checkpoint::Result<keystone_agent::checkpoint::Restored> {
    let checkpoints::Restoring { placed, done, .. } = restoring;
    let result = Arc::new(Mutex::new(None));
    let slot = result.clone();
    cx.spawn(async move |_| {
        if let Ok(placed) = placed.await
            && keep
        {
            placed.keep();
        }
        *slot.lock().unwrap() = Some(done.await.unwrap());
    })
    .detach();
    wait_until(cx, state, |_| result.lock().unwrap().is_some());
    result.lock().unwrap().take().unwrap()
}

/// A repository with one commit, and a runtime with `sessions` in folders of it or of
/// other repositories, numbered from 1, whose providers write files as told.
fn repositories(
    dir: &Path,
    sessions: &[(&str, Provider)],
    cx: &mut gpui::TestAppContext,
) -> Entity<AppState> {
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    for (folder, provider) in sessions {
        let repo = dir.join(folder);
        if !repo.exists() {
            repository(&repo);
        }
        store
            .create(*provider, &WorkingDirectory::new(&repo).unwrap(), 1)
            .unwrap();
    }
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    connect(&state, dir, 0, false, cx);
    state.update(cx, |state, _| state.test_checkpoints = true);
    state
}

/// A checkpoint store the database's lock kept from being named at start is never named
/// on the UI thread: reading a session's checkpoints never waits for the writer, and
/// the read ends without the launch-time error.
#[gpui::test]
fn a_checkpoint_store_that_failed_at_start_is_not_retried_on_the_ui_thread(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(
            Provider::Codex,
            &WorkingDirectory::new(dir.path()).unwrap(),
            1,
        )
        .unwrap();
    store.flush().unwrap();
    let lock = crate::tests::lock_database(&path);
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    drop(lock);
    let stalled = dir.path().join("stalled");
    std::fs::write(&stalled, "").unwrap();
    state.update(cx, |state, _| {
        state.test_checkpoints = true;
        state.test_sync_directory(crate::tests::stall_while_marked);
        state.store.set_draft(1, "held".into()).unwrap();
    });
    let read = Instant::now();
    state.update(cx, |state, cx| state.read_checkpoints(1, cx));
    let took = read.elapsed();
    std::fs::remove_file(&stalled).unwrap();
    assert!(
        took < keystone_core::WRITER_WAIT,
        "the read waited {took:?}"
    );
    wait_until(cx, &state, |state| state.checkpoints(1).is_some());
    state.read_with(cx, |state, _| {
        assert_eq!(state.checkpoints(1), Some(&checkpoints::State::Unavailable))
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

/// A new repository at `repo` with one commit.
fn repository(repo: &Path) {
    std::fs::create_dir(repo).unwrap();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "start\n").unwrap();
    git(repo, &["add", "--all"]);
    git(
        repo,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-q",
            "-m",
            "start",
        ],
    );
}

/// A database lock at launch that clears later still lets a turn save its checkpoint:
/// the store is named when a checkpoint needs it, not only at start.
#[gpui::test]
fn a_checkpoint_store_locked_at_launch_is_named_once_the_lock_clears(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    repository(&repo);
    let path = dir.path().join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(Provider::Codex, &WorkingDirectory::new(&repo).unwrap(), 1)
        .unwrap();
    store.flush().unwrap();
    let lock = crate::tests::lock_database(&path);
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    drop(lock);
    connect(&state, dir.path(), 0, false, cx);
    state.update(cx, |state, _| state.test_checkpoints = true);
    checkpointed_turn(&state, 1, "write a.txt: one", 1, cx);
    state.update(cx, |state, cx| state.teardown(cx));
}

/// Sends `text` to the session and waits for its turn's checkpoint.
fn checkpointed_turn(
    state: &Entity<AppState>,
    session: u64,
    text: &str,
    turns: usize,
    cx: &mut gpui::TestAppContext,
) {
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session,
                text: text.into(),
                attachments: Vec::new(),
            },
            cx,
        ))
    });
    wait_until(cx, state, |state| {
        completed(state, session)
            && matches!(state.checkpoints(session),
                Some(checkpoints::State::Saved(saved)) if saved.turns.len() == turns)
    });
}

/// Makes git's conversion of `*.txt` files in `repo` hang, writing the filter's process
/// id to the returned file.
fn hang_conversions(repo: &Path) -> PathBuf {
    let pid = repo.join(".git/filter-pid");
    git(
        repo,
        &[
            "config",
            "filter.slow.smudge",
            &format!("echo $$ > '{}'; sleep 30; cat", pid.display()),
        ],
    );
    std::fs::write(repo.join(".git/info/attributes"), "*.txt filter=slow\n").unwrap();
    pid
}

fn hanging(pid: &Path) -> bool {
    std::fs::read_to_string(pid).is_ok_and(|pid| pid.trim().parse::<u32>().is_ok())
}

/// A restore stuck in a filter is cancelled: no file changes, and the repository's
/// sessions can be sent to again.
#[gpui::test]
fn a_cancelled_restore_changes_nothing_and_releases_the_repository(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let state = repositories(
        dir.path(),
        &[("repo", Provider::Codex), ("repo", Provider::Claude)],
        cx,
    );
    checkpointed_turn(&state, 2, "write a.txt: one", 1, cx);
    std::fs::write(repo.join("a.txt"), "later\n").unwrap();
    std::fs::write(repo.join("b.txt"), "later\n").unwrap();
    let pid = hang_conversions(&repo);

    let restoring = state.update(cx, |state, cx| state.restore_checkpoint(2, 1, cx).unwrap());
    let cancel = restoring.cancel.clone();
    wait_until(cx, &state, |_| hanging(&pid));
    cancel.cancel();
    let restored = finish(restoring, true, &state, cx);

    assert!(
        matches!(restored, Err(keystone_agent::checkpoint::Error::Cancelled)),
        "{restored:?}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "later\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("b.txt")).unwrap(),
        "later\n"
    );
    state.update(cx, |state, cx| {
        assert!(!state.restoring(1));
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: "after the cancel".into(),
                attachments: Vec::new(),
            },
            cx,
        ));
    });
}

/// A restore that takes long in one repository holds no Codex session in another,
/// though one app-server serves them both.
#[gpui::test]
fn a_slow_restore_holds_no_codex_session_elsewhere(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = repositories(
        dir.path(),
        &[("slow", Provider::Codex), ("other", Provider::Codex)],
        cx,
    );
    checkpointed_turn(&state, 1, "first", 1, cx);
    let slow = dir.path().join("slow");
    std::fs::write(slow.join("a.txt"), "later\n").unwrap();
    let pid = hang_conversions(&slow);
    let restoring = state.update(cx, |state, cx| state.restore_checkpoint(1, 1, cx).unwrap());
    wait_until(cx, &state, |_| hanging(&pid));

    checkpointed_turn(&state, 2, "elsewhere", 1, cx);

    restoring.cancel.cancel();
    assert!(finish(restoring, true, &state, cx).is_err());
}

/// Quitting while a restore is stuck in a filter stops it at once and puts every file
/// back.
#[gpui::test]
fn quitting_during_a_restore_puts_every_file_back(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let state = repositories(dir.path(), &[("repo", Provider::Claude)], cx);
    checkpointed_turn(&state, 1, "write a.txt: one", 1, cx);
    std::fs::write(repo.join("a.txt"), "later\n").unwrap();
    std::fs::write(repo.join("b.txt"), "later\n").unwrap();
    let pid = hang_conversions(&repo);
    let restoring = state.update(cx, |state, cx| state.restore_checkpoint(1, 1, cx).unwrap());
    wait_until(cx, &state, |_| hanging(&pid));

    let start = Instant::now();
    state.update(cx, |state, cx| state.teardown(cx));

    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "later\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("b.txt")).unwrap(),
        "later\n"
    );
    drop(restoring);
}
