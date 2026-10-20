//! How one transcript row looks. The transcript list in `main.rs` lays rows out and
//! virtualizes them; this module only decides a row's content.

use crate::conversation::Row;
use crate::view_state::CopyTarget;
use gpui::{
    AnyElement, App, ClipboardItem, ElementId, FocusHandle, SharedString, Window, div, prelude::*,
    px,
};
use keystone_agent::{ItemKind, ItemStatus, TurnStatus};
use keystone_ui::markdown::Document;
use keystone_ui::prelude::*;
use std::sync::Arc;
use std::time::Duration;

/// The content of a row that [`crate::conversation::visible`] accepts. `index` is the
/// row's place in the conversation, and `reply` the parsed Markdown of a reply row.
pub fn row(
    row: &Row,
    index: usize,
    reply: Option<Arc<Document>>,
    action: Option<(CopyTarget, FocusHandle)>,
    selected: bool,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors;
    let mono = cx.theme().fonts.mono.clone();
    match row {
        Row::Item(item) => match &item.kind {
            ItemKind::UserMessage => h_flex()
                .group("user-message")
                .justify_end()
                .items_start()
                .gap_1()
                .child(
                    div()
                        .when(!selected, |d| d.invisible())
                        .group_hover("user-message", |s| s.visible())
                        .child(
                            CopyButton::new(
                                ("copy-message", index),
                                "Copy message",
                                item.text.clone(),
                            )
                            .keyboard(
                                action
                                    .as_ref()
                                    .filter(|(n, _)| *n == CopyTarget::Row)
                                    .map(|(_, f)| f),
                            ),
                        ),
                )
                .child(
                    div()
                        .max_w(px(620.))
                        .px_3()
                        .py_2()
                        .rounded(radius::ROW)
                        .bg(colors.element_filled)
                        .text_color(colors.text_strong)
                        .child(item.text.clone()),
                )
                .into_any_element(),
            ItemKind::AgentMessage => {
                let document = reply.unwrap_or_else(|| Arc::new(Document::parse(&item.text)));
                let row_copy = item.status != ItemStatus::InProgress;
                let markdown =
                    Markdown::new(SharedString::from(format!("reply-{index}")), document);
                let markdown = match &action {
                    Some((CopyTarget::Code(n), focus)) => markdown.copy_focus(*n, focus),
                    _ => markdown,
                };
                v_flex()
                    .gap_1()
                    .child(markdown)
                    .child(
                        div()
                            .when(item.status == ItemStatus::InProgress, |d| d.invisible())
                            .child(
                                CopyButton::new(
                                    ("copy-reply", index),
                                    "Copy reply",
                                    item.text.clone(),
                                )
                                .keyboard(
                                    action
                                        .as_ref()
                                        .filter(|(n, _)| *n == CopyTarget::Row && row_copy)
                                        .map(|(_, f)| f),
                                ),
                            ),
                    )
                    .into_any_element()
            }
            ItemKind::Reasoning => div()
                .text_size(TextSize::Ui.px())
                .text_color(colors.text_muted)
                .child(item.text.clone())
                .into_any_element(),
            ItemKind::Command { command, exit_code } => {
                let (status, color) = match (item.status, exit_code) {
                    (ItemStatus::InProgress, _) => ("Running".to_owned(), Color::Muted),
                    (ItemStatus::Declined, _) => ("Not run".to_owned(), Color::Muted),
                    (ItemStatus::Failed, None) => ("Stopped".to_owned(), Color::Muted),
                    (_, Some(0)) => ("Exit 0".to_owned(), Color::Success),
                    (_, Some(code)) => (format!("Exit {code}"), Color::Danger),
                    (_, None) => (String::new(), Color::Muted),
                };
                let output = item.text.trim_end();
                let copy = if output.is_empty() {
                    CopyButton::new(("copy-command", index), "Copy command", command.clone())
                } else {
                    CopyButton::new(("copy-command", index), "Copy output", output.to_owned())
                };
                v_flex()
                    .surface(Elevation::Raised, cx)
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .min_h(px(38.))
                            .pl_3()
                            .pr_1()
                            .py_1()
                            .gap_2()
                            .hairline_b(cx)
                            .font_family(mono.clone())
                            .text_size(TextSize::Ui.px())
                            .child(div().flex_1().min_w_0().child(format!("$ {command}")))
                            .child(Label::new(status).size(TextSize::Sm).color(color))
                            .child(
                                copy.keyboard(
                                    action
                                        .as_ref()
                                        .filter(|(n, _)| *n == CopyTarget::Row)
                                        .map(|(_, f)| f),
                                ),
                            ),
                    )
                    .when(!output.is_empty(), |d| {
                        d.child(
                            div()
                                .px_3()
                                .py_2()
                                .font_family(mono)
                                .text_size(TextSize::Ui.px())
                                .text_color(colors.text_muted)
                                .child(output.to_owned()),
                        )
                    })
                    .into_any_element()
            }
            ItemKind::FileChange { paths } => div()
                .text_size(TextSize::Ui.px())
                .text_color(colors.text_muted)
                .child(format!("Changed {}", paths.join(", ")))
                .into_any_element(),
            ItemKind::Tool { name } | ItemKind::Other { name } => div()
                .text_size(TextSize::Ui.px())
                .text_color(colors.text_muted)
                .child(format!("Used {name}"))
                .into_any_element(),
        },
        Row::TurnEnded { status, error } => {
            let (text, color) = match status {
                TurnStatus::Interrupted => ("Stopped", Color::Muted),
                _ => ("Failed", Color::Danger),
            };
            let text = match error {
                Some(error) => format!("{text}: {error}"),
                None => text.to_owned(),
            };
            Label::new(text)
                .size(TextSize::Ui)
                .color(color)
                .into_any_element()
        }
        Row::Decision { action, allowed } => Label::new(format!(
            "{} {action}",
            if *allowed { "Allowed:" } else { "Denied:" }
        ))
        .size(TextSize::Ui)
        .color(Color::Muted)
        .into_any_element(),
    }
}

/// Copies `text`, then shows a check for a moment.
#[derive(IntoElement)]
struct CopyButton {
    id: ElementId,
    label: &'static str,
    text: String,
    focus: Option<FocusHandle>,
}

impl CopyButton {
    fn new(id: impl Into<ElementId>, label: &'static str, text: String) -> Self {
        Self {
            id: id.into(),
            label,
            text,
            focus: None,
        }
    }

    fn keyboard(mut self, focus: Option<&FocusHandle>) -> Self {
        self.focus = focus.cloned();
        self
    }
}

impl RenderOnce for CopyButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let copied = window.use_keyed_state((self.id.clone(), "copied"), cx, |_, _| false);
        let is_copied = *copied.read(cx);
        let selector = match &self.id {
            ElementId::NamedInteger(name, index) => format!("{name}-{index}"),
            id => id.to_string(),
        };
        let text = self.text;
        IconButton::new(
            self.id,
            if is_copied {
                IconName::Check
            } else {
                IconName::Copy
            },
        )
        .size(ButtonSize::Compact)
        .when_some(self.focus, |button, focus| {
            button.track_focus(&focus).reveal_on_focus()
        })
        .debug_selector(move || selector)
        .tooltip(Tooltip::text(if is_copied { "Copied" } else { self.label }))
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            copied.update(cx, |copied, cx| {
                *copied = true;
                cx.notify();
                cx.spawn(async move |copied, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(1500))
                        .await;
                    let _ = copied.update(cx, |copied, cx| {
                        *copied = false;
                        cx.notify();
                    });
                })
                .detach();
            });
        })
    }
}
