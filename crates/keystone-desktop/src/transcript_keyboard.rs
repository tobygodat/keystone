use crate::Keystone;
use crate::conversation::Row;
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

/// What each keyboard action on `row` copies. An edit's controls copy nothing here: its
/// diff text is built only when it is copied ([`Keystone::copy_transcript`]).
fn copy_actions<'a>(
    row: &'a Row,
    document: Option<&'a Document>,
    open_edit: bool,
    message: &[CopyTarget],
) -> Vec<(CopyTarget, &'a str)> {
    let Row::Item(item) = row else {
        // An image row holds its message's Send again, Edit and Discard.
        return message.iter().map(|target| (*target, "")).collect();
    };
    let mut actions = Vec::new();
    match &item.kind {
        ItemKind::UserMessage => {
            actions.push((CopyTarget::Row, item.text.as_str()));
            actions.extend(message.iter().map(|target| (*target, item.text.as_str())));
        }
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
        let text = match &item.kind {
            ItemKind::Command { command, .. } if item.text.trim_end().is_empty() => {
                command.as_str()
            }
            ItemKind::Command { .. } => item.text.trim_end(),
            ItemKind::FileChange { .. } => "",
            _ => item.text.as_str(),
        };
        actions.push((CopyTarget::ActivityToggle, text));
        if open_edit {
            actions.push((CopyTarget::OpenEdit, text));
        }
    }
    if crate::fork::forkable(row) {
        actions.push((CopyTarget::Fork, item.text.as_str()));
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
        if self.selected(cx) != Some(key.session)
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
        if self.selected(cx) != Some(key.session)
            || !self
                .presentation
                .current
                .arrangement
                .visible
                .contains(&key.view)
        {
            return;
        }
        let opens = match self
            .state
            .read(cx)
            .conversations()
            .get(&key.session)
            .and_then(|c| c.rows.get(index))
        {
            Some(Row::Item(item)) if crate::transcript::activity_name(item).is_some() => {
                crate::transcript::opens_expanded(item)
            }
            _ => return,
        };
        let keyboard = self.presentation.current.views.keyboard(key, cx);
        keyboard.row = Some(index);
        keyboard.action = Some(CopyTarget::ActivityToggle);
        keyboard.selection_from_pointer = false;
        keyboard.action_focus.focus(window, cx);
        self.presentation
            .current
            .views
            .toggle_activity(key, index, opens);
        let open = self
            .presentation
            .current
            .views
            .activity_expanded(key, index, opens);
        self.edit_row_toggled(key.session, index, open, cx);
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
        let Some(session) = self.selected(cx) else {
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
        self.follow_widget_focus(key, window, cx);
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
        let rows: Vec<_> = (0..conversation.rows.len())
            .filter(|&n| conversation.shows(n))
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

    /// The edit on the transcript's row as diff text, built when it is copied.
    fn edit_text(&self, key: ViewKey, cx: &App) -> Option<String> {
        let index = self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .and_then(|k| k.row)?;
        let conversation = self.state.read(cx).conversations().get(&key.session)?;
        match conversation.rows.get(index)? {
            Row::Item(keystone_agent::Item {
                kind:
                    ItemKind::FileChange {
                        patch: Some(patch), ..
                    },
                ..
            }) => Some(self.edit_folder(key.session, cx).patch(patch).text()),
            _ => None,
        }
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
        // Open full edit is a stop only while it is drawn: on an open edit longer than
        // its preview.
        let open_edit = match row {
            Row::Item(
                item @ keystone_agent::Item {
                    kind:
                        ItemKind::FileChange {
                            patch: Some(patch), ..
                        },
                    ..
                },
            ) => {
                let lines: usize = patch
                    .files
                    .iter()
                    .flat_map(|f| &f.hunks)
                    .map(|h| h.lines.len())
                    .sum();
                lines > crate::transcript::PREVIEW_LINES
                    && self.presentation.current.views.activity_expanded(
                        key,
                        index,
                        crate::transcript::opens_expanded(item),
                    )
            }
            _ => false,
        };
        let message = self.message_actions(key.session, index, cx);
        read(&copy_actions(row, document.as_deref(), open_edit, &message))
    }

    /// The Send again, Edit and Discard controls row `index` draws and that can run now:
    /// a message's own, or, on its image row, the message's.
    pub(crate) fn message_actions(&self, session: u64, index: usize, cx: &App) -> Vec<CopyTarget> {
        let state = self.state.read(cx);
        let Some(rows) = state.conversations().get(&session).map(|c| &c.rows) else {
            return Vec::new();
        };
        let item = match rows.get(index) {
            Some(Row::Item(item))
                if item.kind == ItemKind::UserMessage
                    && !matches!(rows.get(index + 1), Some(Row::Attachments(a)) if !a.is_empty()) =>
            {
                item
            }
            Some(Row::Attachments(_)) => match index.checked_sub(1).and_then(|i| rows.get(i)) {
                Some(Row::Item(item)) if item.kind == ItemKind::UserMessage => item,
                _ => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        let mut actions = Vec::new();
        match item.status {
            ItemStatus::Failed if state.refused_send(session, &item.id).is_some() => {
                if state.offline(session).is_none() {
                    actions.push(CopyTarget::SendAgain);
                }
                if self.composer_empty(session, cx) {
                    actions.push(CopyTarget::EditMessage);
                }
            }
            ItemStatus::InProgress if state.unreached(session).is_some() => {
                actions.push(CopyTarget::Discard);
            }
            _ => {}
        }
        actions
    }

    /// The row of the widget whose control has focus.
    pub(crate) fn focused_widget_row(
        &self,
        key: ViewKey,
        window: &Window,
        cx: &App,
    ) -> Option<usize> {
        let call = self
            .presentation
            .current
            .views
            .focused_widget(key, window)?;
        self.state
            .read(cx)
            .conversations()
            .get(&key.session)?
            .surface_row(&call)
    }

    /// A focused widget control whose row the list no longer draws, scrolled or pushed
    /// out, receives no keys; the conversation takes focus back. Runs after a paint.
    pub(crate) fn release_undrawn_widget_focus(
        &mut self,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let views = &self.presentation.current.views;
        if views
            .focused_widget(key, window)
            .is_some_and(|call| !views.widget_drawn(key, &call))
        {
            self.presentation
                .current
                .views
                .focus(key, cx)
                .focus(window, cx);
            cx.notify();
        }
    }

    /// A widget control can take focus by a click, outside the transcript's own
    /// keyboard; its row becomes the transcript's row.
    pub(crate) fn follow_widget_focus(
        &mut self,
        key: ViewKey,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.focused_widget_row(key, window, cx) {
            self.presentation.current.views.keyboard(key, cx).row = Some(row);
        }
    }

    /// The controls of the widget in the transcript's row, in Tab order.
    fn row_widget_controls(&self, key: ViewKey, cx: &App) -> Vec<FocusHandle> {
        let views = &self.presentation.current.views;
        let Some(row) = views.keyboard_state(key).and_then(|k| k.row) else {
            return Vec::new();
        };
        match self
            .state
            .read(cx)
            .conversations()
            .get(&key.session)
            .and_then(|c| c.rows.get(row))
        {
            Some(Row::Surface(surface)) => views.widget_controls(key, &surface.call),
            _ => Vec::new(),
        }
    }

    /// Tab and Shift+Tab: the row, then its copy actions, then its widget's controls,
    /// then Jump to latest when it shows, and round again.
    pub(crate) fn transcript_action(
        &mut self,
        key: ViewKey,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.follow_widget_focus(key, window, cx);
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
        let controls = self.row_widget_controls(key, cx);
        let jump = self.presentation.current.views.scrolled_up(key);
        let keyboard = self.presentation.current.views.keyboard(key, cx);
        let at = if keyboard.jump_focus.is_focused(window) {
            actions.len() + controls.len() + 1
        } else if keyboard.action_focus.is_focused(window) {
            keyboard
                .action
                .and_then(|target| actions.iter().position(|t| *t == target))
                .map_or(0, |n| n + 1)
        } else if let Some(n) = controls.iter().position(|c| c.is_focused(window)) {
            actions.len() + n + 1
        } else {
            0
        };
        let count = actions.len() + controls.len() + usize::from(jump) + 1;
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
            n if n > actions.len() && n <= actions.len() + controls.len() => {
                keyboard.action = None;
                controls[n - actions.len() - 1].focus(window, cx);
            }
            n if n == actions.len() + controls.len() + 1 && jump => {
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
        self.follow_widget_focus(key, window, cx);
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
        let chosen = self.with_row_actions(
            key,
            |actions| {
                target
                    .and_then(|target| actions.iter().find(|(t, _)| *t == target))
                    .or_else(|| actions.first())
                    .map(|(target, text)| (*target, (*text).to_owned()))
            },
            cx,
        );
        let text = match chosen {
            Some((CopyTarget::ActivityToggle | CopyTarget::OpenEdit, text)) => {
                self.edit_text(key, cx).or(Some(text))
            }
            chosen => chosen.map(|(_, text)| text),
        };
        if let Some(text) = text.filter(|text| !text.is_empty()) {
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
                    .is_none_or(|c| !c.shows(row))
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
