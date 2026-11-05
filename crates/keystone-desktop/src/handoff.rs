//! Handoff: a session's conversation carried into a new session on another provider,
//! account or model. The new session is linked to its source, which stays unchanged,
//! and starts with the handoff text in its composer for the user to review and send.

use crate::conversation::Row;
use crate::model_selector::{account_label, provider_name};
use crate::view_state::ViewKey;
use crate::{Keystone, arrangement, now_ms};
use gpui::{
    AnyElement, Context, Focusable, FontWeight, IntoElement, ParentElement, Styled, Window, div,
    prelude::*, px,
};
use keystone_agent::{ItemKind, ItemStatus, TurnStatus};
use keystone_core::{Place, Provider, SessionContext};
use keystone_ui::prelude::*;

gpui::actions!(handoff, [HandOff]);

/// How much of the source conversation a handoff carries, in estimated tokens.
pub(crate) const BUDGET_TOKENS: usize = 16_000;

/// The longest a tool call's one line gets, in characters.
const LINE_CHARS: usize = 200;

/// Estimated tokens in `text`: one per four bytes, rounded up.
fn tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// The session a handoff continues.
pub(crate) struct Source<'a> {
    pub id: u64,
    pub title: &'a str,
    pub provider: Provider,
    pub folder: &'a str,
}

/// What one row adds to a handoff: a message in full, or one line. Reasoning, crash
/// recovery reports and unfinished items add nothing.
fn entry(row: &Row, position: usize, provider: Provider) -> Option<(bool, String)> {
    let line = |text: String| Some((false, format!("- #{position} {}", one_line(&text))));
    match row {
        Row::Item(item) if item.status == ItemStatus::InProgress => None,
        Row::Item(item) => match &item.kind {
            ItemKind::UserMessage => Some((true, format!("User (#{position}):\n{}", item.text))),
            ItemKind::AgentMessage => Some((
                true,
                format!("{} (#{position}):\n{}", provider_name(provider), item.text),
            )),
            ItemKind::Reasoning => None,
            ItemKind::Command { command, exit_code } => {
                let outcome = match (item.status, exit_code) {
                    (ItemStatus::Completed, Some(code)) => format!("exit {code}"),
                    (status, _) => status_word(status).to_owned(),
                };
                line(format!("Ran `{}` · {outcome}", one_line(command)))
            }
            ItemKind::FileChange { paths } => {
                let mut shown = paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
                if paths.len() > 5 {
                    shown.push_str(&format!(" and {} more", paths.len() - 5));
                }
                line(format!("Changed {shown} · {}", status_word(item.status)))
            }
            ItemKind::Tool { name } => line(format!("Used {name} · {}", status_word(item.status))),
            ItemKind::Other { name } => line(format!("{name} · {}", status_word(item.status))),
        },
        Row::TurnEnded { status, error, .. } => {
            let word = match status {
                TurnStatus::Completed => "completed",
                TurnStatus::Interrupted => "interrupted",
                TurnStatus::Failed => "failed",
            };
            line(match error {
                Some(error) => format!("Turn {word}: {error}"),
                None => format!("Turn {word}"),
            })
        }
        Row::Decision { action, allowed } => line(format!(
            "The user {} `{}`",
            if *allowed { "allowed" } else { "denied" },
            one_line(action)
        )),
        Row::Surface(surface) => line(match &surface.last_action {
            Some(action) => format!(
                "Showed widget `{}` · the user pressed {}",
                surface.surface_id,
                one_line(action)
            ),
            None => format!("Showed widget `{}`", surface.surface_id),
        }),
        Row::Attachments(attachments) => line(format!(
            "The user attached {}",
            attachments
                .iter()
                .map(|a| format!("`{}`", one_line(&a.name)))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Row::Recovery(_) | Row::OlderNotImported | Row::Note(_) => None,
    }
}

fn status_word(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::InProgress => "running",
        ItemStatus::Completed => "done",
        ItemStatus::Failed => "failed",
        ItemStatus::Declined => "declined",
        ItemStatus::Interrupted => "interrupted",
    }
}

/// `text` on one line, cut to [`LINE_CHARS`] characters.
fn one_line(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(LINE_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// `text` cut in the middle to at most `budget` estimated tokens, keeping both ends.
fn shorten(text: &str, budget: usize) -> String {
    const MARKER: usize = "\n… 0000000000 bytes left out here …\n".len();
    let keep = budget.saturating_mul(4).saturating_sub(MARKER) / 2;
    let mut head = keep.min(text.len());
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len().saturating_sub(keep).max(head);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n… {} bytes left out here …\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The most the closing budget note takes, in estimated tokens.
const NOTE_TOKENS: usize = 128;

/// The least room worth filling with a shortened message rather than leaving it out.
const SHORTEN_MIN_TOKENS: usize = 256;

/// The handoff text for `rows`, the source's conversation as `session_read` numbers
/// it, in at most `budget` estimated tokens. After the opening line, entries are kept
/// newest first while they fit; the first that does not, and everything older, is left
/// out and named as one range of positions. The message that first does not fit is
/// instead kept shortened in the middle to fill the room left, when that room is worth
/// it or nothing is kept yet. The kept entries read oldest first, and the
/// budget note comes last, beside the cursor, where the composer shows it first. The
/// title is cut to one line, so the text stays within any budget that holds the
/// opening line and the note.
pub(crate) fn build(source: Source, rows: &[Row], budget: usize) -> String {
    build_as("Continuing from", source, rows, budget, "")
}

/// [`build`], with the opening line starting with `lead` rather than "Continuing from",
/// and `after` following the text within the budget, beside the cursor.
pub(crate) fn build_as(
    lead: &str,
    source: Source,
    rows: &[Row],
    budget: usize,
    after: &str,
) -> String {
    let Source {
        id,
        title,
        provider,
        folder,
    } = source;
    let mut text = format!(
        "{lead} Keystone session {id}, \u{201c}{}\u{201d} ({}), in {folder}. \
         That session is unchanged. Its conversation follows, oldest first: messages in full \
         and one line per tool call, without reasoning or tool output.",
        one_line(title),
        provider_name(provider),
    );
    let room = budget.saturating_sub(tokens(&text) + NOTE_TOKENS + tokens(after));
    let mut kept = Vec::new();
    let mut used = 0;
    let mut full = false;
    let mut shortened = None;
    let mut newest_omitted = None;
    let mut oldest_omitted = 0;
    let (mut messages_omitted, mut others_omitted) = (0, 0);
    for (position, (message, entry)) in rows
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(position, row)| Some((position, entry(row, position, provider)?)))
    {
        // Two bytes join each entry to the one before it.
        let cost = (entry.len() + 2).div_ceil(4);
        if !full && used + cost <= room {
            used += cost;
            kept.push((message, entry));
            continue;
        }
        let left = room.saturating_sub(used + 1);
        if !full && message && (kept.is_empty() || left >= SHORTEN_MIN_TOKENS) {
            kept.push((message, shorten(&entry, left)));
            shortened = Some(position);
        } else {
            newest_omitted.get_or_insert(position);
            oldest_omitted = position;
            if message {
                messages_omitted += 1;
            } else {
                others_omitted += 1;
            }
        }
        full = true;
    }
    kept.reverse();
    if kept.is_empty() {
        text.push_str("\n\nNo messages yet.");
    }
    let mut previous_message = true;
    for (message, entry) in &kept {
        text.push_str(if *message || previous_message {
            "\n\n"
        } else {
            "\n"
        });
        text.push_str(entry);
        previous_message = *message;
    }
    let mut note = String::new();
    if let Some(position) = shortened {
        note.push_str(&format!(" #{position} is shortened in the middle."));
    }
    if let Some(newest) = newest_omitted {
        let span = if newest == oldest_omitted {
            format!("#{newest}")
        } else {
            format!("#{oldest_omitted}–#{newest}")
        };
        note.push_str(&format!(
            " Left out for the budget: {span} ({}, {}, not counting reasoning). Keystone's session_read tool reads \
             them from session {id}; positions are its row numbers.",
            count(messages_omitted, "message"),
            count(others_omitted, "other item"),
        ));
    }
    let footer = |used: usize| {
        format!(
            "\n\nBudget: about {} of {} estimated tokens, newest kept first.{note}\n\n---\n\n",
            thousands(used),
            thousands(budget),
        )
    };
    let used = tokens(&text) + tokens(&footer(budget)) + tokens(after);
    text.push_str(&footer(used));
    text.push_str(after);
    text
}

/// The top bar's Hand off control.
pub(crate) fn button() -> impl IntoElement {
    IconButton::new("hand-off", IconName::Handoff)
        .debug_selector(|| "hand-off".into())
        .tooltip(Tooltip::with_key(
            "Hand off…",
            keystone_ui::shortcut_text("secondary-shift-h"),
        ))
        .on_click(|_, window, cx| window.dispatch_action(Box::new(HandOff), cx))
}

/// A session's provider, with its account when that provider has more than its default.
pub(crate) fn provider_label(
    session: &keystone_core::Session,
    accounts: &[keystone_core::Account],
) -> String {
    match account_label(session.provider(), session.account(), accounts) {
        Some(account) => format!("{} · {account}", provider_name(session.provider())),
        None => provider_name(session.provider()).to_owned(),
    }
}

/// What the user is told when the selected session cannot be carried into a new one:
/// it has sent nothing, has no folder, or its history is still loading or unreadable.
pub(crate) const UNSENT: &str = "Nothing sent yet.";
pub(crate) const NO_FOLDER: &str = "Choose a conversation folder.";
pub(crate) const LOADING: &str = "Still loading.";
pub(crate) const UNREADABLE: &str = "Could not read the history. Restart Keystone.";

/// The provider a handoff or second opinion goes to unless the user picks another.
pub(crate) fn other(provider: Provider) -> Provider {
    match provider {
        Provider::Claude => Provider::Codex,
        Provider::Codex => Provider::Claude,
    }
}

impl Keystone {
    /// Hands the selected session off: a new session in its folder, linked to it,
    /// with the handoff text as its draft and the model selector open on the other
    /// provider, so the user chooses where it continues before sending.
    pub(crate) fn hand_off(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected(cx) else {
            return;
        };
        let Some(provider) = self.provider(id, cx) else {
            return;
        };
        self.hand_off_to(id, other(provider), None, window, cx);
    }

    /// Hands session `id` off to `target`. With an `account`, the new session runs on
    /// it and the model menu stays closed, since the destination is chosen; without
    /// one it starts on `target`'s default account with the menu open.
    pub(crate) fn hand_off_to(
        &mut self,
        id: u64,
        target: Provider,
        account: Option<Option<keystone_core::AccountId>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, text)) = self.carry(id, cx, |source, rows| build(source, rows, BUDGET_TOKENS))
        else {
            return;
        };
        let window_id = self.window_id;
        let Some(new) = self.create_beside(id, false, cx, |state, show, cx| {
            state.hand_off(id, target, text, now_ms(), window_id, show, cx)
        }) else {
            return;
        };
        // The work goes on in the new session, so the source's scheduled continue must
        // not send as well.
        if self
            .state
            .read(cx)
            .session(id)
            .and_then(|s| s.continue_at())
            .is_some()
        {
            self.state
                .update(cx, |state, cx| state.schedule_continue(id, None, cx));
        }
        match account {
            Some(account) => {
                let result = self.state.update(cx, |state, cx| {
                    state.set_provider_account(new, target, account, cx)
                });
                if let Err(error) = result {
                    self.error = Some(format!(
                        "The new session stays on the default account. {error}"
                    ));
                }
            }
            None => self.model_menu = Some(new),
        }
        self.request_catalog(new, target, cx);
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Opens the model selector of `session`, a new unsent session on `provider`, and
    /// focuses its composer, so the user chooses where it runs and reviews the draft.
    pub(crate) fn open_model_menu(
        &mut self,
        session: u64,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model_menu = Some(session);
        self.request_catalog(session, provider, cx);
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Session `id`'s provider, with the text `text` makes of its conversation. `None`,
    /// with the matching refusal shown, while the session has sent nothing, has no
    /// folder, or its history is still loading or could not be read.
    pub(crate) fn carry(
        &mut self,
        id: u64,
        cx: &mut Context<Self>,
        text: impl FnOnce(Source, &[Row]) -> String,
    ) -> Option<(Provider, String)> {
        let session = self.session(id, cx)?;
        let provider = session.provider();
        let state = self.state.read(cx);
        let refusal = if !session.started() {
            UNSENT
        } else if let SessionContext::Recorded { directory, .. } = session.context() {
            match state.conversations().get(&id).filter(|c| !c.is_loading()) {
                None => LOADING,
                Some(conversation) if conversation.load_error.is_some() => UNREADABLE,
                Some(conversation) => {
                    let source = Source {
                        id,
                        title: session.title(),
                        provider,
                        folder: directory.as_str(),
                    };
                    return Some((provider, text(source, &conversation.rows)));
                }
            }
        } else {
            NO_FOLDER
        };
        self.error = Some(refusal.into());
        self.refresh(cx);
        None
    }

    /// Creates a session from `source`, the selected one, with `create`. It opens in a
    /// tab beside the source, as New tab does, so the source stays a click away: always
    /// when `always` is set, else only beside other views or tabs, and a lone view
    /// follows the selection. Returns the new session once it is selected.
    pub(crate) fn create_beside(
        &mut self,
        source: u64,
        always: bool,
        cx: &mut Context<Self>,
        create: impl FnOnce(
            &mut crate::app_state::AppState,
            Option<(
                &keystone_core::ProjectId,
                keystone_core::Composition,
                keystone_core::SessionTabs,
                keystone_core::ViewId,
            )>,
            &mut Context<crate::app_state::AppState>,
        ) -> Result<keystone_core::Applied<u64>, Box<dyn std::error::Error>>,
    ) -> Option<u64> {
        let shown = self.saved_composition(cx);
        let beside = always
            || !self.session_tabs(cx).is_empty()
            || arrangement::ordered_views(shown.root())
                .into_iter()
                .filter(|&v| shown.component(v) == Some(keystone_core::Component::Conversation))
                .nth(1)
                .is_some();
        let tab = match self
            .layout_for_change(cx)
            .filter(|(_, tabs, active)| beside && tabs.get(*active).is_some())
        {
            Some((c, tabs, active)) => {
                let (view, layout) = self.open_view(&c, Place::TabWith(active), cx)?;
                Some((layout, tabs, view))
            }
            None => None,
        };
        let opened = tab.as_ref().map(|(_, _, view)| *view);
        let project = self.presentation.project.as_ref().ok().cloned();
        let show = tab.and_then(|(layout, tabs, view)| {
            project
                .as_ref()
                .map(|project| (project, layout, tabs, view))
        });
        self.create_with(cx, |state, cx| create(state, show, cx));
        let new = self.selected(cx).filter(|selected| *selected != source)?;
        if let Some(view) = opened.filter(|&view| self.session_tabs(cx).get(view) == Some(new)) {
            self.presentation.current.customization.forget_undo();
            self.presentation.current.arrangement.seen = Some(Some(new));
            self.show_fitted(view, cx);
        }
        Some(new)
    }

    /// "Continued from" and "Continued in" rows for the session in `key`'s view, each
    /// opening the other session, "Forked from" and "Forked in" rows for forks, then
    /// its second opinion rows. `None` when it has no such links.
    pub(crate) fn handoff_links(&self, key: ViewKey, cx: &mut Context<Self>) -> Option<AnyElement> {
        let opinions = self.second_opinion_rows(key, cx);
        let workspace = self.state.read(cx).store().workspace();
        let sessions = workspace.sessions();
        let describe = |session: &keystone_core::Session| {
            (
                session.id(),
                session.title().to_owned(),
                provider_label(session, workspace.accounts()),
            )
        };
        let shown = sessions.iter().find(|s| s.id() == key.session)?;
        let source = |from: Option<u64>| {
            from.and_then(|from| sessions.iter().find(|s| s.id() == from))
                .map(describe)
        };
        let from = source(shown.handed_off_from())
            .map(|link| ("Continued from", "handoff-from".to_owned(), link))
            .into_iter()
            .chain(
                source(shown.forked_from())
                    .map(|link| ("Forked from", "fork-from".to_owned(), link)),
            );
        let to = sessions.iter().filter(|s| s.started()).filter_map(|s| {
            if s.handed_off_from() == Some(key.session) {
                let selector = format!("handoff-to-{}", s.id());
                Some(("Continued in", selector, describe(s)))
            } else if s.forked_from() == Some(key.session) {
                let selector = format!("fork-to-{}", s.id());
                Some(("Forked in", selector, describe(s)))
            } else {
                None
            }
        });
        let rows = from
            .chain(to)
            .map(|(lead, selector, (id, title, provider))| {
                self.link_row(key, lead, &selector, id, title, Some(&provider), cx)
            })
            .chain(opinions)
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .flex_none()
                .w_full()
                .items_center()
                .py_1()
                .hairline_b(cx)
                .child(
                    v_flex()
                        .w(px(crate::CONTENT_WIDTH))
                        .max_w_full()
                        .gap_1()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn link_row(
        &self,
        key: ViewKey,
        lead: &'static str,
        selector: &str,
        id: u64,
        title: String,
        status: Option<&str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let hover = cx.theme().colors.element_hover;
        let selector = arrangement::selector(selector, key.view);
        h_flex()
            .id(selector.clone())
            .automation_id(selector)
            .h(cx.theme().density.control(px(28.)))
            .px_2()
            .gap_2()
            .rounded(radius::CONTROL)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(Label::new(lead).size(TextSize::Ui).color(Color::Muted))
            .child(
                div().min_w_0().flex_shrink(1.).child(
                    Label::new(title)
                        .size(TextSize::Ui)
                        .weight(FontWeight::MEDIUM)
                        .truncate(),
                ),
            )
            .when_some(status, |row, status| {
                row.child(
                    Label::new(format!("· {status}"))
                        .size(TextSize::Ui)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .on_click(self.scoped_listener(cx, move |this, _, _, cx| this.select(id, cx)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_agent::Item;

    fn item(kind: ItemKind, text: &str) -> Row {
        Row::Item(Item {
            id: String::new(),
            kind,
            text: text.into(),
            status: ItemStatus::Completed,
        })
    }

    /// The estimated tokens a handoff text says it carries.
    fn used(text: &str) -> usize {
        let about = text.split_once("Budget: about ").unwrap().1;
        about
            .split(' ')
            .next()
            .unwrap()
            .replace(',', "")
            .parse()
            .unwrap()
    }

    fn source() -> Source<'static> {
        Source {
            id: 3,
            title: "Fix the login bug",
            provider: Provider::Claude,
            folder: "/work/app",
        }
    }

    #[test]
    fn a_short_conversation_is_carried_whole_with_one_line_per_tool_call() {
        let rows = vec![
            item(ItemKind::UserMessage, "Why does login fail?"),
            item(ItemKind::Reasoning, "secret thoughts"),
            item(
                ItemKind::Command {
                    command: "cargo test\n  --all".into(),
                    exit_code: Some(101),
                },
                "lots of output",
            ),
            item(
                ItemKind::FileChange {
                    paths: vec!["src/auth.rs".into()],
                },
                "",
            ),
            Row::Decision {
                action: "rm -rf target".into(),
                allowed: false,
            },
            item(ItemKind::AgentMessage, "The token **expires** early."),
            Row::TurnEnded {
                status: TurnStatus::Failed,
                error: Some("Rate limited".into()),
                limit: None,
            },
        ];
        let text = &build(source(), &rows, BUDGET_TOKENS);
        assert!(text.starts_with(
            "Continuing from Keystone session 3, \u{201c}Fix the login bug\u{201d} (Claude Code), in /work/app."
        ));
        assert!(text.ends_with(&format!(
            "\n\nBudget: about {} of 16,000 estimated tokens, newest kept first.\n\n---\n\n",
            used(text)
        )));
        assert!(used(text) > 0);
        let body = text.split_once("tool output.\n\n").unwrap().1;
        let body = body.split_once("\n\nBudget: about").unwrap().0;
        assert_eq!(
            body,
            "User (#0):\nWhy does login fail?\n\n\
             - #2 Ran `cargo test --all` · exit 101\n\
             - #3 Changed src/auth.rs · done\n\
             - #4 The user denied `rm -rf target`\n\n\
             Claude Code (#5):\nThe token **expires** early.\n\n\
             - #6 Turn failed: Rate limited"
        );
        assert!(!text.contains("secret") && !text.contains("lots of output"));
        assert!(!text.contains("Left out"));
    }

    #[test]
    fn a_widget_is_one_line_naming_what_the_user_pressed() {
        let mut surface = keystone_agent::surface::parse(
            "call-1",
            &keystone_agent::surface::examples::checklist(),
        )
        .unwrap();
        let id = surface.surface_id.clone();
        let rows = |surface: &keystone_agent::surface::Surface| vec![Row::Surface(surface.clone())];
        let text = build(source(), &rows(&surface), BUDGET_TOKENS);
        assert!(
            text.contains(&format!("\n\n- #0 Showed widget `{id}`\n\n")),
            "{text}"
        );
        surface.last_action = Some("submit".into());
        let text = build(source(), &rows(&surface), BUDGET_TOKENS);
        assert!(text.contains(&format!(
            "- #0 Showed widget `{id}` · the user pressed submit"
        )));
    }

    #[test]
    fn the_budget_keeps_the_newest_entries_and_lists_older_ones_by_position() {
        let long = "word ".repeat(2_000);
        let rows = vec![
            item(ItemKind::UserMessage, "first question"),
            item(
                ItemKind::Tool {
                    name: "Read".into(),
                },
                "",
            ),
            item(ItemKind::AgentMessage, &long),
            item(ItemKind::UserMessage, "second question"),
            item(ItemKind::AgentMessage, &long),
            item(ItemKind::UserMessage, "third question"),
            item(ItemKind::AgentMessage, &long),
        ];
        let text = &build(source(), &rows, 5_300);
        assert!(
            tokens(text) <= used(text) && used(text) <= 5_300,
            "{}",
            used(text)
        );
        assert!(text.contains(
            "Left out for the budget: #0–#2 (2 messages, 1 other item, not counting reasoning). Keystone's session_read \
             tool reads them from session 3"
        ));
        assert!(text.contains("User (#3):\nsecond question"));
        assert!(text.contains("Claude Code (#6):"));
        assert!(!text.contains("first question") && !text.contains("Used Read"));
        assert!(text.find("#3").unwrap() < text.find("(#6)").unwrap());
    }

    #[test]
    fn a_message_that_overflows_is_shortened_to_fill_the_room_left() {
        let paste = format!("PASTE-START {} PASTE-END", "log line\n".repeat(10_000));
        let rows = vec![
            item(ItemKind::UserMessage, "old question"),
            item(ItemKind::AgentMessage, "old answer"),
            item(ItemKind::UserMessage, &paste),
            item(ItemKind::AgentMessage, "short reply"),
        ];
        let text = &build(source(), &rows, BUDGET_TOKENS);
        assert!(
            used(text) <= BUDGET_TOKENS && used(text) > BUDGET_TOKENS - 300,
            "{}",
            used(text)
        );
        assert!(text.contains("PASTE-START") && text.contains("PASTE-END"));
        assert!(text.contains("short reply"));
        assert!(text.contains("#2 is shortened in the middle."));
        assert!(text.contains(
            "Left out for the budget: #0–#1 (2 messages, 0 other items, not counting reasoning)"
        ));
        assert!(!text.contains("old answer"));
    }

    #[test]
    fn a_newest_message_larger_than_the_budget_is_shortened_in_the_middle() {
        let huge = format!("START {} END", "é".repeat(50_000));
        let rows = vec![
            item(ItemKind::UserMessage, "question"),
            item(ItemKind::AgentMessage, &huge),
        ];
        let text = &build(source(), &rows, 1_000);
        assert!(
            tokens(text) <= used(text) && used(text) <= 1_000,
            "{}",
            used(text)
        );
        assert!(text.contains(
            "Left out for the budget: #0 (1 message, 0 other items, not counting reasoning)"
        ));
        assert!(text.contains("START") && text.contains("END"));
        assert!(text.contains("bytes left out here"));
        assert!(text.contains("#1 is shortened in the middle"));
    }

    #[test]
    fn a_huge_title_is_cut_to_one_line_and_the_text_stays_within_budget() {
        let title = "t".repeat(70_000);
        let rows = vec![item(ItemKind::UserMessage, &"word ".repeat(20_000))];
        let text = &build(
            Source {
                title: &title,
                ..source()
            },
            &rows,
            BUDGET_TOKENS,
        );
        assert!(
            tokens(text) <= used(text) && used(text) <= BUDGET_TOKENS,
            "{}",
            tokens(text)
        );
        assert!(text.contains(&format!("\u{201c}{}…\u{201d}", "t".repeat(LINE_CHARS))));
    }

    #[test]
    fn rows_left_out_are_one_range_even_with_reasoning_between_them() {
        let long = "word ".repeat(1_000);
        let mut rows = Vec::new();
        for _ in 0..40 {
            rows.push(item(ItemKind::UserMessage, "question"));
            rows.push(item(ItemKind::Reasoning, "thinking"));
            rows.push(item(ItemKind::AgentMessage, &long));
        }
        let text = &build(source(), &rows, BUDGET_TOKENS);
        assert!(tokens(text) <= BUDGET_TOKENS, "{}", tokens(text));
        let note = text.split_once("Left out for the budget: ").unwrap().1;
        let span = note.split_once(' ').unwrap().0;
        let newest = span.rsplit_once('#').unwrap().1.parse::<usize>().unwrap();
        assert!(span.starts_with("#0–#"), "{span}");
        assert!(text.contains(&format!("(#{})", newest + 3)), "{span}");
        assert!(!text.contains(&format!("(#{newest})")));
    }

    #[test]
    fn unfinished_items_and_an_empty_conversation_add_nothing() {
        let rows = vec![Row::Item(Item {
            id: String::new(),
            kind: ItemKind::AgentMessage,
            text: "still streaming".into(),
            status: ItemStatus::InProgress,
        })];
        let text = build(source(), &rows, BUDGET_TOKENS);
        assert!(
            tokens(&text) <= used(&text) && used(&text) < 200,
            "{}",
            used(&text)
        );
        assert!(text.contains("No messages yet."));
        assert!(!text.contains("streaming") && !text.contains("Left out"));
    }

    #[test]
    fn helpers_format_positions_and_numbers() {
        assert_eq!(thousands(16_000), "16,000");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(one_line(&"x".repeat(300)).chars().count(), LINE_CHARS + 1);
    }
}
