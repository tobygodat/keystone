use super::*;
use crate::app_state::Phase;
use crate::{close_window, install_app_state, open_windows, request_quit};
use gpui::{
    AnyWindowHandle, Focusable, Modifiers, MouseButton, TestAppContext, VisualTestContext,
    WeakEntity,
};
use keystone_core::{Provider, Store, WorkingDirectory};
use std::path::Path;

fn working_directory() -> WorkingDirectory {
    WorkingDirectory::new(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// A workspace with three sessions, the first selected in the first window, and
/// whatever `seed` saves before Keystone opens, as a relaunch would find it.
fn seeded(dir: &Path, seed: impl FnOnce(&mut Store)) -> std::path::PathBuf {
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    for (n, provider) in [Provider::Codex, Provider::Claude, Provider::Codex]
        .into_iter()
        .enumerate()
    {
        store
            .create(provider, &working_directory(), n as u64)
            .unwrap();
        store
            .rename(n as u64 + 1, &format!("Session {}", n + 1))
            .unwrap();
    }
    store.select(1).unwrap();
    seed(&mut store);
    path
}

/// Launches Keystone on `path` as `main` does, opening every saved window.
fn launch(path: &Path, cx: &mut TestAppContext) -> Entity<AppState> {
    let store = Store::open(path).unwrap();
    cx.executor().allow_parking();
    let state = cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state = install_app_state(store, cx);
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            open(state.clone(), id, placement, true, false, cx).unwrap();
        }
        state
    });
    cx.run_until_parked();
    state
}

fn views(cx: &mut TestAppContext) -> Vec<Entity<Keystone>> {
    cx.update(|cx| open_windows(cx))
}

fn view_in(id: WindowId, cx: &mut TestAppContext) -> Entity<Keystone> {
    views(cx)
        .into_iter()
        .find(|v| cx.read(|cx| v.read(cx).window_id) == id)
        .unwrap_or_else(|| panic!("no window {id:?}"))
}

fn selected(view: &Entity<Keystone>, cx: &mut TestAppContext) -> Option<u64> {
    cx.read(|cx| view.read(cx).selected(cx))
}

fn handle(view: &Entity<Keystone>, cx: &mut TestAppContext) -> AnyWindowHandle {
    cx.read(|cx| view.read(cx).handle)
}

/// Closes `view`'s window the way the compositor's close does.
fn close(view: &Entity<Keystone>, cx: &mut TestAppContext) -> bool {
    let root: WeakEntity<Keystone> = view.downgrade();
    let closed = handle(view, cx)
        .update(cx, |_, window, cx| {
            let closed = close_window(&root, window, cx);
            if closed {
                window.remove_window();
            }
            closed
        })
        .unwrap();
    cx.run_until_parked();
    closed
}

fn new_window(from: &Entity<Keystone>, session: Option<u64>, cx: &mut TestAppContext) {
    handle(from, cx)
        .update(cx, |_, window, cx| {
            from.update(cx, |this, cx| this.open_new_window(session, window, cx))
        })
        .unwrap();
    cx.run_until_parked();
}

fn running(state: &Entity<AppState>, session: u64, running: bool, cx: &mut TestAppContext) {
    state.update(cx, |state, cx| {
        state.test_conversation(session).running = running;
        state.publish(cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
fn a_new_window_shows_its_own_session_and_its_composer_sends_there(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(2), cx);
    new_window(&first, None, cx);
    assert_eq!(cx.windows().len(), 3);
    let second = view_in(WindowId(1), cx);
    let third = view_in(WindowId(2), cx);
    assert_eq!(selected(&first, cx), Some(1));
    assert_eq!(selected(&second, cx), Some(2));
    assert_eq!(selected(&third, cx), None, "a new window opens empty");

    second.update(cx, |this, cx| this.select(3, cx));
    cx.run_until_parked();
    assert_eq!(selected(&second, cx), Some(3));
    assert_eq!(
        selected(&first, cx),
        Some(1),
        "the other window keeps its own"
    );
    second.update(cx, |this, cx| {
        this.composer
            .update(cx, |input, cx| input.set_value("for three".into(), cx));
        let draft = this.composer_draft(cx).unwrap();
        assert_eq!((draft.session, draft.text.as_str()), (3, "for three"));
    });
    let workspace = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            let w = state.read(cx).store().workspace();
            (w.selected_in(WindowId::FIRST), w.selected_in(WindowId(1)))
        })
    };
    assert_eq!(workspace(cx), (Some(1), Some(3)));
    let titles: Vec<_> = views(cx)
        .iter()
        .map(|v| title(cx.read(|cx| v.read(cx).window_id)))
        .collect();
    assert_eq!(titles, ["Keystone", "Keystone 2", "Keystone 3"]);
}

#[gpui::test]
fn one_session_shown_in_two_windows_keeps_one_draft(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(1), cx);
    let second = view_in(WindowId(1), cx);
    first.update(cx, |this, cx| {
        this.composer
            .update(cx, |input, cx| input.set_value("shared draft".into(), cx));
        assert!(this.save_draft(cx));
    });
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| second.read(cx).composer.read(cx).value().to_owned()),
        "shared draft"
    );
    second.update(cx, |this, cx| this.select(2, cx));
    first.update(cx, |this, cx| {
        this.composer
            .update(cx, |input, cx| input.set_value("only one".into(), cx));
        assert!(this.save_draft(cx));
    });
    assert_eq!(
        cx.read(|cx| second.read(cx).composer.read(cx).value().to_owned()),
        "",
        "a window showing another session keeps its own draft"
    );
}

/// A mention chosen over the same text reaches the other window as a token, leaving
/// that window's selection where it was.
#[gpui::test]
fn a_token_over_the_same_text_reaches_the_other_window_in_place(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(1), cx);
    let second = view_in(WindowId(1), cx);
    first.update(cx, |this, cx| {
        this.composer
            .update(cx, |input, cx| input.set_value("@README.md ".into(), cx));
        assert!(this.save_draft(cx));
    });
    cx.run_until_parked();
    let selection = |cx: &mut TestAppContext| {
        handle(&second, cx)
            .update(cx, |_, window, cx| {
                second.update(cx, |this, cx| {
                    this.composer.update(cx, |input, cx| {
                        gpui::EntityInputHandler::selected_text_range(input, false, window, cx)
                            .map(|s| s.range)
                    })
                })
            })
            .unwrap()
    };
    handle(&second, cx)
        .update(cx, |_, window, cx| {
            second.update(cx, |this, cx| {
                this.composer.update(cx, |input, cx| {
                    input.select_all(&keystone_ui::text_input::SelectAll, window, cx)
                })
            })
        })
        .unwrap();
    assert_eq!(selection(cx), Some(0..11));
    first.update(cx, |this, cx| {
        this.composer
            .update(cx, |input, cx| input.insert_token(0..10, "@README.md", cx));
        assert!(this.save_draft(cx));
    });
    cx.run_until_parked();
    let shown = cx.read(|cx| {
        let input = second.read(cx).composer.read(cx);
        (input.value().to_owned(), input.tokens().to_vec())
    });
    assert_eq!(shown, ("@README.md ".into(), vec![0..10]));
    assert_eq!(selection(cx), Some(0..11), "the selection stays");
}

#[gpui::test]
fn closing_a_window_keeps_its_sessions_running_and_reachable(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(2), cx);
    let second = view_in(WindowId(1), cx);
    running(&state, 2, true, cx);
    second.update(cx, |this, cx| {
        this.composer
            .update(cx, |input, cx| input.set_value("left behind".into(), cx))
    });
    let writes = cx.read(|cx| state.read(cx).store().writes());
    assert!(close(&second, cx));
    assert_eq!(cx.windows().len(), 1);
    cx.read(|cx| {
        let state = state.read(cx);
        assert_eq!(state.phase, Phase::Running);
        assert!(state.conversations()[&2].running, "its turn keeps running");
        let workspace = state.store().workspace();
        assert_eq!(workspace.preferences().windows().len(), 1);
        assert_eq!(workspace.sessions()[1].draft(), "left behind");
        assert!(state.store().writes() <= writes + 3);
    });
    first.update(cx, |this, cx| this.select(2, cx));
    cx.run_until_parked();
    assert_eq!(
        selected(&first, cx),
        Some(2),
        "the sidebar still reaches it"
    );
}

#[gpui::test]
fn closing_the_first_window_hands_its_place_to_the_next(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |_| {});
    let state = launch(&path, cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(3), cx);
    let second = view_in(WindowId(1), cx);
    assert!(close(&first, cx));
    assert_eq!(cx.read(|cx| second.read(cx).window_id), WindowId::FIRST);
    assert_eq!(selected(&second, cx), Some(3));
    cx.read(|cx| {
        let workspace = state.read(cx).store().workspace();
        assert_eq!(workspace.selected(), Some(3));
        assert_eq!(workspace.preferences().windows().len(), 1);
    });
}

#[gpui::test]
fn closing_the_last_window_quits_once_no_turn_runs(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(2), cx);
    running(&state, 1, true, cx);
    assert!(close(&view_in(WindowId(1), cx), cx));
    assert!(close(&first, cx), "a running turn keeps Keystone open");
    assert!(cx.windows().is_empty());
    assert_eq!(cx.read(|cx| state.read(cx).phase), Phase::Running);
    assert!(cx.read(|cx| state.read(cx).quit_when_idle));
    running(&state, 1, false, cx);
    assert_eq!(cx.read(|cx| state.read(cx).phase), Phase::Exiting);
    let reopened = Store::open(dir.path().join("workspace.json")).unwrap();
    assert_eq!(
        reopened.workspace().preferences().windows().len(),
        1,
        "the last window is kept for the next launch"
    );
}

#[gpui::test]
fn closing_the_last_idle_window_quits_at_once(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    assert!(!close(&first, cx), "Quit closes it");
    assert_eq!(cx.read(|cx| state.read(cx).phase), Phase::Exiting);
}

#[gpui::test]
fn quit_saves_every_windows_draft_in_one_write_and_restore_opens_each_window(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |store| {
        store.open_window(None, Some(2), None).unwrap();
        store.open_window(None, Some(3), None).unwrap();
    });
    let state = launch(&path, cx);
    assert_eq!(cx.windows().len(), 3);
    for (id, session, text) in [
        (WindowId::FIRST, 1, "first"),
        (WindowId(1), 2, "second"),
        (WindowId(2), 3, "third"),
    ] {
        let view = view_in(id, cx);
        assert_eq!(selected(&view, cx), Some(session));
        view.update(cx, |this, cx| {
            this.composer
                .update(cx, |input, cx| input.set_value(text.into(), cx))
        });
    }
    let writes = cx.read(|cx| state.read(cx).store().writes());
    cx.update(request_quit);
    cx.run_until_parked();
    cx.read(|cx| {
        let state = state.read(cx);
        assert_eq!(state.phase, Phase::Exiting);
        assert_eq!(state.store().writes(), writes + 1);
    });
    let reopened = Store::open(&path).unwrap();
    let workspace = reopened.workspace();
    let drafts: Vec<_> = workspace.sessions().iter().map(|s| s.draft()).collect();
    assert_eq!(drafts, ["first", "second", "third"]);
    let windows = workspace.preferences().windows();
    assert_eq!(
        windows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [WindowId::FIRST, WindowId(1), WindowId(2)]
    );
    assert!(windows.iter().all(|(_, placement)| placement.is_some()));
}

#[gpui::test]
fn a_right_click_on_a_session_opens_it_in_a_new_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    let window = handle(&first, cx);
    let cx = &mut VisualTestContext::from_window(window, cx);
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    cx.run_until_parked();
    let row = cx.debug_bounds("session-3").expect("session row").center();
    cx.simulate_mouse_down(row, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(row, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    let item = cx
        .debug_bounds("menu-open-in-new-window")
        .expect("the session's menu")
        .center();
    cx.simulate_click(item, Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("menu-open-in-new-window").is_none());
    let opened = cx.update(|_, cx| open_windows(cx));
    assert_eq!(opened.len(), 2);
    cx.update(|_, cx| {
        assert_eq!(opened[1].read(cx).selected(cx), Some(3));
        assert_eq!(opened[0].read(cx).selected(cx), Some(1));
    });
}

#[gpui::test]
fn each_window_reopens_on_the_project_it_showed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let other_dir = dir.path().join("other");
    std::fs::create_dir(&other_dir).unwrap();
    let other = keystone_core::ProjectId::new(&other_dir).unwrap();
    let path = seeded(dir.path(), |store| {
        store.open_window(Some(&other), Some(2), None).unwrap();
    });
    launch(&path, cx);
    let project = |view: &Entity<Keystone>, cx: &mut TestAppContext| {
        cx.read(|cx| view.read(cx).presentation.project.clone().unwrap())
    };
    let first = view_in(WindowId::FIRST, cx);
    let second = view_in(WindowId(1), cx);
    let current = keystone_core::ProjectId::new(&std::env::current_dir().unwrap()).unwrap();
    assert_eq!(project(&first, cx), current);
    assert_eq!(project(&second, cx), other);
    new_window(&second, None, cx);
    let third = view_in(WindowId(2), cx);
    assert_eq!(
        project(&third, cx),
        other,
        "a new window opens on its opener's project"
    );
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened
            .workspace()
            .preferences()
            .window_project(WindowId(2)),
        Some(other)
    );
}

#[gpui::test]
fn a_failed_quit_after_the_last_window_closed_reopens_it_with_the_error(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    running(&state, 1, true, cx);
    assert!(close(&view_in(WindowId::FIRST, cx), cx));
    assert!(cx.windows().is_empty());
    let writer = crate::tests::lock_database(&dir.path().join("workspace.sqlite3"));
    state.update(cx, |state, cx| {
        state.ensure_history(2);
        state.flush_logs(cx).ok();
        state.test_conversation(2).push_local("waiting to be saved");
        state.commit(2);
        state.test_conversation(2).running = false;
        state.test_conversation(1).running = false;
        state.publish(cx);
    });
    cx.run_until_parked();
    assert_eq!(cx.read(|cx| state.read(cx).phase), Phase::Running);
    assert_eq!(
        cx.windows().len(),
        1,
        "the window comes back to show the error"
    );
    let reopened = view_in(WindowId::FIRST, cx);
    let window = handle(&reopened, cx);
    let cx = &mut VisualTestContext::from_window(window, cx);
    cx.run_until_parked();
    assert!(cx.debug_bounds("shutdown-error").is_some());
    drop(writer);
}

fn ask_approval(state: &Entity<AppState>, session: u64, cx: &mut TestAppContext) {
    state.update(cx, |state, cx| {
        let conversation = state.test_conversation(session);
        conversation.running = true;
        conversation.apply(keystone_agent::Event::ApprovalRequested(
            keystone_agent::Approval {
                request: keystone_agent::RequestId("window-approval".into()),
                item_id: None,
                action: "rm -rf build".into(),
                detail: None,
            },
        ));
        state.publish(cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
fn quit_keeps_the_newest_typing_of_a_session_shown_in_two_windows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |_| {});
    launch(&path, cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(1), cx);
    let second = view_in(WindowId(1), cx);
    let window = handle(&second, cx);
    let vcx = &mut VisualTestContext::from_window(window, cx);
    second.update_in(vcx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    vcx.simulate_input("typed in two");
    assert_eq!(
        vcx.read(|cx| first.read(cx).composer.read(cx).value().to_owned()),
        "typed in two",
        "the other window shows the typing at once"
    );
    vcx.update(|_, cx| request_quit(cx));
    vcx.run_until_parked();
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.workspace().sessions()[0].draft(), "typed in two");
}

#[gpui::test]
fn the_last_window_stays_while_a_turn_waits_for_an_answer(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    ask_approval(&state, 2, cx);
    assert!(!close(&first, cx));
    assert_eq!(cx.windows().len(), 1);
    assert_eq!(cx.read(|cx| state.read(cx).phase), Phase::Running);
    assert!(
        cx.read(|cx| first.read(cx).error.clone())
            .is_some_and(|e| e.contains("approval"))
    );
}

#[gpui::test]
fn an_approval_asked_with_no_window_open_opens_the_window_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    running(&state, 1, true, cx);
    assert!(close(&view_in(WindowId::FIRST, cx), cx));
    assert!(cx.windows().is_empty());
    ask_approval(&state, 1, cx);
    assert_eq!(cx.read(|cx| state.read(cx).phase), Phase::Running);
    assert_eq!(cx.windows().len(), 1, "the user can answer it");
    cx.read(|cx| {
        let state = state.read(cx);
        assert!(state.conversations()[&1].running);
        assert!(!state.quit_when_idle);
    });
}

#[gpui::test]
fn an_agent_started_session_changes_no_windows_selection(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    Store::open(&path)
        .unwrap()
        .open_window(None, None, None)
        .unwrap();
    let state = launch(&path, cx);
    let child = state.update(cx, |state, cx| {
        state
            .create_unselected(Provider::Codex, &working_directory(), 1, cx)
            .unwrap()
            .value
    });
    cx.run_until_parked();
    for id in [WindowId::FIRST, WindowId(1)] {
        assert_eq!(selected(&view_in(id, cx), cx), None);
    }
    assert!(
        cx.read(|cx| state.read(cx).session(child).is_some()),
        "it is in every sidebar"
    );
}

#[gpui::test]
fn a_window_entering_a_session_shows_what_another_window_is_typing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |_| {});
    launch(&path, cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(2), cx);
    let second = view_in(WindowId(1), cx);
    let window = handle(&second, cx);
    let vcx = &mut VisualTestContext::from_window(window, cx);
    second.update_in(vcx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    vcx.simulate_input("typed before the save");
    first.update(vcx, |this, cx| this.select(2, cx));
    assert_eq!(
        vcx.read(|cx| first.read(cx).composer.read(cx).value().to_owned()),
        "typed before the save"
    );
    vcx.update(|_, cx| request_quit(cx));
    vcx.run_until_parked();
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened.workspace().sessions()[1].draft(),
        "typed before the save"
    );
}

#[gpui::test]
fn a_window_reopened_for_an_approval_shows_the_project_it_last_showed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let other_dir = dir.path().join("other");
    std::fs::create_dir(&other_dir).unwrap();
    let other = keystone_core::ProjectId::new(&other_dir).unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    first.update(cx, |this, _| this.presentation.project = Ok(other.clone()));
    running(&state, 1, true, cx);
    assert!(close(&first, cx));
    ask_approval(&state, 1, cx);
    let reopened = view_in(WindowId::FIRST, cx);
    assert_eq!(
        cx.read(|cx| reopened.read(cx).presentation.project.clone()),
        Ok(other)
    );
}

#[gpui::test]
fn an_approval_left_by_a_failed_turn_does_not_keep_the_last_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    ask_approval(&state, 2, cx);
    running(&state, 2, false, cx);
    assert!(!close(&view_in(WindowId::FIRST, cx), cx), "Quit closes it");
    assert_eq!(cx.read(|cx| state.read(cx).phase), Phase::Exiting);
}

#[gpui::test]
fn the_last_window_stays_open_mid_turn_when_its_place_cannot_be_saved(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |_| {});
    let mut saved = Store::open(&path).unwrap().saved_snapshot().unwrap();
    saved["workspace"]["preferences"]["windows"] = serde_json::json!([{"id": "later"}]);
    std::fs::remove_file(dir.path().join("workspace.sqlite3")).unwrap();
    std::fs::write(&path, saved.to_string()).unwrap();
    let state = launch(&path, cx);
    let first = view_in(WindowId::FIRST, cx);
    running(&state, 1, true, cx);
    assert!(!close(&first, cx));
    assert_eq!(cx.windows().len(), 1);
    assert!(!cx.read(|cx| state.read(cx).quit_when_idle));
    assert!(
        cx.read(|cx| first.read(cx).error.clone())
            .is_some_and(|e| e.contains("Window not closed"))
    );
}

/// The sessions `view` shows: its tabs and its selection.
fn shown(view: &Entity<Keystone>, cx: &mut TestAppContext) -> Vec<u64> {
    cx.read(|cx| {
        let view = view.read(cx);
        let mut shown: Vec<u64> = view.session_tabs(cx).iter().map(|(_, s)| s).collect();
        shown.extend(view.selected(cx));
        shown.sort();
        shown.dedup();
        shown
    })
}

fn move_from_palette(view: &Entity<Keystone>, row: &str, cx: &mut TestAppContext) {
    let window = handle(view, cx);
    let vcx = &mut VisualTestContext::from_window(window, cx);
    view.update_in(vcx, |this, window, cx| this.focus.focus(window, cx));
    vcx.simulate_keystrokes("ctrl-alt-m");
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("palette").is_some(), "the palette opens");
    vcx.simulate_input(row);
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
}

#[gpui::test]
fn the_palette_moves_the_active_tab_to_another_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(2), cx);
    let second = view_in(WindowId(1), cx);

    move_from_palette(&first, "Keystone 2", cx);

    assert_eq!(shown(&first, cx), Vec::<u64>::new(), "the tab left");
    assert_eq!(shown(&second, cx), vec![1, 2]);
    assert_eq!(selected(&second, cx), Some(1), "and is shown where it went");
    cx.read(|cx| {
        let w = state.read(cx).store().workspace();
        assert_eq!(w.selected_in(WindowId::FIRST), None);
        assert_eq!(w.selected_in(WindowId(1)), Some(1));
    });
    assert!(cx.read(|cx| first.read(cx).error.is_none() && second.read(cx).error.is_none()));
}

#[gpui::test]
fn the_palette_moves_the_active_tab_to_a_new_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);

    move_from_palette(&first, "New window", cx);

    assert_eq!(cx.windows().len(), 2);
    let second = view_in(WindowId(1), cx);
    assert_eq!(shown(&first, cx), Vec::<u64>::new());
    assert_eq!(shown(&second, cx), vec![1]);
    cx.read(|cx| {
        let w = state.read(cx).store().workspace();
        assert_eq!(w.selected_in(WindowId(1)), Some(1));
    });
}

#[gpui::test]
fn a_tab_from_a_later_window_drops_into_an_earlier_one(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(2), cx);
    let second = view_in(WindowId(1), cx);
    {
        let window = handle(&second, cx);
        let vcx = &mut VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(gpui::size(px(1280.), px(800.)));
        second.update_in(vcx, |this, window, cx| {
            this.focus.focus(window, cx);
            this.select(3, cx);
        });
        vcx.run_until_parked();
        vcx.simulate_keystrokes("ctrl-t");
        vcx.run_until_parked();
    }
    let before = shown(&second, cx);
    let drag = cx.read(|cx| {
        let this = second.read(cx);
        let (bar_id, views) = this
            .presentation
            .current
            .arrangement
            .bars
            .first()
            .expect("a tab strip")
            .clone();
        keystone_ui::DraggedTab {
            window: this.handle.window_id(),
            bar_id,
            index: views.len() - 1,
        }
    });
    let leaving = cx.read(|cx| {
        let this = second.read(cx);
        this.view_session(this.dragged_view(&drag).unwrap(), cx)
            .unwrap()
    });

    first.update(cx, |this, cx| this.receive_tab(&drag, Landing::Active, cx));
    cx.run_until_parked();

    assert!(shown(&first, cx).contains(&leaving));
    assert!(!shown(&second, cx).contains(&leaving));
    assert_eq!(shown(&second, cx).len(), before.len() - 1);
}

/// Starts dragging the last tab of `view`'s first strip, out past the window's left edge.
fn drag_tab_out(view: &Entity<Keystone>, vcx: &mut VisualTestContext) -> u64 {
    vcx.simulate_resize(gpui::size(px(1280.), px(800.)));
    view.update_in(vcx, |this, window, cx| this.focus.focus(window, cx));
    vcx.simulate_keystrokes("ctrl-t");
    vcx.run_until_parked();
    let (bar, last) = view.read_with(vcx, |this, _| {
        let (bar, views) = this
            .presentation
            .current
            .arrangement
            .bars
            .first()
            .expect("a tab strip")
            .clone();
        (bar, views.len() - 1)
    });
    let leaving = view.read_with(vcx, |this, cx| {
        let views = &this.presentation.current.arrangement.bars[0].1;
        this.view_session(views[last], cx).unwrap()
    });
    let gpui::ElementId::Name(bar) = bar else {
        panic!("strip id {bar:?}")
    };
    let tab_id: &'static str = format!("{bar}-tab-{last}").leak();
    let tab = vcx.debug_bounds(tab_id).expect("the tab").center();
    vcx.simulate_mouse_down(tab, MouseButton::Left, Modifiers::none());
    for x in [tab.x + px(20.), px(200.), px(-40.)] {
        vcx.simulate_mouse_move(
            gpui::point(x, tab.y + px(10.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        vcx.run_until_parked();
    }
    leaving
}

#[gpui::test]
fn a_tab_dropped_on_the_desktop_opens_in_a_new_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    let window = handle(&first, cx);
    cx.set_start_external_drag_result(window, true);
    let vcx = &mut VisualTestContext::from_window(window, cx);
    let leaving = drag_tab_out(&first, vcx);
    assert!(
        !vcx.update(|_, cx| cx.has_active_drag()),
        "the platform carries it"
    );
    vcx.simulate_event(gpui::FileDropEvent::AppDragDroppedOutside);
    vcx.run_until_parked();

    assert_eq!(cx.windows().len(), 2);
    assert!(!shown(&first, cx).contains(&leaving));
    assert_eq!(shown(&view_in(WindowId(1), cx), cx), vec![leaving]);
}

#[gpui::test]
fn a_release_outside_a_window_that_kept_the_drag_changes_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    let window = handle(&first, cx);
    let vcx = &mut VisualTestContext::from_window(window, cx);
    drag_tab_out(&first, vcx);
    let before = view_in_shown(&first, vcx);
    vcx.simulate_mouse_up(
        gpui::point(px(-40.), px(60.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    vcx.run_until_parked();

    assert_eq!(cx.windows().len(), 1, "it may have been over another app");
    assert_eq!(shown(&first, cx), before);
}

fn view_in_shown(view: &Entity<Keystone>, vcx: &mut VisualTestContext) -> Vec<u64> {
    view.read_with(vcx, |this, cx| {
        let mut shown: Vec<u64> = this.session_tabs(cx).iter().map(|(_, s)| s).collect();
        shown.extend(this.selected(cx));
        shown.sort();
        shown.dedup();
        shown
    })
}

#[gpui::test]
fn a_tab_dragged_onto_another_windows_pane_edge_splits_it_there(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(2), cx);
    let second = view_in(WindowId(1), cx);
    let target = active_view(&second, cx);
    let window = handle(&first, cx);
    cx.set_start_external_drag_result(window, true);
    let leaving = {
        let vcx = &mut VisualTestContext::from_window(window, cx);
        drag_tab_out(&first, vcx)
    };
    let window = handle(&second, cx);
    let vcx = &mut VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(gpui::size(px(1280.), px(800.)));
    vcx.run_until_parked();
    vcx.simulate_event(gpui::FileDropEvent::AppDragEntered {
        position: gpui::point(px(900.), px(400.)),
    });
    vcx.run_until_parked();
    let zone: &'static str = format!("pane-drop-{}-right", target.0).leak();
    let edge = vcx
        .debug_bounds(zone)
        .expect("another window's tab shows the pane edges")
        .center();
    vcx.simulate_event(gpui::FileDropEvent::Pending { position: edge });
    vcx.simulate_event(gpui::FileDropEvent::Submit { position: edge });
    vcx.run_until_parked();

    assert!(!shown(&first, cx).contains(&leaving));
    assert!(shown(&second, cx).contains(&leaving));
    let panes = cx.read(|cx| {
        crate::session_tabs::conversations(&second.read(cx).saved_composition(cx)).len()
    });
    assert_eq!(panes, 2, "beside the pane it was dropped on");
}

#[gpui::test]
fn escape_cancels_a_tab_drag(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    let before = shown(&first, cx);
    let window = handle(&first, cx);
    let vcx = &mut VisualTestContext::from_window(window, cx);
    drag_tab_out(&first, vcx);
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(
        !vcx.update(|_, cx| cx.has_active_drag()),
        "Esc ends the drag"
    );
    vcx.simulate_mouse_up(
        gpui::point(px(-40.), px(60.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    vcx.run_until_parked();

    assert_eq!(cx.windows().len(), 1, "no window opens");
    assert_eq!(
        shown(&first, cx).len(),
        before.len() + 1,
        "only the new tab joined"
    );
}

fn active_view(view: &Entity<Keystone>, cx: &mut TestAppContext) -> ViewId {
    cx.read(|cx| view.read(cx).active_view(cx))
        .expect("an active view")
}

fn writes(state: &Entity<AppState>, cx: &mut TestAppContext) -> u64 {
    cx.read(|cx| state.read(cx).store().writes())
}

#[gpui::test]
fn a_tab_moving_in_keeps_what_the_destination_was_typing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    new_window(&first, Some(2), cx);
    let second = view_in(WindowId(1), cx);
    second.update(cx, |this, cx| {
        this.composer
            .update(cx, |input, cx| input.set_value("not saved yet".into(), cx));
    });
    let view = active_view(&first, cx);

    cx.update(|cx| {
        move_tab(
            &first,
            view,
            Destination::Window(second.clone(), Landing::Active),
            &second,
            cx,
        )
    });
    cx.run_until_parked();

    assert_eq!(selected(&second, cx), Some(1));
    let draft = cx.read(|cx| {
        state
            .read(cx)
            .store()
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == 2)
            .unwrap()
            .draft()
            .to_owned()
    });
    assert_eq!(draft, "not saved yet");
}

#[gpui::test]
fn a_tab_moving_to_where_its_session_has_a_hidden_tab_brings_it_forward_in_one_write(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view_in(WindowId::FIRST, cx);
    first.update(cx, |this, cx| this.select(2, cx));
    cx.run_until_parked();
    new_window(&first, Some(2), cx);
    let second = view_in(WindowId(1), cx);
    {
        let window = handle(&second, cx);
        let vcx = &mut VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(gpui::size(px(1280.), px(800.)));
        second.update_in(vcx, |this, window, cx| this.focus.focus(window, cx));
        vcx.simulate_keystrokes("ctrl-t");
        vcx.run_until_parked();
    }
    assert_ne!(selected(&second, cx), Some(2), "session 2's tab is behind");
    let hidden = cx
        .read(|cx| second.read(cx).session_tabs(cx).view_of(2))
        .unwrap();
    let view = active_view(&first, cx);
    first.update(cx, |this, cx| this.save_draft(cx));
    second.update(cx, |this, cx| this.save_draft(cx));
    let before = writes(&state, cx);

    cx.update(|cx| {
        move_tab(
            &first,
            view,
            Destination::Window(second.clone(), Landing::Active),
            &second,
            cx,
        )
    });
    let moved = writes(&state, cx);
    cx.run_until_parked();
    let window = handle(&second, cx);
    VisualTestContext::from_window(window, cx).run_until_parked();

    assert_eq!(selected(&second, cx), Some(2));
    assert_eq!(active_view(&second, cx), hidden, "its tab comes forward");
    assert_eq!(writes(&state, cx), moved, "nothing saves after the move");
    assert_eq!(moved - before, 1, "the move is one write");
}
