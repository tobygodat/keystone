use super::*;
use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, point};
use keystone_agent::{
    Item, ItemKind, ItemStatus,
    log::{Entry, SessionLog},
};
use keystone_core::{Provider, Store, WorkingDirectory};

fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("missing {id}"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}
fn drag(cx: &mut VisualTestContext, id: &'static str, x: f32, y: f32) {
    let from = cx.debug_bounds(id).unwrap().center();
    let to = point(from.x + px(x), from.y + px(y));
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        point(from.x + px(x / 2.), from.y + px(y / 2.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}
fn open(cx: &mut TestAppContext) -> (tempfile::TempDir, Entity<Keystone>, &mut VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
    store
        .create(Provider::Codex, &working_directory(), 1)
        .unwrap();
    store
        .create(Provider::Claude, &working_directory(), 2)
        .unwrap();
    store.select(1).unwrap();
    let mut log = SessionLog::open(&dir.path().join("sessions"), 1).unwrap();
    for i in 0..50 {
        log.append(&Entry::Item {
            item: Item {
                id: format!("reply-{i}"),
                kind: ItemKind::AgentMessage,
                text: format!(
                    "Reply {i}. {}",
                    "A saved transcript with enough text to scroll. ".repeat(12)
                ),
                status: ItemStatus::Completed,
            },
            seq: None,
            run: None,
        })
        .unwrap();
    }
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        Keystone::new(
            cx.new(|cx| crate::AppState::new(store, cx)),
            false,
            window,
            cx,
        )
    });
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .conversations()
            .get(&1)
            .is_some_and(|c| c.rows.len() == 50)
    }) {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(2));
        cx.run_until_parked();
    }
    click(cx, "customize-toggle");
    click(cx, "component-duplicate");
    (dir, view, cx)
}

#[gpui::test]
fn two_visible_occurrences_keep_scroll_and_focus_across_session_selection(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    let first = ViewKey {
        view: ViewId::MAIN,
        session: 1,
    };
    let second = ViewKey {
        view: ViewId(3),
        session: 1,
    };
    let over = cx.debug_bounds("conversation").unwrap().center();
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: over,
        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(300.))),
        ..Default::default()
    });
    cx.run_until_parked();
    let top = view.read_with(cx, |this, _cx| {
        assert!(this.presentation.current.views.scrolled_up(first));
        assert!(!this.presentation.current.views.scrolled_up(second));
        assert!(
            this.presentation
                .current
                .views
                .get(second)
                .unwrap()
                .viewport_bounds()
                .size
                .height
                > px(0.)
        );
        this.presentation
            .current
            .views
            .get(first)
            .unwrap()
            .logical_scroll_top()
    });
    click(cx, "conversation");
    view.read_with(cx, |this, _cx| {
        assert_eq!(
            this.presentation
                .current
                .views
                .send_target(&this.presentation.current.arrangement.visible, 1),
            Some(first)
        )
    });
    click(cx, "conversation-view-3");
    view.read_with(cx, |this, _cx| {
        assert_eq!(
            this.presentation
                .current
                .views
                .send_target(&this.presentation.current.arrangement.visible, 1),
            Some(second)
        )
    });
    click(cx, "session-2");
    click(cx, "session-1");
    view.read_with(cx, |this, _cx| {
        let restored = this
            .presentation
            .current
            .views
            .get(first)
            .unwrap()
            .logical_scroll_top();
        assert_eq!(restored.item_ix, top.item_ix);
        assert_eq!(restored.offset_in_item, top.offset_in_item);
        assert!(!this.presentation.current.views.scrolled_up(second));
    });
}

#[gpui::test]
fn resize_changes_visible_geometry_without_retyping_auto_or_unrelated_fill(
    cx: &mut TestAppContext,
) {
    let (_dir, view, cx) = open(cx);
    drag(cx, "split-grip-[]-0", 40., 0.);
    view.read_with(cx, |this, cx| {
        let saved = this.saved_composition(cx);
        let c = this.presentation.current.customization.shown(&saved);
        let Node::Split { children, .. } = c.root() else {
            panic!()
        };
        assert_eq!(children[0].size, Size::Px(NonZeroU16::new(304).unwrap()));
        assert_eq!(children[1].size, Size::Fill(NonZeroU16::MIN));
        let Node::Split { children, .. } = &children[1].node else {
            panic!()
        };
        assert_eq!(children.last().unwrap().size, Size::Auto);
        assert!(this.saved_layout(cx).is_none());
    });
    let first = cx.debug_bounds("conversation").unwrap().size.height;
    let second = cx.debug_bounds("conversation-view-3").unwrap().size.height;
    drag(cx, "split-grip-[1]-0", 0., 50.);
    assert!(cx.debug_bounds("conversation").unwrap().size.height > first);
    assert!(cx.debug_bounds("conversation-view-3").unwrap().size.height < second);
}

#[gpui::test]
fn a_narrow_composer_keeps_streaming_controls_inside_the_window(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    let preferred: Composition = serde_json::from_value(serde_json::json!({
        "version": 1,
        "views": [
            {"id": 0, "component": "conversation"},
            {"id": 1, "component": "session_list"},
            {"id": 2, "component": "composer"}
        ],
        "root": {"kind": "split", "axis": "row", "children": [
            {"node": {"kind": "view", "id": 1}, "size": {"px": 302}},
            {"node": {"kind": "view", "id": 0}, "size": {"fill": 1}},
            {"node": {"kind": "view", "id": 2}, "size": {"px": 440}}
        ]}
    }))
    .unwrap();
    view.update_in(cx, |this, window, cx| {
        this.state
            .update(cx, |state, cx| {
                state.set_project_layout(
                    this.presentation.project.as_ref().unwrap(),
                    Some(preferred.clone().into()),
                    cx,
                )
            })
            .unwrap();
        this.customize(crate::arrangement_editor::Action::Discard, window, cx);
        this.state.update(cx, |state, _| {
            state.test_conversations_mut().get_mut(&1).unwrap().running = true;
        });
        cx.notify();
    });
    cx.simulate_resize(gpui::size(px(1280.), px(900.)));
    cx.run_until_parked();
    let mut right = px(0.);
    for id in ["project", "approval-mode", "stop", "send"] {
        let bounds = cx.debug_bounds(id).unwrap();
        assert!(bounds.left() >= right, "{id} overlaps another control");
        assert!(bounds.right() <= px(1280.), "{id} clips at the window edge");
        right = bounds.right();
    }
    view.read_with(cx, |this, cx| {
        assert_eq!(this.saved_composition(cx), preferred)
    });
}

fn unread(view: &Entity<Keystone>, cx: &mut VisualTestContext) {
    view.update_in(cx, |this, window, cx| {
        window.activate_window();
        this.state
            .update(cx, |state, cx| state.record_turn_end(1, false, 10, cx))
            .unwrap();
        cx.notify();
    });
    cx.run_until_parked();
}

fn settle(view: &Entity<Keystone>, cx: &mut VisualTestContext) {
    let state = view.read_with(cx, |this, _| this.state.clone());
    crate::tests::settle(&state, cx);
    cx.run_until_parked();
}

fn viewed(view: &Entity<Keystone>, cx: &VisualTestContext) -> u64 {
    view.read_with(cx, |this, cx| {
        this.selected_session(cx).unwrap().viewed_at().unwrap()
    })
}

#[gpui::test]
fn header_only_conversation_does_not_observe_until_transcript_is_visible(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    click(cx, "customize-apply");
    click(cx, "customize-close");
    cx.simulate_resize(gpui::size(px(1280.), px(210.)));
    cx.run_until_parked();
    unread(&view, cx);
    let transcript = cx.debug_bounds("conversation").unwrap();
    assert_eq!(transcript.size.height, px(0.));
    cx.simulate_keystrokes("secondary-shift-t");
    cx.run_until_parked();
    assert!(view.update_in(cx, |this, window, cx| {
        this.view_contains_focus(ViewId::MAIN, window, cx)
    }));
    assert!(viewed(&view, cx) < 10);
    cx.simulate_resize(gpui::size(px(1280.), px(900.)));
    cx.run_until_parked();
    assert!(viewed(&view, cx) >= 10);
}

#[gpui::test]
fn a_click_above_a_hidden_transcript_does_not_observe_it(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    click(cx, "customize-apply");
    click(cx, "customize-close");
    cx.simulate_resize(gpui::size(px(1280.), px(210.)));
    cx.run_until_parked();
    unread(&view, cx);
    let transcript = cx.debug_bounds("conversation").unwrap();
    assert_eq!(transcript.size.height, px(0.));
    cx.simulate_click(
        point(transcript.center().x, transcript.top() - px(10.)),
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert!(viewed(&view, cx) < 10);
    click(cx, "fitted-tab-0");
    assert!(viewed(&view, cx) < 10);
    cx.simulate_resize(gpui::size(px(1280.), px(900.)));
    cx.run_until_parked();
    click(cx, "conversation");
    assert!(viewed(&view, cx) >= 10);
}

#[gpui::test]
fn failed_observation_waits_for_deliberate_activation_before_retry(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx);
    unread(&view, cx);
    let path = dir.path().join("workspace.sqlite3");
    let backup = dir.path().join("backup.sqlite3");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    click(cx, "conversation");
    settle(&view, cx);
    view.read_with(cx, |this, cx| {
        assert!(this.status_line(cx).unwrap().0.starts_with("Not saved."));
    });
    assert!(viewed(&view, cx) >= 10);
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    for _ in 0..3 {
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    }
    let writes = view.read_with(cx, |this, cx| this.state.read(cx).store().writes());
    click(cx, "composer-field");
    click(cx, "conversation");
    assert!(viewed(&view, cx) >= 10);
    view.read_with(cx, |this, cx| {
        assert_eq!(this.state.read(cx).store().writes(), writes)
    });
    settle(&view, cx);
    view.read_with(cx, |this, cx| {
        assert_eq!(this.state.read(cx).store().writes(), writes + 1)
    });
    click(cx, "conversation");
    view.read_with(cx, |this, cx| {
        assert_eq!(this.state.read(cx).store().writes(), writes + 1)
    });
}

fn tabbed(selected: usize) -> Composition {
    serde_json::from_value(serde_json::json!({
        "version": 1,
        "views": [
            {"id": 0, "component": "conversation"},
            {"id": 2, "component": "composer"},
            {"id": 3, "component": "conversation"}
        ],
        "root": {"kind": "split", "axis": "column", "children": [
            {"node": {"kind": "tabs", "selected": selected, "views": [0, 3]}, "size": {"fill": 1}},
            {"node": {"kind": "view", "id": 2}, "size": "auto"}
        ]}
    }))
    .unwrap()
}

fn tabbed_three() -> Composition {
    serde_json::from_value(serde_json::json!({
        "version": 1,
        "views": [
            {"id": 0, "component": "conversation"},
            {"id": 2, "component": "composer"},
            {"id": 3, "component": "conversation"},
            {"id": 4, "component": "conversation"}
        ],
        "root": {"kind": "split", "axis": "column", "children": [
            {"node": {"kind": "tabs", "selected": 0, "views": [0, 3, 4]}, "size": {"fill": 1}},
            {"node": {"kind": "view", "id": 2}, "size": "auto"}
        ]}
    }))
    .unwrap()
}

/// Saves `layout` for the project and leaves Customize.
fn use_layout(view: &Entity<Keystone>, cx: &mut VisualTestContext, layout: Composition) {
    view.update_in(cx, |this, window, cx| {
        this.state
            .update(cx, |state, cx| {
                state.set_project_layout(
                    this.presentation.project.as_ref().unwrap(),
                    Some(layout.into()),
                    cx,
                )
            })
            .unwrap();
        this.customize(Action::Close, window, cx);
    });
    cx.run_until_parked();
}

fn toggle_files(cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("secondary-shift-e");
    cx.run_until_parked();
}

/// Two conversations and Files in one tab group, with the first conversation in front.
fn tabbed_with_files() -> Composition {
    serde_json::from_value(serde_json::json!({
        "version": 1,
        "views": [
            {"id": 0, "component": "conversation"},
            {"id": 2, "component": "composer"},
            {"id": 3, "component": "conversation"},
            {"id": 5, "component": "files"}
        ],
        "root": {"kind": "split", "axis": "column", "children": [
            {"node": {"kind": "tabs", "selected": 0, "views": [0, 3, 5]}, "size": {"fill": 1}},
            {"node": {"kind": "view", "id": 2}, "size": "auto"}
        ]}
    }))
    .unwrap()
}

/// A lone conversation beside a group of a second conversation and Files, Files behind.
fn files_tabbed_beside() -> Composition {
    serde_json::from_value(serde_json::json!({
        "version": 1,
        "views": [
            {"id": 0, "component": "conversation"},
            {"id": 2, "component": "composer"},
            {"id": 3, "component": "conversation"},
            {"id": 5, "component": "files"}
        ],
        "root": {"kind": "split", "axis": "column", "children": [
            {"node": {"kind": "split", "axis": "row", "children": [
                {"node": {"kind": "view", "id": 0}, "size": {"fill": 1}},
                {"node": {"kind": "tabs", "selected": 0, "views": [3, 5]}, "size": {"fill": 1}}
            ]}, "size": {"fill": 1}},
            {"node": {"kind": "view", "id": 2}, "size": "auto"}
        ]}
    }))
    .unwrap()
}

#[gpui::test]
fn a_tab_opened_from_the_plus_menu_is_the_one_shown(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(0));
    click(cx, "surface-tabs-0-tab-1");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    click(cx, "surface-add");
    click(cx, "surface-add-files");
    assert!(
        cx.debug_bounds("files-tree-row-0").is_some(),
        "the Files tab just opened is in front"
    );
    assert!(cx.debug_bounds("conversation-view-3").is_none());
    toggle_files(cx);
    assert!(
        cx.debug_bounds("files-tree-row-0").is_none(),
        "the tree held focus, so the shortcut closes Files"
    );
}

#[gpui::test]
fn a_fitted_tab_chosen_in_customize_shows_its_view(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed_with_files());
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    click(cx, "customize-toggle");
    assert!(cx.debug_bounds("conversation").is_some());
    click(cx, "fitted-tab-5");
    assert!(
        cx.debug_bounds("files-tree-row-0").is_some(),
        "the Files tab chosen is in front"
    );
    assert!(cx.debug_bounds("conversation").is_none());
    toggle_files(cx);
    assert!(
        cx.debug_bounds("files-tree-row-0").is_none(),
        "the tree held focus, so the shortcut closes Files"
    );
}

#[gpui::test]
fn a_tab_choice_that_cannot_be_saved_leaves_focus_where_it_was(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(0));
    click(cx, "conversation");
    let path = dir.path().join("workspace.sqlite3");
    let backup = dir.path().join("backup.sqlite3");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    click(cx, "surface-tabs-0-tab-1");
    assert!(cx.debug_bounds("conversation").is_none(), "the tab changes");
    settle(&view, cx);
    view.read_with(cx, |this, cx| {
        let status = this.status_line(cx);
        assert!(
            status
                .as_ref()
                .is_some_and(|(e, _)| e.starts_with("Not saved.")),
            "{status:?}"
        );
    });
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    toggle_files(cx);
    assert!(
        cx.debug_bounds("files-tree-row-1").is_some(),
        "shortcuts still reach the app"
    );
}

#[gpui::test]
fn revealing_a_hidden_files_tab_by_shortcut_releases_the_plus_menu(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, files_tabbed_beside());
    click(cx, "surface-add");
    assert!(cx.debug_bounds("surface-add-conversation").is_some());
    toggle_files(cx);
    assert!(
        cx.debug_bounds("files-tree-row-0").is_some(),
        "Files comes forward in its group"
    );
    assert!(cx.debug_bounds("surface-add-conversation").is_none());
    unread(&view, cx);
    click(cx, "conversation");
    assert!(
        viewed(&view, cx) >= 10,
        "the focused conversation acknowledges the reply"
    );
}

#[gpui::test]
fn revealing_files_in_a_wide_window_leaves_no_fitted_choice_behind(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(0));
    toggle_files(cx);
    assert!(cx.debug_bounds("files-tree-row-0").is_some());
    click(cx, "conversation");
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("conversation").is_some(),
        "the fitted tabs start from the conversation the user was in"
    );
    unread(&view, cx);
    assert!(
        viewed(&view, cx) >= 10,
        "the conversation keeps focus and acknowledges the reply"
    );
}

#[gpui::test]
fn switching_tabs_moves_focus_to_the_shown_conversation(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(0));
    click(cx, "conversation");
    click(cx, "surface-tabs-0-tab-1");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    unread(&view, cx);
    assert!(
        viewed(&view, cx) >= 10,
        "the conversation brought forward holds focus and acknowledges the reply"
    );
    toggle_files(cx);
    assert!(
        cx.debug_bounds("files-tree-row-1").is_some(),
        "shortcuts still reach the app after the switch"
    );
}

#[gpui::test]
fn keyboard_actions_follow_occurrences_and_release_hidden_focus(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    click(cx, "customize-apply");
    click(cx, "customize-close");
    click(cx, "conversation");
    cx.simulate_keystrokes("secondary-shift-t home down tab");
    let first = ViewKey {
        view: ViewId::MAIN,
        session: 1,
    };
    let second = ViewKey {
        view: ViewId(3),
        session: 1,
    };
    cx.simulate_keystrokes("f6 home down down tab");
    view.update_in(cx, |v, window, cx| {
        assert_eq!(
            v.presentation
                .current
                .views
                .keyboard_state(first)
                .unwrap()
                .row,
            Some(1)
        );
        assert_eq!(
            v.presentation
                .current
                .views
                .keyboard_state(second)
                .unwrap()
                .row,
            Some(2)
        );
        assert!(
            v.presentation
                .current
                .views
                .contains_focus(second, window, cx)
        );
        assert_eq!(
            v.presentation
                .current
                .views
                .send_target(&v.presentation.current.arrangement.visible, 1),
            Some(second)
        );
    });
    cx.simulate_keystrokes("shift-f6");
    view.read_with(cx, |v, _cx| {
        assert_eq!(
            v.presentation
                .current
                .views
                .keyboard_state(first)
                .unwrap()
                .row,
            Some(1)
        )
    });
    use_layout(&view, cx, tabbed(0));
    cx.simulate_keystrokes("secondary-shift-t home tab");
    click(cx, "surface-tabs-0-tab-1");
    view.update_in(cx, |v, window, cx| {
        assert!(
            !v.presentation
                .current
                .views
                .keyboard_state(first)
                .unwrap()
                .action_focus
                .is_focused(window)
        );
        assert!(
            v.presentation
                .current
                .views
                .contains_focus(second, window, cx)
        );
    });
    cx.simulate_keystrokes("home tab");
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    click(cx, "fitted-tab-0");
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    cx.run_until_parked();
    cx.simulate_keystrokes("secondary-, secondary-shift-t home secondary-shift-c");
    assert!(cx.debug_bounds("settings-page").is_none());
    let text = cx
        .update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
        .unwrap();
    assert!(text.starts_with("Reply 0."));
    cx.simulate_keystrokes("secondary-l");
    cx.simulate_input("still a draft");
    view.read_with(cx, |v, cx| {
        assert_eq!(v.composer.read(cx).value(), "still a draft")
    });
}

#[gpui::test]
fn widening_from_a_fitted_tab_restores_focus_to_the_displayed_conversation(
    cx: &mut TestAppContext,
) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(0));
    click(cx, "conversation");
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    click(cx, "fitted-tab-3");
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("conversation").is_some());
    assert!(cx.debug_bounds("conversation-view-3").is_none());
    view.update_in(cx, |this, window, cx| {
        assert!(this.view_contains_focus(ViewId::MAIN, window, cx));
    });
    unread(&view, cx);
    assert!(viewed(&view, cx) >= 10);
    toggle_files(cx);
    assert!(cx.debug_bounds("files-tree-row-1").is_some());
}

#[gpui::test]
fn a_files_shortcut_that_cannot_save_keeps_focus_on_the_displayed_conversation(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed_with_files());
    click(cx, "conversation");
    let path = dir.path().join("workspace.sqlite3");
    let backup = dir.path().join("backup.sqlite3");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    toggle_files(cx);
    assert!(cx.debug_bounds("conversation").is_none());
    assert!(cx.debug_bounds("files-tree-row-0").is_some());
    settle(&view, cx);
    view.update_in(cx, |this, window, cx| {
        assert!(this.status_line(cx).unwrap().0.starts_with("Not saved."));
        assert!(!this.view_contains_focus(ViewId::MAIN, window, cx));
    });
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    toggle_files(cx);
    assert!(cx.debug_bounds("files-tree-row-0").is_none());
    toggle_files(cx);
    assert!(cx.debug_bounds("files-tree-row-1").is_some());
}

#[gpui::test]
fn a_files_menu_insert_that_cannot_save_does_not_focus_an_absent_tree(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(0));
    click(cx, "conversation");
    let path = dir.path().join("workspace.sqlite3");
    let backup = dir.path().join("backup.sqlite3");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    click(cx, "surface-add");
    click(cx, "surface-add-files");
    assert!(cx.debug_bounds("files-tree-row-0").is_some());
    settle(&view, cx);
    view.update_in(cx, |this, window, cx| {
        assert!(this.status_line(cx).unwrap().0.starts_with("Not saved."));
        let root = this.files_root(cx).unwrap();
        assert!(
            this.files
                .browser(&root, cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        );
    });
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    toggle_files(cx);
    assert!(cx.debug_bounds("files-tree-row-1").is_none());
}

#[gpui::test]
fn switching_fitted_tabs_moves_focus_to_the_shown_conversation(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(0));
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    click(cx, "conversation");
    click(cx, "fitted-tab-3");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    unread(&view, cx);
    assert!(
        viewed(&view, cx) >= 10,
        "the conversation brought forward holds focus and acknowledges the reply"
    );
    toggle_files(cx);
    assert!(
        cx.debug_bounds("files-tree-row-1").is_some(),
        "shortcuts still reach the app after the switch"
    );
}

#[gpui::test]
fn closing_the_focused_tab_in_customize_moves_focus_to_its_neighbor(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(1));
    click(cx, "customize-toggle");
    click(cx, "conversation-view-3");
    click(cx, "surface-tabs-0-tab-1-close");
    assert!(cx.debug_bounds("conversation-view-3").is_none());
    assert!(cx.debug_bounds("conversation").is_some());
    unread(&view, cx);
    assert!(
        viewed(&view, cx) >= 10,
        "the conversation left on screen holds focus and acknowledges the reply"
    );
}

#[gpui::test]
fn closing_an_inactive_tab_keeps_the_tab_the_user_chose(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed_three());
    click(cx, "surface-tabs-0-tab-1");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    let inactive = cx.debug_bounds("surface-tabs-0-tab-2").unwrap().center();
    cx.simulate_mouse_move(inactive, None::<MouseButton>, Modifiers::none());
    cx.run_until_parked();
    click(cx, "surface-tabs-0-tab-2-close");
    assert!(
        cx.debug_bounds("conversation-view-4").is_none(),
        "tab closed"
    );
    assert!(
        cx.debug_bounds("conversation-view-3").is_some(),
        "the chosen tab stays in front"
    );
    assert!(cx.debug_bounds("conversation").is_none());
}

#[gpui::test]
fn closing_a_surface_menu_anchor_no_longer_blocks_observation(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(0));
    toggle_files(cx);
    click(cx, "surface-add");
    assert!(cx.debug_bounds("surface-add-conversation").is_some());
    toggle_files(cx);
    if cx.debug_bounds("files-tree-row-0").is_some() {
        toggle_files(cx);
    }
    assert!(
        cx.debug_bounds("files-tree-row-0").is_none(),
        "Files closed"
    );
    unread(&view, cx);
    click(cx, "conversation");
    assert!(
        viewed(&view, cx) >= 10,
        "the focused conversation acknowledges the reply"
    );
}

#[gpui::test]
fn fitting_away_the_tab_strips_closes_the_surface_menu(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed(0));
    toggle_files(cx);
    click(cx, "surface-add");
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("surface-add").is_none(),
        "no strip when fitted"
    );
    click(cx, "fitted-tab-0");
    unread(&view, cx);
    assert!(
        viewed(&view, cx) >= 10,
        "the conversation brought forward acknowledges the reply"
    );
}

#[gpui::test]
fn a_tab_choice_is_saved_and_customize_previews_replace_it(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    use_layout(&view, cx, tabbed_three());
    let writes = view.read_with(cx, |this, cx| this.state.read(cx).store().writes());
    click(cx, "surface-tabs-0-tab-1");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    view.read_with(cx, |this, cx| {
        assert_eq!(
            this.state.read(cx).store().writes(),
            writes + 1,
            "the choice is saved"
        );
    });
    click(cx, "customize-toggle");
    assert!(
        cx.debug_bounds("conversation-view-3").is_some(),
        "Customize starts from the saved choice"
    );
    click(cx, "surface-tabs-0-tab-0");
    assert!(cx.debug_bounds("conversation").is_some());
    click(cx, "customize-discard");
    assert!(
        cx.debug_bounds("conversation-view-3").is_some(),
        "a discarded draft shows the saved choice again"
    );
    click(cx, "surface-tabs-0-tab-0");
    click(cx, "customize-apply");
    click(cx, "customize-close");
    assert!(cx.debug_bounds("conversation").is_some());
    click(cx, "surface-tabs-0-tab-2");
    assert!(cx.debug_bounds("conversation-view-4").is_some());
    click(cx, "customize-toggle");
    click(cx, "customize-undo");
    assert!(
        cx.debug_bounds("conversation-view-3").is_some(),
        "choosing a tab keeps Undo, which restores the layout before Apply"
    );
}

#[gpui::test]
fn fitted_sidebar_entry_points_without_session_list_do_not_block_observation(
    cx: &mut TestAppContext,
) {
    let (_dir, view, cx) = open(cx);
    view.update_in(cx, |this, window, cx| {
        this.state
            .update(cx, |state, cx| {
                state.set_project_layout(
                    this.presentation.project.as_ref().unwrap(),
                    Some(tabbed(0).into()),
                    cx,
                )
            })
            .unwrap();
        this.customize(Action::Close, window, cx);
    });
    cx.simulate_resize(gpui::size(px(400.), px(420.)));
    cx.run_until_parked();
    unread(&view, cx);
    click(cx, "sidebar-toggle");
    click(cx, "conversation");
    assert!(viewed(&view, cx) >= 10);
    view.read_with(cx, |this, _cx| {
        assert!(!this.presentation.current.arrangement.sidebar_overlay)
    });
    click(cx, "composer-field");
    view.update(cx, |this, cx| {
        this.state
            .update(cx, |state, cx| {
                state.record_turn_end(1, false, crate::now_ms() + 1, cx)
            })
            .unwrap();
        cx.notify();
    });
    cx.dispatch_action(crate::FocusSearch);
    cx.run_until_parked();
    view.read_with(cx, |this, _cx| {
        assert!(!this.presentation.current.arrangement.sidebar_overlay)
    });
    click(cx, "conversation");
    view.read_with(cx, |this, cx| {
        let session = this.selected_session(cx).unwrap();
        assert!(session.viewed_at() >= session.updated_at());
    });
}

#[gpui::test]
fn fitted_checked_selection_survives_apply_and_replaces_temporary_navigation(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(cx);
    view.update_in(cx, |this, window, cx| {
        this.state
            .update(cx, |state, cx| {
                state.set_project_layout(
                    this.presentation.project.as_ref().unwrap(),
                    Some(tabbed(0).into()),
                    cx,
                )
            })
            .unwrap();
        this.customize(Action::Close, window, cx);
    });
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    let path = dir.path().join("workspace.json");
    let original = Store::open(&path).unwrap().saved_snapshot().unwrap();
    click(cx, "fitted-tab-3");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    assert_eq!(
        Store::open(&path).unwrap().saved_snapshot().unwrap(),
        original
    );
    click(cx, "customize-toggle");
    view.update(cx, |this, cx| {
        let saved = this.saved_composition(cx);
        let raw = this.saved_layout(cx).cloned();
        this.presentation.current.customization.dispatch(
            Action::Ask("Select conversation 3".into()),
            raw.as_ref(),
            &saved,
        );
        let request = this.presentation.current.customization.running().unwrap();
        this.receive_customization_outcome(
            request,
            crate::customization_agent::Outcome::Proposal(
                crate::customization_agent::Proposal::Change(tabbed(1)),
            ),
        );
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    click(cx, "customize-apply");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    view.read_with(cx, |this, cx| {
        assert_eq!(this.saved_composition(cx), tabbed(1))
    });
    click(cx, "customize-close");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    click(cx, "customize-toggle");
    click(cx, "fitted-tab-3");
    view.update(cx, |this, cx| {
        let saved = this.saved_composition(cx);
        let raw = this.saved_layout(cx).cloned();
        this.presentation.current.customization.dispatch(
            Action::Ask("Select conversation 0".into()),
            raw.as_ref(),
            &saved,
        );
        let request = this.presentation.current.customization.running().unwrap();
        this.receive_customization_outcome(
            request,
            crate::customization_agent::Outcome::Proposal(
                crate::customization_agent::Proposal::Change(tabbed(0)),
            ),
        );
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("conversation").is_some());
    assert!(cx.debug_bounds("conversation-view-3").is_none());
    let applied = Store::open(&path).unwrap().saved_snapshot().unwrap();
    click(cx, "fitted-tab-3");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    assert_eq!(
        Store::open(&path).unwrap().saved_snapshot().unwrap(),
        applied
    );
    click(cx, "customize-discard");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    click(cx, "fitted-tab-0");
    click(cx, "customize-close");
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    click(cx, "customize-toggle");
    click(cx, "customize-undo");
    assert!(cx.debug_bounds("conversation").is_some());
    view.read_with(cx, |this, cx| {
        assert_eq!(this.saved_composition(cx), tabbed(0))
    });
}

#[gpui::test]
fn fitting_uses_the_first_displayed_preferred_pane_and_refits_to_it_again(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx);
    let preferred: Composition = serde_json::from_value(serde_json::json!({
        "version": 1,
        "views": [
            {"id": 0, "component": "conversation"},
            {"id": 2, "component": "composer"},
            {"id": 3, "component": "conversation"},
            {"id": 4, "component": "conversation"}
        ],
        "root": {"kind": "split", "axis": "column", "children": [
            {"node": {"kind": "view", "id": 2}, "size": "auto"},
            {"node": {"kind": "split", "axis": "row", "children": [
                {"node": {"kind": "tabs", "selected": 1, "views": [0, 3]}, "size": {"fill": 1}},
                {"node": {"kind": "view", "id": 4}, "size": {"fill": 1}}
            ]}, "size": {"fill": 1}}
        ]}
    }))
    .unwrap();
    view.update_in(cx, |this, window, cx| {
        this.state
            .update(cx, |state, cx| {
                state.set_project_layout(
                    this.presentation.project.as_ref().unwrap(),
                    Some(preferred.into()),
                    cx,
                )
            })
            .unwrap();
        this.customize(Action::Close, window, cx);
    });
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    assert!(cx.debug_bounds("conversation").is_none());
    assert!(cx.debug_bounds("conversation-view-4").is_none());
    let saved = Store::open(dir.path().join("workspace.json"))
        .unwrap()
        .saved_snapshot()
        .unwrap();
    click(cx, "fitted-tab-4");
    assert!(cx.debug_bounds("conversation-view-4").is_some());
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    assert!(cx.debug_bounds("conversation-view-4").is_some());
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("conversation-view-3").is_some(),
        "refitting starts from the layout's choice again"
    );
    assert!(cx.debug_bounds("conversation-view-4").is_none());
    assert_eq!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .saved_snapshot()
            .unwrap(),
        saved
    );
}

fn open_auto_row(
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, Entity<Keystone>, &mut VisualTestContext) {
    let (dir, view, cx) = open(cx);
    cx.simulate_resize(gpui::size(px(1280.), px(900.)));
    click(cx, "select-component-3");
    click(cx, "component-right");
    click(cx, "select-component-2");
    click(cx, "component-right");
    click(cx, "customize-apply");
    click(cx, "customize-close");
    (dir, view, cx)
}

#[gpui::test]
fn an_auto_row_of_conversation_and_composer_keeps_both_visible(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open_auto_row(cx);
    view.read_with(cx, |this, cx| {
        let Node::Split {
            axis: Axis::Row,
            children,
        } = this.saved_composition(cx).root().clone()
        else {
            panic!("root is not a row")
        };
        let Node::Split {
            axis: Axis::Column,
            children,
        } = children[1].node.clone()
        else {
            panic!("body is not a column")
        };
        assert_eq!(children[0].node, Node::View { id: ViewId::MAIN });
        assert_eq!(children[0].size, Size::Fill(NonZeroU16::MIN));
        assert_eq!(children[1].size, Size::Auto);
        let Node::Split {
            axis: Axis::Row,
            children,
        } = children[1].node.clone()
        else {
            panic!("second slot is not a row")
        };
        assert_eq!(children[0].node, Node::View { id: ViewId(3) });
        assert_eq!(children[0].size, Size::Fill(NonZeroU16::MIN));
        assert_eq!(children[1].node, Node::View { id: ViewId(2) });
        assert_eq!(children[1].size, Size::Fill(NonZeroU16::MIN));
    });
    let upper = cx.debug_bounds("conversation").unwrap();
    let lower = cx.debug_bounds("conversation-view-3").unwrap();
    let field = cx.debug_bounds("composer-field").unwrap();
    assert!(
        lower.size.height > px(0.),
        "conversation 3 collapsed: {lower:?}"
    );
    assert!(
        field.top() >= upper.bottom(),
        "composer field {field:?} overlaps conversation {upper:?}"
    );
}

#[gpui::test]
fn a_multiline_draft_grows_the_auto_row_instead_of_overflowing_it(cx: &mut TestAppContext) {
    let (_dir, _view, cx) = open_auto_row(cx);
    let before = cx.debug_bounds("conversation-view-3").unwrap();
    click(cx, "composer-field");
    cx.simulate_input("one\ntwo\nthree\nfour\nfive\nsix");
    cx.run_until_parked();
    let upper = cx.debug_bounds("conversation").unwrap();
    let lower = cx.debug_bounds("conversation-view-3").unwrap();
    let field = cx.debug_bounds("composer-field").unwrap();
    assert!(field.size.height > px(44.), "draft did not grow: {field:?}");
    assert!(
        lower.size.height > before.size.height,
        "row did not grow with the draft: {before:?} then {lower:?}"
    );
    assert!(
        field.top() >= upper.bottom(),
        "composer field {field:?} overlaps conversation {upper:?}"
    );
    assert!(
        field.bottom() <= px(900.),
        "composer field leaves the window: {field:?}"
    );
}

fn working_directory() -> WorkingDirectory {
    WorkingDirectory::new(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[gpui::test]
fn escape_leaves_a_resize_drag_running(cx: &mut TestAppContext) {
    let (_dir, _view, cx) = open(cx);
    let from = cx.debug_bounds("split-grip-[]-0").unwrap().center();
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    for dx in [20., 40.] {
        cx.simulate_mouse_move(
            point(from.x + px(dx), from.y),
            MouseButton::Left,
            Modifiers::none(),
        );
    }
    assert!(cx.update(|_, cx| cx.has_active_drag()));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.update(|_, cx| cx.has_active_drag()),
        "Esc cancels only tab drags"
    );
}
