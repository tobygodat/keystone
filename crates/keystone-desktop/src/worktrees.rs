//! Sessions in their own git worktree. "New session in worktree" runs the user's own
//! git to add a worktree and a branch inside the project's repository, and the session
//! works there. Removing a worktree is a separate, explicit action: git refuses while
//! the worktree holds uncommitted or unpushed work, and the branch always stays.

use crate::{Keystone, now_ms};
use gpui::{
    Context, Focusable, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Task, Window, div, prelude::FluentBuilder, px,
};
use keystone_core::{ProjectId, Provider, Session, SessionContext, WorktreeContext, worktree};
use keystone_ui::prelude::*;
use std::rc::Rc;

gpui::actions!(keystone, [NewWorktreeSession, RemoveWorktree]);

/// The open worktree dialog. A running git command keeps its task here, so closing
/// the dialog drops its result.
pub(crate) enum WorktreeDialog {
    Create {
        focus: DialogFocus,
        provider: Provider,
        error: Option<String>,
        task: Option<Task<()>>,
    },
    Remove {
        worktree: WorktreeContext,
        focus: DialogFocus,
        error: Option<String>,
        task: Option<Task<()>>,
    },
}

impl WorktreeDialog {
    pub(crate) fn busy(&self) -> bool {
        match self {
            Self::Create { task, .. } | Self::Remove { task, .. } => task.is_some(),
        }
    }

    fn set_error(&mut self, message: String) {
        match self {
            Self::Create { error, task, .. } | Self::Remove { error, task, .. } => {
                *error = Some(message);
                *task = None;
            }
        }
    }
}

/// A session's branch as the sidebar and top bar show it, and whether its worktree
/// folder is gone or is no longer that worktree.
pub(crate) fn branch(session: &Session) -> Option<(String, bool)> {
    let worktree = session.worktree()?;
    Some((worktree.branch.clone(), !worktree::is_intact(worktree)))
}

/// The top bar's branch label, and a warning when the worktree folder is gone.
pub(crate) fn branch_chip(branch: String, missing: bool) -> impl IntoElement {
    h_flex()
        .min_w_0()
        .gap(px(5.))
        .child(
            Icon::new(IconName::Branch)
                .size(px(16.))
                .color(Color::Muted),
        )
        .child(
            div()
                .automation_id("worktree-branch")
                .min_w(px(48.))
                .max_w(px(200.))
                .child(Label::new(branch).color(Color::Muted).truncate()),
        )
        .when(missing, |d| {
            d.child(
                div()
                    .flex_shrink_0()
                    .automation_id("worktree-missing")
                    .child(
                        Label::new("Worktree folder missing")
                            .size(TextSize::Sm)
                            .color(Color::Warning),
                    ),
            )
        })
}

impl Keystone {
    pub(crate) fn worktree_pill(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        h_flex()
            .id("welcome-worktree")
            .automation_id("welcome-worktree")
            .h(px(30.))
            .px(px(11.))
            .gap(px(7.))
            .rounded(px(15.))
            .bg(colors.element_filled)
            .cursor_pointer()
            .hover(move |s| s.bg(colors.element_filled_hover))
            .child(
                Icon::new(IconName::Branch)
                    .size(px(16.))
                    .color(Color::Muted),
            )
            .child(
                Label::new("In worktree")
                    .size(TextSize::Ui)
                    .weight(gpui::FontWeight::MEDIUM),
            )
            .tooltip(Tooltip::with_key("New session in worktree", "Ctrl+Alt+G").builder())
            .on_click(
                self.scoped_listener(cx, |this, _, window, cx| this.open_new_worktree(window, cx)),
            )
    }

    pub(crate) fn open_new_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = match self.presentation.creation_directory() {
            Ok(project) => project,
            Err(error) => {
                self.error = Some(format!("No worktree created. {error}"));
                return self.refresh(cx);
            }
        };
        self.new_menu = false;
        let folder = project
            .path()
            .ancestors()
            .find(|path| path.join(".git").is_dir())
            .unwrap_or(project.path())
            .join(".keystone")
            .join("worktrees");
        let state = self.state.read(cx);
        let recorded = |name: &str| {
            state.store().workspace().sessions().iter().any(|s| {
                s.worktree()
                    .is_some_and(|w| w.branch == format!("keystone/{name}"))
            })
        };
        let name = (1..)
            .map(|n| format!("session-{n}"))
            .find(|name| !folder.join(name).exists() && !recorded(name))
            .unwrap_or_default();
        let field = self.worktree_input.focus_handle(cx);
        self.worktree_dialog = Some(WorktreeDialog::Create {
            focus: DialogFocus::with_body(field, window, cx),
            provider: Provider::Claude,
            error: None,
            task: None,
        });
        self.worktree_input.update(cx, |input, cx| {
            input.set_value(name, cx);
            input.select_all(&keystone_ui::text_input::SelectAll, window, cx);
        });
        self.refresh(cx);
    }

    /// Opens the Remove dialog for `session`'s worktree. Nothing is removed until
    /// Remove is pressed.
    pub(crate) fn open_remove_worktree(
        &mut self,
        session: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = None;
        let Some(worktree) = self
            .session(session, cx)
            .and_then(|s| s.worktree().cloned())
        else {
            self.error = Some("This session does not work in a worktree.".into());
            return self.refresh(cx);
        };
        if !worktree.directory.path().exists() {
            self.error = Some(format!(
                "Nothing to remove. The worktree folder {} is already gone; its branch {} stays. `git worktree prune` in the repository forgets it.",
                worktree.directory.as_str(),
                worktree.branch
            ));
            return self.refresh(cx);
        }
        self.worktree_dialog = Some(WorktreeDialog::Remove {
            worktree,
            focus: DialogFocus::new(window, cx),
            error: None,
            task: None,
        });
        self.refresh(cx);
    }

    /// Git cannot be stopped once it runs, so the dialog stays until it answers.
    fn close_worktree_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .worktree_dialog
            .as_ref()
            .is_some_and(WorktreeDialog::busy)
        {
            return;
        }
        self.worktree_dialog = None;
        self.focus_composer(window, cx);
        self.refresh(cx);
    }

    fn confirm_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.worktree_dialog.as_mut().filter(|d| !d.busy()) else {
            return;
        };
        match dialog {
            WorktreeDialog::Create {
                provider,
                error,
                task,
                ..
            } => {
                let provider = *provider;
                let name = self.worktree_input.read(cx).value().trim().to_owned();
                let project = match self.presentation.creation_directory() {
                    Ok(project) => project,
                    Err(e) => {
                        *error = Some(format!("No worktree created. {e}"));
                        return self.refresh(cx);
                    }
                };
                *error = None;
                *task = Some(cx.spawn_in(window, async move |this, cx| {
                    let created = cx
                        .background_executor()
                        .spawn(async move { worktree::create(project.path(), &name, None) })
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.worktree_created(provider, created, window, cx)
                    });
                }));
            }
            WorktreeDialog::Remove {
                worktree,
                error,
                task,
                ..
            } => {
                let state = self.state.read(cx);
                let running = state.store().workspace().sessions().iter().any(|s| {
                    matches!(s.context(), SessionContext::Recorded { directory, .. } if directory.path().starts_with(worktree.directory.path()))
                        && state.conversations().get(&s.id()).is_some_and(|c| c.running)
                });
                if running {
                    *error = Some(
                        "Not removed. A session's turn is still running in this worktree. Stop it first."
                            .into(),
                    );
                    return self.refresh(cx);
                }
                let worktree = worktree.clone();
                let Some(hold) = self.state.read(cx).hold_worktree(&worktree) else {
                    *error = Some("Not removed. Another window is removing this worktree.".into());
                    return self.refresh(cx);
                };
                *error = None;
                *task = Some(cx.spawn_in(window, async move |this, cx| {
                    let removed = cx
                        .background_executor()
                        .spawn(async move {
                            let _hold = hold;
                            worktree::remove(&worktree)
                        })
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| match removed {
                        Ok(()) => {
                            this.worktree_dialog = None;
                            this.focus_composer(window, cx);
                            this.refresh(cx);
                        }
                        Err(error) => {
                            if let Some(dialog) = &mut this.worktree_dialog {
                                dialog.set_error(format!("Not removed. {error}"));
                            }
                            this.refresh(cx);
                        }
                    });
                }));
            }
        }
        self.refresh(cx);
    }

    fn worktree_created(
        &mut self,
        provider: Provider,
        created: std::io::Result<WorktreeContext>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let worktree = match created {
            Ok(worktree) => worktree,
            Err(error) => {
                if let Some(dialog) = &mut self.worktree_dialog {
                    dialog.set_error(format!("No worktree created. {error}"));
                }
                return self.refresh(cx);
            }
        };
        self.worktree_dialog = None;
        let window_id = self.window_id;
        self.create_with(cx, |state, cx| {
            state.create_worktree_showing(provider, &worktree, now_ms(), window_id, None, cx)
        });
        let created = self
            .state
            .read(cx)
            .store()
            .workspace()
            .sessions()
            .iter()
            .any(|s| s.worktree() == Some(&worktree));
        if !created {
            let error = self.error.take().unwrap_or_default();
            self.error = Some(format!(
                "{error} The worktree at {} and branch {} were created and are kept.",
                worktree.directory.as_str(),
                worktree.branch
            ));
        } else if self
            .presentation
            .project
            .as_ref()
            .is_ok_and(|project| !project.contains(worktree.directory.path()))
        {
            self.open_project(ProjectId::of(&worktree.repository), cx);
        }
        self.focus_composer(window, cx);
        self.refresh(cx);
    }

    fn set_worktree_provider(&mut self, chosen: Provider, cx: &mut Context<Self>) {
        if let Some(WorktreeDialog::Create { provider, .. }) = &mut self.worktree_dialog {
            *provider = chosen;
            self.refresh(cx);
        }
    }

    pub(crate) fn worktree_dialog_view(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let dialog = self.worktree_dialog.as_ref()?;
        let colors = cx.theme().colors;
        let busy = dialog.busy();
        let cancel = cx.listener(|this, _: &(), window, cx| this.close_worktree_dialog(window, cx));
        let confirm =
            Rc::new(cx.listener(|this, _: &(), window, cx| this.confirm_worktree(window, cx)));
        let status = |error: &Option<String>, working: &'static str| {
            error
                .clone()
                .map(|error| {
                    div()
                        .automation_id("worktree-error")
                        .child(Label::new(error).size(TextSize::Sm).color(Color::Danger))
                })
                .or_else(|| {
                    busy.then(|| {
                        div()
                            .automation_id("worktree-busy")
                            .child(Label::new(working).size(TextSize::Sm).color(Color::Muted))
                    })
                })
        };
        let dialog = match dialog {
            WorktreeDialog::Create {
                focus,
                provider,
                error,
                ..
            } => {
                let submit = confirm.clone();
                let field = div()
                    .px(px(10.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.border_focused)
                    .bg(colors.background)
                    .automation_id("worktree-name")
                    .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "enter" {
                            submit(&(), window, cx);
                            cx.stop_propagation();
                        }
                    })
                    .child(self.worktree_input.clone());
                let pick = |chosen: Provider, id: &'static str, cx: &mut Context<Self>| {
                    Button::new(id, crate::model_selector::provider_name(chosen))
                        .icon(crate::provider_icon(chosen))
                        .selected(*provider == chosen)
                        .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                            this.set_worktree_provider(chosen, cx)
                        }))
                };
                let providers = h_flex()
                    .gap(px(6.))
                    .child(div().automation_id("worktree-claude").child(pick(
                        Provider::Claude,
                        "worktree-claude",
                        cx,
                    )))
                    .child(div().automation_id("worktree-codex").child(pick(
                        Provider::Codex,
                        "worktree-codex",
                        cx,
                    )));
                let body = v_flex()
                    .gap(spacing::SM)
                    .child(field)
                    .child(providers)
                    .children(status(error, "Creating the worktree…"));
                ConfirmDialog::new(
                    "worktree-dialog",
                    focus,
                    "New session in worktree",
                    "Adds .keystone/worktrees/<name> on a new branch keystone/<name> from the project's current HEAD. The session works there.",
                )
                .icon(IconName::Branch)
                .tone(Color::Accent)
                .confirm_label("Create")
                .body(body)
            }
            WorktreeDialog::Remove {
                worktree,
                focus,
                error,
                ..
            } => {
                let body = v_flex()
                    .gap(spacing::SM)
                    .child(
                        Label::new(crate::projects::display_path(worktree.directory.path()))
                            .size(TextSize::Sm)
                            .color(Color::Muted),
                    )
                    .children(status(error, "Checking the worktree…"));
                ConfirmDialog::new(
                    "remove-worktree-dialog",
                    focus,
                    "Remove worktree",
                    format!(
                        "Removes the folder only when it has no uncommitted changes and every commit on its branch is pushed or on another local branch. The branch {} stays, and so does this conversation.",
                        worktree.branch
                    ),
                )
                .icon(IconName::Branch)
                .tone(Color::Warning)
                .confirm_label("Remove")
                .body(body)
            }
        };
        Some(
            dialog
                .on_cancel(move |window, cx| cancel(&(), window, cx))
                .on_confirm(move |window, cx| confirm(&(), window, cx)),
        )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "worktrees_tests.rs"]
mod tests;
