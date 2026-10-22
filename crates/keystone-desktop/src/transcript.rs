//! How one transcript row looks. The transcript list in `main.rs` lays rows out and
//! virtualizes them; this module only decides a row's content.

use crate::conversation::Row;
use crate::view_state::CopyTarget;
use gpui::{
    AnyElement, App, ClipboardItem, ElementId, FocusHandle, SharedString, Window, div, prelude::*,
    px,
};
use keystone_agent::recovery::{Limit, Outcome, RecoveryReport, TurnEvidence};
use keystone_agent::{Item, ItemKind, ItemStatus, TurnStatus};
use keystone_ui::markdown::Document;
use keystone_ui::prelude::*;
use std::sync::Arc;
use std::time::Duration;

pub struct ActivityControl {
    pub expanded: bool,
    pub selector: String,
    pub body_selector: String,
    pub on_toggle: Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>,
}

#[derive(Default)]
pub struct RowControls {
    pub action: Option<(CopyTarget, FocusHandle)>,
    pub selected: bool,
    pub activity: Option<ActivityControl>,
    pub selection_start: Option<Box<dyn Fn(&mut Window, &mut App) -> Option<FocusHandle>>>,
    /// What a widget row's inputs and Buttons do.
    pub widget: Option<crate::widget::WidgetControl>,
    /// For a launched session's delivered result, that session's title.
    pub result: Option<SharedString>,
}

pub fn activity_name(item: &Item) -> Option<String> {
    match &item.kind {
        ItemKind::Reasoning => Some("reasoning".into()),
        ItemKind::Command { .. } => Some("command output".into()),
        ItemKind::Tool { name } => Some(format!("tool {name}")),
        ItemKind::Other { name }
            if !item.id.starts_with(keystone_agent::log::LOSS_MARKER_PREFIX) =>
        {
            Some(format!("activity {name}"))
        }
        _ => None,
    }
}

fn activity_toggle(
    index: usize,
    name: String,
    action: &Option<(CopyTarget, FocusHandle)>,
    control: ActivityControl,
    compact: bool,
) -> AnyElement {
    let label = format!(
        "{} {name}",
        if control.expanded {
            "Collapse"
        } else {
            "Expand"
        }
    );
    if compact {
        return IconButton::new(
            ("activity-toggle", index),
            if control.expanded {
                IconName::Chevron
            } else {
                IconName::Right
            },
        )
        .size(ButtonSize::Compact)
        .reserve_focus_ring()
        .aria_label(label.clone())
        .aria_expanded(control.expanded)
        .tooltip(Tooltip::text(label))
        .debug_selector(move || control.selector)
        .when_some(
            action
                .as_ref()
                .filter(|(target, _)| *target == CopyTarget::ActivityToggle),
            |button, (_, focus)| button.track_focus(focus).reveal_on_focus(),
        )
        .on_click(move |event, window, cx| (control.on_toggle)(event, window, cx))
        .into_any_element();
    }
    h_flex()
        .items_start()
        .child(
            Button::new(("activity-toggle", index), label.clone())
                .size(ButtonSize::Compact)
                .reserve_focus_ring()
                .aria_label(label)
                .aria_expanded(control.expanded)
                .debug_selector(move || control.selector)
                .when_some(
                    action
                        .as_ref()
                        .filter(|(target, _)| *target == CopyTarget::ActivityToggle),
                    |button, (_, focus)| button.track_focus(focus).reveal_on_focus(),
                )
                .on_click(move |event, window, cx| (control.on_toggle)(event, window, cx)),
        )
        .into_any_element()
}

/// The content of a row that [`crate::conversation::Conversation::shows`] accepts. `index` is the
/// row's place in the conversation, and `reply` the parsed Markdown of a reply row.
pub fn row(
    row: &Row,
    index: usize,
    reply: Option<Arc<Document>>,
    controls: RowControls,
    cx: &App,
) -> AnyElement {
    let RowControls {
        action,
        selected,
        activity,
        selection_start,
        widget,
        result,
    } = controls;
    let colors = cx.theme().colors;
    match row {
        Row::Item(item) => match &item.kind {
            ItemKind::UserMessage
                if let Some((title, read)) =
                    result.zip(crate::app_state::delivery::parse(&item.text)) =>
            {
                result_row(index, &title, &read, reply, &item.text, &action, cx)
            }
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
                    Markdown::new(SharedString::from(format!("reply-{index}")), document)
                        .managed_focus()
                        .when_some(selection_start, |markdown, start| {
                            markdown.on_selection_start(start)
                        });
                let markdown = match &action {
                    Some((CopyTarget::Code(n), focus)) => markdown.copy_focus(*n, focus),
                    Some((CopyTarget::DiagramToggle(n), focus)) => {
                        markdown.diagram_toggle_focus(*n, focus)
                    }
                    Some((CopyTarget::TextSelection, focus)) => markdown.selection_focus(focus),
                    _ => markdown,
                };
                v_flex()
                    .gap_1()
                    .child(markdown)
                    .when(item.status == ItemStatus::Interrupted, |d| {
                        d.child(
                            h_flex().child(
                                badge("Interrupted", Color::Muted, cx)
                                    .automation_id(format!("interrupted-{index}")),
                            ),
                        )
                    })
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
            ItemKind::Reasoning => {
                let control = activity.expect("reasoning disclosure");
                let expanded = control.expanded;
                let body_selector = control.body_selector.clone();
                v_flex()
                    .gap_1()
                    .child(activity_toggle(
                        index,
                        "reasoning".into(),
                        &action,
                        control,
                        false,
                    ))
                    .when(expanded, |d| {
                        d.child(
                            div()
                                .automation_id(body_selector)
                                .text_size(TextSize::Ui.px())
                                .text_color(colors.text_muted)
                                .child(item.text.clone()),
                        )
                    })
                    .into_any_element()
            }
            ItemKind::Command { command, exit_code } => {
                let (status, color) = match (item.status, exit_code) {
                    (ItemStatus::InProgress, _) => ("Running".to_owned(), Color::Muted),
                    (ItemStatus::Declined, _) => ("Not run".to_owned(), Color::Muted),
                    (ItemStatus::Interrupted, _) => ("Interrupted".to_owned(), Color::Muted),
                    (ItemStatus::Failed, None) => ("Stopped".to_owned(), Color::Muted),
                    (_, Some(0)) => ("Exit 0".to_owned(), Color::Success),
                    (_, Some(code)) => (format!("Exit {code}"), Color::Danger),
                    (_, None) => (String::new(), Color::Muted),
                };
                let output = item.text.trim_end();
                let control = activity.expect("command disclosure");
                let expanded = control.expanded;
                let body_selector = control.body_selector.clone();
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
                            .font_mono(cx)
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
                            )
                            .child(activity_toggle(
                                index,
                                "command output".into(),
                                &action,
                                control,
                                true,
                            )),
                    )
                    .when(expanded && !output.is_empty(), |d| {
                        d.child(
                            div()
                                .automation_id(body_selector)
                                .px_3()
                                .py_2()
                                .font_mono(cx)
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
                .child(format!(
                    "Changed {}{}",
                    paths.join(", "),
                    outcome_note(item.status)
                ))
                .into_any_element(),
            ItemKind::Other { .. }
                if item.id.starts_with(keystone_agent::log::LOSS_MARKER_PREFIX) =>
            {
                report(format!("log-loss-notice-{index}"), loss_summary(item), cx)
            }
            ItemKind::Tool { .. } | ItemKind::Other { .. } => {
                let control = activity.expect("tool disclosure");
                let expanded = control.expanded;
                let body_selector = control.body_selector.clone();
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(activity_toggle(
                                index,
                                activity_name(item).unwrap(),
                                &action,
                                control,
                                false,
                            ))
                            .child(
                                div()
                                    .automation_id(format!("activity-status-{index}"))
                                    .child(
                                        Label::new(match item.status {
                                            ItemStatus::InProgress => "Running",
                                            ItemStatus::Completed => "Completed",
                                            ItemStatus::Failed => "Failed",
                                            ItemStatus::Declined => "Declined",
                                            ItemStatus::Interrupted => "Interrupted",
                                        })
                                        .size(TextSize::Sm)
                                        .color(Color::Muted),
                                    ),
                            ),
                    )
                    .when(expanded, |d| {
                        d.child(
                            div()
                                .automation_id(body_selector)
                                .text_size(TextSize::Ui.px())
                                .text_color(colors.text_muted)
                                .when(!item.text.is_empty(), |d| {
                                    d.child(div().child(item.text.clone()))
                                }),
                        )
                    })
                    .into_any_element()
            }
        },
        Row::TurnEnded {
            status,
            error,
            limit,
        } => {
            let (text, color) = match (status, limit) {
                (TurnStatus::Interrupted, _) => ("Stopped", Color::Muted),
                (_, Some(_)) => ("Usage limit reached", Color::Warning),
                _ => ("Failed", Color::Danger),
            };
            // Under the usage limit card, the provider's own words, which the card's
            // headline does not repeat.
            let text = match (limit, error) {
                (Some(_), Some(error)) => error.clone(),
                (_, Some(error)) => format!("{text}: {error}"),
                (_, None) => text.to_owned(),
            };
            Label::new(text)
                .size(TextSize::Ui)
                .color(color)
                .into_any_element()
        }
        Row::Recovery(recovery) => report(
            format!("recovery-report-{index}"),
            recovery_summary(recovery),
            cx,
        ),
        Row::Surface(surface) => {
            crate::widget::SurfaceView::new(surface.clone(), widget.expect("widget controls"))
                .into_any_element()
        }
        Row::Attachments(attachments) => crate::attachments::sent(index, attachments, cx),
        Row::Decision { action, allowed } => Label::new(format!(
            "{} {action}",
            if *allowed { "Allowed:" } else { "Denied:" }
        ))
        .size(TextSize::Ui)
        .color(Color::Muted)
        .into_any_element(),
    }
}

/// A launched session's result that started this turn: who it came from, how its turn
/// ended, and its reply. It came from an agent, so it sits with the agent's rows rather
/// than in a user bubble.
fn result_row(
    index: usize,
    title: &str,
    read: &crate::app_state::delivery::ResultMessage<'_>,
    reply: Option<Arc<Document>>,
    text: &str,
    action: &Option<(CopyTarget, FocusHandle)>,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors;
    let (status, color, detail) = match read.outcome {
        "It finished." => ("Finished", Color::Muted, None),
        "It was stopped." => ("Stopped", Color::Muted, None),
        outcome => (
            "Failed",
            Color::Danger,
            Some(
                outcome
                    .strip_prefix("It failed: ")
                    .unwrap_or(outcome)
                    .to_owned(),
            ),
        ),
    };
    v_flex()
        .automation_id(format!("result-{index}"))
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
                .text_size(TextSize::Ui.px())
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            Label::new("Result from")
                                .size(TextSize::Ui)
                                .color(Color::Muted),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(colors.text_strong)
                                .child(format!("\u{201c}{title}\u{201d}")),
                        ),
                )
                .child(badge(status, color, cx))
                .child(
                    CopyButton::new(("copy-result", index), "Copy result", text.to_owned())
                        .keyboard(
                            action
                                .as_ref()
                                .filter(|(n, _)| *n == CopyTarget::Row)
                                .map(|(_, f)| f),
                        ),
                ),
        )
        .child(
            v_flex()
                .px_3()
                .py_2()
                .gap_1()
                .when_some(detail, |d, detail| {
                    d.child(Label::new(detail).size(TextSize::Ui).color(Color::Danger))
                })
                .child(Markdown::new(
                    SharedString::from(format!("result-reply-{index}")),
                    reply.unwrap_or_else(|| Arc::new(Document::parse(read.reply))),
                )),
        )
        .into_any_element()
}

/// A report row: badges, then the headline, then a line per detail.
fn report(id: String, summary: RecoverySummary, cx: &App) -> AnyElement {
    v_flex()
        .automation_id(id)
        .gap_1()
        .child(
            h_flex()
                .items_start()
                .gap_2()
                .children(
                    summary
                        .badges
                        .iter()
                        .map(|(text, color)| badge(text, *color, cx)),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(summary.headline)
                            .size(TextSize::Ui)
                            .color(Color::Muted),
                    ),
                ),
        )
        .children(summary.details.into_iter().map(|line| {
            Label::new(line)
                .size(TextSize::Sm)
                .color(Color::Muted)
                .into_any_element()
        }))
        .into_any_element()
}

/// A small outlined label beside a row.
fn badge(text: &str, color: Color, cx: &App) -> gpui::Div {
    div()
        .flex_none()
        .px_1p5()
        .rounded(radius::CONTROL)
        .border_1()
        .border_color(cx.theme().colors.border)
        .child(Label::new(text.to_owned()).size(TextSize::Sm).color(color))
}

/// Said after a tool row whose outcome is not the plain one.
fn outcome_note(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::Interrupted => " · no recorded result",
        ItemStatus::Failed => " · failed",
        ItemStatus::Declined => " · not run",
        _ => "",
    }
}

/// How a recovery report reads: badges, one headline, and a line per limitation.
pub struct RecoverySummary {
    pub badges: Vec<(&'static str, Color)>,
    pub headline: String,
    pub details: Vec<String>,
}

fn plural(count: u64, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// How a log gap marker reads, as a partial recovery does: what is shown here is not
/// everything that happened.
pub fn loss_summary(marker: &Item) -> RecoverySummary {
    // A recovery has since added what the provider recorded there.
    if keystone_agent::log::lost_count(marker).is_none() {
        return RecoverySummary {
            badges: Vec::new(),
            headline: marker.text.clone(),
            details: Vec::new(),
        };
    }
    RecoverySummary {
        badges: vec![("Partial", Color::Warning)],
        headline: marker.text.clone(),
        details: vec![
            "Keystone showed them while it ran, but its log was full, so they were not saved."
                .into(),
        ],
    }
}

pub fn recovery_summary(report: &RecoveryReport) -> RecoverySummary {
    let mut badges = Vec::new();
    if report.turn != TurnEvidence::Unaffected {
        badges.push(("Interrupted", Color::Muted));
    }
    match report.outcome {
        Outcome::Recovered => {}
        Outcome::Partial => badges.push(("Partial", Color::Warning)),
        Outcome::Conflict => badges.push(("Conflict", Color::Warning)),
    }
    let mut recovered = Vec::new();
    if report.messages > 0 {
        recovered.push(plural(report.messages.into(), "message", "messages"));
    }
    if report.tools > 0 {
        recovered.push(plural(report.tools.into(), "tool call", "tool calls"));
    }
    let mut parts = Vec::new();
    if !recovered.is_empty() {
        parts.push(format!("Recovered {}", recovered.join(" and ")));
    }
    if report.extended > 0 {
        parts.push(format!(
            "completed {}",
            plural(report.extended.into(), "partial row", "partial rows")
        ));
    }
    let found = if parts.is_empty() {
        "Nothing more was recovered from the provider's saved conversation.".to_owned()
    } else {
        let mut text = parts.join("; ");
        text.push_str(" from the provider's saved conversation.");
        let mut chars = text.chars();
        chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect())
            .unwrap_or(text)
    };
    let headline = match report.turn {
        TurnEvidence::Recorded(TurnStatus::Completed) => {
            format!("Connection interrupted; the provider recorded this turn as complete. {found}")
        }
        TurnEvidence::Recorded(TurnStatus::Interrupted) => format!(
            "Connection interrupted; the provider recorded this turn as interrupted. {found}"
        ),
        TurnEvidence::Recorded(TurnStatus::Failed) => {
            format!("Connection interrupted; the provider recorded this turn as failed. {found}")
        }
        TurnEvidence::Unknown => {
            format!("Interrupted; this turn's final outcome is unknown. {found}")
        }
        TurnEvidence::Unaffected => found,
    };
    let mut details: Vec<String> = report
        .limits
        .iter()
        .map(|limit| match limit {
            Limit::TornTail => "The last saved record was cut off and was left out.".into(),
            Limit::Malformed { count } => format!(
                "{} could not be read.",
                plural(*count, "saved record", "saved records")
            ),
            Limit::Unsupported { count } => format!(
                "{} of kinds Keystone does not show were skipped.",
                plural(*count, "record", "records")
            ),
            Limit::Unstable => "The provider's saved conversation changed while it was read.".into(),
            Limit::Truncated => "Only part of a long history was checked.".into(),
            Limit::OlderNotRead => {
                "Only the newest part of the provider's history was read, and it does not reach back to what is shown here.".into()
            }
            Limit::Compacted => {
                "The provider compacted this conversation, so older parts are summaries.".into()
            }
            Limit::ItemsNotLoaded => {
                "The provider returned summaries instead of some items.".into()
            }
            Limit::UnknownOutcome { count } => format!(
                "{} no recorded result. Any approval that was waiting has expired.",
                if *count == 1 {
                    "1 tool call has".to_owned()
                } else {
                    format!("{count} tool calls have")
                }
            ),
            Limit::PromptNotRecorded => {
                "Your last message is not in the provider's record, so it may not have been delivered."
                    .into()
            }
            Limit::EarlierHistoryDiffers { count } => format!(
                "{} earlier in the provider's record {} not match this transcript and {} left out.",
                plural(*count, "item", "items"),
                if *count == 1 { "does" } else { "do" },
                if *count == 1 { "was" } else { "were" },
            ),
            Limit::Branched => {
                "The provider's record has more than one branch, so nothing was matched.".into()
            }
            Limit::IdentityMismatch => {
                "The provider's record names a different folder, so it was not used.".into()
            }
            Limit::ReusedIds => "The provider reuses item IDs, so nothing was matched.".into(),
            Limit::OrderDiffers => {
                "Matching items appear in a different order, so nothing was matched.".into()
            }
            Limit::NoCommonPoint => {
                "No part of the provider's record matches this transcript, so nothing was added."
                    .into()
            }
            Limit::ConflictingText { count } => format!(
                "{} the provider's copy; Keystone's copy was kept.",
                if *count == 1 {
                    "1 saved message differs from".to_owned()
                } else {
                    format!("{count} saved messages differ from")
                }
            ),
        })
        .collect();
    if report.turn == TurnEvidence::Unknown {
        details.push("Text that was streaming may not have been saved by the provider.".into());
    }
    RecoverySummary {
        badges,
        headline,
        details,
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
