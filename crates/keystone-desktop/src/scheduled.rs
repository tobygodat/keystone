//! The scheduler: runs each scheduled task when it comes due, while Keystone runs, and
//! once at launch for a task that came due while it was closed. See
//! `docs/design/scheduled-tasks.md`.

use super::*;
use crate::app_state::delivery::Hold;
use keystone_core::{Outcome, ScheduledTask, Target, TaskFields};
use std::collections::HashSet;

/// How long a run into a session whose history is loading waits before it skips.
const LOAD_WAIT_MS: u64 = 5_000;
/// How often a waiting run checks the session's history again.
const LOAD_POLL_MS: u64 = 250;
/// How long a run whose first save failed waits to try again.
const RETRY_MS: u64 = 5_000;
const NOT_SENT: &str = "The prompt was not sent.";

#[derive(Default)]
pub(crate) struct Scheduler {
    tasks: Vec<ScheduledTask>,
    /// Tasks the user asked to run now that have not run yet.
    run_now: HashSet<u64>,
    /// Tasks waiting on an existing session's history, since when.
    waiting: HashMap<u64, u64>,
    /// The next check. Replacing it cancels the one before.
    timer: Option<gpui::Task<()>>,
    /// The last save that failed, shown in Settings until a later one succeeds.
    pub(crate) error: Option<String>,
}

/// The local UTC offset in seconds at `at`.
pub(crate) fn offset(at: u64) -> i64 {
    crate::relative_time::LocalTime::at(at).offset_secs
}

fn hold_reason(hold: Hold) -> &'static str {
    match hold {
        Hold::Settled => "The session is settled.",
        Hold::Loading => "The session's history did not load in time.",
        Hold::Recovery => "Crash recovery holds the session.",
        Hold::Turn => "A turn was running in the session.",
        Hold::Answer => "The session waits for an approval or an answer.",
    }
}

impl AppState {
    /// Reads the saved tasks and wakes when the first is due. Called once, at launch.
    pub(crate) fn load_scheduled_tasks(&mut self, cx: &mut Context<Self>) {
        match self.store.database().scheduled_tasks() {
            Ok(tasks) => self.scheduler.tasks = tasks,
            Err(error) => {
                self.scheduler.error = Some(format!("Scheduled tasks could not be read. {error}"))
            }
        }
        self.arm_scheduler(cx);
    }

    pub(crate) fn scheduled_tasks(&self) -> &[ScheduledTask] {
        &self.scheduler.tasks
    }

    pub(crate) fn scheduler_error(&self) -> Option<&str> {
        self.scheduler.error.as_deref()
    }

    pub(crate) fn add_scheduled_task(
        &mut self,
        fields: TaskFields,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let now = now_ms();
        // Unix milliseconds, so an ID is never reused after a delete.
        let id = self
            .scheduler
            .tasks
            .iter()
            .map(|task| task.id() + 1)
            .fold(now, u64::max);
        self.save_task(ScheduledTask::new(id, fields, now, offset), cx)
    }

    pub(crate) fn pause_scheduled_task(
        &mut self,
        id: u64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.scheduler.waiting.remove(&id);
        self.change_task(id, ScheduledTask::pause, cx)
    }

    pub(crate) fn resume_scheduled_task(
        &mut self,
        id: u64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.change_task(id, |task| task.resume(now_ms(), offset), cx)
    }

    pub(crate) fn delete_scheduled_task(
        &mut self,
        id: u64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.store
            .database()
            .remove_scheduled_task(id)
            .map_err(|error| error.to_string())?;
        self.scheduler.tasks.retain(|task| task.id() != id);
        self.scheduler.run_now.remove(&id);
        self.scheduler.waiting.remove(&id);
        self.arm_scheduler(cx);
        self.refresh(cx);
        Ok(())
    }

    /// Runs `id` at once, leaving its next run where it was.
    pub(crate) fn run_scheduled_task_now(&mut self, id: u64, cx: &mut Context<Self>) {
        // A history wait left from an earlier run does not shorten this one's.
        if self.scheduler.run_now.insert(id) {
            self.scheduler.waiting.remove(&id);
        }
        self.check_schedules(cx);
    }

    fn change_task(
        &mut self,
        id: u64,
        change: impl FnOnce(&mut ScheduledTask),
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(mut task) = self.scheduler.tasks.iter().find(|t| t.id() == id).cloned() else {
            return Err("The task was deleted.".into());
        };
        change(&mut task);
        self.save_task(task, cx)
    }

    /// Saves `task` and then shows it, replacing the task with its ID.
    fn save_task(&mut self, task: ScheduledTask, cx: &mut Context<Self>) -> Result<(), String> {
        self.store
            .database()
            .save_scheduled_task(&task)
            .map_err(|error| error.to_string())?;
        self.scheduler.error = None;
        match self
            .scheduler
            .tasks
            .iter_mut()
            .find(|t| t.id() == task.id())
        {
            Some(old) => *old = task,
            None => self.scheduler.tasks.push(task),
        }
        self.arm_scheduler(cx);
        self.refresh(cx);
        Ok(())
    }

    /// Wakes for the earliest next run, and at least once a minute to read the wall
    /// clock again, since the timer's clock stops while the computer sleeps. A run that
    /// could not go yet is checked again sooner.
    fn arm_scheduler(&mut self, cx: &mut Context<Self>) {
        let now = now_ms();
        let tasks = &self.scheduler.tasks;
        let delay = if tasks.iter().any(|task| task.due(now).is_some())
            || !self.scheduler.run_now.is_empty()
        {
            if self.quitting() {
                RETRY_MS
            } else if !self.scheduler.waiting.is_empty() {
                LOAD_POLL_MS
            } else if self.scheduler.error.is_some() {
                RETRY_MS
            } else {
                0
            }
        } else {
            match tasks.iter().filter_map(ScheduledTask::next_run).min() {
                Some(next) => (next - now).min(crate::limit::CLOCK_CHECK_MS),
                None => {
                    self.scheduler.timer = None;
                    return;
                }
            }
        };
        self.scheduler.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay))
                .await;
            let _ = this.update(cx, |state, cx| state.check_schedules(cx));
        }));
    }

    /// Whether Keystone is quitting, now or once no turn runs. Nothing starts then, and
    /// what is due runs at the next launch if the quit goes through.
    fn quitting(&self) -> bool {
        self.phase != Phase::Running || self.quit_when_idle
    }

    /// Runs every task that is due, or that the user asked to run now.
    fn check_schedules(&mut self, cx: &mut Context<Self>) {
        if !self.quitting() {
            let now = now_ms();
            let mut runs: Vec<(u64, Option<u64>)> = self
                .scheduler
                .tasks
                .iter()
                .filter_map(|task| Some((task.id(), Some(task.due(now)?))))
                .collect();
            // A due task the user also asked to run now runs once.
            let scheduler = &mut self.scheduler;
            scheduler
                .run_now
                .retain(|id| !runs.iter().any(|(due, _)| due == id));
            scheduler.waiting.retain(|id, _| {
                scheduler.run_now.contains(id) || runs.iter().any(|(due, _)| due == id)
            });
            runs.extend(scheduler.run_now.iter().map(|&id| (id, None)));
            for (id, due) in runs {
                if self.run_task(id, due, cx) && due.is_none() {
                    self.scheduler.run_now.remove(&id);
                }
            }
        } else {
            // A history wait starts again once the quit is called off.
            self.scheduler.waiting.clear();
        }
        if self.phase != Phase::Exiting {
            self.arm_scheduler(cx);
        }
    }

    /// Runs task `id`, `due` at a time or now. Returns false when it has to wait: for
    /// its session's history, or because its run could not be saved first.
    fn run_task(&mut self, id: u64, due: Option<u64>, cx: &mut Context<Self>) -> bool {
        let Some(task) = self.scheduler.tasks.iter().find(|t| t.id() == id).cloned() else {
            return true;
        };
        let now = now_ms();
        if let Target::Session { session } = task.target()
            && self.session(session).is_some()
            && self.result_hold(session) == Some(Hold::Loading)
        {
            let since = *self.scheduler.waiting.entry(id).or_insert(now);
            if now < since + LOAD_WAIT_MS {
                self.ensure_history(session);
                return false;
            }
        }
        self.scheduler.waiting.remove(&id);
        // Saved before the run starts, so a crash during it never runs it again.
        let mut ran = task.clone();
        ran.begin(due, now, offset);
        if let Err(error) = self.store.database().save_scheduled_task(&ran) {
            self.scheduler.error = Some(format!(
                "\u{201c}{}\u{201d} did not run and tries again shortly. {error}",
                task.title()
            ));
            self.refresh(cx);
            return false;
        }
        let outcome = self.start_run(&task, cx);
        ran.finish(outcome);
        if let Err(error) = self.save_task(ran.clone(), cx) {
            self.scheduler.error = Some(format!(
                "\u{201c}{}\u{201d} ran, but its run was not saved. {error}",
                task.title()
            ));
            if let Some(old) = self.scheduler.tasks.iter_mut().find(|t| t.id() == id) {
                *old = ran;
            }
            self.refresh(cx);
        }
        true
    }

    fn start_run(&mut self, task: &ScheduledTask, cx: &mut Context<Self>) -> Outcome {
        let draft = |session| ComposerDraft {
            session,
            text: task.prompt().to_owned(),
            attachments: Vec::new(),
        };
        match task.target() {
            Target::New { provider } => {
                let directory = match WorkingDirectory::new(task.project().path()) {
                    Ok(directory) => directory,
                    Err(error) => {
                        return Outcome::Failed {
                            session: None,
                            reason: format!("The project folder could not be opened. {error}"),
                        };
                    }
                };
                let session = match self.create_quick(provider, None, None, &directory, cx) {
                    Ok(applied) => applied.value,
                    Err(error) => {
                        return Outcome::Failed {
                            session: None,
                            reason: format!("No session was created. {error}"),
                        };
                    }
                };
                // A title that cannot be saved leaves the placeholder; the run still goes.
                let _ = self.rename(session, task.title(), cx);
                if self.send_message_from(draft(session), None, SendKind::Scheduled, cx) {
                    return Outcome::Started { session };
                }
                // The prompt waits in the composer, where the user can send it.
                let _ = self.set_draft(session, task.prompt().to_owned(), cx);
                let alert = vec![(session, crate::notifications::Alert::TurnFailed)];
                cx.defer(move |cx| crate::notifications::offer(alert, cx));
                self.failed_send(session)
            }
            Target::Session { session } => {
                if self.session(session).is_none() {
                    return Outcome::Skipped {
                        reason: "The session no longer exists.".into(),
                    };
                }
                if let Some(hold) = self.result_hold(session) {
                    return Outcome::Skipped {
                        reason: hold_reason(hold).into(),
                    };
                }
                if self.restoring(session) {
                    return Outcome::Skipped {
                        reason: "Files were being restored in its repository.".into(),
                    };
                }
                // A message would cancel the continue the user scheduled.
                if self
                    .session(session)
                    .and_then(|s| s.continue_at())
                    .is_some()
                {
                    return Outcome::Skipped {
                        reason: "The session waits to continue after its usage limit.".into(),
                    };
                }
                if self.send_message_from(draft(session), None, SendKind::Scheduled, cx) {
                    Outcome::Started { session }
                } else {
                    self.failed_send(session)
                }
            }
        }
    }

    fn failed_send(&self, session: u64) -> Outcome {
        let reason = self
            .conversations
            .get(&session)
            .and_then(|conversation| conversation.error.clone())
            .unwrap_or_else(|| NOT_SENT.into());
        Outcome::Failed {
            session: Some(session),
            reason,
        }
    }
}
