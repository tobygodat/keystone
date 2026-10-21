use super::*;
use crate::arrangement::ordered_views;
use gpui::{
    AppContext, Entity, Focusable, Modifiers, MouseButton, TestAppContext, VisualTestContext,
    point, px,
};
use keystone_core::{Node, Provider, Store, WorkingDirectory};
use std::path::{Path, PathBuf};

fn working_directory() -> WorkingDirectory {
    WorkingDirectory::new(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn project() -> keystone_core::ProjectId {
    keystone_core::ProjectId::new(&std::env::current_dir().unwrap()).unwrap()
}

/// A workspace with three sessions, 1 selected, and whatever `seed` saves before the
/// window opens, as a relaunch would find it.
fn seeded(dir: &Path, seed: impl FnOnce(&mut Store)) -> PathBuf {
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

fn open<'a>(
    path: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<Keystone>, &'a mut VisualTestContext) {
    let store = Store::open(path).unwrap();
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
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    view.update_in(cx, |this, window, cx| this.focus.focus(window, cx));
    cx.run_until_parked();
    (view, cx)
}

fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} not rendered"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

fn keys(cx: &mut VisualTestContext, keys: &str) {
    cx.simulate_keystrokes(keys);
    cx.run_until_parked();
}

/// Each conversation view in layout order, with the session it shows.
fn panes(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Vec<(ViewId, Option<u64>)> {
    view.read_with(cx, |this, cx| {
        let c = this.saved_composition(cx);
        ordered_views(c.root())
            .into_iter()
            .filter(|&v| c.component(v) == Some(Component::Conversation))
            .map(|v| (v, this.view_session(v, cx)))
            .collect()
    })
}

fn selected(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Option<u64> {
    view.read_with(cx, |this, cx| {
        this.state.read(cx).store().workspace().selected()
    })
}

fn active(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Option<(ViewId, Option<u64>)> {
    view.read_with(cx, |this, cx| {
        this.active_view(cx).map(|v| (v, this.view_session(v, cx)))
    })
}

fn composer(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |this, cx| this.composer.read(cx).value().to_owned())
}

fn split_layout(store: &mut Store) {
    let c = keystone_core::Composition::default()
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
        .unwrap()
        .edit(Edit::Open {
            view: ViewId(4),
            component: Component::Conversation,
            at: Place::TabWith(ViewId(3)),
            size: Size::Fill(std::num::NonZeroU16::MIN),
        })
        .unwrap();
    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId::MAIN, 1);
    tabs.hold(ViewId(3), 2);
    tabs.hold(ViewId(4), 3);
    store.set_draft(2, "kept in pane two".into()).unwrap();
    store.select(3).unwrap();
    store.show(&project(), Some(c), tabs, Some(3)).unwrap();
}

#[gpui::test]
fn an_old_state_shows_the_selected_session_in_one_pane_without_tabs(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |_| {});
    let (view, cx) = open(&path, cx);
    assert_eq!(panes(&view, cx), [(ViewId::MAIN, Some(1))]);
    assert!(cx.debug_bounds("surface-tabs-0").is_none());
    assert!(cx.debug_bounds("split-right").is_some());
    click(cx, "session-2");
    assert_eq!(panes(&view, cx), [(ViewId::MAIN, Some(2))]);
    let store = Store::open(&path).unwrap();
    assert!(
        store
            .workspace()
            .preferences()
            .session_tabs(&project())
            .is_empty()
    );
}

#[gpui::test]
fn split_right_shows_two_sessions_side_by_side_and_saves_them_together(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |store| {
        store.record_message(2, "recent", 50).unwrap();
    });
    let (view, cx) = open(&path, cx);
    keys(cx, "ctrl-\\");
    let shown = panes(&view, cx);
    assert_eq!(shown.len(), 2);
    assert_eq!(shown[0], (ViewId::MAIN, Some(1)));
    assert_eq!(shown[1].1, Some(2), "the most recent session not on screen");
    assert_eq!(active(&view, cx), Some(shown[1]));
    assert_eq!(selected(&view, cx), Some(2));
    let left = cx.debug_bounds("conversation").unwrap();
    let right = cx
        .debug_bounds(format!("conversation-view-{}", shown[1].0.0).leak())
        .unwrap();
    assert!(right.left() >= left.right(), "the new pane is to the right");

    let store = Store::open(&path).unwrap();
    let workspace = store.workspace();
    assert_eq!(
        workspace.preferences().composition(&project()),
        view.read_with(cx, |this, cx| this.saved_composition(cx))
    );
    assert_eq!(
        workspace
            .preferences()
            .session_tabs(&project())
            .iter()
            .collect::<Vec<_>>(),
        [(ViewId::MAIN, 1), (shown[1].0, 2)]
    );
    assert_eq!(workspace.selected(), Some(2));
}

#[gpui::test]
fn a_relaunch_restores_panes_tabs_the_active_tab_and_each_draft(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    assert_eq!(
        panes(&view, cx),
        [
            (ViewId::MAIN, Some(1)),
            (ViewId(3), Some(2)),
            (ViewId(4), Some(3))
        ]
    );
    assert_eq!(active(&view, cx), Some((ViewId(4), Some(3))));
    assert!(cx.debug_bounds("surface-tabs-0-tab-0").is_some());
    assert!(cx.debug_bounds("surface-tabs-3-tab-1").is_some());
    assert!(cx.debug_bounds("conversation-view-4").is_some());
    assert!(
        cx.debug_bounds("conversation-view-3").is_none(),
        "tab 2 is behind"
    );
    click(cx, "surface-tabs-3-tab-0");
    assert_eq!(active(&view, cx), Some((ViewId(3), Some(2))));
    assert_eq!(selected(&view, cx), Some(2));
    assert_eq!(composer(&view, cx), "kept in pane two");
}

#[gpui::test]
fn sidebar_select_focuses_an_existing_tab_instead_of_opening_another(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    let before = panes(&view, cx);
    click(cx, "session-2");
    assert_eq!(panes(&view, cx), before, "no duplicate opens");
    assert_eq!(active(&view, cx), Some((ViewId(3), Some(2))));
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    let store = Store::open(&path).unwrap();
    assert_eq!(store.workspace().selected(), Some(2));
    let Node::Split { children, .. } = store
        .workspace()
        .preferences()
        .composition(&project())
        .root()
        .clone()
    else {
        panic!()
    };
    assert!(
        format!("{:?}", children).contains("selected: 0"),
        "the saved pane shows tab 2"
    );

    click(cx, "conversation");
    assert_eq!(active(&view, cx), Some((ViewId::MAIN, Some(1))));
    assert_eq!(selected(&view, cx), Some(1));
}

#[gpui::test]
fn the_keyboard_cycles_closes_and_collapses_tabs(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    keys(cx, "ctrl-tab");
    assert_eq!(active(&view, cx), Some((ViewId::MAIN, Some(1))), "wraps");
    keys(cx, "ctrl-tab");
    assert_eq!(active(&view, cx), Some((ViewId(3), Some(2))));
    keys(cx, "ctrl-shift-tab");
    assert_eq!(active(&view, cx), Some((ViewId::MAIN, Some(1))));
    keys(cx, "ctrl-shift-tab");
    assert_eq!(active(&view, cx), Some((ViewId(4), Some(3))));

    keys(cx, "ctrl-w");
    assert_eq!(
        panes(&view, cx),
        [(ViewId::MAIN, Some(1)), (ViewId(3), Some(2))]
    );
    assert_eq!(active(&view, cx), Some((ViewId(3), Some(2))));
    keys(cx, "ctrl-w");
    assert_eq!(panes(&view, cx), [(ViewId::MAIN, Some(1))]);
    assert_eq!(selected(&view, cx), Some(1));
    view.read_with(cx, |this, cx| {
        assert_eq!(
            this.saved_composition(cx),
            keystone_core::Composition::default(),
            "the split collapses"
        )
    });
    keys(cx, "ctrl-w");
    assert_eq!(
        panes(&view, cx),
        [(ViewId::MAIN, Some(1))],
        "the last view stays"
    );
    click(cx, "session-3");
    assert_eq!(panes(&view, cx), [(ViewId::MAIN, Some(3))]);
    let store = Store::open(&path).unwrap();
    assert!(
        store
            .workspace()
            .preferences()
            .session_tabs(&project())
            .is_empty()
    );
    assert_eq!(store.workspace().selected(), Some(3));
}

#[gpui::test]
fn new_tab_starts_a_session_beside_the_active_one(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    click(cx, "conversation");
    let writes = view.read_with(cx, |this, cx| this.state.read(cx).store().writes());
    keys(cx, "ctrl-t");
    assert_eq!(
        view.read_with(cx, |this, cx| this.state.read(cx).store().writes()),
        writes + 1,
        "the session, its tab and the selection are one write"
    );
    let saved = Store::open(&path).unwrap();
    assert_eq!(saved.workspace().selected(), Some(4));
    assert!(
        saved
            .workspace()
            .preferences()
            .session_tabs(&project())
            .view_of(4)
            .is_some()
    );
    let shown = panes(&view, cx);
    assert_eq!(shown.len(), 4);
    let (new, session) = active(&view, cx).unwrap();
    assert_eq!(session, Some(4));
    assert_eq!(shown[1], (new, Some(4)), "in pane 1, after its tab");
    assert_eq!(selected(&view, cx), Some(4));
    assert!(
        view.read_with(cx, |this, cx| this
            .state
            .read(cx)
            .store()
            .workspace()
            .sessions()
            .len())
            == 4
    );
}

#[gpui::test]
fn a_middle_click_in_the_sidebar_opens_a_session_in_a_new_tab(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |_| {});
    let (view, cx) = open(&path, cx);
    let row = cx.debug_bounds("session-3").unwrap().center();
    cx.simulate_mouse_down(row, MouseButton::Middle, Modifiers::none());
    cx.simulate_mouse_up(row, MouseButton::Middle, Modifiers::none());
    cx.run_until_parked();
    let shown = panes(&view, cx);
    assert_eq!(shown.len(), 2);
    assert_eq!(
        shown[0],
        (ViewId::MAIN, Some(1)),
        "the open session keeps its tab"
    );
    assert_eq!(active(&view, cx), Some((shown[1].0, Some(3))));
    assert!(cx.debug_bounds("surface-tabs-0-tab-1").is_some());
}

fn drag(cx: &mut VisualTestContext, from: &'static str, to: gpui::Point<gpui::Pixels>) {
    let from = cx.debug_bounds(from).unwrap().center();
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        point(from.x + px(8.), from.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn dragging_a_tab_reorders_it_and_moves_it_between_panes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    let target = cx.debug_bounds("surface-tabs-3-tab-0").unwrap().center();
    drag(cx, "surface-tabs-3-tab-1", target);
    assert_eq!(
        panes(&view, cx),
        [
            (ViewId::MAIN, Some(1)),
            (ViewId(4), Some(3)),
            (ViewId(3), Some(2))
        ]
    );

    let target = cx.debug_bounds("surface-tabs-0-tab-0").unwrap().center();
    drag(cx, "surface-tabs-4-tab-1", target);
    let shown = panes(&view, cx);
    assert_eq!(
        shown,
        [
            (ViewId(3), Some(2)),
            (ViewId::MAIN, Some(1)),
            (ViewId(4), Some(3))
        ]
    );
    assert_eq!(active(&view, cx), Some((ViewId(3), Some(2))));
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.workspace().preferences().composition(&project()),
        view.read_with(cx, |this, cx| this.saved_composition(cx))
    );
}

#[gpui::test]
fn dragging_a_tab_to_a_pane_edge_splits_it_out(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    let pane = cx.debug_bounds("conversation-view-4").unwrap();
    let edge = point(pane.right() - px(10.), pane.center().y);
    let from = cx.debug_bounds("surface-tabs-3-tab-1").unwrap().center();
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        point(from.x + px(8.), from.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("pane-drop-4-right").is_some(),
        "edges show while dragging"
    );
    cx.simulate_mouse_move(edge, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(edge, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("pane-drop-4-right").is_none());
    assert_eq!(
        panes(&view, cx),
        [
            (ViewId::MAIN, Some(1)),
            (ViewId(3), Some(2)),
            (ViewId(4), Some(3))
        ]
    );
    view.read_with(cx, |this, cx| {
        let c = this.saved_composition(cx);
        let Node::Split { children, .. } = c.root() else {
            panic!()
        };
        let Node::Split { children, .. } = &children[1].node else {
            panic!()
        };
        let Node::Split { children, axis } = &children[0].node else {
            panic!()
        };
        assert_eq!(*axis, Axis::Row);
        let leaves: Vec<_> = children.iter().map(|c| c.node.clone()).collect();
        assert_eq!(
            leaves,
            [
                Node::View { id: ViewId::MAIN },
                Node::View { id: ViewId(3) },
                Node::View { id: ViewId(4) }
            ],
            "three panes side by side"
        );
    });
    assert_eq!(active(&view, cx), Some((ViewId(4), Some(3))));
}

/// 200 sessions, 40 of them open as tabs in two panes.
fn many_tabs(dir: &Path) -> PathBuf {
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    for n in 0..200u64 {
        let provider = if n % 2 == 0 {
            Provider::Claude
        } else {
            Provider::Codex
        };
        store.create(provider, &working_directory(), n).unwrap();
    }
    let mut c = keystone_core::Composition::default()
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
    for n in 0..38u32 {
        let view = ViewId(4 + n);
        let anchor = if n % 2 == 0 { ViewId::MAIN } else { ViewId(3) };
        c = c
            .edit(Edit::Open {
                view,
                component: Component::Conversation,
                at: Place::TabWith(anchor),
                size: Size::Fill(std::num::NonZeroU16::MIN),
            })
            .unwrap();
        tabs.hold(view, u64::from(n) + 3);
    }
    store.select(1).unwrap();
    store.show(&project(), Some(c), tabs, Some(1)).unwrap();
    path
}

/// The time `action` takes, saving included, plus the frame it draws.
fn timed(
    view: &Entity<Keystone>,
    cx: &mut VisualTestContext,
    action: impl FnOnce(&mut Keystone, &mut gpui::Window, &mut gpui::Context<Keystone>),
) -> f64 {
    let start = std::time::Instant::now();
    view.update_in(cx, action);
    let acted = start.elapsed();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    eprintln!(
        "DBG act={:.2} frame={:.2}",
        acted.as_secs_f64() * 1000.,
        (start.elapsed() - acted).as_secs_f64() * 1000.
    );
    start.elapsed().as_secs_f64() * 1000.
}

fn report(name: &str, mut frames: Vec<f64>) {
    frames.sort_by(f64::total_cmp);
    eprintln!(
        "PERF {name}_ms={:.2} {name}_p95_ms={:.2}",
        frames[frames.len() / 2],
        frames[frames.len() * 95 / 100]
    );
}

#[gpui::test]
#[ignore = "benchmark; run through scripts/perf.py"]
fn perf_tab_switch(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = many_tabs(dir.path());
    let (view, cx) = open(&path, cx);
    assert_eq!(panes(&view, cx).len(), 40);
    let mut frames = Vec::new();
    for _ in 0..60 {
        frames.push(timed(&view, cx, |this, window, cx| {
            this.cycle_tab(false, window, cx)
        }));
    }
    report("tab_switch", frames);
}

#[gpui::test]
#[ignore = "benchmark; run through scripts/perf.py"]
fn perf_split_open(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = many_tabs(dir.path());
    let (view, cx) = open(&path, cx);
    let mut frames = Vec::new();
    for _ in 0..20 {
        let before = panes(&view, cx);
        frames.push(timed(&view, cx, |this, window, cx| {
            this.split_right(window, cx)
        }));
        let (moved, _) = active(&view, cx).unwrap();
        let home = before
            .iter()
            .map(|(v, _)| *v)
            .find(|&v| v != moved)
            .unwrap();
        view.update(cx, |this, cx| this.move_tab(moved, home, 0, cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    assert_eq!(panes(&view, cx).len(), 40);
    report("split_open", frames);
}

#[gpui::test]
fn dragging_a_pane_divider_resizes_once_and_saves_on_release(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    let left = cx.debug_bounds("conversation").unwrap().size.width;
    let writes = view.read_with(cx, |this, cx| this.state.read(cx).store().writes());
    let grip = cx.debug_bounds("split-grip-[1, 0]-0").unwrap().center();
    cx.simulate_mouse_down(grip, MouseButton::Left, Modifiers::none());
    for step in 1..=4 {
        cx.simulate_mouse_move(
            point(grip.x + px(30. * step as f32), grip.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
    }
    assert!(cx.debug_bounds("conversation").unwrap().size.width > left + px(100.));
    assert_eq!(
        view.read_with(cx, |this, cx| this.state.read(cx).store().writes()),
        writes,
        "nothing is saved mid-drag"
    );
    let end = point(grip.x + px(120.), grip.y);
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    let width = cx.debug_bounds("conversation").unwrap().size.width;
    assert!(width > left + px(100.));
    assert_eq!(
        view.read_with(cx, |this, cx| this.state.read(cx).store().writes()),
        writes + 1
    );
    let reopened = Store::open(&path).unwrap();
    let saved = reopened.workspace().preferences().composition(&project());
    assert_eq!(
        saved,
        view.read_with(cx, |this, cx| this.saved_composition(cx))
    );
    assert_ne!(saved.root(), keystone_core::Composition::default().root());
}

#[gpui::test]
fn a_narrow_window_shows_the_tab_the_composer_sends_to(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    cx.simulate_resize(gpui::size(px(720.), px(800.)));
    cx.run_until_parked();
    let shown = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        let (active, session) = active(view, cx).unwrap();
        let id = if active == ViewId::MAIN {
            "conversation".to_owned()
        } else {
            format!("conversation-view-{}", active.0)
        };
        assert!(
            cx.debug_bounds(id.leak()).is_some(),
            "{active:?} is on screen"
        );
        assert_eq!(session, selected(view, cx));
        session
    };
    assert_eq!(shown(&view, cx), Some(3));
    click(cx, "fitted-tab-0");
    assert_eq!(shown(&view, cx), Some(1));
    keys(cx, "ctrl-tab");
    assert_eq!(shown(&view, cx), Some(2));
    view.update(cx, |this, cx| this.select(3, cx));
    cx.run_until_parked();
    assert_eq!(shown(&view, cx), Some(3));
    keys(cx, "ctrl-shift-tab");
    assert_eq!(shown(&view, cx), Some(2));
}

#[gpui::test]
fn a_selection_no_tab_shows_after_a_crash_selects_the_first_tab_instead(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |store| {
        split_layout(store);
        let tabs = store.workspace().preferences().session_tabs(&project());
        let mut kept = SessionTabs::default();
        for (view, session) in tabs.iter().filter(|&(_, s)| s != 3) {
            kept.hold(view, session);
        }
        kept.hold(ViewId(4), 2);
        let mut c = store.workspace().preferences().composition(&project());
        c = c.edit(Edit::Close(ViewId(3))).unwrap();
        store.show(&project(), Some(c), kept, Some(3)).unwrap();
    });
    let (view, cx) = open(&path, cx);
    assert_eq!(
        panes(&view, cx),
        [(ViewId::MAIN, Some(1)), (ViewId(4), Some(2))],
        "every saved tab is kept"
    );
    assert_eq!(selected(&view, cx), Some(1));
    assert_eq!(active(&view, cx), Some((ViewId::MAIN, Some(1))));
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .workspace()
            .preferences()
            .session_tabs(&project())
            .iter()
            .collect::<Vec<_>>(),
        [(ViewId::MAIN, 1), (ViewId(4), 2)]
    );
}

#[gpui::test]
fn removing_the_active_pane_in_customize_moves_the_composer_to_a_shown_tab(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    view.update_in(cx, |this, window, cx| {
        use crate::arrangement_editor::Action;
        this.customize(Action::Open, window, cx);
        this.customize(Action::Edit(Edit::Close(ViewId(3))), window, cx);
        this.customize(Action::Edit(Edit::Close(ViewId(4))), window, cx);
        this.customize(Action::Apply, window, cx);
        this.customize(Action::Close, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(panes(&view, cx), [(ViewId::MAIN, Some(1))]);
    assert_eq!(
        selected(&view, cx),
        Some(1),
        "not the removed pane's session"
    );
}

#[gpui::test]
fn tab_shortcuts_leave_settings_first(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    keys(cx, "ctrl-,");
    assert!(view.read_with(cx, |this, _| this.settings.is_some()));
    keys(cx, "ctrl-tab");
    assert!(view.read_with(cx, |this, _| this.settings.is_none()));
    assert_eq!(active(&view, cx), Some((ViewId::MAIN, Some(1))));
    keys(cx, "ctrl-,");
    keys(cx, "ctrl-t");
    assert!(view.read_with(cx, |this, _| this.settings.is_none()));
    assert!(view.update_in(cx, |this, window, cx| {
        this.composer.focus_handle(cx).is_focused(window)
    }));
}

#[gpui::test]
fn only_the_selected_sessions_recovery_card_takes_keyboard_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    click(cx, "conversation");
    view.update(cx, |this, cx| {
        this.state.update(cx, |state, _| {
            for id in [1, 3] {
                state.test_conversation(id).recovery = Some(
                    crate::conversation::Recovery::Unavailable("scripted".into()),
                );
            }
        });
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("recovery").is_some());
    assert!(cx.debug_bounds("recovery-view-4").is_some());
    view.update_in(cx, |this, window, cx| {
        this.recovery_focus.0.clone().focus(window, cx);
    });
    cx.run_until_parked();
    assert!(view.update_in(cx, |this, window, cx| {
        this.view_contains_focus(ViewId::MAIN, window, cx)
            && !this.view_contains_focus(ViewId(4), window, cx)
    }));
}

/// What a sidebar click costs without tabs, the same way, for comparison.
#[gpui::test]
#[ignore = "benchmark; run through scripts/perf.py"]
fn perf_select_without_tabs(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    for n in 0..200u64 {
        store
            .create(Provider::Claude, &working_directory(), n)
            .unwrap();
    }
    drop(store);
    let (view, cx) = open(&path, cx);
    let mut frames = Vec::new();
    for n in 0..60u64 {
        frames.push(timed(&view, cx, |this, _, cx| this.select(n % 40 + 1, cx)));
    }
    report("select_without_tabs", frames);
}

#[gpui::test]
fn a_relaunch_shows_a_selection_no_tab_holds_in_a_free_pane_and_keeps_every_tab(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |store| {
        let c = keystone_core::Composition::default()
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
        tabs.hold(ViewId::MAIN, 2);
        store.show(&project(), Some(c), tabs, Some(1)).unwrap();
    });
    let (view, cx) = open(&path, cx);
    assert_eq!(
        panes(&view, cx),
        [(ViewId::MAIN, Some(2)), (ViewId(3), Some(1))]
    );
    assert_eq!(active(&view, cx), Some((ViewId(3), Some(1))));
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .workspace()
            .preferences()
            .session_tabs(&project())
            .iter()
            .collect::<Vec<_>>(),
        [(ViewId::MAIN, 2)],
        "the held tab is not replaced"
    );
}

#[gpui::test]
fn a_sidebar_select_and_closing_the_active_tab_each_save_in_one_write(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |store| {
        store
            .create(Provider::Claude, &working_directory(), 3)
            .unwrap();
        store.rename(4, "Session 4").unwrap();
        split_layout(store);
    });
    let (view, cx) = open(&path, cx);
    let writes = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        view.read_with(cx, |this, cx| this.state.read(cx).store().writes())
    };
    assert_eq!(active(&view, cx), Some((ViewId(4), Some(3))));
    let before = writes(&view, cx);
    click(cx, "session-4");
    assert_eq!(writes(&view, cx), before + 1);
    assert_eq!(active(&view, cx), Some((ViewId(4), Some(4))));
    let store = Store::open(&path).unwrap();
    assert_eq!(store.workspace().selected(), Some(4));
    assert_eq!(
        store.workspace().preferences().held(&project(), ViewId(4)),
        Some(4)
    );

    let before = writes(&view, cx);
    keys(cx, "ctrl-w");
    assert_eq!(writes(&view, cx), before + 1);
    assert_eq!(active(&view, cx), Some((ViewId(3), Some(2))));
    let store = Store::open(&path).unwrap();
    assert_eq!(store.workspace().selected(), Some(2));
    assert_eq!(
        store
            .workspace()
            .preferences()
            .session_tabs(&project())
            .iter()
            .collect::<Vec<_>>(),
        [(ViewId::MAIN, 1), (ViewId(3), 2)]
    );
}

#[gpui::test]
fn closing_a_tab_by_keyboard_does_nothing_in_customize(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    view.update_in(cx, |this, window, cx| {
        this.customize(crate::arrangement_editor::Action::Open, window, cx)
    });
    cx.run_until_parked();
    let before = view.read_with(cx, |this, cx| this.shown_composition(cx));
    keys(cx, "ctrl-w");
    assert_eq!(
        view.read_with(cx, |this, cx| this.shown_composition(cx)),
        before
    );
}

#[gpui::test]
fn a_relaunch_brings_forward_the_free_tab_the_selection_shows_in(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |store| {
        let c = keystone_core::Composition::default()
            .edit(Edit::Open {
                view: ViewId(3),
                component: Component::Conversation,
                at: Place::TabWith(ViewId::MAIN),
                size: Size::Fill(std::num::NonZeroU16::MIN),
            })
            .unwrap()
            .edit(Edit::SelectTab(ViewId::MAIN))
            .unwrap();
        let mut tabs = SessionTabs::default();
        tabs.hold(ViewId::MAIN, 2);
        store.show(&project(), Some(c), tabs, Some(1)).unwrap();
    });
    let (view, cx) = open(&path, cx);
    assert_eq!(active(&view, cx), Some((ViewId(3), Some(1))));
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    assert!(cx.debug_bounds("conversation").is_none());
    let store = Store::open(&path).unwrap();
    let c = store.workspace().preferences().composition(&project());
    assert_eq!(
        tabs_holding(c.root(), ViewId(3)).map(|(views, shown)| views[shown]),
        Some(ViewId(3))
    );
    assert_eq!(
        store
            .workspace()
            .preferences()
            .held(&project(), ViewId::MAIN),
        Some(2)
    );
}

#[gpui::test]
fn clicking_a_tab_saves_the_tab_and_the_selection_in_one_write(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (view, cx) = open(&path, cx);
    assert_eq!(active(&view, cx), Some((ViewId(4), Some(3))));
    let before = view.read_with(cx, |this, cx| this.state.read(cx).store().writes());
    click(cx, "surface-tabs-3-tab-0");
    assert_eq!(
        view.read_with(cx, |this, cx| this.state.read(cx).store().writes()),
        before + 1
    );
    assert_eq!(active(&view, cx), Some((ViewId(3), Some(2))));
    assert!(cx.debug_bounds("conversation-view-3").is_some());
    let store = Store::open(&path).unwrap();
    assert_eq!(store.workspace().selected(), Some(2));
}

#[gpui::test]
fn a_sidebar_select_made_in_customize_shows_once_it_closes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |store| {
        store
            .create(Provider::Claude, &working_directory(), 3)
            .unwrap();
        store.rename(4, "Session 4").unwrap();
        split_layout(store);
    });
    let (view, cx) = open(&path, cx);
    view.update_in(cx, |this, window, cx| {
        this.customize(crate::arrangement_editor::Action::Open, window, cx)
    });
    cx.run_until_parked();
    click(cx, "session-4");
    view.update_in(cx, |this, window, cx| {
        this.customize(crate::arrangement_editor::Action::Close, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(selected(&view, cx), Some(4));
    assert_eq!(active(&view, cx), Some((ViewId(4), Some(4))));
    assert_eq!(
        panes(&view, cx),
        [
            (ViewId::MAIN, Some(1)),
            (ViewId(3), Some(2)),
            (ViewId(4), Some(4))
        ]
    );
}

#[gpui::test]
fn opening_a_tab_or_a_pane_withdraws_customize_undo(cx: &mut TestAppContext) {
    for shortcut in ["ctrl-t", "ctrl-\\"] {
        let dir = tempfile::tempdir().unwrap();
        let path = seeded(dir.path(), split_layout);
        let (view, cx) = open(&path, cx);
        view.update_in(cx, |this, window, cx| {
            use crate::arrangement_editor::Action;
            this.customize(Action::Open, window, cx);
            this.customize(Action::Edit(Edit::Close(ViewId(4))), window, cx);
            this.customize(Action::Apply, window, cx);
            this.customize(Action::Close, window, cx);
        });
        cx.run_until_parked();
        let can_undo = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
            view.read_with(cx, |this, cx| {
                this.presentation
                    .current
                    .customization
                    .can_undo(&this.saved_composition(cx))
            })
        };
        assert!(can_undo(&view, cx));
        let before = panes(&view, cx).len();
        keys(cx, shortcut);
        assert_eq!(panes(&view, cx).len(), before + 1, "{shortcut}");
        assert!(!can_undo(&view, cx), "{shortcut} withdraws Undo");
    }
}

#[gpui::test]
fn fitted_tabs_with_long_titles_keep_the_shown_tab_in_the_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), |store| {
        for id in 1..=3 {
            store
                .rename(
                    id,
                    &format!("Session {id} with a long title that keeps going on"),
                )
                .unwrap();
        }
        split_layout(store);
    });
    let (view, cx) = open(&path, cx);
    cx.simulate_resize(gpui::size(px(720.), px(800.)));
    cx.run_until_parked();
    cx.run_until_parked();
    assert_eq!(active(&view, cx), Some((ViewId(4), Some(3))));
    for id in ["fitted-tab-0", "fitted-tab-3", "fitted-tab-4"] {
        let bounds = cx.debug_bounds(id).unwrap();
        assert!(f32::from(bounds.size.width) <= 200., "{id} is {bounds:?}");
    }
    let shown = cx.debug_bounds("fitted-tab-4").unwrap();
    assert!(f32::from(shown.right()) <= 720., "{shown:?}");
}

#[gpui::test]
fn the_fitted_hint_lines_up_with_the_tab_labels(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), split_layout);
    let (_view, cx) = open(&path, cx);
    cx.simulate_resize(gpui::size(px(720.), px(800.)));
    cx.run_until_parked();
    let hint = cx.debug_bounds("fitted-hint").unwrap();
    let tabs = cx.debug_bounds("fitted-tabs").unwrap();
    let tab = cx.debug_bounds("fitted-tab-0").unwrap();
    let label = f32::from(tab.left()) + 9.;
    assert!(
        (f32::from(hint.left()) - label).abs() <= 1.,
        "the hint starts where the tab label does: {hint:?} {tab:?}"
    );
    assert!(
        f32::from(tabs.top()) >= f32::from(hint.bottom()),
        "{hint:?} {tabs:?}"
    );
}

#[gpui::test]
fn new_tab_still_starts_a_session_when_saved_tabs_cannot_be_changed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    std::fs::write(
        &path,
        r#"{"version":1,"next_id":3,"workspace":{"sessions":[{"id":1,"title":"One","provider":"Codex","settled":false,"draft":""},{"id":2,"title":"Two","provider":"Claude","settled":false,"draft":""}],"selected":1,"preferences":{"project_tabs":"wide"}}}"#,
    )
    .unwrap();
    let (view, cx) = open(&path, cx);
    let sessions = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        view.read_with(cx, |this, cx| {
            this.state.read(cx).store().workspace().sessions().len()
        })
    };
    assert_eq!(sessions(&view, cx), 2);
    keys(cx, "ctrl-t");
    assert_eq!(sessions(&view, cx), 3);
    assert_eq!(selected(&view, cx), Some(3));
    let error = view.read_with(cx, |this, _| this.error.clone());
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.starts_with("Layout not changed.")),
        "{error:?}"
    );
}
