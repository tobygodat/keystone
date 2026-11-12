use super::*;
use crate::{AppState, close_window, install_app_state, open_windows};
use gpui::{
    AnyWindowHandle, Entity, Focusable, SharedString, TestAppContext, VisualTestContext, WeakEntity,
};
use keystone_agent::{Approval, RequestId};
use keystone_core::{Provider, Store, WorkingDirectory};
use std::path::Path;

/// A workspace with three sessions, "Session 1" to "Session 3", the first selected.
fn seeded(dir: &Path, seed: impl FnOnce(&mut Store)) -> std::path::PathBuf {
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    let directory = WorkingDirectory::new(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    for n in 0..3u64 {
        store.create(Provider::Claude, &directory, n).unwrap();
        store.rename(n + 1, &format!("Session {}", n + 1)).unwrap();
    }
    store.select(1).unwrap();
    seed(&mut store);
    path
}

fn launch(path: &Path, cx: &mut TestAppContext) -> Entity<AppState> {
    let store = Store::open(path).unwrap();
    cx.executor().allow_parking();
    let state = cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state = install_app_state(store, cx);
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            crate::windows::open(state.clone(), id, placement, true, false, cx).unwrap();
        }
        state
    });
    let first = view(WindowId::FIRST, cx);
    activate(&first, cx);
    state
}

fn view(id: WindowId, cx: &mut TestAppContext) -> Entity<Keystone> {
    cx.update(|cx| open_windows(cx))
        .into_iter()
        .find(|v| cx.read(|cx| v.read(cx).window_id) == id)
        .unwrap()
}

fn handle(view: &Entity<Keystone>, cx: &mut TestAppContext) -> AnyWindowHandle {
    cx.read(|cx| view.read(cx).handle)
}

fn activate(view: &Entity<Keystone>, cx: &mut TestAppContext) {
    handle(view, cx)
        .update(cx, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
}

fn deactivate(view: &Entity<Keystone>, cx: &mut TestAppContext) {
    let mut cx = VisualTestContext::from_window(handle(view, cx), cx);
    cx.deactivate_window();
    cx.run_until_parked();
}

fn finished() -> Event {
    Event::TurnCompleted {
        status: TurnStatus::Completed,
        error: None,
        stopped: false,
    }
}

fn approval(request: &str) -> Event {
    Event::ApprovalRequested(Approval {
        request: RequestId(request.into()),
        item_id: None,
        action: "echo scripted".into(),
        detail: None,
    })
}

fn receive(state: &Entity<AppState>, events: Vec<(u64, Event)>, cx: &mut TestAppContext) {
    state.update(cx, |state, cx| state.receive(Vec::new(), events, cx));
    cx.run_until_parked();
}

/// Each notification sent so far, as its title and body.
fn sent(cx: &mut TestAppContext) -> Vec<(String, String)> {
    cx.shown_system_notifications()
        .into_iter()
        .map(|n| (n.title.to_string(), n.body.to_string()))
        .collect()
}

fn select(view: &Entity<Keystone>, session: u64, cx: &mut TestAppContext) {
    view.update(cx, |this, cx| this.select(session, cx));
    cx.run_until_parked();
}

fn selected(view: &Entity<Keystone>, cx: &mut TestAppContext) -> Option<u64> {
    cx.read(|cx| view.read(cx).selected(cx))
}

fn set(state: &Entity<AppState>, notifications: Notifications, cx: &mut TestAppContext) {
    state
        .update(cx, |state, cx| state.set_notifications(notifications, cx))
        .unwrap();
}

#[gpui::test]
fn a_turn_ending_where_nobody_looks_notifies_once_until_the_session_is_seen(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    assert_eq!(
        cx.read(|cx| cx.active_window()),
        Some(handle(&first, cx)),
        "the window is active"
    );

    receive(&state, vec![(1, finished())], cx);
    assert!(sent(cx).is_empty(), "the active window shows session 1");

    receive(&state, vec![(2, finished())], cx);
    assert_eq!(sent(cx), [("Session 2".into(), "Turn finished".into())]);
    receive(&state, vec![(2, finished())], cx);
    assert_eq!(sent(cx).len(), 1, "coalesced until the user sees session 2");

    select(&first, 2, cx);
    select(&first, 1, cx);
    receive(&state, vec![(2, finished())], cx);
    assert_eq!(sent(cx).len(), 2, "seen, so the next end notifies again");
    assert_eq!(
        cx.app_identity(),
        Some(("keystone".into(), "Keystone".into()))
    );
}

#[gpui::test]
fn an_unfocused_window_does_not_count_as_seeing_its_session(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    deactivate(&first, cx);
    assert_eq!(cx.read(|cx| cx.active_window()), None);

    receive(&state, vec![(1, finished())], cx);
    assert_eq!(sent(cx), [("Session 1".into(), "Turn finished".into())]);

    // Showing it in an unfocused window does not clear it.
    receive(&state, vec![(1, finished())], cx);
    assert_eq!(sent(cx).len(), 1);
    activate(&first, cx);
    deactivate(&first, cx);
    receive(&state, vec![(1, finished())], cx);
    assert_eq!(sent(cx).len(), 2, "focusing the window showed it");
}

#[gpui::test]
fn approvals_and_failures_notify_once_each_and_a_stop_notifies_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    receive(&state, vec![(2, approval("a")), (2, approval("b"))], cx);
    assert_eq!(
        sent(cx),
        [("Session 2".into(), "Approval requested".into())],
        "two requests in one unseen stretch notify once"
    );
    receive(
        &state,
        vec![
            (
                3,
                Event::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    error: None,
                    stopped: true,
                },
            ),
            (
                2,
                Event::TurnCompleted {
                    status: TurnStatus::Failed,
                    error: Some("scripted".into()),
                    stopped: false,
                },
            ),
            (
                3,
                Event::Failed {
                    message: "could not start".into(),
                },
            ),
        ],
        cx,
    );
    assert_eq!(
        sent(cx)[1..],
        [
            ("Session 2".into(), "Turn failed".into()),
            ("Session 3".into(), "Turn failed".into()),
        ]
    );
    receive(&state, vec![(3, finished()), (3, Event::TurnStarted)], cx);
    assert_eq!(
        sent(cx).len(),
        3,
        "a held follow-up starting as the turn ends is not a finished session"
    );
    let attention = cx.read(|cx| {
        let state = state.read(cx);
        let session = state.session(2).unwrap();
        session.attention(false, false, false)
    });
    assert_eq!(
        attention,
        Some(keystone_core::Attention::Failed),
        "attention follows the turn, not the notification"
    );
}

#[gpui::test]
fn the_settings_page_hides_the_session_it_covers(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    handle(&first, cx)
        .update(cx, |_, window, cx| {
            first.update(cx, |this, cx| this.toggle_settings(window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    receive(&state, vec![(1, finished())], cx);
    assert_eq!(sent(cx), [("Session 1".into(), "Turn finished".into())]);
    receive(&state, vec![(1, finished())], cx);
    assert_eq!(
        sent(cx).len(),
        1,
        "an active window behind settings did not see it"
    );
}

#[gpui::test]
fn the_switches_turn_each_event_and_all_of_them_off(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    set(
        &state,
        Notifications {
            turn_finished: false,
            ..Notifications::default()
        },
        cx,
    );
    receive(&state, vec![(2, finished()), (3, approval("a"))], cx);
    assert_eq!(
        sent(cx),
        [("Session 3".into(), "Approval requested".into())]
    );

    set(
        &state,
        Notifications {
            enabled: false,
            ..Notifications::default()
        },
        cx,
    );
    receive(
        &state,
        vec![
            (
                2,
                Event::Failed {
                    message: "x".into(),
                },
            ),
            (3, finished()),
        ],
        cx,
    );
    assert_eq!(sent(cx).len(), 1, "off sends nothing");
}

#[gpui::test]
fn activating_a_notification_selects_its_session_and_leaves_settings(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    receive(&state, vec![(3, finished())], cx);
    let tag: SharedString = cx.shown_system_notifications()[0].tag.clone();
    assert_eq!(tag, "session-3");

    handle(&first, cx)
        .update(cx, |_, window, cx| {
            first.update(cx, |this, cx| this.toggle_settings(window, cx))
        })
        .unwrap();
    deactivate(&first, cx);
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag,
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(selected(&first, cx), Some(3));
    assert!(cx.read(|cx| first.read(cx).settings.is_none()));
    assert_eq!(
        cx.read(|cx| cx.active_window()),
        Some(handle(&first, cx)),
        "its window is asked to focus"
    );

    receive(&state, vec![(3, finished())], cx);
    assert_eq!(sent(cx).len(), 1, "now shown in the active window");
}

#[gpui::test]
fn a_notification_opens_in_the_window_showing_its_session(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    handle(&first, cx)
        .update(cx, |_, window, cx| {
            first.update(cx, |this, cx| this.open_new_window(Some(2), window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    let second = view(WindowId(1), cx);
    activate(&first, cx);
    receive(&state, vec![(2, finished())], cx);
    assert_eq!(
        sent(cx).len(),
        1,
        "only the inactive second window shows it"
    );

    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-2".into(),
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(cx.read(|cx| cx.active_window()), Some(handle(&second, cx)));
    assert_eq!(selected(&first, cx), Some(1), "the first keeps its own");
    assert_eq!(selected(&second, cx), Some(2));

    // A session no window shows opens in the window used last, not the first.
    deactivate(&second, cx);
    receive(&state, vec![(3, finished())], cx);
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-3".into(),
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(selected(&second, cx), Some(3));
    assert_eq!(selected(&first, cx), Some(1));
}

/// One conversation, the composer and Files, with Files in the conversation's tab
/// group.
fn conversation_and_files(view: &Entity<Keystone>, cx: &mut TestAppContext) {
    let layout: keystone_core::Composition = serde_json::from_value(serde_json::json!({
        "version": 1,
        "views": [
            {"id": 0, "component": "conversation"},
            {"id": 2, "component": "composer"},
            {"id": 5, "component": "files"}
        ],
        "root": {"kind": "split", "axis": "column", "children": [
            {"node": {"kind": "tabs", "selected": 0, "views": [0, 5]}, "size": {"fill": 1}},
            {"node": {"kind": "view", "id": 2}, "size": "auto"}
        ]}
    }))
    .unwrap();
    view.update(cx, |this, cx| {
        let project = this.presentation.project.clone().unwrap();
        this.state
            .update(cx, |state, cx| {
                state.set_project_layout(&project, Some(layout.into()), cx)
            })
            .unwrap();
    });
    cx.run_until_parked();
}

#[gpui::test]
fn files_in_front_hides_the_selected_conversation_and_its_notification_brings_it_back(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    conversation_and_files(&first, cx);
    let mut window = VisualTestContext::from_window(handle(&first, cx), cx);
    assert!(window.debug_bounds("conversation").is_some());
    window.simulate_keystrokes("secondary-shift-e");
    window.run_until_parked();
    assert!(
        window.debug_bounds("conversation").is_none(),
        "Files is in front"
    );

    receive(&state, vec![(1, finished())], cx);
    assert_eq!(
        sent(cx),
        [("Session 1".into(), "Turn finished".into())],
        "the composer alone does not show the conversation"
    );

    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-1".into(),
        action_id: None,
    });
    cx.run_until_parked();
    let mut window = VisualTestContext::from_window(handle(&first, cx), cx);
    assert!(
        window.debug_bounds("conversation").is_some(),
        "the notification brought the conversation forward"
    );
    deactivate(&first, cx);
    receive(&state, vec![(1, finished())], cx);
    assert_eq!(
        sent(cx).len(),
        2,
        "the frame that showed it while active counted as seeing it"
    );
}

#[gpui::test]
fn a_notification_after_the_last_window_closed_reopens_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    for session in [2, 3] {
        state.update(cx, |state, cx| {
            state.test_conversation(session).running = true;
            state.publish(cx);
        });
    }
    let root: WeakEntity<Keystone> = first.downgrade();
    handle(&first, cx)
        .update(cx, |_, window, cx| {
            assert!(close_window(&root, window, cx));
            window.remove_window();
        })
        .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| open_windows(cx)).is_empty());
    assert!(cx.read(|cx| state.read(cx).quit_when_idle));

    receive(&state, vec![(3, finished())], cx);
    assert_eq!(sent(cx), [("Session 3".into(), "Turn finished".into())]);
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-3".into(),
        action_id: None,
    });
    cx.run_until_parked();
    let reopened = view(WindowId::FIRST, cx);
    assert_eq!(selected(&reopened, cx), Some(3));
    assert!(
        !cx.read(|cx| state.read(cx).quit_when_idle),
        "Keystone stays open once session 2 ends"
    );
}

#[gpui::test]
fn a_notification_activated_while_quitting_changes_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    receive(&state, vec![(3, finished())], cx);
    assert!(state.update(cx, |state, _| state.reserve_quit(true)));
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-3".into(),
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(selected(&first, cx), Some(1));
}

#[gpui::test]
fn a_notification_for_another_pane_keeps_its_selection(cx: &mut TestAppContext) {
    use keystone_core::{Axis, Component, Composition, Edit, Place, SessionTabs, Size, ViewId};
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |store| {
        let c = Composition::default()
            .edit(Edit::Open {
                view: ViewId(3),
                component: Component::Conversation,
                at: Place::Beside {
                    target: ViewId::MAIN,
                    axis: Axis::Row,
                    after: true,
                },
                size: Size::Fill(std::num::NonZeroU16::MIN),
            })
            .unwrap();
        let mut tabs = SessionTabs::default();
        tabs.hold(ViewId::MAIN, 1);
        tabs.hold(ViewId(3), 2);
        let project = keystone_core::ProjectId::new(&std::env::current_dir().unwrap()).unwrap();
        store.show(&project, Some(c), tabs, Some(1)).unwrap();
    });
    let state = launch(&path, cx);
    let first = view(WindowId::FIRST, cx);
    let mut window = VisualTestContext::from_window(handle(&first, cx), cx);
    window.simulate_resize(gpui::size(gpui::px(1280.), gpui::px(800.)));
    let pane = window.debug_bounds("conversation").unwrap();
    window.simulate_click(pane.center(), gpui::Modifiers::none());
    window.run_until_parked();
    assert_eq!(
        selected(&first, cx),
        Some(1),
        "keyboard focus is in session 1's pane"
    );

    deactivate(&first, cx);
    receive(&state, vec![(2, finished())], cx);
    assert_eq!(sent(cx), [("Session 2".into(), "Turn finished".into())]);
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-2".into(),
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(
        selected(&first, cx),
        Some(2),
        "focus left session 1's pane, so it does not take the selection back"
    );
}

#[gpui::test]
fn a_narrow_window_with_its_sidebar_over_the_conversation_does_not_show_it(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    let window = &mut VisualTestContext::from_window(handle(&first, cx), cx);
    window.simulate_resize(gpui::size(gpui::px(500.), gpui::px(800.)));
    window.run_until_parked();
    first.update(cx, |this, cx| {
        this.presentation.current.arrangement.sidebar_overlay = true;
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.read(|cx| first.read(cx).presentation.current.arrangement.fitted));
    receive(&state, vec![(1, finished())], cx);
    assert_eq!(sent(cx), [("Session 1".into(), "Turn finished".into())]);
}

#[gpui::test]
fn activating_a_notification_closes_customize_as_settings_does(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    handle(&first, cx)
        .update(cx, |_, window, cx| {
            first.update(cx, |this, cx| {
                this.customize(crate::arrangement_editor::Action::Open, window, cx)
            })
        })
        .unwrap();
    cx.run_until_parked();
    assert!(cx.read(|cx| first.read(cx).presentation.current.customization.is_open()));
    receive(&state, vec![(2, finished())], cx);
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-2".into(),
        action_id: None,
    });
    cx.run_until_parked();
    assert!(!cx.read(|cx| first.read(cx).presentation.current.customization.is_open()));
    assert_eq!(selected(&first, cx), Some(2));
}

/// A notification for a session in another project opens that project in the window it
/// lands in, so the session shows instead of an empty conversation.
#[gpui::test]
fn a_notification_for_another_projects_session_opens_its_project(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let beta = root.join("beta");
    std::fs::create_dir(root.join("alpha")).unwrap();
    std::fs::create_dir(&beta).unwrap();
    let path = seeded(&root, |store| {
        let alpha = WorkingDirectory::new(&root.join("alpha")).unwrap();
        let beta = WorkingDirectory::new(&beta).unwrap();
        store.create(Provider::Claude, &alpha, 10).unwrap();
        store.create(Provider::Claude, &beta, 11).unwrap();
        store.rename(5, "Beta session").unwrap();
        store.select(4).unwrap();
    });
    let state = launch(&path, cx);
    let first = view(WindowId::FIRST, cx);
    assert_eq!(selected(&first, cx), Some(4));
    receive(&state, vec![(5, finished())], cx);
    assert_eq!(sent(cx), [("Beta session".into(), "Turn finished".into())]);

    deactivate(&first, cx);
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-5".into(),
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| first.read(cx).presentation.project.clone().ok()),
        Some(keystone_core::ProjectId::new(&beta).unwrap())
    );
    assert_eq!(selected(&first, cx), Some(5));
    let mut window = VisualTestContext::from_window(handle(&first, cx), cx);
    assert!(
        window.update(|window, cx| first.read(cx).composer.focus_handle(cx).is_focused(window)),
        "the composer keeps focus through the project switch"
    );
}

/// The project a notification opens may have been left with Customize open; it closes,
/// so the notified session comes forward.
#[gpui::test]
fn a_notification_closes_customize_in_the_project_it_opens(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for name in ["alpha", "beta"] {
        std::fs::create_dir(root.join(name)).unwrap();
    }
    let path = seeded(&root, |store| {
        for (name, at) in [("alpha", 10), ("beta", 11), ("beta", 12)] {
            let folder = WorkingDirectory::new(&root.join(name)).unwrap();
            store.create(Provider::Claude, &folder, at).unwrap();
        }
        store.select(4).unwrap();
    });
    let alpha = keystone_core::ProjectId::new(&root.join("alpha")).unwrap();
    let beta = keystone_core::ProjectId::new(&root.join("beta")).unwrap();
    let state = launch(&path, cx);
    let first = view(WindowId::FIRST, cx);
    first.update(cx, |this, cx| this.open_project(beta.clone(), cx));
    cx.run_until_parked();
    assert_eq!(selected(&first, cx), Some(6));
    handle(&first, cx)
        .update(cx, |_, window, cx| {
            first.update(cx, |this, cx| {
                this.customize(crate::arrangement_editor::Action::Open, window, cx)
            })
        })
        .unwrap();
    cx.run_until_parked();
    first.update(cx, |this, cx| this.open_project(alpha.clone(), cx));
    cx.run_until_parked();
    assert!(!cx.read(|cx| first.read(cx).presentation.current.customization.is_open()));
    deactivate(&first, cx);
    receive(&state, vec![(5, finished())], cx);
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-5".into(),
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| first.read(cx).presentation.project.clone().ok()),
        Some(beta)
    );
    assert!(!cx.read(|cx| first.read(cx).presentation.current.customization.is_open()));
    assert_eq!(selected(&first, cx), Some(5));
}

/// A window already on the session's project takes its notification before the window
/// used last, which keeps its own project.
#[gpui::test]
fn a_notification_prefers_a_window_on_its_project_to_the_one_used_last(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for name in ["alpha", "beta"] {
        std::fs::create_dir(root.join(name)).unwrap();
    }
    let path = seeded(&root, |store| {
        for (name, at) in [("alpha", 10), ("beta", 11), ("beta", 12)] {
            let folder = WorkingDirectory::new(&root.join(name)).unwrap();
            store.create(Provider::Claude, &folder, at).unwrap();
        }
        store.select(4).unwrap();
    });
    let alpha = keystone_core::ProjectId::new(&root.join("alpha")).unwrap();
    let beta = keystone_core::ProjectId::new(&root.join("beta")).unwrap();
    let state = launch(&path, cx);
    let first = view(WindowId::FIRST, cx);
    handle(&first, cx)
        .update(cx, |_, window, cx| {
            first.update(cx, |this, cx| this.open_new_window(None, window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    let second = view(WindowId(1), cx);
    second.update(cx, |this, cx| this.open_project(beta.clone(), cx));
    select(&second, 5, cx);
    activate(&first, cx);
    deactivate(&first, cx);
    receive(&state, vec![(6, finished())], cx);
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: "session-6".into(),
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(selected(&second, cx), Some(6));
    assert_eq!(selected(&first, cx), Some(4));
    let project = |view: &Entity<Keystone>, cx: &mut TestAppContext| {
        cx.read(|cx| view.read(cx).presentation.project.clone().ok())
    };
    assert_eq!(project(&first, cx), Some(alpha));
    assert_eq!(project(&second, cx), Some(beta));
}

/// On macOS a notification replaces any earlier one with its tag, so a failed check
/// must not take the tag an approval waiting on the same session has.
#[gpui::test]
fn a_pull_request_alert_never_replaces_an_approval_and_opens_its_session(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    receive(&state, vec![(3, approval("r1"))], cx);
    cx.update(|cx| offer(vec![(3, Alert::CheckFailed)], cx));
    cx.run_until_parked();
    let tags: Vec<SharedString> = cx
        .shown_system_notifications()
        .into_iter()
        .map(|n| n.tag)
        .collect();
    assert_eq!(tags, ["session-3", "session-3-pull-request"]);
    assert_eq!(sent(cx)[1].1, "Check failed");

    deactivate(&first, cx);
    cx.simulate_system_notification_response(SystemNotificationResponse {
        tag: tags[1].clone(),
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(selected(&first, cx), Some(3));
}

#[gpui::test]
fn a_dock_click_after_the_last_window_closed_reopens_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), |_| {}), cx);
    let first = view(WindowId::FIRST, cx);
    state.update(cx, |state, cx| {
        state.test_conversation(2).running = true;
        state.publish(cx);
    });
    let root: WeakEntity<Keystone> = first.downgrade();
    handle(&first, cx)
        .update(cx, |_, window, cx| {
            assert!(close_window(&root, window, cx));
            window.remove_window();
        })
        .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| open_windows(cx)).is_empty());

    cx.update(crate::reopen_from_dock);
    cx.run_until_parked();
    view(WindowId::FIRST, cx);
    assert!(!cx.read(|cx| state.read(cx).quit_when_idle));
}
