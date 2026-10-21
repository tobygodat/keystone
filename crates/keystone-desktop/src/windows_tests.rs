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
