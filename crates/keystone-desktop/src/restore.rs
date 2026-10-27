//! Restore to here: after asking, puts the session's repository's files back the way a
//! turn's checkpoint saved them, then continues the conversation from that turn in a
//! fork, so the source session is not changed.

use crate::Keystone;
use crate::app_state::checkpoints::{State, UNAVAILABLE};
use crate::view_state::ViewKey;
use gpui::{Context, IntoElement, ParentElement, Task, Window, div};
use keystone_ui::prelude::*;

gpui::actions!(restore, [RestoreToHere]);

/// The open Restore to here question. A restore in flight keeps its task here.
pub(crate) struct RestoreDialog {
    key: ViewKey,
    /// The reply row the conversation forks from once the files are back.
    index: usize,
    turn: u64,
    focus: DialogFocus,
    error: Option<String>,
    task: Option<Task<()>>,
}

impl RestoreDialog {
    pub(crate) fn busy(&self) -> bool {
        self.task.is_some()
    }
}

impl Keystone {
    /// Asks before restoring to the checkpoint of the turn whose last reply is at
    /// `index`.
    pub(crate) fn open_restore(
        &mut self,
        key: ViewKey,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.read(cx);
        if let Some(State::Unavailable) = state.checkpoints(key.session) {
            return self.state.update(cx, |state, cx| {
                state.checkpoint_notice(key.session, UNAVAILABLE, cx)
            });
        }
        let Some(turn) = state
            .conversations()
            .get(&key.session)
            .and_then(|conversation| conversation.checkpoint_at(index))
        else {
            self.error = Some("Choose a turn's last reply.".into());
            return self.refresh(cx);
        };
        self.restore_dialog = Some(RestoreDialog {
            key,
            index,
            turn,
            focus: DialogFocus::new(window, cx),
            error: None,
            task: None,
        });
        self.refresh(cx);
    }

    /// Restore to here from the transcript row the keyboard is on.
    pub(crate) fn restore_focused_row(
        &mut self,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.follow_widget_focus(key, window, cx);
        match self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .and_then(|k| k.row)
        {
            Some(index) => self.open_restore(key, index, window, cx),
            None => {
                self.error = Some("Choose a turn's last reply.".into());
                self.refresh(cx);
            }
        }
    }

    /// A restore cannot be stopped once it runs, so the dialog stays until it ends.
    fn close_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .restore_dialog
            .as_ref()
            .is_some_and(RestoreDialog::busy)
        {
            return;
        }
        self.restore_dialog = None;
        self.focus_composer(window, cx);
        self.refresh(cx);
    }

    fn confirm_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.restore_dialog.as_ref().filter(|d| !d.busy()) else {
            return;
        };
        let (key, index, turn) = (dialog.key, dialog.index, dialog.turn);
        let restoring = self.state.update(cx, |state, cx| {
            state.restore_checkpoint(key.session, turn, cx)
        });
        let Some(dialog) = &mut self.restore_dialog else {
            return;
        };
        match restoring {
            Err(error) => dialog.error = Some(error),
            Ok(restored) => {
                dialog.error = None;
                dialog.task = Some(cx.spawn_in(window, async move |this, cx| {
                    let restored = restored
                        .await
                        .unwrap_or_else(|_| Err("Keystone stopped before restoring.".into()));
                    let _ = this.update_in(cx, |this, window, cx| match restored {
                        Ok(restored) => {
                            this.restore_dialog = None;
                            // Rows may have moved while the files were restored.
                            let index = this
                                .state
                                .read(cx)
                                .conversations()
                                .get(&key.session)
                                .and_then(|conversation| conversation.checkpoint_row(turn))
                                .unwrap_or(index);
                            this.fork_from(key, index, window, cx);
                            let skipped: Vec<String> = restored
                                .skipped
                                .iter()
                                .take(3)
                                .map(|path| path.display().to_string())
                                .collect();
                            let notice = match restored.skipped.len() {
                                0 => None,
                                1..=3 => Some(format!("Not restored: {}.", skipped.join(", "))),
                                count => Some(format!(
                                    "Not restored: {} and {} more.",
                                    skipped.join(", "),
                                    count - 3
                                )),
                            };
                            // Said in the fork the restore opened.
                            let fork = this.selected(cx);
                            this.state.update(cx, |state, cx| {
                                if let (Some(notice), Some(fork)) = (notice, fork) {
                                    state.checkpoint_notice(fork, &notice, cx);
                                }
                                state.restore_finished(cx);
                            });
                            this.refresh(cx);
                        }
                        Err(error) => {
                            this.state
                                .update(cx, |state, cx| state.restore_finished(cx));
                            if let Some(dialog) = &mut this.restore_dialog {
                                dialog.error = Some(error);
                                dialog.task = None;
                            }
                            this.refresh(cx);
                        }
                    });
                }));
            }
        }
        self.refresh(cx);
    }

    pub(crate) fn restore_dialog_view(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let dialog = self.restore_dialog.as_ref()?;
        let cancel = cx.listener(|this, _: &(), window, cx| this.close_restore(window, cx));
        let confirm = cx.listener(|this, _: &(), window, cx| this.confirm_restore(window, cx));
        let status = dialog
            .error
            .clone()
            .map(|error| {
                div()
                    .automation_id("restore-error")
                    .child(Label::new(error).size(TextSize::Sm).color(Color::Danger))
            })
            .or_else(|| {
                dialog.busy().then(|| {
                    div().automation_id("restore-busy").child(
                        Label::new("Restoring…")
                            .size(TextSize::Sm)
                            .color(Color::Muted),
                    )
                })
            });
        Some(
            ConfirmDialog::new(
                "restore-dialog",
                &dialog.focus,
                "Restore to here",
                "Files go back to this turn, and the conversation continues in a fork.",
            )
            .icon(IconName::Restore)
            .tone(Color::Warning)
            .confirm_label("Restore")
            .body(v_flex().children(status))
            .on_cancel(move |window, cx| cancel(&(), window, cx))
            .on_confirm(move |window, cx| confirm(&(), window, cx)),
        )
    }
}
