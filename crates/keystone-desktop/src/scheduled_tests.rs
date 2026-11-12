use super::*;
use keystone_core::{Outcome, Schedule, ScheduledTask, Target, TaskFields};

const HOUR: u64 = 3_600_000;

fn fields(dir: &Path, title: &str, target: Target) -> TaskFields {
    TaskFields {
        title: title.into(),
        prompt: "Summarize what changed".into(),
        project: keystone_core::ProjectId::new(dir).unwrap(),
        schedule: Schedule::Every { minutes: 60 },
        target,
    }
}

fn task(state: &Entity<AppState>, cx: &mut gpui::TestAppContext) -> ScheduledTask {
    state.read_with(cx, |state, _| state.scheduled_tasks()[0].clone())
}

#[gpui::test]
fn a_task_that_came_due_while_closed_runs_once_at_launch_in_a_new_titled_session(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("workspace.json")).unwrap();
    let database = store.database();
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    connect(&state, dir.path(), 5, false, cx);
    let created = now_ms() - 3 * HOUR;
    let saved = ScheduledTask::new(
        1,
        fields(
            dir.path(),
            "Nightly summary",
            Target::New {
                provider: Provider::Claude,
            },
        ),
        created,
        crate::app_state::scheduled::offset,
    );
    let due = saved.next_run().unwrap();
    state.update(cx, |state, _| {
        state.store.save_scheduled_task(saved.clone());
        state.store.flush().unwrap();
    });

    state.update(cx, |state, cx| state.load_scheduled_tasks(cx));
    wait_until(cx, &state, |state| {
        state
            .store
            .workspace()
            .sessions()
            .first()
            .is_some_and(|session| completed(state, session.id()))
    });
    cx.run_until_parked();

    let (session, title, draft, count) = state.read_with(cx, |state, _| {
        let sessions = state.store.workspace().sessions();
        let first = &sessions[0];
        (
            first.id(),
            first.title().to_owned(),
            first.draft().to_owned(),
            sessions.len(),
        )
    });
    assert_eq!(count, 1, "three missed runs start one session");
    assert_eq!(title, "Nightly summary");
    assert_eq!(draft, "", "the prompt was sent, not left as a draft");
    let ran = task(&state, cx);
    assert_eq!(ran.runs().len(), 1);
    assert_eq!(ran.runs()[0].due, Some(due), "the run records it was late");
    assert_eq!(ran.runs()[0].outcome, Outcome::Started { session });
    assert!(
        ran.next_run().unwrap() > now_ms() + HOUR - 60_000,
        "the next run counts from the launch"
    );
    assert_eq!(
        Store::open(database.path())
            .unwrap()
            .scheduled_tasks()
            .to_vec(),
        [ran],
        "the run is saved"
    );
}

#[gpui::test]
fn a_run_into_an_existing_session_keeps_its_draft_and_never_steers_a_running_turn(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    connect(&state, dir.path(), 400, false, cx);
    let claude = state.read_with(cx, |state, _| {
        state
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.provider() == Provider::Claude)
            .unwrap()
            .id()
    });
    state
        .update(cx, |state, cx| {
            state.set_draft(claude, "my own draft".into(), cx)
        })
        .unwrap();
    state.update(cx, |state, _| state.ensure_history(claude));
    wait_until(cx, &state, |state| {
        state
            .conversations
            .get(&claude)
            .is_some_and(|c| !c.is_loading())
    });
    state
        .update(cx, |state, cx| {
            state.add_scheduled_task(
                fields(
                    dir.path(),
                    "Into the session",
                    Target::Session { session: claude },
                ),
                cx,
            )
        })
        .unwrap();
    let id = task(&state, cx).id();
    let next = task(&state, cx).next_run();

    state.update(cx, |state, cx| {
        state.run_scheduled_task_now(id, cx);
        state.run_scheduled_task_now(id, cx);
    });
    wait_until(cx, &state, |state| {
        state.scheduled_tasks()[0].runs().len() == 2
            && matches!(
                state.scheduled_tasks()[0].runs()[1].outcome,
                Outcome::Skipped { .. }
            )
    });
    let runs = task(&state, cx).runs().to_vec();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].outcome, Outcome::Started { session: claude });
    assert_eq!(
        runs[1].outcome,
        Outcome::Skipped {
            reason: "A turn was running in the session.".into()
        },
        "the second run does not steer the first one's turn"
    );
    assert!(runs.iter().all(|run| run.due.is_none()));
    assert_eq!(
        task(&state, cx).next_run(),
        next,
        "Run now leaves the schedule alone"
    );

    wait_until(cx, &state, |state| completed(state, claude));
    state.read_with(cx, |state, _| {
        assert_eq!(state.session(claude).unwrap().draft(), "my own draft");
        let sent = state.conversations[&claude]
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Item(item) if item.kind == ItemKind::UserMessage))
            .count();
        assert_eq!(sent, 1, "one prompt went");
    });
}

#[gpui::test]
fn pause_resume_and_delete_are_saved(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let database = state.read_with(cx, |state, _| state.store.database());
    state
        .update(cx, |state, cx| {
            state.add_scheduled_task(
                fields(
                    dir.path(),
                    "Paused one",
                    Target::New {
                        provider: Provider::Codex,
                    },
                ),
                cx,
            )
        })
        .unwrap();
    let id = task(&state, cx).id();
    state
        .update(cx, |state, cx| state.pause_scheduled_task(id, cx))
        .unwrap();
    assert_eq!(
        Store::open(database.path())
            .unwrap()
            .scheduled_tasks()
            .to_vec()[0]
            .next_run(),
        None
    );
    let resumed = now_ms();
    state
        .update(cx, |state, cx| state.resume_scheduled_task(id, cx))
        .unwrap();
    assert!(
        Store::open(database.path())
            .unwrap()
            .scheduled_tasks()
            .to_vec()[0]
            .next_run()
            .unwrap()
            >= resumed + HOUR
    );
    state
        .update(cx, |state, cx| state.delete_scheduled_task(id, cx))
        .unwrap();
    assert!(
        Store::open(database.path())
            .unwrap()
            .scheduled_tasks()
            .to_vec()
            .is_empty()
    );
    assert!(state.read_with(cx, |state, _| state.scheduled_tasks().is_empty()));
}

fn launched_with_a_due_task(
    dir: &Path,
    cx: &mut gpui::TestAppContext,
) -> (Entity<AppState>, keystone_core::Database, u64) {
    let store = Store::open(dir.join("workspace.json")).unwrap();
    let database = store.database();
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    connect(&state, dir, 5, false, cx);
    let saved = ScheduledTask::new(
        1,
        fields(
            dir,
            "Due one",
            Target::New {
                provider: Provider::Claude,
            },
        ),
        now_ms() - 2 * HOUR,
        crate::app_state::scheduled::offset,
    );
    state.update(cx, |state, _| {
        state.store.save_scheduled_task(saved.clone());
        state.store.flush().unwrap();
    });
    (state, database, saved.id())
}

fn session_count(state: &Entity<AppState>, cx: &mut gpui::TestAppContext) -> usize {
    state.read_with(cx, |state, _| state.store.workspace().sessions().len())
}

#[gpui::test]
fn run_now_on_a_task_that_is_already_due_runs_it_once(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (state, _, id) = launched_with_a_due_task(dir.path(), cx);
    state.update(cx, |state, cx| {
        state.load_scheduled_tasks(cx);
        state.run_scheduled_task_now(id, cx);
    });
    wait_until(cx, &state, |state| {
        state.store.workspace().sessions().len() == 1
    });
    assert_eq!(session_count(&state, cx), 1);
    let runs = task(&state, cx).runs().to_vec();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].due.is_some(), "the due run is the one that went");
}

#[gpui::test]
fn a_run_whose_first_save_fails_starts_nothing_and_goes_once_saving_works(
    cx: &mut gpui::TestAppContext,
) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (state, database, _) = launched_with_a_due_task(dir.path(), cx);
    // Repair covers the database and the sidecars a read-only period can leave read-only.
    let mode = |mode| {
        for name in [
            "workspace.sqlite3",
            "workspace.sqlite3-wal",
            "workspace.sqlite3-shm",
        ] {
            let file = dir.path().join(name);
            if file.exists() {
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }
    };
    mode(0o444);
    state.update(cx, |state, cx| state.load_scheduled_tasks(cx));
    wait_until(cx, &state, |state| state.scheduler_error().is_some());
    let refused = state.read_with(cx, |state, _| state.scheduler_error().map(str::to_owned));
    mode(0o644);
    // A try the writer started while the files were read-only can still be running and
    // fail, so the clock moves on only once the writer is past it.
    state.read_with(cx, |state, _| state.store.writes());
    assert!(
        refused.is_some_and(|error| error.contains("did not run")),
        "the failed save is shown"
    );
    assert_eq!(session_count(&state, cx), 0, "nothing ran unsaved");
    assert_eq!(
        task(&state, cx).runs()[0].outcome,
        Outcome::Failed {
            session: None,
            reason: keystone_core::INTERRUPTED.into()
        }
    );

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(6));
    wait_until(cx, &state, |state| {
        state.store.workspace().sessions().len() == 1
    });
    assert_eq!(
        Store::open(database.path())
            .unwrap()
            .scheduled_tasks()
            .to_vec()[0]
            .runs()
            .len(),
        1
    );
    assert!(state.read_with(cx, |state, _| state.scheduler_error().is_none()));
}

#[gpui::test]
fn task_edits_and_run_now_never_wait_for_a_stalled_save_and_send_once_after_it(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    connect(&state, dir.path(), 400, false, cx);
    state.read_with(cx, |state, _| state.store.flush().unwrap());
    let stalled = dir.path().join("stalled");
    std::fs::write(&stalled, "").unwrap();
    state.update(cx, |state, _| {
        state.test_sync_directory(crate::tests::stall_while_marked)
    });
    let before = session_count(&state, cx);
    let start = std::time::Instant::now();
    state.update(cx, |state, cx| {
        state
            .add_scheduled_task(
                fields(
                    dir.path(),
                    "Held task",
                    Target::New {
                        provider: Provider::Claude,
                    },
                ),
                cx,
            )
            .unwrap();
        let id = state.scheduled_tasks()[0].id();
        state.pause_scheduled_task(id, cx).unwrap();
        state.resume_scheduled_task(id, cx).unwrap();
        state.run_scheduled_task_now(id, cx);
    });
    let elapsed = start.elapsed();
    assert_eq!(
        session_count(&state, cx),
        before,
        "no task starts before its claim is durable"
    );
    std::fs::remove_file(stalled).unwrap();
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "task calls blocked for {elapsed:?}"
    );
    wait_until(cx, &state, |state| {
        matches!(
            state.scheduled_tasks()[0].runs()[0].outcome,
            Outcome::Started { .. }
        )
    });
    let started = task(&state, cx);
    assert_eq!(started.runs().len(), 1);
    assert_eq!(session_count(&state, cx), before + 1);
    let Outcome::Started { session } = started.runs()[0].outcome else {
        unreachable!()
    };
    wait_until(cx, &state, |state| completed(state, session));
    state.read_with(cx, |state, _| {
        let sent = state.conversations[&session]
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Item(item) if item.kind == ItemKind::UserMessage))
            .count();
        assert_eq!(sent, 1, "durability polling cannot send twice");
        state.store.flush().unwrap();
        assert_eq!(
            Store::open(state.store.path()).unwrap().scheduled_tasks(),
            &[started]
        );
    });
}

#[gpui::test]
fn deleting_a_task_while_its_run_saves_cancels_the_unsent_run(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    state.read_with(cx, |state, _| state.store.flush().unwrap());
    let stalled = dir.path().join("stalled");
    std::fs::write(&stalled, "").unwrap();
    state.update(cx, |state, _| {
        state.test_sync_directory(crate::tests::stall_while_marked)
    });
    let before = session_count(&state, cx);
    let start = std::time::Instant::now();
    state.update(cx, |state, cx| {
        state
            .add_scheduled_task(
                fields(
                    dir.path(),
                    "Deleted task",
                    Target::New {
                        provider: Provider::Claude,
                    },
                ),
                cx,
            )
            .unwrap();
        let id = state.scheduled_tasks()[0].id();
        state.run_scheduled_task_now(id, cx);
        state.delete_scheduled_task(id, cx).unwrap();
    });
    let elapsed = start.elapsed();
    std::fs::remove_file(stalled).unwrap();
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "delete blocked for {elapsed:?}"
    );
    state.read_with(cx, |state, _| state.store.flush().unwrap());
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(session_count(&state, cx), before);
    state.read_with(cx, |state, _| {
        assert!(
            Store::open(state.store.path())
                .unwrap()
                .scheduled_tasks()
                .is_empty()
        )
    });
}
