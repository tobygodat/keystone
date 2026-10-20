use gpui::{Context, FocusHandle, IntoElement, Render, TestAppContext, Window, div, prelude::*};
use keystone_ui::{Markdown, markdown::IncrementalParser};

fn press(cx: &mut gpui::VisualTestContext, key: &str) {
    cx.simulate_keystrokes(key);
    cx.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse(key).unwrap(),
    });
}

fn wait_for_selector(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        if cx.debug_bounds(selector).is_some() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Timed out waiting for {selector}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

struct Reply {
    parser: IncrementalParser,
    entry: FocusHandle,
}

impl Render for Reply {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .track_focus(&self.entry)
            .on_key_down(|event, window, cx| {
                if event.keystroke.key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                    cx.stop_propagation();
                }
            })
            .child(Markdown::new("reply", self.parser.display()))
    }
}

#[gpui::test]
fn streaming_and_closed_diagrams_copy_exact_source_with_keyboard(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(keystone_ui::init);
    let source = "flowchart TD\r\n  A[é] --> B  \r\n";
    let open = format!("```mermaid\r\n{source}");
    let (view, cx) = cx.add_window_view(|window, cx| {
        let entry = cx.focus_handle();
        window.focus(&entry, cx);
        let mut parser = IncrementalParser::new();
        parser.set_text(&open);
        Reply { parser, entry }
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("reply-diagram-1-source").is_some());
    assert!(cx.debug_bounds("reply-diagram-1-toggle").is_none());
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    press(cx, "enter");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
            .as_deref(),
        Some(source)
    );
    view.update(cx, |this, cx| {
        this.parser.append("```\r\n");
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("reply-diagram-1-toggle").is_some());
    cx.simulate_keystrokes("shift-tab");
    cx.run_until_parked();
    press(cx, "space");
    cx.run_until_parked();
    assert!(cx.debug_bounds("reply-diagram-1-source").is_some());
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    press(cx, "enter");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
            .as_deref(),
        Some(source)
    );
}

#[gpui::test]
fn two_diagrams_have_independent_source_and_copy_controls(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(keystone_ui::init);
    let (_view, cx) = cx.add_window_view(|_, cx| {
        let mut parser = IncrementalParser::new();
        parser
            .set_text("```mermaid\nflowchart TD\nA-->B\n```\n\n```mermaid\npie\n\"A\" : 1\n```\n");
        Reply {
            parser,
            entry: cx.focus_handle(),
        }
    });
    cx.run_until_parked();
    for (selector, expected) in [
        ("reply-diagram-1-copy", "flowchart TD\nA-->B\n"),
        ("reply-diagram-2-copy", "pie\n\"A\" : 1\n"),
    ] {
        let bounds = cx.debug_bounds(selector).expect("copy button");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .as_deref(),
            Some(expected)
        );
    }
}

#[gpui::test]
fn a_failed_diagram_can_be_explicitly_retried_without_a_render_loop(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(keystone_ui::init);
    let (_view, cx) = cx.add_window_view(|_, cx| {
        let mut parser = IncrementalParser::new();
        parser.set_text("```mermaid\nflowchart TD\nA[Unclosed\n```\n");
        Reply {
            parser,
            entry: cx.focus_handle(),
        }
    });
    wait_for_selector(cx, "reply-diagram-1-error");
    let bounds = cx.debug_bounds("reply-diagram-1-toggle").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("reply-diagram-1-error").is_none());
    assert!(cx.debug_bounds("reply-diagram-1-source").is_some());
    let bounds = cx.debug_bounds("reply-diagram-1-toggle").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    wait_for_selector(cx, "reply-diagram-1-error");
}
