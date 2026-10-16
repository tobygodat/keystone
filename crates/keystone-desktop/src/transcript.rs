//! How one transcript row looks. The transcript list in `main.rs` lays rows out and
//! virtualizes them; this module only decides a row's content.

use crate::conversation::Row;
use gpui::{AnyElement, App, ClipboardItem, SharedString, div, prelude::*, px};
use keystone_agent::{ItemKind, ItemStatus, TurnStatus};
use keystone_ui::markdown::Document;
use keystone_ui::prelude::*;
use std::sync::Arc;

/// The content of a row that [`crate::conversation::visible`] accepts. `index` is the
/// row's place in the conversation, and `reply` the parsed Markdown of a reply row.
pub fn row(row: &Row, index: usize, reply: Option<Arc<Document>>, cx: &App) -> AnyElement {
    let colors = cx.theme().colors;
    let mono = cx.theme().fonts.mono.clone();
    match row {
        Row::Item(item) => match &item.kind {
            ItemKind::UserMessage => v_flex()
                .items_end()
                .child(
                    div()
                        .max_w(px(620.))
                        .px_3()
                        .py_2()
                        .rounded(radius::ROW)
                        .bg(colors.element_selected)
                        .child(item.text.clone()),
                )
                .into_any_element(),
            ItemKind::AgentMessage => {
                let text = item.text.clone();
                let document = reply.unwrap_or_else(|| Arc::new(Document::parse(&item.text)));
                v_flex()
                    .gap_1()
                    .child(Markdown::new(
                        SharedString::from(format!("reply-{index}")),
                        document,
                    ))
                    .when(item.status == ItemStatus::Completed, |d| {
                        d.child(
                            IconButton::new(("copy-reply", index), IconName::Copy)
                                .size(ButtonSize::Compact)
                                .debug_selector(move || format!("copy-reply-{index}"))
                                .tooltip(Tooltip::text("Copy reply"))
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                                }),
                        )
                    })
                    .into_any_element()
            }
            ItemKind::Reasoning => div()
                .text_size(TextSize::Ui.px())
                .text_color(colors.text_muted)
                .child(item.text.clone())
                .into_any_element(),
            ItemKind::Command { command, exit_code } => {
                let status = match (item.status, exit_code) {
                    (ItemStatus::InProgress, _) => "Running".to_owned(),
                    (ItemStatus::Declined, _) => "Not run".to_owned(),
                    (ItemStatus::Failed, None) => "Stopped".to_owned(),
                    (_, Some(code)) => format!("Exit {code}"),
                    (_, None) => String::new(),
                };
                let output = item.text.trim_end();
                v_flex()
                    .surface(Elevation::Raised, cx)
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .px_3()
                            .py_2()
                            .gap_2()
                            .hairline_b(cx)
                            .font_family(mono.clone())
                            .text_size(TextSize::Ui.px())
                            .child(div().flex_1().min_w_0().child(format!("$ {command}")))
                            .child(Label::new(status).size(TextSize::Sm).color(Color::Muted)),
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
