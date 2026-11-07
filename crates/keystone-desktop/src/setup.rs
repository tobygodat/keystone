//! A new worktree's setup script (see [`keystone_agent::setup`]), from its start to
//! the session's first message. The session shows the script's progress in its status
//! area, and no message goes while it runs or after it fails, until the user retries
//! or skips it. Nothing about a setup is saved: a restart forgets it.

use super::{AppState, Phase, RowChange};
use crate::view_state::ViewKey;
use crate::{Keystone, arrangement};
use gpui::{
    AnyElement, App, Context, FocusHandle, InteractiveElement, IntoElement, ParentElement, Styled,
    Window, div, prelude::FluentBuilder, px,
};
use keystone_agent::setup::{self, KEPT_LINES, SetupEvent, SetupExit, SetupRun};
use keystone_core::{ProjectId, WorktreeContext};
use keystone_ui::prelude::*;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

gpui::actions!(
    keystone,
    [RetrySetup, SkipSetup, CancelSetup, EditSetupScript]
);

/// Output lines the card shows.
const SHOWN_LINES: usize = 12;
/// How long quitting lets running scripts end before it kills them.
const QUIT_GRACE: Duration = Duration::from_millis(1500);

pub(crate) const STILL_RUNNING: &str = "Not sent. The setup script is still running.";
pub(crate) const NOT_DONE: &str = "Not sent. Retry or skip the setup script first.";
const BEING_REMOVED: &str = "its worktree is being removed.";
const NOT_INTACT: &str = "its worktree folder is missing or is no longer its worktree.";

pub(crate) enum SetupState {
    Running(SetupRun),
    /// Cancel was pressed and the script has not exited yet.
    Stopping(SetupRun),
    Succeeded {
        took: Duration,
    },
    Failed(SetupExit),
    Cancelled,
    CouldNotStart(String),
}

pub(crate) struct Setup {
    project: ProjectId,
    worktree: WorktreeContext,
    pub(crate) output: VecDeque<String>,
    started: Instant,
    pub(crate) state: SetupState,
}

impl Setup {
    pub(crate) fn running(&self) -> bool {
        matches!(self.state, SetupState::Running(_) | SetupState::Stopping(_))
    }

    fn run(&self) -> Option<&SetupRun> {
        match &self.state {
            SetupState::Running(run) | SetupState::Stopping(run) => Some(run),
            _ => None,
        }
    }

    /// Why a message to its session cannot go now, if it cannot.
    fn holds_send(&self) -> Option<&'static str> {
        match self.state {
            SetupState::Running(_) | SetupState::Stopping(_) => Some(STILL_RUNNING),
            SetupState::Succeeded { .. } => None,
            SetupState::Failed(_) | SetupState::Cancelled | SetupState::CouldNotStart(_) => {
                Some(NOT_DONE)
            }
        }
    }

    fn apply(&mut self, event: SetupEvent) {
        match event {
            SetupEvent::Line(line) => {
                if self.output.len() == KEPT_LINES {
                    self.output.pop_front();
                }
                self.output.push_back(line);
            }
            SetupEvent::Exited(exit) => {
                self.state = match (&self.state, exit.succeeded()) {
                    (SetupState::Stopping(_), _) => SetupState::Cancelled,
                    (_, true) => SetupState::Succeeded {
                        took: self.started.elapsed(),
                    },
                    (_, false) => SetupState::Failed(exit),
                };
            }
        }
    }
}

impl AppState {
    pub(crate) fn setup(&self, session: u64) -> Option<&Setup> {
        self.setups.get(&session)
    }

    /// Runs `project`'s setup script in `worktree` for `session`. Does nothing when the
    /// project has no script.
    pub(crate) fn start_setup(
        &mut self,
        session: u64,
        project: ProjectId,
        worktree: WorktreeContext,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running {
            return;
        }
        let Some(script) = self
            .store
            .workspace()
            .setup_script(&project)
            .map(str::to_owned)
        else {
            return;
        };
        let started = Instant::now();
        let state = self.launch(session, &project, &worktree, &script, started, cx);
        self.setups.insert(
            session,
            Setup {
                project,
                worktree,
                output: VecDeque::new(),
                started,
                state,
            },
        );
        self.conversation(session);
        self.setup_state_changed(session, cx);
    }

    /// Starts `script` in `worktree` unless the worktree is being removed or is no
    /// longer intact, and ticks the session's elapsed time while it runs.
    fn launch(
        &self,
        session: u64,
        project: &ProjectId,
        worktree: &WorktreeContext,
        script: &str,
        started: Instant,
        cx: &mut Context<Self>,
    ) -> SetupState {
        if self
            .removing_worktrees
            .lock()
            .unwrap()
            .contains(worktree.directory.as_str())
        {
            return SetupState::CouldNotStart(BEING_REMOVED.into());
        }
        if !keystone_core::worktree::is_intact(worktree) {
            return SetupState::CouldNotStart(NOT_INTACT.into());
        }
        let wake = self.wake.clone();
        let run = match setup::start(
            &self.setup_shell,
            script,
            worktree.directory.path(),
            project.path(),
            move || wake(),
        ) {
            Ok(run) => run,
            Err(error) => return SetupState::CouldNotStart(error.to_string()),
        };
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let ticking = this.update(cx, |this, cx| {
                    let running = this
                        .setups
                        .get(&session)
                        .is_some_and(|s| s.started == started && s.running());
                    if running {
                        this.setup_changed(session, cx);
                    }
                    running
                });
                if !matches!(ticking, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
        SetupState::Running(run)
    }

    /// Clears the notice of a send the setup held, since what holds it changed, and
    /// redraws.
    fn setup_state_changed(&mut self, session: u64, cx: &mut Context<Self>) {
        if let Some(conversation) = self.conversations.get_mut(&session)
            && conversation
                .error
                .as_deref()
                .is_some_and(|error| error == STILL_RUNNING || error == NOT_DONE)
        {
            conversation.error = None;
        }
        self.setup_changed(session, cx);
    }

    /// Redraws the session's status area, where its setup shows.
    fn setup_changed(&mut self, session: u64, cx: &mut Context<Self>) {
        if let Some(conversation) = self.conversations.get(&session) {
            self.changes.rows.push(RowChange {
                session,
                rows: conversation.rows.len(),
                changed: Vec::new(),
                replies: Vec::new(),
            });
        }
        self.publish(cx);
    }

    /// Applies what each running script reported since the last pump.
    pub(crate) fn pump_setups(&mut self, cx: &mut Context<Self>) {
        let mut changed = Vec::new();
        for (&session, setup) in &mut self.setups {
            let Some(run) = setup.run() else {
                continue;
            };
            let events = run.drain();
            if events.is_empty() {
                continue;
            }
            let before = std::mem::discriminant(&setup.state);
            for event in events {
                setup.apply(event);
            }
            changed.push((session, std::mem::discriminant(&setup.state) != before));
        }
        for (session, state_changed) in changed {
            if state_changed {
                self.setup_state_changed(session, cx);
            } else {
                self.setup_changed(session, cx);
            }
        }
    }

    pub(crate) fn cancel_setup(&mut self, session: u64, cx: &mut Context<Self>) {
        let Some(setup) = self.setups.get_mut(&session) else {
            return;
        };
        let previous = std::mem::replace(&mut setup.state, SetupState::Cancelled);
        if let SetupState::Running(run) = previous {
            run.stop();
            setup.state = SetupState::Stopping(run);
            self.setup_state_changed(session, cx);
        } else {
            setup.state = previous;
        }
    }

    /// Runs the project's script again, as it is saved now, so an edit applies. A
    /// project whose script was removed meanwhile lets the session's messages go.
    pub(crate) fn retry_setup(&mut self, session: u64, cx: &mut Context<Self>) {
        if self.phase != Phase::Running {
            return;
        }
        if !self.setups.get(&session).is_some_and(|s| {
            matches!(
                s.state,
                SetupState::Failed(_) | SetupState::Cancelled | SetupState::CouldNotStart(_)
            )
        }) {
            return;
        }
        let Some(mut setup) = self.setups.remove(&session) else {
            return;
        };
        if let Some(script) = self
            .store
            .workspace()
            .setup_script(&setup.project)
            .map(str::to_owned)
        {
            setup.output.clear();
            setup.started = Instant::now();
            setup.state = self.launch(
                session,
                &setup.project,
                &setup.worktree,
                &script,
                setup.started,
                cx,
            );
            self.setups.insert(session, setup);
        }
        self.setup_state_changed(session, cx);
    }

    /// Lets the session's messages go without the script having succeeded.
    pub(crate) fn skip_setup(&mut self, session: u64, cx: &mut Context<Self>) {
        if self.setups.get(&session).is_some_and(|s| !s.running()) {
            self.setups.remove(&session);
            self.setup_state_changed(session, cx);
        }
    }

    /// Opens the worktree's folder with the system's file manager, off the UI thread.
    pub(crate) fn open_setup_folder(&mut self, session: u64, cx: &mut Context<Self>) {
        let Some(path) = self
            .setups
            .get(&session)
            .map(|s| s.worktree.directory.path().to_owned())
        else {
            return;
        };
        let (done, opened) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("setup-open-folder".into())
            .spawn(move || {
                let _ = done.send(crate::files::open_with_system(&path));
            });
        if spawned.is_err() {
            return;
        }
        cx.spawn(async move |this, cx| {
            if opened.await == Ok(false) {
                let _ = this.update(cx, |this, cx| {
                    this.conversation(session).error = Some("Could not open the folder.".into());
                    this.setup_changed(session, cx);
                });
            }
        })
        .detach();
    }

    /// Why a message to `session` waits for its setup, if it does. A send that went
    /// forgets a finished setup.
    pub(crate) fn setup_holds_send(&self, session: u64) -> Option<&'static str> {
        self.setups.get(&session).and_then(Setup::holds_send)
    }

    pub(crate) fn setup_sent(&mut self, session: u64) {
        self.setups.remove(&session);
    }

    /// Saves `script` as `project`'s setup script. See
    /// [`keystone_core::Store::set_project_setup_script`].
    pub(crate) fn set_project_setup_script(
        &mut self,
        project: &ProjectId,
        script: String,
        cx: &mut Context<Self>,
    ) -> Result<keystone_core::Applied<()>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        let result = self.store.set_project_setup_script(project, Some(script));
        self.publish(cx);
        result
    }

    /// Whether a setup script is running in `worktree`.
    pub(crate) fn setup_running_in(&self, worktree: &WorktreeContext) -> bool {
        self.setups
            .values()
            .any(|s| s.running() && s.worktree == *worktree)
    }

    /// Stops every running script, killing what is left of each after a short grace.
    pub(crate) fn stop_setups(&mut self) {
        let runs = self.setups.values().filter_map(Setup::run);
        if !setup::stop_and_wait(runs, QUIT_GRACE) {
            eprintln!("Keystone stopped a setup script that had not ended yet.");
        }
        self.setups.clear();
    }
}

/// The card's buttons, which take keyboard focus in the view the composer sends to.
pub(crate) struct SetupFocus {
    cancel: FocusHandle,
    retry: FocusHandle,
    skip: FocusHandle,
    open: FocusHandle,
}

impl SetupFocus {
    pub(crate) fn new(cx: &mut App) -> Self {
        let stop = || cx.focus_handle().tab_stop(true);
        Self {
            cancel: stop(),
            retry: stop(),
            skip: stop(),
            open: stop(),
        }
    }
}

fn seconds(duration: Duration) -> String {
    format!("{} s", duration.as_secs())
}

/// The first line of `script`, as the worktree dialog names it.
pub(crate) fn first_line(script: &str) -> &str {
    script.lines().next().unwrap_or_default().trim()
}

impl Keystone {
    /// The setup card in `key`'s status area, while the session has a setup.
    pub(crate) fn setup_card(&self, key: ViewKey, cx: &Context<Self>) -> Option<AnyElement> {
        let session = key.session;
        let setup = self.state.read(cx).setup(session)?;
        let colors = cx.theme().colors;
        let selector = |name: &str| arrangement::selector(name, key.view);
        let (message, mark) = match &setup.state {
            SetupState::Running(_) => ("Running setup script\u{2026}".to_owned(), None),
            SetupState::Stopping(_) => ("Cancelling setup script\u{2026}".to_owned(), None),
            SetupState::Succeeded { took } => (
                format!("Setup script finished in {}.", seconds(*took)),
                None,
            ),
            SetupState::Failed(SetupExit::Code(code)) => (
                format!("Setup script failed (exit {code})."),
                Some((IconName::Close, Color::Danger)),
            ),
            SetupState::Failed(SetupExit::Signal(signal)) => (
                format!("Setup script failed (signal {signal})."),
                Some((IconName::Close, Color::Danger)),
            ),
            SetupState::Cancelled => (
                "Setup script cancelled.".to_owned(),
                Some((IconName::WedgeFilled, Color::Warning)),
            ),
            SetupState::CouldNotStart(error) => (
                format!("Setup script could not start: {error}"),
                Some((IconName::Close, Color::Danger)),
            ),
        };
        // The status color sits on the mark. The text stays ink, brighter when it needs
        // the user.
        let attention = mark.is_some();
        let color = if attention {
            Color::Default
        } else {
            Color::Muted
        };
        if let SetupState::Succeeded { .. } = setup.state {
            return Some(
                div()
                    .automation_id(selector("setup"))
                    .child(Label::new(message).size(TextSize::Ui).color(color))
                    .into_any_element(),
            );
        }
        let keyboard = self.selected(cx) == Some(session)
            && self
                .presentation
                .current
                .views
                .send_target(&self.showing(session, cx), session)
                == Some(key);
        let focus = &self.setup_focus;
        let button = |name: &'static str, label: &'static str, handle: &FocusHandle| {
            let id = selector(name);
            let debug = id.clone();
            Button::new(id, label)
                .style(ButtonStyle::Subtle)
                .when(keyboard, |b| b.track_focus(handle))
                .debug_selector(move || debug.to_string())
        };
        let buttons = match setup.state {
            SetupState::Running(_) => vec![
                button("setup-cancel", "Cancel", &focus.cancel)
                    .icon(IconName::Close)
                    .on_click(self.setup_listener(cx, AppState::cancel_setup, session)),
            ],
            SetupState::Stopping(_) | SetupState::Succeeded { .. } => Vec::new(),
            SetupState::Failed(_) | SetupState::Cancelled | SetupState::CouldNotStart(_) => vec![
                button("setup-retry", "Retry", &focus.retry)
                    .style(ButtonStyle::Primary)
                    .icon(IconName::Refresh)
                    .on_click(self.setup_listener(cx, AppState::retry_setup, session)),
                button("setup-skip", "Skip", &focus.skip).on_click(self.setup_listener(
                    cx,
                    AppState::skip_setup,
                    session,
                )),
                button("setup-open", "Open folder", &focus.open)
                    .icon(IconName::Folder)
                    .on_click(self.setup_listener(cx, AppState::open_setup_folder, session)),
            ],
        };
        let shown = setup.output.len().saturating_sub(SHOWN_LINES);
        let (code_size, code_line) = cx.theme().transcript_code_text();
        let output = (!setup.output.is_empty()).then(|| {
            v_flex()
                .automation_id(selector("setup-output"))
                .p_2()
                .rounded(radius::CONTROL)
                .bg(colors.background)
                .font_mono(cx)
                .text_size(code_size)
                .line_height(code_line)
                .text_color(colors.text_muted)
                .children(
                    setup
                        .output
                        .iter()
                        .skip(shown)
                        .map(|line| div().child(line.clone())),
                )
        });
        let elapsed = setup.running().then(|| seconds(setup.started.elapsed()));
        Some(
            v_flex()
                .id(selector("setup"))
                .automation_id(selector("setup"))
                .key_context("Setup")
                .surface(Elevation::Raised, cx)
                .when(attention, |d| {
                    d.border_1().border_color(colors.border_strong)
                })
                .p_3()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .children(
                            mark.map(|(icon, color)| Icon::new(icon).size(px(14.)).color(color)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(message).size(TextSize::Ui).color(color)),
                        )
                        .children(elapsed.map(|elapsed| {
                            div()
                                .automation_id(selector("setup-elapsed"))
                                .child(Label::new(elapsed).size(TextSize::Sm).color(Color::Muted))
                        })),
                )
                .children(output)
                .when(!buttons.is_empty(), |d| {
                    d.child(h_flex().gap_2().children(buttons))
                })
                .into_any_element(),
        )
    }

    fn setup_listener(
        &self,
        cx: &Context<Self>,
        act: fn(&mut AppState, u64, &mut Context<AppState>),
        session: u64,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        self.scoped_listener(cx, move |this, _, window, cx| {
            this.state.update(cx, |state, cx| act(state, session, cx));
            // The pressed button may be gone, and the next keys are the user's message.
            this.focus_composer(window, cx);
        })
    }

    /// The palette's setup commands, acting on the selected session.
    pub(crate) fn setup_actions<E: InteractiveElement>(&self, root: E, cx: &Context<Self>) -> E {
        let on_selected = |act: fn(&mut AppState, u64, &mut Context<AppState>)| {
            move |this: &mut Self, cx: &mut Context<Self>| {
                if let Some(id) = this.selected(cx) {
                    this.state.update(cx, |state, cx| act(state, id, cx));
                }
            }
        };
        let retry = on_selected(AppState::retry_setup);
        let skip = on_selected(AppState::skip_setup);
        let cancel = on_selected(AppState::cancel_setup);
        root.on_action(self.scoped_listener(cx, move |this, _: &RetrySetup, _, cx| retry(this, cx)))
            .on_action(self.scoped_listener(cx, move |this, _: &SkipSetup, _, cx| skip(this, cx)))
            .on_action(
                self.scoped_listener(cx, move |this, _: &CancelSetup, _, cx| cancel(this, cx)),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &EditSetupScript, window, cx| {
                    this.open_project_settings(window, cx)
                }),
            )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "setup_tests.rs"]
mod tests;
