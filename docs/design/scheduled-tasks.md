# Scheduled tasks

A scheduled task sends a saved prompt in a project on a schedule: every so many minutes, or at a time of day on chosen days. Each run starts a new Claude or Codex session in the project, or sends into one existing session. Settings → Scheduled lists the tasks with their last runs, and adds, pauses, resumes, runs now and deletes them. The behavior follows t3code's scheduled tasks (`apps/server/src/scheduledTasks/`). [ADR 0010](../adr/0010-terminal-browser-scheduling-editor-diffs-remote-devices.md) brings the feature into scope and leaves two questions to this design: whether Keystone must be running for a task to fire (below), and what a run does at an approval (under What a run does). Webhook triggers and Monos are out of scope.

## Tasks fire only while Keystone runs

Keystone checks its tasks itself, and a task whose time comes while Keystone is closed runs once at the next launch. There is no background helper.

A helper would need a launchd agent on macOS and a systemd unit on Linux. It would also have to run sessions without the app, which holds the workspace's file lock and owns every provider process, so in practice it would have to be Keystone. It also keeps Quit's meaning from ADR 0001: an explicit Quit ends everything, with nothing left running. The cost is that a laptop that is shut, or a Keystone that is not open, runs nothing on time. The next launch catches up instead.

- **On launch.** A task whose next run passed while Keystone was closed runs once, however many times it came due meanwhile. Its run records both when it was due and when it ran, so the history shows it ran late. Its next run then counts from the launch.
- **While running.** A timer wakes at the earliest next run. It wakes at least once a minute to read the wall clock again, because the timer's clock stops while the computer sleeps. A run due during sleep therefore goes within a minute of waking.
- **While quitting.** Nothing starts once Quit begins, or while Keystone waits to quit for a running turn after its last window closed. What comes due then runs at the next launch.

## Schedules

- **Every N minutes**, at least 1. The next run counts from when the last one ran, in elapsed time, so daylight saving changes don't move it.
- **At HH:MM on chosen days**, in local time. The form offers every day or weekdays (Monday to Friday). The task stores the days as a set of weekdays, so a later form can offer any set. When clocks spring forward, a time that doesn't exist runs an hour later on the clock. When they fall back, a time that comes twice runs at its first occurrence only.

Run now runs the task at once and leaves its next run where it was. On a task that is already due, Run now is that due run. Pausing clears the next run, and resuming counts it from the moment of resuming. A paused task never catches up.

## What a run does

A **new session** is created in the task's project on the chosen provider's default account and model, titled with the task's title, selected in no window. The prompt goes as its first message. Its turn then notifies like any session nobody is looking at: turn finished, approval requested or turn failed (see `notifications.rs`). A run uses the provider's own approval mode, like any new session. When it hits an approval, it waits for the user, who gets the Approval requested notification. Keystone never approves on the user's behalf (ADR 0001). If Keystone refuses the message before it goes, for example because the session's account is gone, the prompt stays in the session's draft, the session shows why, and the run is recorded as failed. Started means the prompt was handed to the provider. A provider that then fails, such as a CLI that cannot start, fails the turn in the session, which notifies as Turn failed, and the prompt stays in its transcript.

An **existing session** gets the prompt as a new turn, and never as a steer. A run skips, saying why, when the session is gone, settled, mid-turn, waiting for an approval or an answer, held by crash recovery, restoring files in its repository, or waiting to continue after a usage limit (a message would cancel that continue). While the session's history loads, the run waits up to 5 seconds before it skips. The session's draft is never touched.

## Saving

Each task is one row in the database's `metadata` table under `scheduled_task/<id>`, with a `scheduled_task.upserted` or `scheduled_task.removed` event in the same transaction. This is the pattern account usage and integration copies use. It needs no schema version, and older builds leave the rows alone. A task keeps its last 20 runs.

A run goes at most once. Before it starts, the task is saved with its next run moved on and the run recorded as failed because Keystone stopped during it. Once the run has started or failed, that record is replaced with what happened. A crash in between leaves the honest record, and the run does not repeat on the next launch. Task edits apply in memory and join Store's ordered save queue. Failed edits stay visible with the shared save error and retry on the next change or Quit. A run waits asynchronously for its claim and every save before its send to become durable. If saving fails, nothing runs, Settings shows the error, and the task retries saving 5 seconds later. While waiting, it reuses one new session and checks an existing session's holds again before sending. Pausing or deleting a task cancels its pending unsent run. Quit flushes task changes through the same Store queue.

## Code

- `keystone-core/src/schedule.rs` holds the task, its schedule and its runs, and computes the next run. It takes the local UTC offset as a function, so it stays free of the clock and time zone and its tests can fix both. `Store::save_scheduled_task`, `remove_scheduled_task` and `scheduled_tasks` apply changes and read tasks in memory. The database loads their metadata rows with the workspace at startup and commits their projections with each Store snapshot.
- `keystone-desktop/src/scheduled.rs` is the scheduler on `AppState`: it keeps the timer and pending durable claims and runs tasks. It sends through `send_message_from` with `SendKind::Scheduled`, which, like a launched session's result, only ever starts a new turn.
- `keystone-desktop/src/settings_scheduled.rs` is the Settings section.

## Not done

Cron expressions (the form's every-N-minutes and time-of-day-on-days schedules cover the common ones), editing a task (delete it and add it again), choosing an account, model or worktree for a task's sessions, any set of days other than every day or weekdays in the form, and running while Keystone is closed.
