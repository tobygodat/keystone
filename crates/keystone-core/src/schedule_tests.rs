use super::*;
use crate::Database;

const HOUR: u64 = 3_600_000;
/// Friday 2026-11-06 08:00 UTC.
const FRIDAY_8_UTC: u64 = 1_793_952_000_000;
/// Saturday 2026-11-07 08:00 UTC.
const SATURDAY_8_UTC: u64 = 1_794_038_400_000;
/// Monday 2026-11-09 08:00 UTC.
const MONDAY_8_UTC: u64 = 1_794_211_200_000;

/// One hour ahead of UTC, as in central Europe in winter.
fn plus_one(_: u64) -> i64 {
    3600
}

fn at_nine(days: Days) -> Schedule {
    Schedule::At {
        hour: 9,
        minute: 0,
        days,
    }
}

fn task(schedule: Schedule, now: u64) -> ScheduledTask {
    ScheduledTask::new(
        7,
        TaskFields {
            title: "Morning review".into(),
            prompt: "Summarize yesterday's commits".into(),
            project: ProjectId::from_saved("/work/keystone"),
            schedule,
            target: Target::New {
                provider: Provider::Claude,
            },
        },
        now,
        plus_one,
    )
}

#[test]
fn a_time_of_day_runs_at_that_local_time_on_the_chosen_days() {
    let before_nine = FRIDAY_8_UTC - HOUR;
    assert_eq!(
        at_nine(Days::EVERY).next_after(before_nine, plus_one),
        FRIDAY_8_UTC,
        "09:00 at UTC+1 is 08:00 UTC the same day"
    );
    assert_eq!(
        at_nine(Days::EVERY).next_after(FRIDAY_8_UTC, plus_one),
        SATURDAY_8_UTC,
        "a run is strictly after the moment asked about"
    );
    assert_eq!(
        at_nine(Days::WEEKDAYS).next_after(FRIDAY_8_UTC + 2 * HOUR, plus_one),
        MONDAY_8_UTC,
        "weekdays skip Saturday and Sunday"
    );
}

#[test]
fn a_time_of_day_keeps_the_local_time_across_a_daylight_saving_change() {
    // Clocks go from UTC+1 to UTC+2 at 01:00 UTC on Sunday 2026-03-29.
    let change = 1_774_746_000_000;
    let offset = |at: u64| if at >= change { 7200 } else { 3600 };
    let saturday_noon_utc = change - 13 * HOUR;
    assert_eq!(
        at_nine(Days::EVERY).next_after(saturday_noon_utc, offset),
        change + 6 * HOUR,
        "Sunday 09:00 at UTC+2 is 07:00 UTC"
    );
    let half_past_two = Schedule::At {
        hour: 2,
        minute: 30,
        days: Days::EVERY,
    };
    assert_eq!(
        half_past_two.next_after(change - 3 * HOUR, offset),
        change + HOUR / 2,
        "02:30, which the change skips, runs when the clock shows 03:30"
    );
}

#[test]
fn a_time_a_fall_back_repeats_runs_once_that_day() {
    // Clocks go from UTC+2 back to UTC+1 at 01:00 UTC on Sunday 2026-10-25, so 02:30
    // comes at 00:30 and again at 01:30 UTC.
    let change = 1_792_890_000_000;
    let offset = |at: u64| if at >= change { 3600 } else { 7200 };
    let half_past_two = Schedule::At {
        hour: 2,
        minute: 30,
        days: Days::EVERY,
    };
    assert_eq!(
        half_past_two.next_after(change - 2 * HOUR, offset),
        change - HOUR / 2,
        "the first 02:30"
    );
    assert_eq!(
        half_past_two.next_after(change, offset),
        change + 24 * HOUR + HOUR / 2,
        "a catch-up between the two runs next on Monday, not at the second 02:30"
    );
}

#[test]
fn every_n_minutes_counts_elapsed_time_from_the_last_run() {
    let schedule = Schedule::Every { minutes: 15 };
    assert_eq!(schedule.next_after(1_000, plus_one), 1_000 + 15 * 60_000);
    assert_eq!(
        Schedule::Every { minutes: 0 }.next_after(1_000, plus_one),
        61_000,
        "never more often than once a minute"
    );
}

#[test]
fn runs_missed_while_closed_collapse_into_one_that_records_when_it_was_due() {
    let created = FRIDAY_8_UTC;
    let mut task = task(Schedule::Every { minutes: 10 }, created);
    let due = created + 10 * 60_000;
    assert_eq!(task.next_run(), Some(due));
    assert_eq!(task.due(due - 1), None);

    let launch = created + 5 * HOUR;
    assert_eq!(
        task.due(launch),
        Some(due),
        "thirty missed runs are one due run"
    );
    task.begin(Some(due), launch, plus_one);
    assert_eq!(task.next_run(), Some(launch + 10 * 60_000));
    assert_eq!(task.due(launch), None);
    assert_eq!(
        task.runs(),
        [Run {
            due: Some(due),
            at: launch,
            outcome: Outcome::Failed {
                session: None,
                reason: INTERRUPTED.into()
            },
        }],
        "until it has started, a run says Keystone stopped first"
    );
    task.finish(Outcome::Started { session: 41 });
    assert_eq!(task.runs()[0].outcome, Outcome::Started { session: 41 });
}

#[test]
fn run_now_leaves_the_schedule_alone_and_history_keeps_the_newest_runs() {
    let mut task = task(at_nine(Days::EVERY), FRIDAY_8_UTC - HOUR);
    for at in 0..KEPT_RUNS as u64 + 5 {
        task.begin(None, FRIDAY_8_UTC - HOUR + at, plus_one);
        task.finish(Outcome::Started { session: at });
    }
    assert_eq!(task.next_run(), Some(FRIDAY_8_UTC));
    assert_eq!(task.runs().len(), KEPT_RUNS);
    assert_eq!(task.runs()[0].outcome, Outcome::Started { session: 5 });
    assert!(task.runs().iter().all(|run| run.due.is_none()));
}

#[test]
fn a_paused_task_never_comes_due_and_resumes_from_the_moment_it_resumes() {
    let mut task = task(Schedule::Every { minutes: 30 }, FRIDAY_8_UTC);
    task.pause();
    assert_eq!(task.next_run(), None);
    assert_eq!(task.due(u64::MAX), None);
    task.resume(SATURDAY_8_UTC, plus_one);
    assert_eq!(task.next_run(), Some(SATURDAY_8_UTC + HOUR / 2));
}

#[test]
fn a_saved_task_round_trips_and_keeps_fields_a_newer_build_added() {
    let task = task(at_nine(Days::WEEKDAYS), FRIDAY_8_UTC);
    let mut saved: Value = serde_json::to_value(&task).unwrap();
    assert_eq!(saved["schedule"]["days"], 0b011_1110);
    saved["from_a_newer_build"] = "kept".into();
    let read: ScheduledTask = serde_json::from_value(saved.clone()).unwrap();
    assert_eq!(serde_json::to_value(&read).unwrap(), saved);

    saved["schedule"]["days"] = 0.into();
    assert!(
        serde_json::from_value::<ScheduledTask>(saved).is_err(),
        "a task with no days is refused"
    );
}

#[test]
fn tasks_are_saved_with_an_event_reopened_in_id_order_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    crate::Store::open(&path).unwrap();
    let database = Database::open(&path).unwrap();
    let mut first = task(Schedule::Every { minutes: 5 }, FRIDAY_8_UTC);
    let mut tenth = first.clone();
    tenth.id = 10;
    first.id = 2;
    database.save_scheduled_task(&tenth).unwrap();
    database.save_scheduled_task(&first).unwrap();
    first.begin(Some(FRIDAY_8_UTC), FRIDAY_8_UTC, plus_one);
    database.save_scheduled_task(&first).unwrap();

    let reopened = Database::open(&path).unwrap().scheduled_tasks().unwrap();
    assert_eq!(reopened, [first.clone(), tenth.clone()]);

    database.remove_scheduled_task(10).unwrap();
    assert_eq!(database.scheduled_tasks().unwrap(), [first]);
    let kinds: Vec<(String, String)> = database
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind.starts_with("scheduled_task."))
        .map(|event| (event.kind, event.entity_id))
        .collect();
    assert_eq!(
        kinds,
        [
            ("scheduled_task.upserted".into(), "10".into()),
            ("scheduled_task.upserted".into(), "2".into()),
            ("scheduled_task.upserted".into(), "2".into()),
            ("scheduled_task.removed".into(), "10".into()),
        ]
    );
}
