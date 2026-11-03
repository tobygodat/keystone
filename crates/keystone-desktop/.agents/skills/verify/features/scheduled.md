# Scheduled tasks

A saved prompt sent in a project on a schedule, while Keystone runs. A task that came due while Keystone was closed runs once at the next launch. See `docs/design/scheduled-tasks.md`.

## Sub-features

- `scheduled-store` saves each task in `metadata` under `scheduled_task/<id>` with a `scheduled_task.upserted` or `scheduled_task.removed` event: id, title, prompt, project, `schedule` (`{"kind":"every","minutes":N}` or `{"kind":"at","hour":H,"minute":M,"days":bits}`, bit 0 Sunday), `target` (`{"kind":"new","provider":"Claude"}` or `{"kind":"session","session":N}`), `next_run` (unix ms, `null` while paused) and the last 20 `runs` (`due`, `at`, `outcome`). The driver's `workspace` does not list them; read the table.
- `scheduled-library` is Settings → Scheduled: each task (`scheduled-row-<id>`) with its schedule, project, target and next run, and Run now, Pause or Resume and Delete (`scheduled-run-<id>`, `scheduled-pause-<id>`, `scheduled-delete-<id>`). Below each, its last five runs, newest first (`scheduled-history-<id>-<n>`), each with Open (`scheduled-open-<id>-<n>`) while its session exists. Open closes Settings and shows the session.
- `scheduled-form` adds a task: `scheduled-title`, `scheduled-prompt`, `scheduled-kind` (Every N minutes, Every day at, Weekdays at), `scheduled-when` (minutes, or HH:MM), `scheduled-project`, `scheduled-target` (a new Claude or Codex session, or one of the project's sessions) and `scheduled-add`. A missing title, prompt or project, a bad time, or a target outside the chosen project ("Choose where the task runs in this project.") is refused in `settings-error`; a failed scheduler save shows in `scheduled-problem`.
- `scheduled-run`: a new session is titled after the task, on the provider's default account and model, selected in no window. A send that fails leaves the prompt in its draft and records the run as failed. A run into an existing session never steers and never touches its draft; a busy session skips the run, saying why.
- `scheduled-catch-up`: at launch a task whose next run has passed runs once, with `due` before `at`.

## How to get to it (user POV)

- Ctrl+Comma, then Scheduled. Fill the form and Add.
- A run's Open, or the session in the sidebar.

## Driving it

- On a Mac, `python3 scripts/macos-scheduled.py` runs the whole journey: add a one-minute task by mouse, check it fires within 3 s of its saved next run, Open its session, quit past the next run and relaunch to see the catch-up, then Run now and Pause.
- With keystone-verify, `start` with the scripted providers and `KEYSTONE_PROJECT_DIR`, then do the same through `click` and `type`, reading the `metadata` table of the run's `database` for the task.

## Gotchas

- On macOS a `cargo run` build posts no notifications (no bundle, TOB-220), so the run's turn-finished notification shows only on Linux.
- Every N minutes counts from the last scheduled run, not from the clock's hour; Run now does not move the next run.
