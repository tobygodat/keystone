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
