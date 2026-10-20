use super::*;
use app_state::Phase;
use gpui::{TestAppContext, VisualTestContext};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

struct RestoredRow(Row);

impl Render for RestoredRow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        transcript::row(
            &self.0,
            0,
            None,
            transcript::RowControls {
                activity: Some(transcript::ActivityControl {
                    expanded: true,
                    selector: "activity-expanded-0".into(),
                    body_selector: "activity-body-0".into(),
                    on_toggle: Box::new(|_, _, _| {}),
                }),
                ..Default::default()
            },
            cx,
        )
    }
}

fn open<'a>(
    dir: &Path,
    cx: &'a mut TestAppContext,
) -> (
    Entity<Keystone>,
    Entity<AppState>,
    &'a mut VisualTestContext,
) {
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    let directory = keystone_core::WorkingDirectory::new(dir).unwrap();
    store.create(Provider::Codex, &directory, 1).unwrap();
    store.create(Provider::Claude, &directory, 2).unwrap();
    store.select(1).unwrap();
    let state = cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
        install_app_state(store, cx)
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = Keystone::new(state.clone(), false, window, cx);
        view.focus.focus(window, cx);
        view
    });
    cx.update(|window, cx| register_window(view.downgrade(), window, cx));
    (view, state, cx)
}

fn draft(view: &Entity<Keystone>, cx: &mut VisualTestContext, text: &str) {
    view.update(cx, |view, cx| {
        view.composer
            .update(cx, |input, cx| input.set_value(text.into(), cx))
    });
}

fn phase(state: &Entity<AppState>, cx: &mut VisualTestContext) -> Phase {
    state.read_with(cx, |state, _| state.phase)
}

fn tracked_provider(dir: &Path, provider: Provider, starting: bool) -> PathBuf {
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
    assert!(binary.is_file());
    let path = dir.join(format!("{name}-wrapper"));
    let code = format!(
        "#!/usr/bin/python3\nimport os,subprocess,sys,time\nprobe = '--version' in sys.argv\nif not probe or {starting}:\n child = subprocess.Popen(['/bin/sleep','60'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n with open({},'a') as f: f.write(f'{{os.getpid()}} {{child.pid}}\\n')\nif probe and {starting}: time.sleep(1)\nos.execve({},[{}]+sys.argv[1:],{{'PATH':'/usr/bin:/bin','KEYSTONE_FAKE_CODEX_DELAY_MS':'150','KEYSTONE_FAKE_CLAUDE_DELAY_MS':'150'}})\n",
        serde_json::to_string(&dir.join(format!("{name}.pids"))).unwrap(),
        serde_json::to_string(&binary).unwrap(),
        serde_json::to_string(&binary).unwrap(),
        starting = if starting { "True" } else { "False" }
    );
    std::fs::write(&path, code).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|s| !s.split_once(") ").unwrap().1.starts_with('Z'))
}

fn pids(dir: &Path, provider: Provider) -> Vec<u32> {
    let name = match provider {
        Provider::Codex => "codex",
        Provider::Claude => "claude",
    };
    std::fs::read_to_string(dir.join(format!("{name}.pids")))
        .unwrap_or_default()
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect()
}

fn wait(cx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "condition did not become true");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn start(
    state: &Entity<AppState>,
    cx: &mut VisualTestContext,
    dir: &Path,
    provider: Provider,
    starting: bool,
) {
    let script = tracked_provider(dir, provider, starting);
    state.update(cx, |state, cx| {
        let wake = state.test_wake().clone();
        match provider {
            Provider::Codex => {
                state.test_set_codex(Some(CodexConnection::new(Some(script), "test", wake)))
            }
            Provider::Claude => {
                state.test_set_claude(Some(ClaudeConnection::new(Some(script), wake)))
            }
        }
        let id = if provider == Provider::Codex { 1 } else { 2 };
        assert!(state.send_message(
            ComposerDraft {
                session: id,
                text: "long turn for shutdown".into()
            },
            cx
        ));
    });
    wait(cx, |cx| {
        !pids(dir, provider).is_empty()
            && (starting
                || state.read_with(cx, |s, _| {
                    s.conversations()[&if provider == Provider::Codex { 1 } else { 2 }].running
                }))
    });
}

#[gpui::test]
fn dirty_and_reentrant_quit_collects_one_exact_snapshot(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, cx) = open(dir.path(), cx);
    let before = state.read_with(cx, |s, _| s.store().writes());
    view.update(cx, |view, cx| {
        view.composer.update(cx, |input, cx| {
            input.set_value("  Exact\nunsaved draft  ".into(), cx)
        });
        request_quit(cx);
        request_quit(cx);
    });
    assert_eq!(phase(&state, cx), Phase::Exiting);
    state.read_with(cx, |s, _| {
        assert_eq!(s.save_calls, 1);
        assert_eq!(s.store().writes(), before + 1);
    });
    assert_eq!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft(),
        "  Exact\nunsaved draft  "
    );
    view.read_with(cx, |v, _| assert!(v.draft_save_task.is_none()));
}

#[gpui::test]
fn clean_global_quit_without_a_window_does_not_write_a_snapshot(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = cx.update(|cx| {
        install_app_state(Store::open(dir.path().join("workspace.json")).unwrap(), cx)
    });
    cx.update(request_quit);
    state.read_with(cx, |s, _| {
        assert_eq!(s.phase, Phase::Exiting);
        assert_eq!(s.save_calls, 1);
        assert_eq!(s.store().writes(), 0);
    });
}

#[gpui::test]
fn workspace_failure_keeps_settings_window_and_live_providers_then_retries(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    let (view, state, cx) = open(&state_dir, cx);
    for provider in [Provider::Codex, Provider::Claude] {
        start(&state, cx, dir.path(), provider, false);
    }
    draft(&view, cx, "Keep the exact failed-Quit draft");
    view.update_in(cx, |v, window, cx| v.toggle_settings(window, cx));
    std::fs::rename(&state_dir, dir.path().join("backup")).unwrap();
    cx.simulate_keystrokes("ctrl-q");
    cx.run_until_parked();
    assert_eq!(phase(&state, cx), Phase::Running);
    assert!(cx.debug_bounds("shutdown-error").is_some());
    let settings_error = cx.debug_bounds("settings-error").unwrap();
    let shutdown_error = cx.debug_bounds("shutdown-error").unwrap();
    assert!(
        shutdown_error.top() >= settings_error.top()
            && shutdown_error.bottom() <= settings_error.bottom(),
        "Settings must render one shutdown error in its existing error area"
    );
    for provider in [Provider::Codex, Provider::Claude] {
        assert!(pids(dir.path(), provider).iter().all(|p| alive(*p)));
    }
    view.read_with(cx, |v, cx| {
        assert_eq!(
            v.composer.read(cx).value(),
            "Keep the exact failed-Quit draft"
        )
    });
    std::fs::rename(dir.path().join("backup"), &state_dir).unwrap();
    view.update(cx, |v, cx| {
        assert!(v.save_draft(cx));
    });
    assert!(state.read_with(cx, |s, _| s.shutdown_error.is_some()));
    cx.simulate_keystrokes("ctrl-q");
    cx.run_until_parked();
    assert_eq!(phase(&state, cx), Phase::Exiting);
    wait(cx, |_| {
        [Provider::Codex, Provider::Claude]
            .iter()
            .all(|p| pids(dir.path(), *p).iter().all(|pid| !alive(*pid)))
    });
    assert_eq!(
        Store::open(state_dir.join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft(),
        "Keep the exact failed-Quit draft"
    );
}

#[gpui::test]
fn failed_nonselected_log_barrier_vetoes_close_and_retry_needs_no_send(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, cx) = open(dir.path(), cx);
    state.update(cx, |s, cx| {
        s.ensure_history(2);
        s.flush_logs(cx).unwrap();
    });
    let sessions = dir.path().join("sessions");
    std::fs::write(&sessions, "blocked").unwrap();
    state.update(cx, |s, cx| {
        s.test_conversation(2).push_local("retained exactly once");
        s.commit(2);
        s.publish(cx);
    });
    draft(&view, cx, "close draft");
    assert!(!cx.simulate_close());
    cx.run_until_parked();
    assert_eq!(phase(&state, cx), Phase::Running);
    assert!(cx.debug_bounds("shutdown-error").is_some());
    assert!(state.read_with(cx, |s, _| s.conversations()[&2].save_error.is_some()));
    std::fs::remove_file(sessions).unwrap();
    assert!(!cx.simulate_close());
    cx.run_until_parked();
    assert_eq!(phase(&state, cx), Phase::Exiting);
    let entries = keystone_agent::log::read(&dir.path().join("sessions"), 2).unwrap();
    let texts: Vec<_> = entries
        .iter()
        .filter_map(|e| match e {
            keystone_agent::log::Entry::Item { item, .. } => Some(item.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["retained exactly once"]);
}

#[gpui::test]
fn terminal_budget_loss_stays_visible_but_repaired_pending_writes_allow_quit(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, cx) = open(dir.path(), cx);
    state.update(cx, |state, cx| {
        state.ensure_history(2);
        state.flush_logs(cx).unwrap();
    });
    let sessions = dir.path().join("sessions");
    std::fs::write(&sessions, "blocked").unwrap();
    state.update(cx, |state, cx| {
        state
            .test_conversation(2)
            .push_local("retained before the gap");
        state.commit(2);
        state
            .test_conversation(2)
            .apply(Event::ItemCompleted(keystone_agent::Item {
                id: "oversized-rejected-item".into(),
                kind: ItemKind::Other {
                    name: "scripted output".into(),
                },
                text: "a".repeat(16 * 1024 * 1024),
                status: keystone_agent::ItemStatus::Completed,
            }));
        state.commit(2);
        state.publish(cx);
    });
    draft(&view, cx, "exact draft despite terminal loss");
    view.update_in(cx, |view, window, cx| view.toggle_settings(window, cx));
    cx.simulate_keystrokes("ctrl-q");
    cx.run_until_parked();
    assert_eq!(phase(&state, cx), Phase::Running);
    assert!(state.read_with(cx, |state, _| {
        state.conversations()[&2].save_error.is_some()
    }));
    view.read_with(cx, |view, cx| {
        let message = view.status_line(cx).unwrap().0;
        assert!(message.contains("Could not save conversation logs"));
        let io = std::fs::create_dir_all(&sessions).unwrap_err().to_string();
        assert_eq!(
            message.matches(&io).count(),
            1,
            "failed Quit must show each storage error once: {message}"
        );
    });
    std::fs::remove_file(&sessions).unwrap();
    cx.simulate_keystrokes("ctrl-q");
    cx.run_until_parked();
    assert_eq!(
        phase(&state, cx),
        Phase::Exiting,
        "already discarded entries must not cause an irreparable Quit veto"
    );
    let entries = keystone_agent::log::read(&sessions, 2).unwrap();
    let mut restored = Conversation::loading();
    restored.restore(Ok(entries));
    let (_, rendered) = cx.add_window_view(|_, _| RestoredRow(restored.rows[1].clone()));
    assert!(
        rendered.debug_bounds("log-loss-notice-0").is_some(),
        "the restored loss marker must render as a plain notice, not a tool-use row"
    );
    let texts: Vec<_> = restored
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Item(item) => Some(item.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        [
            "retained before the gap",
            "1 log entries were lost (log budget exceeded)."
        ]
    );
    assert_eq!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft(),
        "exact draft despite terminal loss"
    );
}

#[gpui::test]
fn nonselected_terminal_loss_is_visible_before_any_quit_attempt(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, cx) = open(dir.path(), cx);
    state.update(cx, |state, cx| {
        state.ensure_history(2);
        state.flush_logs(cx).unwrap();
        state
            .test_conversation(2)
            .apply(Event::ItemCompleted(keystone_agent::Item {
                id: "oversized-rejected-item".into(),
                kind: ItemKind::Other {
                    name: "scripted output".into(),
                },
                text: "a".repeat(16 * 1024 * 1024),
                status: keystone_agent::ItemStatus::Completed,
            }));
        state.commit(2);
        state.publish(cx);
    });
    view.read_with(cx, |view, cx| {
        assert!(
            view.status_line(cx)
                .unwrap()
                .0
                .contains("1 entries were lost")
        );
    });
    view.update_in(cx, |view, window, cx| view.toggle_settings(window, cx));
    assert!(cx.debug_bounds("workspace-error").is_some());
    state.update(cx, |state, cx| state.flush_logs(cx).unwrap());
    view.read_with(cx, |view, cx| {
        assert!(
            view.status_line(cx)
                .unwrap()
                .0
                .contains("1 entries were lost")
        );
    });
}

#[gpui::test]
fn directory_sync_failure_vetoes_title_bar_close_until_explicit_retry(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, cx) = open(dir.path(), cx);
    state.update(cx, |s, _| {
        s.test_sync_directory(|_| Err(std::io::Error::other("injected sync failure")))
    });
    draft(&view, cx, "directory sync draft");
    let close = cx.debug_bounds("close").unwrap();
    cx.simulate_click(close.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(phase(&state, cx), Phase::Running);
    assert!(cx.debug_bounds("shutdown-error").is_some());
    let writes = state.read_with(cx, |s, _| s.store().writes());
    state.update(cx, |s, _| s.test_sync_directory(std::fs::File::sync_all));
    let close = cx.debug_bounds("close").unwrap();
    cx.simulate_click(close.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    state.read_with(cx, |s, _| {
        assert_eq!(s.phase, Phase::Exiting);
        assert_eq!(s.store().writes(), writes);
        assert_eq!(s.save_calls, 2);
    });
}

#[gpui::test]
fn failed_nonselected_provider_metadata_is_replayed_before_quit(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, cx) = open(dir.path(), cx);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    state.update(cx, |s, cx| {
        s.receive(
            vec![],
            vec![
                (
                    2,
                    Event::ThreadReady {
                        thread_id: "retained-thread".into(),
                    },
                ),
                (2, Event::TurnStarted),
            ],
            cx,
        )
    });
    cx.update(|_, cx| request_quit(cx));
    assert_eq!(phase(&state, cx), Phase::Running);
    view.read_with(cx, |v, cx| {
        let message = v.status_line(cx).unwrap().0;
        assert_eq!(
            message
                .matches("Could not save the conversation ID")
                .count(),
            1,
            "failed Quit must show each metadata error once: {message}"
        )
    });
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    cx.update(|_, cx| request_quit(cx));
    assert_eq!(phase(&state, cx), Phase::Exiting);
    let restored = Store::open(dir.path().join("workspace.json")).unwrap();
    assert_eq!(
        restored.workspace().sessions()[1].provider_thread(),
        Some("retained-thread")
    );
    assert_eq!(
        restored.workspace().sessions()[1].last_turn(),
        Some(keystone_core::LastTurn::Running)
    );
}

#[gpui::test]
fn quit_during_both_provider_starts_reaps_owned_probes_and_descendants(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (_, state, cx) = open(dir.path(), cx);
    for provider in [Provider::Codex, Provider::Claude] {
        start(&state, cx, dir.path(), provider, true);
    }
    cx.update(|_, cx| {
        request_quit(cx);
        request_quit(cx);
    });
    assert_eq!(phase(&state, cx), Phase::Exiting);
    wait(cx, |_| {
        [Provider::Codex, Provider::Claude]
            .iter()
            .all(|p| pids(dir.path(), *p).iter().all(|pid| !alive(*pid)))
    });
    state.update(cx, |s, cx| {
        assert!(!s.send_message(
            ComposerDraft {
                session: 1,
                text: "must not start".into()
            },
            cx
        ));
        assert!(
            s.create(
                Provider::Codex,
                &keystone_core::WorkingDirectory::new(dir.path()).unwrap(),
                3,
                cx
            )
            .is_err()
        );
        s.request_catalog(1, Provider::Codex, cx);
        assert!(s.test_codex().is_none() && s.test_claude().is_none());
        assert_eq!(s.save_calls, 1);
    });
}

#[gpui::test]
fn global_quit_reaches_palette_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, cx) = open(dir.path(), cx);
    draft(&view, cx, "palette-owned focus draft");
    view.update_in(cx, |v, window, cx| v.toggle_palette(window, cx));
    cx.simulate_keystrokes("ctrl-q");
    cx.run_until_parked();
    assert_eq!(phase(&state, cx), Phase::Exiting);
    assert_eq!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft(),
        "palette-owned focus draft"
    );
}

#[gpui::test]
fn global_quit_reaches_a_focused_transcript_copy_action(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, cx) = open(dir.path(), cx);
    state.update(cx, |s, cx| {
        s.flush_logs(cx).unwrap();
        s.test_conversation(1).push_local("copy action row");
        s.commit(1);
        s.publish(cx);
    });
    draft(&view, cx, "action-owned focus draft");
    cx.simulate_keystrokes("ctrl-shift-t home tab");
    view.update_in(cx, |v, window, _| {
        assert!(
            v.presentation
                .current
                .views
                .keyboard_state(ViewKey {
                    view: keystone_core::ViewId::MAIN,
                    session: 1
                })
                .unwrap()
                .action_focus
                .is_focused(window)
        );
    });
    cx.simulate_keystrokes("ctrl-q");
    cx.run_until_parked();
    assert_eq!(phase(&state, cx), Phase::Exiting);
    assert_eq!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft(),
        "action-owned focus draft"
    );
}

#[gpui::test]
fn global_quit_reaches_root_with_an_open_menu(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, cx) = open(dir.path(), cx);
    draft(&view, cx, "menu-owned draft");
    view.update_in(cx, |v, window, cx| {
        v.new_menu = true;
        v.focus.focus(window, cx);
        cx.notify();
    });
    cx.simulate_keystrokes("ctrl-q");
    cx.run_until_parked();
    assert_eq!(phase(&state, cx), Phase::Exiting);
    assert_eq!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft(),
        "menu-owned draft"
    );
}

#[gpui::test]
fn runtime_notifications_reach_two_independent_window_presentations(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut other = cx.clone();
    let (first, state, one) = open(dir.path(), cx);
    state.update(one, |s, cx| {
        s.flush_logs(cx).unwrap();
        for i in 0..30 {
            s.test_conversation(1).push_local(&format!(
                "Retained row {i}. {}",
                "Content for measured scroll. ".repeat(8)
            ));
        }
        s.commit(1);
        s.publish(cx);
    });
    let (second, two) =
        other.add_window_view(|window, cx| Keystone::new(state.clone(), false, window, cx));
    let key = ViewKey {
        view: keystone_core::ViewId::MAIN,
        session: 1,
    };
    first.update(one, |v, cx| {
        v.presentation
            .current
            .views
            .list(key, 30)
            .scroll_to(ListOffset {
                item_ix: 3,
                offset_in_item: px(10.),
            });
        v.presentation.current.views.focus(key, cx);
    });
    second.update(two, |v, cx| {
        v.presentation
            .current
            .views
            .list(key, 30)
            .scroll_to(ListOffset {
                item_ix: 7,
                offset_in_item: px(12.),
            });
        v.presentation.current.views.focus(key, cx);
    });
    state.update(one, |s, cx| {
        s.receive(
            vec![],
            vec![
                (1, Event::TurnStarted),
                (
                    1,
                    Event::ItemStarted(keystone_agent::Item {
                        id: "fanout".into(),
                        kind: ItemKind::AgentMessage,
                        text: String::new(),
                        status: keystone_agent::ItemStatus::InProgress,
                    }),
                ),
                (
                    1,
                    Event::ItemDelta {
                        item_id: "fanout".into(),
                        text: "Paced fan-out. ".repeat(100),
                    },
                ),
            ],
            cx,
        )
    });
    first.read_with(one, |v, _| {
        let list = v.presentation.current.views.get(key).unwrap();
        assert_eq!(list.item_count(), 32);
        assert_eq!(list.logical_scroll_top().item_ix, 3);
        assert_eq!(list.logical_scroll_top().offset_in_item, px(10.));
        assert!(v.replies.borrow().pacing());
    });
    second.read_with(two, |v, _| {
        let list = v.presentation.current.views.get(key).unwrap();
        assert_eq!(list.item_count(), 32);
        assert_eq!(list.logical_scroll_top().item_ix, 7);
        assert_eq!(list.logical_scroll_top().offset_in_item, px(12.));
        assert!(v.replies.borrow().pacing());
    });
    first.update(one, |v, _| {
        v.replies.get_mut().settle(1);
    });
    second.read_with(two, |v, _| assert!(v.replies.borrow().pacing()));
    assert_ne!(
        first.read_with(one, |v, _| v.focus.clone()),
        second.read_with(two, |v, _| v.focus.clone())
    );
    assert_eq!(
        first.read_with(one, |v, _| v.state.entity_id()),
        second.read_with(two, |v, _| v.state.entity_id())
    );
}
