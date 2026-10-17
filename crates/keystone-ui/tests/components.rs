use gpui::{Context, IntoElement, Modifiers, Render, TestAppContext, Window, div, prelude::*};
use keystone_ui::prelude::*;

#[derive(Default)]
struct Harness {
    clicks: Vec<&'static str>,
    open: bool,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().h_8().debug_selector(|| "outside".into()))
            .child(
                Button::new("enabled", "Enabled")
                    .debug_selector(|| "enabled".into())
                    .on_click(record("enabled", cx)),
            )
            .child(
                Button::new("disabled", "Disabled")
                    .disabled(true)
                    .debug_selector(|| "disabled".into())
                    .on_click(record("disabled", cx)),
            )
            .child(
                ListItem::new("row")
                    .debug_selector(|| "row".into())
                    .on_click(record("row", cx))
                    .child(Label::new("Row")),
            )
            .child(
                PopoverMenu::new(
                    "menu",
                    IconButton::new("trigger", IconName::Plus).debug_selector(|| "trigger".into()),
                )
                .open(self.open)
                .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                    this.open = *open;
                    cx.notify();
                }))
                .child(
                    MenuItem::new("item", "Item")
                        .debug_selector(|| "item".into())
                        .on_click(record("item", cx)),
                ),
            )
    }
}

fn record(
    name: &'static str,
    cx: &mut Context<Harness>,
) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _, _, cx| {
        this.clicks.push(name);
        cx.notify();
    })
}

fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = cx.debug_bounds(selector).expect(selector);
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn clicks_reach_enabled_controls_only(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let (view, cx) = cx.add_window_view(|_, _| Harness::default());
    click(cx, "enabled");
    click(cx, "disabled");
    click(cx, "row");
    view.read_with(cx, |h, _| assert_eq!(h.clicks, vec!["enabled", "row"]));
}

#[gpui::test]
fn popover_opens_from_trigger_and_closes_on_any_outside_press(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let (view, cx) = cx.add_window_view(|_, _| Harness::default());
    let open = |cx: &mut gpui::VisualTestContext| view.read_with(cx, |h, _| h.open);
    click(cx, "trigger");
    assert!(open(cx));
    let item = cx.debug_bounds("item").expect("item");
    click(cx, "item");
    click(cx, "outside");
    assert!(!open(cx));
    assert!(cx.debug_bounds("item").is_none());
    cx.simulate_click(item.center(), Modifiers::none());
    cx.run_until_parked();
    view.read_with(cx, |h, _| assert_eq!(h.clicks, vec!["item"]));
    click(cx, "trigger");
    assert!(open(cx));
    click(cx, "trigger");
    assert!(!open(cx));
}

struct Reply;

impl Render for Reply {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(Markdown::from_source(
            "reply",
            "Some **bold** [text](https://example.com).\n\n```rust\nfn main() {}\n```\n\n```\nsecond\n```\n",
        ))
    }
}

#[gpui::test]
fn markdown_code_blocks_copy_their_code(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let (_view, cx) = cx.add_window_view(|_, _| Reply);
    click(cx, "reply-copy-1");
    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
    assert_eq!(copied.as_deref(), Some("fn main() {}"));
    click(cx, "reply-copy-2");
    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
    assert_eq!(copied.as_deref(), Some("second"));
}

/// Agent replies are untrusted, so nesting far past any real reply must still render.
struct Adversarial(&'static str);

impl Render for Adversarial {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let deep = |unit: &str, n: usize| unit.repeat(n);
        let source = match self.0 {
            "quotes" => deep("> ", 5_000) + "hello\n\n```\ndeep\n```\n",
            "lists" => (0..2_000)
                .map(|i| format!("{}- item {i}\n", "  ".repeat(i)))
                .collect(),
            "mixed" => deep("> - 1. ", 1_500) + "hello\n\n```\ndeep\n```\n",
            _ => deep("*_", 5_000) + "hello" + &deep("_*", 5_000),
        };
        div()
            .size_full()
            .child(Markdown::from_source("deep", &source))
    }
}

#[gpui::test]
fn markdown_renders_adversarial_nesting(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    for kind in ["quotes", "lists", "mixed", "emphasis"] {
        let (_view, cx) = cx.add_window_view(|_, _| Adversarial(kind));
        cx.run_until_parked();
        if kind == "quotes" || kind == "mixed" {
            // The code block under the nesting still renders and copies.
            click(cx, "deep-copy-1");
            let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
            assert_eq!(copied.as_deref(), Some("deep"), "{kind}");
        }
    }
}

struct Confirm {
    focus: Option<keystone_ui::DialogFocus>,
    behind: gpui::FocusHandle,
    answers: Vec<&'static str>,
}

impl Render for Confirm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let close = |answer| {
            cx.listener(move |this: &mut Confirm, _: &(), window, cx| {
                this.answers.push(answer);
                this.focus = None;
                window.focus(&this.behind, cx);
                cx.notify();
            })
        };
        let (cancel, confirm) = (close("cancel"), close("confirm"));
        div()
            .size_full()
            .track_focus(&self.behind)
            .child(
                Button::new("behind", "Behind")
                    .debug_selector(|| "behind".into())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.answers.push("behind");
                        cx.notify();
                    })),
            )
            .when_some(self.focus.as_ref(), |d, focus| {
                d.child(
                    ConfirmDialog::new("ask", focus, "Allow?", "It will happen without asking.")
                        .tone(Color::Warning)
                        .on_cancel(move |window, cx| cancel(&(), window, cx))
                        .on_confirm(move |window, cx| confirm(&(), window, cx)),
                )
            })
    }
}

fn open_confirm(cx: &mut TestAppContext) -> (gpui::Entity<Confirm>, &mut gpui::VisualTestContext) {
    cx.update(keystone_ui::init);
    let (view, cx) = cx.add_window_view(|_, cx| Confirm {
        focus: None,
        behind: cx.focus_handle(),
        answers: Vec::new(),
    });
    view.update_in(cx, |this, window, cx| {
        this.focus = Some(keystone_ui::DialogFocus::new(window, cx));
        cx.notify();
    });
    cx.run_until_parked();
    (view, cx)
}

/// A key press and release. GPUI clicks a focused button on Enter's release.
fn press(cx: &mut gpui::VisualTestContext, key: &str) {
    cx.simulate_keystrokes(key);
    cx.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse(key).unwrap(),
    });
}

#[gpui::test]
fn a_confirm_dialog_answers_once_and_returns_focus(cx: &mut TestAppContext) {
    for (how, expected) in [
        ("cancel", "cancel"),
        ("escape", "cancel"),
        ("enter", "cancel"),
        ("scrim", "cancel"),
        ("confirm", "confirm"),
        ("tab enter", "confirm"),
        ("tab tab enter", "cancel"),
    ] {
        let (view, cx) = open_confirm(cx);
        assert!(cx.debug_bounds("ask").is_some(), "{how}: dialog shows");
        match how {
            "cancel" => click(cx, "ask-cancel"),
            "confirm" => click(cx, "ask-confirm"),
            // The card is centered, so the window's corner is scrim.
            "scrim" => {
                cx.simulate_click(gpui::point(gpui::px(2.), gpui::px(2.)), Modifiers::none());
                cx.run_until_parked();
            }
            keys => keys.split(' ').for_each(|key| press(cx, key)),
        }
        view.read_with(cx, |c, _| {
            assert_eq!(c.answers, vec![expected], "{how}");
            assert!(c.focus.is_none(), "{how}: dialog closed");
        });
        cx.update(|window, cx| {
            let behind = view.read(cx).behind.clone();
            assert!(behind.is_focused(window), "{how}: focus returned");
        });
    }
}

#[gpui::test]
fn a_confirm_dialog_blocks_presses_behind_it(cx: &mut TestAppContext) {
    let (view, cx) = open_confirm(cx);
    // `behind` sits at the window's corner, under the scrim, so the press cancels.
    click(cx, "behind");
    view.read_with(cx, |c, _| assert_eq!(c.answers, vec!["cancel"]));
}

struct DraggedPane;

struct Splits {
    state: gpui::Entity<keystone_ui::SplitState>,
    drops: Vec<keystone_ui::PaneEdge>,
}

impl Render for Splits {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("tab")
                    .h_8()
                    .w_32()
                    .debug_selector(|| "tab".into())
                    .on_drag(DraggedPane, |_, _, _, cx| cx.new(|_| gpui::Empty)),
            )
            .child(
                div().flex_1().child(
                    Split::new("split", &self.state)
                        .on_drop_edge(move |_: &DraggedPane, target, _, cx| {
                            view.update(cx, |this, cx| {
                                this.drops.push(target);
                                cx.notify();
                            })
                        })
                        .children((0..3).map(|n| Label::new(format!("Pane {n}")))),
                ),
            )
    }
}

fn splits(cx: &mut TestAppContext) -> (gpui::Entity<Splits>, &mut gpui::VisualTestContext) {
    cx.update(keystone_ui::init);
    cx.add_window_view(|_, cx| Splits {
        state: cx.new(|_| SplitState::new(gpui::Axis::Horizontal, 3)),
        drops: Vec::new(),
    })
}

#[gpui::test]
fn split_divider_drags_neighbours_and_double_click_resets(cx: &mut TestAppContext) {
    let (view, cx) = splits(cx);
    let widths = |cx: &mut gpui::VisualTestContext| {
        (0..3)
            .map(|n| {
                let pane = format!("split-pane-{n}").leak();
                f32::from(cx.debug_bounds(pane).expect("pane").size.width).round()
            })
            .collect::<Vec<_>>()
    };
    let before = widths(cx);
    let divider = cx
        .debug_bounds("split-divider-0")
        .expect("divider")
        .center();
    drag(
        cx,
        divider,
        divider + gpui::point(gpui::px(100.), gpui::px(0.)),
    );
    let after = widths(cx);
    assert_eq!(after[0], before[0] + 100., "{before:?} -> {after:?}");
    assert_eq!(after[1], before[1] - 100., "{before:?} -> {after:?}");
    assert_eq!(after[2], before[2], "{before:?} -> {after:?}");

    let divider = cx
        .debug_bounds("split-divider-0")
        .expect("divider")
        .center();
    for _ in 0..2 {
        cx.simulate_event(gpui::MouseDownEvent {
            position: divider,
            modifiers: Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: divider,
            modifiers: Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 2,
        });
    }
    assert_eq!(widths(cx), before);
    view.read_with(cx, |s, _| assert!(s.drops.is_empty()));
}

#[gpui::test]
fn split_drop_edge_reports_edge(cx: &mut TestAppContext) {
    let (view, cx) = splits(cx);
    let tab = cx.debug_bounds("tab").expect("tab").center();
    let pane = cx.debug_bounds("split-pane-1").expect("pane");
    let right = gpui::point(pane.right() - gpui::px(10.), pane.center().y);
    drag(cx, tab, right);
    view.read_with(cx, |s, _| {
        assert_eq!(
            s.drops,
            vec![keystone_ui::PaneEdge {
                pane: 1,
                edge: keystone_ui::Edge::Right
            }]
        )
    });
    let pane = cx.debug_bounds("split-pane-0").expect("pane");
    drag(
        cx,
        tab,
        gpui::point(pane.center().x, pane.top() + gpui::px(10.)),
    );
    view.read_with(cx, |s, _| {
        assert_eq!(s.drops[1].pane, 0);
        assert_eq!(s.drops[1].edge, keystone_ui::Edge::Top);
    });
}

#[gpui::test]
fn split_drop_lands_after_a_single_move(cx: &mut TestAppContext) {
    use gpui::MouseButton::Left;
    let (view, cx) = splits(cx);
    let tab = cx.debug_bounds("tab").expect("tab").center();
    let pane = cx.debug_bounds("split-pane-2").expect("pane");
    let bottom = gpui::point(pane.center().x, pane.bottom() - gpui::px(10.));
    cx.simulate_mouse_down(tab, Left, Modifiers::none());
    cx.simulate_mouse_move(bottom, Left, Modifiers::none());
    cx.simulate_mouse_up(bottom, Left, Modifiers::none());
    view.read_with(cx, |s, _| {
        assert_eq!(
            s.drops,
            vec![keystone_ui::PaneEdge {
                pane: 2,
                edge: keystone_ui::Edge::Bottom
            }]
        )
    });
}

#[gpui::test]
fn split_drop_highlight_covers_half_the_pane_and_clears_when_the_drag_ends(
    cx: &mut TestAppContext,
) {
    use gpui::MouseButton::Left;
    let (_view, cx) = splits(cx);
    let tab = cx.debug_bounds("tab").expect("tab").center();
    let pane = cx.debug_bounds("split-pane-1").expect("pane");
    let right = gpui::point(pane.right() - gpui::px(10.), pane.center().y);
    cx.simulate_mouse_down(tab, Left, Modifiers::none());
    cx.simulate_mouse_move(pane.center(), Left, Modifiers::none());
    cx.simulate_mouse_move(right, Left, Modifiers::none());
    let half = cx.debug_bounds("split-drop-target").expect("highlight");
    assert_eq!(half.right(), pane.right());
    assert_eq!(half.size.width, pane.size.width / 2.);
    assert_eq!(half.size.height, pane.size.height);

    cx.update(|window, cx| cx.stop_active_drag(window));
    cx.simulate_mouse_up(right, Left, Modifiers::none());
    let divider = cx
        .debug_bounds("split-divider-0")
        .expect("divider")
        .center();
    cx.simulate_mouse_down(divider, Left, Modifiers::none());
    for step in 1..=3 {
        let at = divider + gpui::point(gpui::px(20. * step as f32), gpui::px(0.));
        cx.simulate_mouse_move(at, Left, Modifiers::none());
        assert!(
            cx.debug_bounds("split-drop-target").is_none(),
            "step {step}"
        );
    }
    cx.simulate_mouse_up(divider, Left, Modifiers::none());
}

#[gpui::test]
#[ignore = "benchmark; run with --release -- --ignored --nocapture"]
fn perf_split_drag(cx: &mut TestAppContext) {
    let (view, cx) = splits(cx);
    // Release builds have no debug selectors. Divider 0 sits a third of the way across.
    let viewport = cx.update(|window, _| window.viewport_size());
    let divider = gpui::point(viewport.width / 3., viewport.height / 2.);
    cx.simulate_mouse_down(divider, gpui::MouseButton::Left, Modifiers::none());
    // Each move dispatches the drag and then draws the dirtied window, as a frame does.
    let mut frames = Vec::new();
    for step in 0..400 {
        let offset = gpui::px(((step % 100) as f32 - 50.) * 4.);
        let at = divider + gpui::point(offset, gpui::px(0.));
        let start = std::time::Instant::now();
        cx.simulate_mouse_move(at, gpui::MouseButton::Left, Modifiers::none());
        frames.push(start.elapsed().as_secs_f64() * 1000.);
    }
    let moved = view.read_with(cx, |s, cx| s.state.read(cx).fractions()[0]);
    assert!(
        (moved - 1. / 3.).abs() > 0.1,
        "the drag moved divider 0: {moved}"
    );
    cx.simulate_mouse_up(divider, gpui::MouseButton::Left, Modifiers::none());
    let start = std::time::Instant::now();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    eprintln!(
        "PERF split_full_draw_ms={:.3}",
        start.elapsed().as_secs_f64() * 1000.
    );
    let frames = &mut frames[20..];
    frames.sort_by(f64::total_cmp);
    let at = |q: f64| frames[((frames.len() - 1) as f64 * q).round() as usize];
    eprintln!(
        "PERF split_drag_p50_ms={:.3} split_drag_p95_ms={:.3} split_drag_max_ms={:.3}",
        at(0.5),
        at(0.95),
        at(1.)
    );
}

struct Tabs {
    order: Vec<usize>,
    selected: usize,
    selects: usize,
}

impl Tabs {
    fn new(count: usize) -> Self {
        Self {
            order: (0..count).collect(),
            selected: 0,
            selects: 0,
        }
    }
}

impl Render for Tabs {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self
            .order
            .iter()
            .map(|&n| Tab::new(("tab", n), format!("Tab {}", n + 1)));
        div()
            .size_full()
            .child(
                TabBar::new("bar")
                    .children(tabs)
                    .selected(self.selected)
                    .on_select(cx.listener(|this, &ix: &usize, _, cx| {
                        this.selected = ix;
                        this.selects += 1;
                        cx.notify();
                    }))
                    .on_close(cx.listener(|this, &ix: &usize, _, cx| {
                        this.order.remove(ix);
                        cx.notify();
                    }))
                    .on_move(cx.listener(|this, &(from, to): &(usize, usize), _, cx| {
                        let tab = this.order.remove(from);
                        this.order.insert(to, tab);
                        cx.notify();
                    })),
            )
            .child(div().h_8().debug_selector(|| "below".into()))
    }
}

fn tabs(
    cx: &mut TestAppContext,
    count: usize,
) -> (gpui::Entity<Tabs>, &mut gpui::VisualTestContext) {
    cx.update(keystone_ui::init);
    cx.add_window_view(|_, _| Tabs::new(count))
}

fn order(view: &gpui::Entity<Tabs>, cx: &mut gpui::VisualTestContext) -> Vec<usize> {
    view.read_with(cx, |t, _| t.order.clone())
}

/// Presses on `from`, moves past the drag threshold in steps, and releases on `to`.
fn drag(
    cx: &mut gpui::VisualTestContext,
    from: gpui::Point<gpui::Pixels>,
    to: gpui::Point<gpui::Pixels>,
) {
    use gpui::MouseButton::Left;
    cx.simulate_mouse_down(from, Left, Modifiers::none());
    for step in 1..=4 {
        let t = step as f32 / 4.;
        cx.simulate_mouse_move(from + (to - from) * t, Left, Modifiers::none());
    }
    cx.simulate_mouse_up(to, Left, Modifiers::none());
    cx.run_until_parked();
}

fn center(cx: &mut gpui::VisualTestContext, selector: &'static str) -> gpui::Point<gpui::Pixels> {
    cx.debug_bounds(selector).expect(selector).center()
}

#[gpui::test]
fn tab_click_selects(cx: &mut TestAppContext) {
    let (view, cx) = tabs(cx, 8);
    click(cx, "bar-tab-2");
    view.read_with(cx, |t, _| assert_eq!(t.selected, 2));
    assert_eq!(order(&view, cx), (0..8).collect::<Vec<_>>());
}

#[gpui::test]
fn tab_close_button_closes(cx: &mut TestAppContext) {
    let (view, cx) = tabs(cx, 8);
    // Tab 2 is not selected, so its close button shows on hover.
    let tab = center(cx, "bar-tab-1");
    cx.simulate_mouse_move(tab, None, Modifiers::none());
    click(cx, "bar-tab-1-close");
    assert_eq!(order(&view, cx), vec![0, 2, 3, 4, 5, 6, 7]);
    view.read_with(cx, |t, _| {
        assert_eq!(t.selected, 0, "closing did not select")
    });
}

#[gpui::test]
fn tab_middle_click_closes(cx: &mut TestAppContext) {
    let (view, cx) = tabs(cx, 8);
    let tab = center(cx, "bar-tab-4");
    cx.simulate_mouse_down(tab, gpui::MouseButton::Middle, Modifiers::none());
    cx.simulate_mouse_up(tab, gpui::MouseButton::Middle, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(order(&view, cx), vec![0, 1, 2, 3, 5, 6, 7]);
    view.read_with(cx, |t, _| {
        assert_eq!(t.selected, 0, "middle click did not select")
    });
}

#[gpui::test]
fn tab_drag_reorders(cx: &mut TestAppContext) {
    let (view, cx) = tabs(cx, 8);
    let selects = |cx: &mut gpui::VisualTestContext| view.read_with(cx, |t, _| t.selects);
    let (first, fourth) = (center(cx, "bar-tab-0"), center(cx, "bar-tab-3"));
    drag(cx, first, fourth);
    assert_eq!(
        order(&view, cx),
        vec![1, 2, 3, 0, 4, 5, 6, 7],
        "tab 1 after tab 4"
    );
    assert_eq!(selects(cx), 0, "a drag is not a click");

    let (last, first) = (center(cx, "bar-tab-7"), center(cx, "bar-tab-0"));
    drag(cx, last, first);
    assert_eq!(
        order(&view, cx),
        vec![7, 1, 2, 3, 0, 4, 5, 6],
        "last tab to the front"
    );

    let bar = cx.debug_bounds("bar").expect("bar");
    let end = gpui::point(bar.right() - gpui::px(12.), bar.center().y);
    let first = center(cx, "bar-tab-0");
    drag(cx, first, end);
    assert_eq!(
        order(&view, cx),
        vec![1, 2, 3, 0, 4, 5, 6, 7],
        "empty end moves it last"
    );

    let (first, below) = (center(cx, "bar-tab-0"), center(cx, "below"));
    drag(cx, first, below);
    assert_eq!(
        order(&view, cx),
        vec![1, 2, 3, 0, 4, 5, 6, 7],
        "a drop outside changes nothing"
    );
    assert_eq!(selects(cx), 0, "no drag selected a tab");
}

/// The draw time of a 50-tab bar while a tab is dragged across it. Run with
/// `cargo test -p keystone-ui --features ui-tests --release perf_tab_bar_drag_50 -- --ignored --nocapture`.
#[gpui::test]
#[ignore]
fn perf_tab_bar_drag_50(cx: &mut TestAppContext) {
    use gpui::MouseButton::Left;
    cx.executor().allow_parking();
    let (view, cx) = tabs(cx, 50);
    // Release builds drop debug selectors, so this presses by position: the bar
    // runs along the window's top edge, and its first tab starts near the left.
    let width = cx.update(|window, _| window.viewport_size().width);
    let start = gpui::point(gpui::px(30.), gpui::px(17.));
    cx.simulate_mouse_down(start, Left, Modifiers::none());
    let mut frames = Vec::new();
    for step in 0..200 {
        let x = (width - gpui::px(1.)) * ((step % 100) as f32 / 100.);
        cx.simulate_mouse_move(gpui::point(x, start.y), Left, Modifiers::none());
        cx.update(|window, cx| {
            let started = std::time::Instant::now();
            window.draw(cx).clear(cx);
            frames.push(started.elapsed());
        });
    }
    cx.update(|_, cx| assert!(cx.has_active_drag(), "the drag stayed active"));
    cx.simulate_mouse_up(start, Left, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(order(&view, cx), (0..50).collect::<Vec<_>>());
    frames.sort();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.;
    let p95 = ms(frames[frames.len() * 95 / 100]);
    eprintln!(
        "PERF tab_bar_drag_50_ms p50={:.3} p95={p95:.3} max={:.3}",
        ms(frames[frames.len() / 2]),
        ms(frames[frames.len() - 1]),
    );
    assert!(p95 < 4., "drag frame p95 {p95:.3} ms is over 4 ms");
}

#[gpui::test]
fn tab_selected_on_open_is_scrolled_into_view(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let (_view, cx) = cx.add_window_view(|_, _| Tabs {
        selected: 49,
        ..Tabs::new(50)
    });
    cx.run_until_parked();
    let requested = cx.update(|window, cx| window.simulate_next_frame(cx));
    assert!(
        requested > 0,
        "the first frame asks for a second to reveal the tab"
    );
    cx.run_until_parked();
    let bar = cx.debug_bounds("bar").expect("bar");
    let last = cx.debug_bounds("bar-tab-49").expect("last tab");
    assert!(
        last.left() >= bar.left() && last.right() <= bar.right(),
        "the selected last tab {last:?} shows inside the bar {bar:?}"
    );
}

/// Two bars. Only `right` accepts tabs from another bar.
struct TwoBars {
    left: Vec<&'static str>,
    right: Vec<&'static str>,
}

impl Render for TwoBars {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = |names: &[&'static str]| {
            names
                .iter()
                .map(|&name| Tab::new(name, name))
                .collect::<Vec<_>>()
        };
        div()
            .size_full()
            .child(
                TabBar::new("left")
                    .children(tabs(&self.left))
                    .on_move(|_, _, _| {}),
            )
            .child(
                TabBar::new("right")
                    .children(tabs(&self.right))
                    .on_move(|_, _, _| {})
                    .on_insert(
                        cx.listener(|this, (drag, to): &(DraggedTab, usize), _, cx| {
                            assert_eq!(drag.bar_id, "left".into());
                            let tab = this.left.remove(drag.index);
                            this.right.insert(*to, tab);
                            cx.notify();
                        }),
                    ),
            )
    }
}

#[gpui::test]
fn tab_drop_on_another_bar_inserts(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let (view, cx) = cx.add_window_view(|_, _| TwoBars {
        left: vec!["a", "b", "c"],
        right: vec!["x", "y"],
    });
    let (b, y) = (center(cx, "left-tab-1"), center(cx, "right-tab-1"));
    drag(cx, b, y);
    view.read_with(cx, |t, _| {
        assert_eq!(t.left, vec!["a", "c"]);
        assert_eq!(
            t.right,
            vec!["x", "b", "y"],
            "inserted before the tab it was dropped on"
        );
    });

    let right = cx.debug_bounds("right").expect("right bar");
    let end = gpui::point(right.right() - gpui::px(12.), right.center().y);
    let a = center(cx, "left-tab-0");
    drag(cx, a, end);
    view.read_with(cx, |t, _| {
        assert_eq!(t.right, vec!["x", "b", "y", "a"], "the end appends")
    });

    let (x, c) = (center(cx, "right-tab-0"), center(cx, "left-tab-0"));
    drag(cx, x, c);
    view.read_with(cx, |t, _| {
        assert_eq!(
            t.left,
            vec!["c"],
            "a bar without on_insert ignores foreign tabs"
        );
        assert_eq!(t.right, vec!["x", "b", "y", "a"]);
    });
}

struct List {
    state: gpui::Entity<VirtualListState>,
    confirmed: Option<usize>,
}

impl Render for List {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            VirtualList::new("list", &self.state, |ix, _, _, _| {
                div()
                    .h(gpui::px(20.))
                    .child(format!("Row {ix}"))
                    .into_any_element()
            })
            .on_confirm(cx.listener(|this, ix: &usize, _, cx| {
                this.confirmed = Some(*ix);
                cx.notify();
            }))
            .h(gpui::px(200.)),
        )
    }
}

/// A focused 200 px list of 20 px rows, so a page is 10 rows.
fn open_list(
    cx: &mut TestAppContext,
    count: usize,
) -> (gpui::Entity<List>, &mut gpui::VisualTestContext) {
    cx.update(keystone_ui::init);
    let (view, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| VirtualListState::new(count, cx));
        window.focus(&gpui::Focusable::focus_handle(state.read(cx), cx), cx);
        List {
            state,
            confirmed: None,
        }
    });
    cx.run_until_parked();
    (view, cx)
}

fn highlighted(view: &gpui::Entity<List>, cx: &mut gpui::VisualTestContext) -> Option<usize> {
    view.read_with(cx, |list, cx| list.state.read(cx).highlighted())
}

#[gpui::test]
fn virtual_list_keys_move_highlight(cx: &mut TestAppContext) {
    let (view, cx) = open_list(cx, 50_000);
    assert_eq!(highlighted(&view, cx), Some(0));
    for (keys, expected) in [
        ("down down down", 3),
        ("up", 2),
        ("pagedown", 12),
        ("pagedown pagedown", 32),
        ("pageup", 22),
        ("end", 49_999),
        ("up", 49_998),
        ("home", 0),
    ] {
        cx.simulate_keystrokes(keys);
        assert_eq!(highlighted(&view, cx), Some(expected), "{keys}");
        assert!(
            row_bounds(cx, expected).is_some(),
            "{keys}: row {expected} on screen"
        );
    }
}

#[gpui::test]
fn virtual_list_clamps_at_ends(cx: &mut TestAppContext) {
    let (view, cx) = open_list(cx, 5);
    for (keys, expected) in [
        ("up", 0),
        ("pageup", 0),
        ("pagedown", 4),
        ("down", 4),
        ("end down", 4),
        ("home up", 0),
    ] {
        cx.simulate_keystrokes(keys);
        assert_eq!(highlighted(&view, cx), Some(expected), "{keys}");
    }
}

#[gpui::test]
fn virtual_list_enter_confirms(cx: &mut TestAppContext) {
    let (view, cx) = open_list(cx, 100);
    cx.simulate_keystrokes("down down enter");
    view.read_with(cx, |list, _| assert_eq!(list.confirmed, Some(2)));
    click_row(cx, 4);
    view.read_with(cx, |list, _| assert_eq!(list.confirmed, Some(4)));
    assert_eq!(highlighted(&view, cx), Some(4));
}

fn row_bounds(cx: &mut gpui::VisualTestContext, ix: usize) -> Option<gpui::Bounds<gpui::Pixels>> {
    cx.debug_bounds(format!("list-row-{ix}").leak())
}

fn click_row(cx: &mut gpui::VisualTestContext, ix: usize) {
    let bounds = row_bounds(cx, ix).expect("row");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn virtual_list_set_count_keeps_highlight_in_range(cx: &mut TestAppContext) {
    let (view, cx) = open_list(cx, 50);
    let set_count = |count, cx: &mut gpui::VisualTestContext| {
        view.update(cx, |list, cx| {
            list.state.update(cx, |state, cx| {
                state.set_count(count);
                cx.notify();
            })
        });
        cx.run_until_parked();
    };
    cx.simulate_keystrokes("end");
    set_count(10, cx);
    assert_eq!(highlighted(&view, cx), Some(9));
    assert!(row_bounds(cx, 9).is_some());
    set_count(0, cx);
    assert_eq!(highlighted(&view, cx), None);
    cx.simulate_keystrokes("down end enter");
    assert_eq!(highlighted(&view, cx), None);
    view.read_with(cx, |list, _| assert_eq!(list.confirmed, None));
    set_count(30, cx);
    assert_eq!(highlighted(&view, cx), Some(0));
    cx.simulate_keystrokes("end");
    assert_eq!(highlighted(&view, cx), Some(29));
    assert!(row_bounds(cx, 29).is_some());
}

#[gpui::test]
fn virtual_list_hover_moves_highlight(cx: &mut TestAppContext) {
    let (view, cx) = open_list(cx, 100);
    let row = row_bounds(cx, 3).expect("row 3");
    cx.simulate_mouse_move(row.center(), None, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(highlighted(&view, cx), Some(3));
    cx.simulate_keystrokes("down");
    assert_eq!(highlighted(&view, cx), Some(4));
}

struct CachedHost {
    list: gpui::Entity<List>,
}

impl Render for CachedHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            gpui::AnyView::from(self.list.clone())
                .cached(gpui::StyleRefinement::default().size_full()),
        )
    }
}

#[gpui::test]
fn virtual_list_redraws_inside_a_cached_view(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let (host, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| VirtualListState::new(50_000, cx));
        window.focus(&gpui::Focusable::focus_handle(state.read(cx), cx), cx);
        CachedHost {
            list: cx.new(|_| List {
                state,
                confirmed: None,
            }),
        }
    });
    cx.run_until_parked();
    // The first key switches the window to keyboard modality, which redraws everything.
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("end");
    let view = host.read_with(cx, |host, _| host.list.clone());
    assert_eq!(highlighted(&view, cx), Some(49_999));
    assert!(row_bounds(cx, 49_999).is_some(), "row 49999 on screen");
}

struct Palette {
    input: gpui::FocusHandle,
    state: gpui::Entity<VirtualListState>,
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(div().h_8().track_focus(&self.input))
            .child(
                VirtualList::new("list", &self.state, |ix, _, _, _| {
                    div()
                        .h(gpui::px(20.))
                        .child(format!("Row {ix}"))
                        .into_any_element()
                })
                .focusable(false)
                .h(gpui::px(200.)),
            )
    }
}

#[gpui::test]
fn virtual_list_can_leave_focus_with_its_host(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let (view, cx) = cx.add_window_view(|window, cx| {
        let input = cx.focus_handle();
        window.focus(&input, cx);
        Palette {
            input,
            state: cx.new(|cx| VirtualListState::new(3, cx)),
        }
    });
    cx.run_until_parked();
    let below_rows =
        row_bounds(cx, 2).expect("row 2").bottom_left() + gpui::point(gpui::px(5.), gpui::px(40.));
    cx.simulate_click(below_rows, Modifiers::none());
    cx.run_until_parked();
    let input = view.read_with(cx, |palette, _| palette.input.clone());
    cx.update(|window, _| assert!(input.is_focused(window), "the host keeps focus"));
}

fn focused_input(
    mode: InputMode,
    cx: &mut TestAppContext,
) -> (gpui::Entity<TextInput>, &mut gpui::VisualTestContext) {
    cx.update(TextInput::bind_keys);
    cx.update(keystone_ui::init);
    let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Search", mode, cx));
    input.update_in(cx, |input, window, cx| {
        window.focus(&gpui::Focusable::focus_handle(input, cx), cx)
    });
    (input, cx)
}

fn count<E: 'static>(
    input: &gpui::Entity<TextInput>,
    cx: &mut gpui::VisualTestContext,
) -> std::rc::Rc<std::cell::Cell<usize>>
where
    TextInput: gpui::EventEmitter<E>,
{
    let seen = std::rc::Rc::new(std::cell::Cell::new(0));
    let counter = seen.clone();
    cx.update(|_, cx| {
        cx.subscribe(input, move |_, _: &E, _| counter.set(counter.get() + 1))
            .detach()
    });
    seen
}

#[gpui::test]
fn single_line_input_rejects_newline(cx: &mut TestAppContext) {
    let (input, cx) = focused_input(InputMode::SingleLine, cx);
    let rejected = count::<keystone_ui::text_input::RejectedMultiline>(&input, cx);
    cx.simulate_input("query");
    cx.simulate_keystrokes("enter");
    cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("one\ntwo".into())));
    cx.simulate_keystrokes("ctrl-v");
    input.read_with(cx, |input, _| assert_eq!(input.value(), "query"));
    assert!(rejected.get() > 0, "newline rejection is reported");
}

#[gpui::test]
fn single_line_input_reports_changed(cx: &mut TestAppContext) {
    let (input, cx) = focused_input(InputMode::SingleLine, cx);
    let changed = count::<keystone_ui::text_input::Changed>(&input, cx);
    cx.simulate_input("ab");
    let after_typing = changed.get();
    assert!(after_typing > 0, "typing reports a change");
    cx.simulate_keystrokes("backspace");
    input.read_with(cx, |input, _| assert_eq!(input.value(), "a"));
    assert!(changed.get() > after_typing, "deleting reports a change");
}

struct CapturingHost {
    input: gpui::Entity<TextInput>,
    captured: Vec<&'static str>,
}

impl Render for CapturingHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use keystone_ui::text_input::{Down, Newline, Up};
        div()
            .capture_action(cx.listener(|this, _: &Up, _, cx| {
                this.captured.push("up");
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Down, _, cx| {
                this.captured.push("down");
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Newline, _, cx| {
                this.captured.push("enter");
                cx.stop_propagation();
            }))
            .child(self.input.clone())
    }
}

#[gpui::test]
fn a_host_takes_navigation_keys_before_the_input(cx: &mut TestAppContext) {
    cx.update(TextInput::bind_keys);
    cx.update(keystone_ui::init);
    let (host, cx) = cx.add_window_view(|_, cx| CapturingHost {
        input: cx.new(|cx| TextInput::new("Filter", InputMode::Multiline, cx)),
        captured: Vec::new(),
    });
    let input = host.read_with(cx, |host, _| host.input.clone());
    input.update_in(cx, |input, window, cx| {
        window.focus(&gpui::Focusable::focus_handle(input, cx), cx)
    });
    cx.simulate_input("ab");
    cx.simulate_keystrokes("up down enter");
    host.read_with(cx, |host, _| {
        assert_eq!(host.captured, vec!["up", "down", "enter"])
    });
    input.read_with(cx, |input, _| assert_eq!(input.value(), "ab"));
}
