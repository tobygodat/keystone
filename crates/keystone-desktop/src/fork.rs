//! Fork from here: a new session with the same provider, account and model, holding
//! the conversation up to a message, linked to its source, which stays unchanged.
//!
//! Where the provider named a fork point for the message's turn, the fork shows the
//! source's rows through that turn and its first message opens the provider's own
//! fork of the source there. Otherwise the fork is labelled a handoff fork: it starts
//! a new conversation with the rows carried as handoff text in its composer.

use crate::conversation::{ForkFrom, Row};
use crate::handoff::{BUDGET_TOKENS, Source, build};
use crate::view_state::ViewKey;
use crate::{Keystone, now_ms};
use gpui::{Context, Focusable, Window};
use keystone_agent::ItemKind;
use keystone_core::SessionContext;

gpui::actions!(fork, [ForkFromHere]);

/// Why a fork didn't open, when nothing more is known.
const NOT_OPENED: &str = "The fork didn't open.";

/// What a row's Fork control says about the fork it makes.
pub(crate) fn label(from: &ForkFrom) -> &'static str {
    match from {
        ForkFrom::Native { .. } | ForkFrom::Running => "Fork from here",
        ForkFrom::Handoff { .. } => "Fork from here (handoff)",
    }
}

/// Whether a row offers Fork from here: a message the user sent or a finished reply.
pub(crate) fn forkable(row: &Row) -> bool {
    matches!(row, Row::Item(item) if item.kind == ItemKind::UserMessage
        || (item.kind == ItemKind::AgentMessage && item.status != keystone_agent::ItemStatus::InProgress))
}

impl Keystone {
    /// Forks the session in `key`'s view from the row at `index`, opening the fork in a
    /// tab beside it with the composer focused, or says why it couldn't.
    pub(crate) fn fork_from(
        &mut self,
        key: ViewKey,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.fork(key, index, window, cx) {
            self.error = Some(error);
            self.refresh(cx);
        }
    }

    /// [`Keystone::fork_from`], returning the fork, or why it couldn't be made.
    pub(crate) fn fork(
        &mut self,
        key: ViewKey,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<u64, String> {
        if self.selected(cx) != Some(key.session) {
            self.activate(key.view, cx);
        }
        let Some(session) = self
            .selected_session(cx)
            .filter(|session| session.id() == key.session)
        else {
            return Err(NOT_OPENED.into());
        };
        let (id, provider) = (session.id(), session.provider());
        let SessionContext::Recorded { directory, .. } = session.context() else {
            return Err(crate::handoff::NO_FOLDER.into());
        };
        let state = self.state.read(cx);
        let Some(conversation) = state.conversations().get(&id).filter(|c| !c.is_loading()) else {
            return Err(crate::handoff::LOADING.into());
        };
        if conversation.load_error.is_some() {
            return Err(crate::handoff::UNREADABLE.into());
        }
        if !conversation.rows.get(index).is_some_and(forkable) {
            return Err("Choose a message to fork from.".into());
        }
        let (point, draft, log) = match conversation.fork_from(index) {
            ForkFrom::Running => return Err("This turn is still running.".into()),
            ForkFrom::Native { point, end } => (
                Some(point.clone()),
                String::new(),
                conversation.fork_log(end),
            ),
            ForkFrom::Handoff { end } => {
                let text = build(
                    Source {
                        id,
                        title: session.title(),
                        provider,
                        folder: directory.as_str(),
                    },
                    &conversation.rows[..end],
                    BUDGET_TOKENS,
                );
                (None, text, Vec::new())
            }
        };
        let window_id = self.window_id;
        let fork = self
            .create_beside(id, true, cx, |state, show, cx| {
                state.fork(id, point, draft, log, now_ms(), window_id, show, cx)
            })
            .ok_or_else(|| self.error.take().unwrap_or_else(|| NOT_OPENED.into()))?;
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
        Ok(fork)
    }

    /// Forks from the transcript row the keyboard is on.
    pub(crate) fn fork_focused_row(
        &mut self,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.follow_widget_focus(key, window, cx);
        let Some(index) = self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .and_then(|k| k.row)
        else {
            self.error = Some("Choose a message to fork from.".into());
            return self.refresh(cx);
        };
        self.fork_from(key, index, window, cx);
    }
}
