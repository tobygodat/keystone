use gpui::{Context, IntoElement, Render, Window, div, prelude::*, px};
use keystone_ui::prelude::*;

pub struct DiagramsPage {
    narrow: bool,
    closed: bool,
    stream_demo: bool,
    malformed: bool,
}

impl DiagramsPage {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            narrow: false,
            closed: true,
            stream_demo: false,
            malformed: false,
        }
    }
}

const FIXTURE: &str = include_str!("../../fixtures/diagrams.md");

impl Render for DiagramsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let source = if self.malformed {
            "```mermaid\nflowchart TD\nA[Unclosed\n```\n".to_owned()
        } else if self.stream_demo {
            format!(
                "```mermaid\nflowchart TD\n  A[Stream] --> B[Source until closed]\n{}",
                if self.closed { "```\n" } else { "" }
            )
        } else {
            FIXTURE.to_owned()
        };
        v_flex()
            .size_full()
            .min_h_0()
            .child(
                h_flex()
                    .gap_3()
                    .px_6()
                    .pb_3()
                    .child(
                        Button::new(
                            "diagram-width",
                            if self.narrow {
                                "Width: 320"
                            } else {
                                "Width: 640"
                            },
                        )
                        .debug_selector(|| "diagram-width".into())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.narrow = !this.narrow;
                            cx.notify();
                        })),
                    )
                    .child(
                        Button::new(
                            "diagram-stream",
                            if self.closed {
                                "Open streaming fence"
                            } else {
                                "Close fence"
                            },
                        )
                        .debug_selector(|| "diagram-stream".into())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.closed = !this.closed;
                            this.stream_demo = true;
                            this.malformed = false;
                            cx.notify();
                        })),
                    )
                    .child(
                        Button::new("diagram-fixtures", "All diagrams")
                            .debug_selector(|| "diagram-fixtures".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.stream_demo = false;
                                this.closed = true;
                                this.malformed = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("diagram-malformed", "Malformed")
                            .debug_selector(|| "diagram-malformed".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.malformed = !this.malformed;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("diagram-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_6()
                    .child(
                        div()
                            .w(px(if self.narrow { 320. } else { 640. }))
                            .max_w_full()
                            .child(Markdown::from_source("gallery-diagrams", &source)),
                    ),
            )
    }
}
