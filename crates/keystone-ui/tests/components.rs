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
    click(cx, "item");
    click(cx, "outside");
    assert!(!open(cx));
    // GPUI 0.2.2 keeps stale debug bounds, so this press lands where the closed item was.
    click(cx, "item");
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
