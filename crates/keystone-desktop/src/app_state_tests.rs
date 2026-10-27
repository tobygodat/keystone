use super::*;
use keystone_agent::log::{Entry, Reader, SessionLog};
use keystone_agent::{Item, ItemStatus};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

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
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("keystone-fake-{name}"));
    assert!(
        binary.is_file(),
        "build scripted providers with the UI lane"
    );
    let environment = serde_json::json!({
        "PATH": "/usr/bin:/bin",
        "KEYSTONE_FAKE_CODEX_DELAY_MS": delay_ms.to_string(),
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS": delay_ms.to_string(),
        "KEYSTONE_FAKE_CLAUDE_LOG_DIR": dir.to_str().unwrap(),
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
    std::fs::write(
        &script,
        format!(
            "#!/usr/bin/python3\nimport os, subprocess, sys\nos.environ.clear()\nos.environ.update({environment})\n{tracking}binary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
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
                    Choices::Ready(_)
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
            state.set_model(id, Some(model.into()), cx).unwrap();
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
                    text: "long reply without a view".into(),
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
        assert!(state.conversations[&2].running);
    });
    wait_until(cx, &state, |state| {
        completed(state, 1) && completed(state, 2)
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
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
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
        state.persist(PendingSave::Message(
            1,
            "Recovered sidebar title".into(),
            now_ms(),
        ));
        state.publish(cx);
    });
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
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("keystone-fake-{name}"));
    let environment = serde_json::json!({
        "KEYSTONE_FAKE_ENV_RECEIPT": dir.join("receipts.jsonl").to_str().unwrap(),
        "KEYSTONE_FAKE_CODEX_DELAY_MS": "5",
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS": "5",
        "KEYSTONE_FAKE_CLAUDE_LOG_DIR": dir.to_str().unwrap(),
    });
    let script = dir.join(format!("{name}-account-wrapper"));
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
            Choices::Ready(_)
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
            std::fs::canonicalize(dir.path().join("elsewhere")).unwrap(),
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
            base.join("real/work"),
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

#[path = "recovery_tests.rs"]
mod recovery;

#[path = "idle_tests.rs"]
mod idle;

#[path = "delivery_tests.rs"]
mod delivery;

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
