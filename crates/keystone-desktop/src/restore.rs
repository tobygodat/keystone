//! Restore to here: after asking, puts the session's repository's files back the way a
//! turn's checkpoint saved them, then continues the conversation from that turn in a
//! fork, so the source session is not changed. The files are kept only once the fork
//! opens; a cancel, or a fork that can't open, puts every file back.

use crate::Keystone;
use crate::app_state::checkpoints::{Placed, Restoring, State, UNAVAILABLE};
use crate::view_state::ViewKey;
use gpui::{Context, IntoElement, ParentElement, Task, Window, div};
use keystone_agent::checkpoint::{self, Restored};
use keystone_agent::git::Cancel;
use keystone_ui::prelude::*;

gpui::actions!(restore, [RestoreToHere]);

/// The open Restore to here question, and the restore it started, while one runs.
pub(crate) struct RestoreDialog {
    key: ViewKey,
    /// The reply row the conversation forks from once the files are back.
    index: usize,
    turn: u64,
    focus: DialogFocus,
    pub(crate) error: Option<String>,
    running: Option<(Cancel, Task<()>)>,
}

impl RestoreDialog {
    pub(crate) fn busy(&self) -> bool {
        self.running.is_some()
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
        let turn = if let Some(State::Unavailable) = state.checkpoints(key.session) {
            None
        } else {
            state
                .conversations()
                .get(&key.session)
                .and_then(|conversation| conversation.checkpoint_at(index))
        };
        let Some(turn) = turn else {
            self.error = Some(match state.checkpoints(key.session) {
                Some(State::Unavailable) => UNAVAILABLE.into(),
                _ => "Choose a turn's last reply.".into(),
            });
            return self.refresh(cx);
        };
        self.restore_dialog = Some(RestoreDialog {
            key,
            index,
            turn,
            focus: DialogFocus::new(window, cx),
            error: None,
            running: None,
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

    /// Closes the question; while a restore runs, cancels it instead, and the dialog
    /// closes once every file is back.
    fn close_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.restore_dialog else {
            return;
        };
        if let Some((cancel, _)) = &dialog.running {
            return cancel.cancel();
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
            Ok(Restoring {
                cancel,
                placed,
                done,
            }) => {
                dialog.error = None;
                let stopped = cancel.clone();
                let task = cx.spawn_in(window, async move |this, cx| {
                    let mut fork = None;
                    if let Ok(placed) = placed.await {
                        let _ = this.update_in(cx, |this, window, cx| {
                            fork =
                                this.keep_restore(key, index, turn, placed, &stopped, window, cx);
                        });
                    }
                    let done = done
                        .await
                        .unwrap_or_else(|_| Err(checkpoint::Error::Cancelled));
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.restore_ended(fork, done, window, cx)
                    });
                });
                dialog.running = Some((cancel, task));
            }
        }
        self.refresh(cx);
    }

    /// Every file is in place: opens the fork, keeping the files when it opens. Returns
    /// the fork or why it didn't open, or `None` when cancelled.
    #[allow(clippy::too_many_arguments)]
    fn keep_restore(
        &mut self,
        key: ViewKey,
        index: usize,
        turn: u64,
        placed: Placed,
        cancel: &Cancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Result<u64, String>> {
        if cancel.is_cancelled() {
            return None;
        }
        // Rows may have moved while the files were restored.
        let index = self
            .state
            .read(cx)
            .conversations()
            .get(&key.session)
            .and_then(|conversation| conversation.checkpoint_row(turn))
            .unwrap_or(index);
        let fork = self.fork(key, index, window, cx);
        if fork.is_ok() {
            placed.keep();
        }
        Some(fork)
    }

    fn restore_ended(
        &mut self,
        fork: Option<Result<u64, String>>,
        done: checkpoint::Result<Restored>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let error = match (fork, done) {
            // The fork's reason says more than the cancel its failure caused.
            (Some(Err(reason)), Err(checkpoint::Error::NotPutBack { kept, .. })) => {
                checkpoint::Error::NotPutBack {
                    cause: Box::new(checkpoint::Error::Io(std::io::Error::other(reason))),
                    kept,
                }
                .to_string()
            }
            (Some(Ok(fork)), Ok(restored)) => {
                self.restore_dialog = None;
                if let Some(notice) = not_restored(&restored) {
                    self.state
                        .update(cx, |state, cx| state.checkpoint_notice(fork, &notice, cx));
                }
                return self.refresh(cx);
            }
            (_, Err(error @ checkpoint::Error::NotPutBack { .. })) => error.to_string(),
            (Some(Err(reason)), _) => reason,
            (_, Err(checkpoint::Error::Cancelled)) | (_, Ok(_)) => {
                self.restore_dialog = None;
                self.focus_composer(window, cx);
                return self.refresh(cx);
            }
            (_, Err(error)) => error.to_string(),
        };
        if let Some(dialog) = &mut self.restore_dialog {
            // Git's last line says what failed; the lines before it repeat it.
            let last = error.lines().rev().find(|line| !line.trim().is_empty());
            dialog.error = Some(last.unwrap_or_default().to_owned());
            dialog.running = None;
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
            .working(dialog.busy())
            .body(v_flex().children(status))
            .on_cancel(move |window, cx| cancel(&(), window, cx))
            .on_confirm(move |window, cx| confirm(&(), window, cx)),
        )
    }
}

/// What the fork says about the paths a restore left as they were.
fn not_restored(restored: &Restored) -> Option<String> {
    let first: Vec<String> = restored
        .skipped
        .iter()
        .take(3)
        .map(|path| path.display().to_string())
        .collect();
    match restored.skipped.len() {
        0 => None,
        1..=3 => Some(format!("Not restored: {}.", first.join(", "))),
        count => Some(format!(
            "Not restored: {} and {} more.",
            first.join(", "),
            count - 3
        )),
    }
}
