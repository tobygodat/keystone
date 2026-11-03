//! Scheduled tasks: a saved prompt sent in a project on a schedule, as a new session or
//! into an existing one. See `docs/design/scheduled-tasks.md`.

use crate::{ProjectId, Provider};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MINUTE_MS: u64 = 60_000;
const DAY_SECS: i64 = 86_400;
/// How many runs a task keeps, newest last.
pub const KEPT_RUNS: usize = 20;
/// What a run says until it has started or failed, so a crash in between leaves this.
pub const INTERRUPTED: &str = "Keystone stopped during this run.";

/// A set of weekdays, bit 0 for Sunday through bit 6 for Saturday. Never empty.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Days(u8);

impl Days {
    pub const EVERY: Self = Self(0b111_1111);
    pub const WEEKDAYS: Self = Self(0b011_1110);

    fn contains(self, weekday: u8) -> bool {
        self.0 & (1 << weekday) != 0
    }
}

impl TryFrom<u8> for Days {
    type Error = &'static str;

    fn try_from(bits: u8) -> Result<Self, Self::Error> {
        if bits == 0 || bits > Self::EVERY.0 {
            return Err("a day set names at least one of the seven weekdays");
        }
        Ok(Self(bits))
    }
}

impl From<Days> for u8 {
    fn from(days: Days) -> Self {
        days.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Every `minutes` minutes of elapsed time, at least one.
    Every { minutes: u32 },
    /// At `hour`:`minute` local time on `days`.
    At { hour: u8, minute: u8, days: Days },
}

impl Schedule {
    /// The first run strictly after `from`, in unix milliseconds. `offset` gives the
    /// local UTC offset in seconds at a moment, so tests can fix the time zone.
    pub fn next_after(self, from: u64, offset: impl Fn(u64) -> i64) -> u64 {
        match self {
            Self::Every { minutes } => from + u64::from(minutes.max(1)) * MINUTE_MS,
            Self::At { hour, minute, days } => {
                let today = (from / 1000) as i64 + offset(from);
                let today = today.div_euclid(DAY_SECS);
                let time = i64::from(hour.min(23)) * 3600 + i64::from(minute.min(59)) * 60;
                for day in today..=today + 7 {
                    if !days.contains(weekday(day)) {
                        continue;
                    }
                    let at = instant(day * DAY_SECS + time, &offset);
                    if at > from {
                        return at;
                    }
                }
                unreachable!("a day set is never empty, so a week holds one of its days")
            }
        }
    }
}

/// The moment, in unix milliseconds, a clock showing `local` (seconds since 1970 on
/// the local calendar) reaches it. Of a time a fall back repeats, the first, so a day's
/// run never comes again an hour later. A time a spring forward skips maps through the
/// offset before the change, an hour later on the clock.
fn instant(local: i64, offset: &impl Fn(u64) -> i64) -> u64 {
    let at = |offset_secs: i64| (local - offset_secs).max(0) as u64 * 1000;
    // A clock changes at most once a day, so the offsets a day either side are the two
    // that can apply.
    let before = offset(at(0).saturating_sub(DAY_SECS as u64 * 1000));
    let after = offset(at(0) + DAY_SECS as u64 * 1000);
    [before, after]
        .into_iter()
        .map(at)
        .filter(|&candidate| local - (candidate / 1000) as i64 == offset(candidate))
        .min()
        .unwrap_or_else(|| at(before))
}

/// 0 for Sunday. 1970-01-01, day 0, was a Thursday.
fn weekday(day: i64) -> u8 {
    (day + 4).rem_euclid(7) as u8
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// A new session on `provider`'s default account and model.
    New { provider: Provider },
    /// The existing session `session`, as a new turn.
    Session { session: u64 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Started {
        session: u64,
    },
    /// The run did not go, and `session`, when there is one, shows why.
    Failed {
        session: Option<u64>,
        reason: String,
    },
    /// An existing session could not take the run.
    Skipped {
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Run {
    /// When the run was due, or `None` for Run now.
    pub due: Option<u64>,
    /// When it ran.
    pub at: u64,
    pub outcome: Outcome,
}

/// A task as the user saved it, with when it runs next and its last runs.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScheduledTask {
    id: u64,
    title: String,
    prompt: String,
    /// The project's key, its canonical folder.
    project: String,
    schedule: Schedule,
    target: Target,
    /// `None` while paused.
    next_run: Option<u64>,
    runs: Vec<Run>,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

/// What the user enters for a new task.
#[derive(Clone, Debug)]
pub struct TaskFields {
    pub title: String,
    pub prompt: String,
    pub project: ProjectId,
    pub schedule: Schedule,
    pub target: Target,
}

impl ScheduledTask {
    /// A new task, running first at its schedule's next time after `now`.
    pub fn new(id: u64, fields: TaskFields, now: u64, offset: impl Fn(u64) -> i64) -> Self {
        Self {
            id,
            title: fields.title,
            prompt: fields.prompt,
            project: fields.project.as_saved().to_owned(),
            schedule: fields.schedule,
            target: fields.target,
            next_run: Some(fields.schedule.next_after(now, offset)),
            runs: Vec::new(),
            unknown: Map::new(),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn project(&self) -> ProjectId {
        ProjectId::from_saved(&self.project)
    }

    pub fn schedule(&self) -> Schedule {
        self.schedule
    }

    pub fn target(&self) -> Target {
        self.target
    }

    pub fn next_run(&self) -> Option<u64> {
        self.next_run
    }

    /// Oldest first.
    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// When the run that is due at `now` was due, if one is.
    pub fn due(&self, now: u64) -> Option<u64> {
        self.next_run.filter(|&at| at <= now)
    }

    /// Records a run starting at `now`, as [`INTERRUPTED`] until [`Self::finish`]. A
    /// scheduled run, `due` at a time, also moves the next run on from `now`, so runs
    /// missed while Keystone was closed collapse into this one. Run now (`due` of
    /// `None`) leaves the next run alone.
    pub fn begin(&mut self, due: Option<u64>, now: u64, offset: impl Fn(u64) -> i64) {
        if due.is_some() && self.next_run.is_some() {
            self.next_run = Some(self.schedule.next_after(now, offset));
        }
        self.runs.push(Run {
            due,
            at: now,
            outcome: Outcome::Failed {
                session: None,
                reason: INTERRUPTED.into(),
            },
        });
        let over = self.runs.len().saturating_sub(KEPT_RUNS);
        self.runs.drain(..over);
    }

    /// Replaces the outcome of the run [`Self::begin`] recorded.
    pub fn finish(&mut self, outcome: Outcome) {
        if let Some(run) = self.runs.last_mut() {
            run.outcome = outcome;
        }
    }

    pub fn pause(&mut self) {
        self.next_run = None;
    }

    /// Resumes a paused task, counting its next run from `now`.
    pub fn resume(&mut self, now: u64, offset: impl Fn(u64) -> i64) {
        if self.next_run.is_none() {
            self.next_run = Some(self.schedule.next_after(now, offset));
        }
    }
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod tests;
