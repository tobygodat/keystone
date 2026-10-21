use crate::Keystone;
use crate::conversation::{self, Row};
use crate::view_state::{CopyTarget, ViewKey};
use gpui::{App, ClipboardItem, Context, FocusHandle, Window};
use keystone_agent::{ItemKind, ItemStatus};
use keystone_ui::markdown::{Block, Document};

gpui::actions!(
    transcript,
    [
        FocusTranscript,
        PreviousRow,
        NextRow,
        FirstRow,
        LastRow,
        NextAction,
        PreviousAction,
        CopyTranscript,
        FocusLatest,
        NextTranscript,
        PreviousTranscript,
        LeaveTranscript
    ]
);

fn code_actions<'a>(block: &'a Block, actions: &mut Vec<(CopyTarget, &'a str)>, n: &mut usize) {
    match block {
        Block::Code { code, .. } => {
            *n += 1;
            actions.push((CopyTarget::Code(*n), code.as_ref()));
        }
        Block::Diagram { source, closed } => {
            *n += 1;
            if *closed {
                actions.push((CopyTarget::DiagramToggle(*n), source.as_ref()));
            }
            actions.push((CopyTarget::Code(*n), source.as_ref()));
        }
        Block::Quote(blocks) => {
            for block in blocks {
                code_actions(block, actions, n);
            }
        }
        Block::List { items, .. } => {
            for block in items.iter().flatten() {
                code_actions(block, actions, n);
            }
        }
        _ => {}
    }
}

fn copy_actions<'a>(row: &'a Row, document: Option<&'a Document>) -> Vec<(CopyTarget, &'a str)> {
    let Row::Item(item) = row else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    match &item.kind {
        ItemKind::UserMessage => actions.push((CopyTarget::Row, item.text.as_str())),
        ItemKind::AgentMessage => {
            if item.status != ItemStatus::InProgress {
                actions.push((CopyTarget::Row, item.text.as_str()));
            }
            if let Some(document) = document {
                let mut n = 0;
                for top in &document.blocks {
                    code_actions(&top.block, &mut actions, &mut n);
                }
                if document.has_selectable_text() {
                    actions.push((CopyTarget::TextSelection, item.text.as_str()));
                }
            }
        }
        ItemKind::Command { command, .. } => {
            let output = item.text.trim_end();
            actions.push((
                CopyTarget::Row,
                if output.is_empty() {
                    command.as_str()
                } else {
                    output
                },
            ));
        }
        _ => {}
    }
    if crate::transcript::activity_name(item).is_some() {
        let text = if let ItemKind::Command { command, .. } = &item.kind {
            if item.text.trim_end().is_empty() {
                command.as_str()
            } else {
                item.text.trim_end()
            }
        } else {
            item.text.as_str()
        };
        actions.push((CopyTarget::ActivityToggle, text));
    }
    actions
}

impl Keystone {
    pub(crate) fn begin_text_selection(
        &mut self,
        key: ViewKey,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<FocusHandle> {
        if self.state.read(cx).store().workspace().selected() != Some(key.session)
            || !self
                .presentation
                .current
                .arrangement
                .visible
                .contains(&key.view)
        {
            return None;
        }
        let Some(Row::Item(item)) = self
            .state
            .read(cx)
            .conversations()
            .get(&key.session)
            .and_then(|c| c.rows.get(index))
        else {
            return None;
        };
        if item.kind != ItemKind::AgentMessage {
            return None;
        }
        let keyboard = self.presentation.current.views.keyboard(key, cx);
        keyboard.row = Some(index);
        keyboard.action = Some(CopyTarget::TextSelection);
        keyboard.selection_from_pointer = true;
        let focus = keyboard.action_focus.clone();
        focus.focus(window, cx);
        self.presentation.current.views.activate(key);
        cx.notify();
        Some(focus)
    }
    pub(crate) fn toggle_activity(
        &mut self,
        key: ViewKey,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).store().workspace().selected() != Some(key.session)
            || !self.presentation.current.arrangement.visible.contains(&key.view)
            || !self.state.read(cx).conversations().get(&key.session).and_then(|c| c.rows.get(index)).is_some_and(|row| matches!(row, Row::Item(item) if crate::transcript::activity_name(item).is_some())) {
            return;
        }
        let keyboard = self.presentation.current.views.keyboard(key, cx);
        keyboard.row = Some(index);
        keyboard.action = Some(CopyTarget::ActivityToggle);
        keyboard.selection_from_pointer = false;
        keyboard.action_focus.focus(window, cx);
        self.presentation.current.views.toggle_activity(key, index);
        self.presentation.current.views.activate(key);
        cx.notify();
    }
    pub(crate) fn leave_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_menu
            || self.new_menu
            || self.approval_menu
            || self.model_menu.is_some()
            || self.presentation.current.arrangement.surface_menu.is_some()
            || self.presentation.current.customization.is_open()
        {
            cx.propagate();
            return;
        }
        self.focus_composer(window, cx);
    }
    pub(crate) fn cycle_transcript(
        &mut self,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let views: Vec<_> = self
            .presentation
            .current
            .arrangement
            .visible
            .clone()
            .into_iter()
            .filter_map(|view| self.view_key(view, cx))
            .collect();
        if views.is_empty() {
            return;
        }
        let at = views.iter().position(|key| {
            self.presentation
                .current
                .views
                .contains_focus(*key, window, cx)
        });
        let next = match (at, backwards) {
            (Some(n), false) => (n + 1) % views.len(),
            (Some(n), true) => (n + views.len() - 1) % views.len(),
            (None, false) => 0,
            (None, true) => views.len() - 1,
        };
        self.activate(views[next].view, cx);
        self.transcript_row(views[next], 0, window, cx);
    }
    pub(crate) fn focus_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.state.read(cx).store().workspace().selected() else {
            return;
        };
        let Some(key) = self
            .presentation
            .current
            .views
            .send_target(&self.showing(session, cx), session)
        else {
            return;
        };
        self.transcript_row(key, 0, window, cx);
    }

    pub(crate) fn transcript_row(
        &mut self,
        key: ViewKey,
        direction: i8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conversation) = self.state.read(cx).conversations().get(&key.session) else {
            return;
        };
        if conversation.is_loading() {
            self.presentation
                .current
                .views
                .focus(key, cx)
                .focus(window, cx);
            return;
        }
        let rows: Vec<_> = conversation
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| conversation::visible(row))
            .map(|(n, _)| n)
            .collect();
        let list = self
            .presentation
            .current
            .views
            .list(key, conversation.rows.len());
        let top = list.logical_scroll_top().item_ix;
        let keyboard = self.presentation.current.views.keyboard(key, cx);
        let had_row = keyboard.row.is_some_and(|row| rows.contains(&row));
        let current = keyboard
            .row
            .and_then(|at| {
                rows.iter()
                    .copied()
                    .find(|n| *n >= at)
                    .or_else(|| rows.last().copied())
            })
            .or_else(|| rows.iter().copied().find(|n| *n >= top))
            .or_else(|| rows.last().copied());
        let row = match direction {
            -2 => rows.first().copied(),
            2 => rows.last().copied(),
            -1 | 1 if !had_row => current,
            -1 => current
                .and_then(|at| rows.iter().copied().rev().find(|n| *n < at))
                .or(current),
            1 => current
                .and_then(|at| rows.iter().copied().find(|n| *n > at))
                .or(current),
            _ => current,
        };
        keyboard.row = row;
        keyboard.action = None;
        keyboard.selection_from_pointer = false;
        self.presentation
            .current
            .views
            .focus(key, cx)
            .focus(window, cx);
        self.presentation.current.views.activate(key);
        if let Some(row) = row {
            let visible = self
                .presentation
                .current
                .views
                .row_is_visible(key, row, direction != 0);
            if !visible {
                list.scroll_to_reveal_item(row);
            }
        }
        cx.notify();
    }

    fn with_row_actions<R>(
        &self,
        key: ViewKey,
        read: impl FnOnce(&[(CopyTarget, &str)]) -> R,
        cx: &App,
    ) -> R {
        let Some(index) = self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .and_then(|k| k.row)
        else {
            return read(&[]);
        };
        let Some(row) = self
            .state
            .read(cx)
            .conversations()
            .get(&key.session)
            .and_then(|c| c.rows.get(index))
        else {
            return read(&[]);
        };
        let document = match row {
            Row::Item(item) if item.kind == ItemKind::AgentMessage => Some(
                self.replies
                    .borrow_mut()
                    .document((key.session, index), item),
            ),
            _ => None,
        };
        read(&copy_actions(row, document.as_deref()))
    }

    pub(crate) fn transcript_action(
        &mut self,
        key: ViewKey,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .and_then(|k| k.row)
            .is_none()
        {
            self.transcript_row(key, 0, window, cx);
        }
        let actions: Vec<_> = self.with_row_actions(
            key,
            |actions| actions.iter().map(|(target, _)| *target).collect(),
            cx,
        );
        let jump = self.presentation.current.views.scrolled_up(key);
        let keyboard = self.presentation.current.views.keyboard(key, cx);
        let at = if keyboard.jump_focus.is_focused(window) {
            actions.len() + 1
        } else if keyboard.action_focus.is_focused(window) {
            keyboard
                .action
                .and_then(|target| actions.iter().position(|t| *t == target))
                .map_or(0, |n| n + 1)
        } else {
            0
        };
        let count = actions.len() + usize::from(jump) + 1;
        keyboard.selection_from_pointer = false;
        let next = if backwards {
            (at + count - 1) % count
        } else {
            (at + 1) % count
        };
        match next {
            n if n > 0 && n <= actions.len() => {
                keyboard.action = Some(actions[n - 1]);
                keyboard.action_focus.focus(window, cx);
            }
            n if n == actions.len() + 1 && jump => {
                keyboard.action = None;
                keyboard.jump_focus.focus(window, cx);
            }
            _ => {
                keyboard.action = None;
                self.presentation
                    .current
                    .views
                    .focus(key, cx)
                    .focus(window, cx);
            }
        }
        self.presentation.current.views.activate(key);
        cx.notify();
    }

    pub(crate) fn copy_transcript(
        &mut self,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .is_some_and(|k| k.jump_focus.is_focused(window))
        {
            return;
        }
        if self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .and_then(|k| k.row)
            .is_none()
        {
            self.transcript_row(key, 0, window, cx);
        }
        let target = self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .and_then(|k| k.action);
        let text = self.with_row_actions(
            key,
            |actions| {
                target
                    .and_then(|target| actions.iter().find(|(t, _)| *t == target))
                    .or_else(|| actions.first())
                    .map(|(_, text)| (*text).to_owned())
            },
            cx,
        );
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        self.presentation.current.views.note_focus(key);
    }

    pub(crate) fn focus_latest(
        &mut self,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.presentation.current.views.scrolled_up(key) {
            self.presentation
                .current
                .views
                .keyboard(key, cx)
                .jump_focus
                .focus(window, cx);
            self.presentation.current.views.activate(key);
            cx.notify();
        }
    }

    pub(crate) fn activate_latest(
        &mut self,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keyboard = self.presentation.current.views.keyboard(key, cx);
        keyboard.row = None;
        keyboard.action = None;
        keyboard.selection_from_pointer = false;
        self.presentation
            .current
            .views
            .focus(key, cx)
            .focus(window, cx);
        self.presentation.current.views.activate(key);
        self.presentation.current.views.scroll_to_end(key);
        cx.notify();
    }

    pub(crate) fn reconcile_transcript_keyboard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible = self.presentation.current.arrangement.visible.clone();
        for view in visible {
            let Some(key) = self.view_key(view, cx) else {
                continue;
            };
            let session = key.session;
            if self
                .state
                .read(cx)
                .conversations()
                .get(&session)
                .is_some_and(|c| c.is_loading())
            {
                continue;
            }
            let Some(keyboard) = self.presentation.current.views.keyboard_state(key) else {
                continue;
            };
            let hidden_row = keyboard.row.is_some_and(|row| {
                self.state
                    .read(cx)
                    .conversations()
                    .get(&session)
                    .and_then(|c| c.rows.get(row))
                    .is_none_or(|row| !conversation::visible(row))
            });
            if hidden_row
                && (keyboard.action_focus.is_focused(window)
                    || self.presentation.current.views.focused_region(key, window))
            {
                self.transcript_row(key, 0, window, cx);
                continue;
            }
            let invalid_action = keyboard.action_focus.is_focused(window)
                && !self.with_row_actions(
                    key,
                    |actions| actions.iter().any(|(t, _)| Some(*t) == keyboard.action),
                    cx,
                );
            let hidden_jump = keyboard.jump_focus.is_focused(window)
                && !self.presentation.current.views.scrolled_up(key);
            if invalid_action || hidden_jump {
                let keyboard = self.presentation.current.views.keyboard(key, cx);
                keyboard.action = None;
                keyboard.selection_from_pointer = false;
                self.presentation
                    .current
                    .views
                    .focus(key, cx)
                    .focus(window, cx);
            }
        }
    }
}
