//! Second opinions: the selected session's conversation, or the part of it up to one
//! reply, put to another provider or account in a new session beside it. That session
//! carries the conversation as a handoff does, with a question the user can edit, and
//! starts read-only. The source is unchanged. Its view lists each second opinion as it
//! runs and finishes, and Use in conversation puts the finished reply in the source's
//! composer for the user to send. A second opinion never starts a turn in its source.

use crate::conversation::{Conversation, Recovery, Row};
use crate::handoff::{self, BUDGET_TOKENS, Source, provider_label};
use crate::model_selector::provider_name;
use crate::view_state::ViewKey;
use crate::{Keystone, arrangement, now_ms};
use gpui::{
    AnyElement, Context, Focusable, IntoElement, ParentElement, Styled, Window, div, prelude::*,
};
use keystone_agent::{ItemKind, ItemStatus};
use keystone_core::{LastTurn, Session};
use keystone_ui::prelude::*;

gpui::actions!(
    second_opinion,
    [
        /// About the reply focused in the transcript, else the conversation.
        AskSecondOpinion,
        /// About the conversation, from the top bar.
        AskAboutConversation
    ]
);

/// Whether `row` is a reply that has finished streaming.
fn finished_reply(row: &Row) -> bool {
    matches!(row, Row::Item(item)
        if item.kind == ItemKind::AgentMessage && item.status != ItemStatus::InProgress)
}

/// A second opinion's draft: the conversation as a handoff carries it, up to and
/// including `reply` when that is a finished reply, then a question to edit.
pub(crate) fn text(source: Source, rows: &[Row], reply: Option<usize>) -> String {
    let provider = provider_name(source.provider);
    let reply = reply.filter(|&at| rows.get(at).is_some_and(finished_reply));
    let (rows, about) = match reply {
        Some(at) => (&rows[..=at], format!("{provider}'s reply #{at}")),
        None => (
            rows,
            format!("this conversation, especially {provider}'s latest reply"),
        ),
    };
    let question = format!(
        "Give a second opinion on {about}. Read only: change nothing. Where do you agree, \
         where do you disagree and why, and what would you do instead?"
    );
    handoff::build_as(
        "For a second opinion on",
        source,
        rows,
        BUDGET_TOKENS,
        &question,
    )
}

/// The reply that answers a conversation's latest message: the last finished reply
/// after it, once its turn completed. `None` while a turn runs, when the turn was
/// stopped, failed or cut off (each leaves a row after the message), or when it
/// produced no finished reply.
fn answer(conversation: &Conversation) -> Option<&str> {
    if conversation.running || conversation.is_loading() {
        return None;
    }
    let mut reply = None;
    for row in conversation.rows.iter().rev() {
        match row {
            Row::Item(item) if item.kind == ItemKind::UserMessage => return reply,
            Row::Item(item)
                if reply.is_none()
                    && item.kind == ItemKind::AgentMessage
                    && item.status == ItemStatus::Completed =>
            {
                reply = Some(item.text.as_str());
            }
            Row::TurnEnded { .. } | Row::Recovery(_) => return None,
            _ => {}
        }
    }
    None
}

/// How far a second opinion has got, as its source shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Progress {
    Running,
    /// Its turn waits for the user: an approval, a widget, or a recovery choice.
    /// Recovery's own steps, which need nothing from the user, count as running.
    Waiting,
    /// Its turn ended and its history is still being read.
    Loading,
    /// Its latest turn completed with a reply to use.
    Done,
    Failed,
    /// Its turn was stopped, cut off when Keystone closed, or ended without a reply.
    Stopped,
}

impl Progress {
    fn of(session: &Session, live: Option<&Conversation>) -> Self {
        // A turn held for crash recovery continues once the user decides.
        if let Some(live) = live.filter(|c| c.running || c.recovery.is_some()) {
            let choice = matches!(
                live.recovery,
                Some(Recovery::Review(_) | Recovery::Unavailable(_) | Recovery::Unsaved(_))
            );
            return if choice || live.awaiting_approval() || live.awaiting_widget() {
                Self::Waiting
            } else {
                Self::Running
            };
        }
        match (session.last_turn(), live) {
            (Some(LastTurn::Failed), _) => Self::Failed,
            (Some(LastTurn::Finished), Some(live)) if answer(live).is_some() => Self::Done,
            (Some(LastTurn::Finished), Some(live)) if !live.is_loading() => Self::Stopped,
            (Some(LastTurn::Finished), _) => Self::Loading,
            (Some(LastTurn::Running) | None, _) => Self::Stopped,
        }
    }

    fn word(self) -> Option<&'static str> {
        match self {
            Self::Running | Self::Loading | Self::Done => None,
            Self::Waiting => Some("waiting for you"),
            Self::Failed => Some("failed"),
            Self::Stopped => Some("stopped"),
        }
    }
}

/// The top bar's Second opinion control.
pub(crate) fn button() -> impl IntoElement {
    IconButton::new("second-opinion", IconName::Chat)
        .debug_selector(|| "second-opinion".into())
        .tooltip(Tooltip::with_key(
            "Second opinion…",
            keystone_ui::shortcut_text("secondary-alt-o"),
        ))
        .on_click(|_, window, cx| window.dispatch_action(Box::new(AskAboutConversation), cx))
}

impl Keystone {
    /// Asks for a second opinion on the selected session, or on its reply at `reply`: a
    /// new read-only session beside it, linked to it, with the conversation and a
    /// question as its draft and the model selector open on the other provider, so the
    /// user chooses where it runs and edits the question before sending.
    pub(crate) fn ask_second_opinion(
        &mut self,
        reply: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected(cx) else {
            return;
        };
        let Some((provider, text)) = self.carry(id, cx, |source, rows| text(source, rows, reply))
        else {
            return;
        };
        let target = handoff::other(provider);
        let window_id = self.window_id;
        let Some(new) = self.create_beside(id, true, cx, |state, show, cx| {
            state.second_opinion(id, target, text, now_ms(), window_id, show, cx)
        }) else {
            return;
        };
        self.open_model_menu(new, target, window, cx);
    }

    /// The shortcut and palette: a second opinion on the reply focused in the
    /// active view's transcript, else on the whole conversation.
    pub(crate) fn second_opinion_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let views = &self.presentation.current.views;
        let reply = self
            .active_view(cx)
            .and_then(|view| self.view_key(view, cx))
            .filter(|key| Some(key.session) == self.selected(cx))
            .and_then(|key| {
                let keyboard = views.keyboard_state(key)?;
                let focused =
                    keyboard.action_focus.is_focused(window) || views.focused_region(key, window);
                let row = keyboard.row.filter(|_| focused)?;
                let conversation = self.state.read(cx).conversations().get(&key.session)?;
                conversation
                    .rows
                    .get(row)
                    .is_some_and(finished_reply)
                    .then_some(row)
            });
        self.ask_second_opinion(reply, window, cx)
    }

    /// A reply's Second opinion button, in `key`'s view.
    pub(crate) fn second_opinion_on_reply(
        &mut self,
        key: ViewKey,
        reply: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected(cx) != Some(key.session) {
            self.activate(key.view, cx);
        }
        if self.selected(cx) == Some(key.session) {
            self.ask_second_opinion(Some(reply), window, cx)
        }
    }

    /// The link rows a view shows for second opinions. A second opinion names the
    /// session it is on. A source has a row for each second opinion that has sent,
    /// with its provider and progress, and Use in conversation once it is done.
    pub(crate) fn second_opinion_rows(
        &self,
        key: ViewKey,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let state = self.state.read(cx);
        let workspace = state.store().workspace();
        let sessions = workspace.sessions();
        let Some(shown) = sessions.iter().find(|s| s.id() == key.session) else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        if let Some(on) = shown
            .handed_off_from()
            .filter(|_| shown.second_opinion())
            .and_then(|on| sessions.iter().find(|s| s.id() == on))
        {
            rows.push(self.link_row(
                key,
                "Second opinion on",
                "opinion-of",
                on.id(),
                on.title().to_owned(),
                None,
                cx,
            ));
        }
        let opinions: Vec<_> = sessions
            .iter()
            .filter(|s| {
                s.second_opinion() && s.handed_off_from() == Some(key.session) && s.started()
            })
            .map(|s| (s.id(), provider_label(s, workspace.accounts())))
            .collect();
        for (id, provider) in opinions {
            // Its history tells a completed answer from a stopped turn. Only a finished
            // turn needs it, so a cut-off one does not start recovery from here.
            if self
                .state
                .read(cx)
                .session(id)
                .is_some_and(|s| s.last_turn() == Some(LastTurn::Finished))
            {
                self.state.update(cx, |state, _| state.ensure_history(id));
            }
            let state = self.state.read(cx);
            let progress = state.session(id).map_or(Progress::Stopped, |s| {
                Progress::of(s, state.conversations().get(&id))
            });
            let done = progress == Progress::Done;
            let link = self.link_row(
                key,
                "Second opinion from",
                &format!("opinion-{id}"),
                id,
                provider,
                progress.word(),
                cx,
            );
            let use_it = arrangement::selector(&format!("opinion-use-{id}"), key.view);
            rows.push(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(div().min_w_0().flex_1().child(link))
                    .when(done, |row| {
                        row.child(
                            Button::new(use_it.clone(), "Use in conversation")
                                .size(ButtonSize::Compact)
                                .style(ButtonStyle::Subtle)
                                .debug_selector(move || use_it.to_string())
                                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                                    this.use_second_opinion(key, id, window, cx)
                                })),
                        )
                    })
                    .into_any_element(),
            );
        }
        rows
    }

    /// Adds the answer of second opinion `opinion` to the composer of the session in
    /// `key`'s view, after any draft there, for the user to edit and send. Nothing is
    /// sent. The answer is read as the press happens, so a second opinion that has since
    /// started another turn offers nothing until that turn completes.
    pub(crate) fn use_second_opinion(
        &mut self,
        key: ViewKey,
        opinion: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.read(cx);
        let Some(session) = state
            .session(opinion)
            .filter(|s| s.second_opinion() && s.handed_off_from() == Some(key.session))
        else {
            return;
        };
        let provider = provider_label(session, state.store().workspace().accounts());
        let live = state.conversations().get(&opinion);
        let Some(reply) = live
            .filter(|_| Progress::of(session, live) == Progress::Done)
            .and_then(answer)
        else {
            self.error = Some("No finished reply yet.".into());
            return self.refresh(cx);
        };
        let quote =
            format!("Second opinion from {provider} (Keystone session {opinion}):\n\n{reply}");
        if self.selected(cx) != Some(key.session) {
            self.activate(key.view, cx);
        }
        if self.selected(cx) != Some(key.session) {
            return;
        }
        let draft = self.composer.read(cx).value().trim_end().to_owned();
        if !draft.contains(&quote) {
            let text = if draft.is_empty() {
                quote
            } else {
                format!("{draft}\n\n{quote}")
            };
            self.composer
                .update(cx, |input, cx| input.set_value(text, cx));
            self.save_draft(cx);
        }
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_agent::Item;
    use keystone_core::Provider;

    fn item(kind: ItemKind, text: &str, status: ItemStatus) -> Row {
        Row::Item(Item {
            id: String::new(),
            kind,
            text: text.into(),
            status,
        })
    }

    fn source() -> Source<'static> {
        Source {
            id: 3,
            title: "Fix the login bug",
            provider: Provider::Claude,
            folder: "/work/app",
        }
    }

    fn rows() -> Vec<Row> {
        vec![
            item(
                ItemKind::UserMessage,
                "Why does login fail?",
                ItemStatus::Completed,
            ),
            item(
                ItemKind::AgentMessage,
                "The token expires early.",
                ItemStatus::Completed,
            ),
            item(ItemKind::UserMessage, "Fix it then", ItemStatus::Completed),
            item(
                ItemKind::AgentMessage,
                "Still writing",
                ItemStatus::InProgress,
            ),
        ]
    }

    #[test]
    fn a_second_opinion_on_the_conversation_carries_it_whole_and_asks_about_the_latest_reply() {
        let text = text(source(), &rows(), None);
        assert!(text.starts_with(
            "For a second opinion on Keystone session 3, \u{201c}Fix the login bug\u{201d} (Claude Code), in /work/app."
        ));
        assert!(text.contains("Claude Code (#1):\nThe token expires early."));
        assert!(text.contains("User (#2):\nFix it then"));
        assert!(!text.contains("Still writing"));
        assert!(text.contains("Budget: about "));
        assert!(text.ends_with(
            "\n\n---\n\nGive a second opinion on this conversation, especially Claude Code's latest \
             reply. Read only: change nothing. Where do you agree, where do you disagree and why, \
             and what would you do instead?"
        ));
    }

    #[test]
    fn a_second_opinion_on_a_reply_carries_the_conversation_up_to_it() {
        let text = text(source(), &rows(), Some(1));
        assert!(text.contains("Claude Code (#1):\nThe token expires early."));
        assert!(!text.contains("Fix it then"));
        assert!(text.contains("Give a second opinion on Claude Code's reply #1. Read only"));

        for not_a_reply in [Some(0), Some(3), Some(9)] {
            let text = super::text(source(), &rows(), not_a_reply);
            assert!(text.contains("Fix it then"), "{not_a_reply:?}");
            assert!(
                text.contains("on this conversation, especially"),
                "{not_a_reply:?}"
            );
        }
    }
}
