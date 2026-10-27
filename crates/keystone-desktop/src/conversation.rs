//! Per-session conversation state: rows to show, the running turn, and pending
//! approvals. Provider events are applied here in batches.
//!
//! Nothing here touches the disk. Entries to log collect in an outbox that the app
//! hands to [`keystone_agent::log::Logs`], and saved history arrives through
//! [`Conversation::restore`].

use keystone_agent::log::{Entry, Write, lost_count, with_lost_count};
use keystone_agent::recovery::{Operation, RecoveryBatch, RecoveryReport};
use keystone_agent::surface::{self, Status, Surface};
use keystone_agent::{
    Approval, Event, Item, ItemKind, ItemStatus, OUTPUT_CAP, RequestId, TurnStatus, cap_output,
};
use keystone_core::{ForkPoint, UsageLimit};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

/// ID prefix for a sent message shown before the provider echoes it.
const LOCAL: &str = "local:";

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Item(Item),
    /// A turn that did not complete normally, and the usage limit that refused it.
    TurnEnded {
        status: TurnStatus,
        error: Option<String>,
        limit: Option<UsageLimit>,
    },
    /// A recorded approval decision.
    Decision {
        action: String,
        allowed: bool,
    },
    /// What crash recovery found for the turn before it.
    Recovery(RecoveryReport),
    /// An agent-built widget and the user's state in it.
    Surface(Surface),
    /// The images sent with the user message in the row before it.
    Attachments(Vec<keystone_core::Attachment>),
    /// The session was imported without the provider's older turns.
    OlderNotImported,
}

/// The rows of one turn the provider can fork the conversation through, and where.
#[derive(Clone, Debug, PartialEq)]
pub struct ForkMark {
    pub rows: Range<usize>,
    pub point: ForkPoint,
}

/// The rows of one turn that has a checkpoint of its repository's files, and its
/// number among the session's checkpoints.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointMark {
    pub rows: Range<usize>,
    pub turn: u64,
}

/// How a fork from a row can go.
#[derive(Debug, PartialEq)]
pub enum ForkFrom<'a> {
    /// The provider forks its own conversation through the row's turn, whose rows end
    /// before `end`.
    Native { point: &'a ForkPoint, end: usize },
    /// No native point is known for the row's turn, so the conversation up to `end` is
    /// carried as handoff text.
    Handoff { end: usize },
    /// The row's turn is still running, so it has no point yet.
    Running,
}

/// A Button press on a widget: what to send the agent, and whether the provider's
/// tool call still waits for it as its result.
#[derive(Debug, PartialEq)]
pub struct Press {
    pub call: String,
    pub action: String,
    pub waiting: bool,
}

/// Where crash recovery stands while it holds the next message back.
#[derive(Clone, Debug, PartialEq)]
pub enum Recovery {
    /// A message waits for the saved history, which decides whether to check.
    Loading,
    /// Reading the provider's saved conversation.
    Checking,
    /// Recovered rows are shown but not confirmed saved; `error` says why saving failed.
    Saving {
        report: RecoveryReport,
        error: Option<String>,
    },
    /// A partial or conflicting recovery, shown until the user continues.
    Review(RecoveryReport),
    /// The provider's saved conversation could not be read, so this turn's outcome is
    /// unknown. Nothing was saved for it.
    Unavailable(String),
    /// Recovered rows are shown, but the log rejected the batch over its budget. Retry
    /// logs it again; once the user continues, a later restart may not show it.
    Unsaved(RecoveryBatch),
}

#[derive(Default)]
pub struct Conversation {
    /// In the order they first appeared. Rows are only appended or replaced in place.
    pub rows: Vec<Row>,
    pub running: bool,
    /// Approval requests waiting for an answer, oldest first.
    pub approvals: Vec<Approval>,
    pub error: Option<String>,
    pub provider_notice: Option<String>,
    /// The model the provider last said this session runs on, in this run of the app.
    /// Cleared when the process or conversation that reported it is gone, so a stale
    /// report is never shown as current. A failed turn keeps it: the provider is still
    /// there.
    pub reported_model: Option<String>,
    /// The tokens the conversation's context held at its provider's last report, and
    /// the model's window when known. Not saved.
    pub context: Option<(u64, Option<u64>)>,
    /// Set while the log cannot be written. Kept apart from provider errors, so it
    /// stays visible until saving works again.
    pub save_error: Option<String>,
    /// Set when the saved history could not be read. It stays for the session: the
    /// history is not shown, but what happens now is still saved after it.
    pub load_error: Option<String>,
    /// Where the current turn's rows begin. Item IDs are matched only from here, so
    /// an ID a provider reuses in a later turn never replaces an earlier row.
    turn_start: usize,
    /// This run of the app, logged with every placed entry.
    run: Run,
    /// The first row this run added; row `i` is logged as `seq` `i - run_start`. The
    /// log places each run after every row before it, so logging never depends on
    /// knowing the saved history.
    run_start: usize,
    /// While saved history is loading, what happens meanwhile waits here.
    pending: Option<Vec<Pending>>,
    /// Set while recovery holds the next message back. The app owns it.
    pub recovery: Option<Recovery>,
    /// The restored history ends in a turn Keystone never saw end, or in a message it
    /// never saw the provider take, and no recovery has settled it.
    uncertain_tail: bool,
    /// The row restore added to say Keystone closed mid-turn, replaced by recovery.
    closed_marker: Option<usize>,
    /// While restore applies saved recovery batches: the log position each row came
    /// from, so a batch only touches rows logged before it.
    origins: Option<Vec<usize>>,
    writes: Vec<Write>,
    /// Rows added or changed since [`Conversation::take_changes`].
    changed: BTreeSet<usize>,
    /// Widgets edited since they were last logged, by the call that showed them.
    unsaved_surfaces: BTreeSet<String>,
    /// The usage limit the provider said refused the turn now ending.
    limit: Option<UsageLimit>,
    /// Each turn the provider named a fork point for, in row order.
    pub forks: Vec<ForkMark>,
    /// Each turn given a checkpoint, in row order. A checkpoint is only on disk once
    /// the app has saved it, and only until it is pruned.
    pub checkpoints: Vec<CheckpointMark>,
    /// The highest checkpoint number this session has given a turn.
    last_checkpoint: u64,
    /// A checkpoint given to a turn that just ended, for the app to save.
    checkpoint_due: Option<u64>,
    /// A turn end was applied that [`Conversation::take_turn_end`] has not taken.
    turn_ended: bool,
}

/// Marks that log positions `first` and `last` bound, by the rows they are at now.
/// One whose rows are gone or out of order is dropped, and so is one overlapping an
/// earlier mark.
fn placed<T>(spans: Vec<(T, usize, usize)>, at: &HashMap<usize, usize>) -> Vec<(Range<usize>, T)> {
    let mut marks: Vec<(Range<usize>, T)> = spans
        .into_iter()
        .filter_map(|(mark, first, last)| {
            let (first, last) = (*at.get(&first)?, *at.get(&last)?);
            (first <= last).then_some((first..last + 1, mark))
        })
        .collect();
    marks.sort_by_key(|(rows, _)| rows.start);
    marks.dedup_by(|(later, _), (kept, _)| later.start < kept.end);
    marks
}

/// Identifies a run of the app in the log. Each conversation takes a new one, unique
/// across restarts.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Run(u64);

impl Default for Run {
    fn default() -> Self {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Self(nanos.wrapping_add(COUNT.fetch_add(1, Ordering::Relaxed)))
    }
}

enum Pending {
    Local(String, Vec<keystone_core::Attachment>),
    Event(Event),
}

impl Conversation {
    pub fn clear_provider_notice(&mut self) {
        self.provider_notice = None;
        if let Some(pending) = &mut self.pending {
            pending.retain(|entry| !matches!(entry, Pending::Event(Event::ProviderNotice { .. })));
        }
    }

    /// A conversation whose saved history has been requested but not restored yet.
    pub fn loading() -> Self {
        Self {
            pending: Some(Vec::new()),
            ..Self::default()
        }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether a request still waits for an answer, counting requests that arrived
    /// while the history loads and are not shown yet.
    pub fn awaiting_approval(&self) -> bool {
        let mut waiting: Vec<&RequestId> = self.approvals.iter().map(|a| &a.request).collect();
        for pending in self.pending.iter().flatten() {
            match pending {
                Pending::Event(Event::ApprovalRequested(approval)) => {
                    waiting.push(&approval.request)
                }
                Pending::Event(Event::ApprovalResolved { request }) => {
                    waiting.retain(|waiting| *waiting != request)
                }
                Pending::Event(Event::TurnCompleted { .. }) => waiting.clear(),
                _ => {}
            }
        }
        !waiting.is_empty()
    }

    /// Why the session cannot be settled now: a request or widget still waits for the
    /// user, and settling never answers either.
    pub fn settle_blocker(&self) -> Option<&'static str> {
        if self.awaiting_approval() {
            Some("Answer the approval request before settling this session.")
        } else if self.awaiting_widget() {
            Some("Answer the widget before settling this session.")
        } else {
            None
        }
    }

    /// Whether a widget waits for the user to answer it, counting widgets that arrived
    /// while the history loads.
    pub fn awaiting_widget(&self) -> bool {
        let mut waiting: HashSet<&str> = self
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Surface(surface) if surface.waiting() => Some(surface.call.as_str()),
                _ => None,
            })
            .collect();
        for pending in self.pending.iter().flatten() {
            match pending {
                Pending::Event(Event::SurfaceShown {
                    surface,
                    waiting: true,
                }) => {
                    waiting.insert(&surface.call);
                }
                Pending::Event(Event::SurfaceStopped { call }) => {
                    waiting.remove(call.as_str());
                }
                Pending::Event(Event::TurnCompleted { .. }) => waiting.clear(),
                _ => {}
            }
        }
        !waiting.is_empty()
    }

    /// Rebuilds the rows from the session's log, in the order they were first shown,
    /// then applies anything that happened while loading.
    ///
    /// When the log could not be read, the conversation goes on without its history
    /// and says so. What was sent meanwhile is shown and saved: it is logged as this
    /// run's, so a later restore still places it after the history.
    pub fn restore(&mut self, entries: Result<Vec<Entry>, String>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match entries {
            Ok(entries) => self.rebuild(entries),
            Err(error) => {
                self.load_error = Some(format!(
                    "Could not read this conversation's history: {error}."
                ))
            }
        }
        self.run_start = self.rows.len();
        self.changed.extend(0..self.rows.len());
        for pending in pending {
            match pending {
                Pending::Local(text, attachments) => self.push_sent(&text, attachments),
                Pending::Event(event) => {
                    self.apply(event);
                }
            }
        }
    }

    fn rebuild(&mut self, entries: Vec<Entry>) {
        // Each row's place: its logged `seq`, counted from the start of its run, or
        // for older logs, its order in the file.
        let mut places: Vec<u64> = Vec::new();
        let mut next = 0;
        let mut run = None;
        let mut base = 0;
        let mut open_turn = false;
        let mut batches = Vec::new();
        let mut origins: Vec<usize> = Vec::new();
        // Whether a recovery came after the last row or turn, and the report still
        // waiting for an acknowledgement: one that no later message followed.
        let mut settled = false;
        let mut review = None;
        let mut last_local = false;
        let mut exited = false;
        let mut points: Vec<(ForkPoint, usize, usize)> = Vec::new();
        let mut checkpoints: Vec<(u64, usize, usize)> = Vec::new();
        // The marker for a recovery batch the log rejected is named after the batch, and
        // a Retry that saved the batch later means nothing was lost.
        let saved: BTreeSet<String> = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Recovery { batch } => Some(format!(
                    "{}{}",
                    keystone_agent::log::LOSS_MARKER_PREFIX,
                    batch.id
                )),
                _ => None,
            })
            .collect();
        for (position, entry) in entries.into_iter().enumerate() {
            if matches!(&entry, Entry::Item { item, .. } if saved.contains(&item.id)) {
                continue;
            }
            let (seq, entry_run) = match &entry {
                Entry::TurnStarted
                | Entry::Recovery { .. }
                | Entry::ProviderExited
                | Entry::ForkPoint { .. }
                | Entry::ForkPointsLost
                | Entry::OlderNotImported
                | Entry::Checkpoint { .. } => (None, None),
                Entry::Item { seq, run, .. }
                | Entry::Approval { seq, run, .. }
                | Entry::TurnEnded { seq, run, .. }
                | Entry::Attachments { seq, run, .. }
                | Entry::Surface { seq, run, .. } => (*seq, *run),
            };
            match &entry {
                Entry::ProviderExited => {
                    exited = true;
                    continue;
                }
                // The turn's rows are the ones since it started, so an ID reused in
                // another turn never moves its point. A row is named by the log entry
                // that added it, which the ordering below keeps.
                Entry::ForkPoint { point, first, last } => {
                    if let Some((first, last)) = self.logged_span(&origins, first, last) {
                        points.push((point.clone(), first, last));
                    }
                    continue;
                }
                Entry::Checkpoint { turn, first, last } => {
                    self.last_checkpoint = self.last_checkpoint.max(*turn);
                    if let Some((first, last)) = self.logged_span(&origins, first, last) {
                        checkpoints.push((*turn, first, last));
                    }
                    continue;
                }
                Entry::ForkPointsLost => {
                    points.clear();
                    continue;
                }
                Entry::Recovery { batch } => {
                    exited = false;
                    settled = true;
                    open_turn &= !batch.closes_turn;
                    review = batch
                        .report
                        .needs_acknowledgement()
                        .then(|| batch.report.clone());
                    batches.push((position, batch.clone()));
                    continue;
                }
                Entry::TurnStarted => {
                    settled = false;
                    exited = false;
                    review = None;
                }
                // A gap marker says what is missing, not what happened, so it changes
                // neither the recovery check nor the turn it is in.
                Entry::Item { item, .. }
                    if item.id.starts_with(keystone_agent::log::LOSS_MARKER_PREFIX) => {}
                Entry::Item { item, .. } => {
                    settled = false;
                    last_local = item.id.starts_with(LOCAL) && item.kind == ItemKind::UserMessage;
                }
                Entry::TurnEnded { .. } => last_local = false,
                Entry::Approval { .. }
                | Entry::Surface { .. }
                | Entry::Attachments { .. }
                | Entry::OlderNotImported => {}
            }
            if entry_run.is_some() && entry_run != run {
                // A new run starts after every row before it, and never replaces them.
                run = entry_run;
                base = next;
                self.turn_start = self.rows.len();
            }
            let place = match (seq, entry_run) {
                (Some(seq), Some(_)) => base + seq,
                (Some(seq), None) => seq,
                (None, _) => next,
            };
            let mut ended = false;
            let row = match entry {
                Entry::TurnStarted => {
                    open_turn = true;
                    None
                }
                Entry::Item { item, .. } => match self.position_for(&item) {
                    Some(index) => {
                        // A gap marker saved again may stand for an earlier lost entry.
                        if lost_count(&item).is_some() {
                            places[index] = places[index].min(place);
                        }
                        self.rows[index] = Row::Item(item);
                        None
                    }
                    None => Some(Row::Item(item)),
                },
                Entry::Approval {
                    action, allowed, ..
                } => Some(Row::Decision { action, allowed }),
                Entry::OlderNotImported => Some(Row::OlderNotImported),
                Entry::Attachments { attachments, .. } => Some(Row::Attachments(attachments)),
                Entry::Surface { mut surface, .. } => {
                    // The tool call that waited for it ended with the run that showed it.
                    if surface.status == Status::Waiting {
                        surface.status = Status::Stopped;
                    }
                    match self.surface_row(&surface.call) {
                        Some(index) => {
                            self.rows[index] = Row::Surface(surface);
                            None
                        }
                        None => Some(Row::Surface(surface)),
                    }
                }
                Entry::Recovery { .. }
                | Entry::ProviderExited
                | Entry::ForkPoint { .. }
                | Entry::ForkPointsLost
                | Entry::Checkpoint { .. } => None,
                Entry::TurnEnded {
                    status,
                    error,
                    limit,
                    ..
                } => {
                    open_turn = false;
                    ended = true;
                    (status != TurnStatus::Completed).then_some(Row::TurnEnded {
                        status,
                        error,
                        limit,
                    })
                }
            };
            if let Some(row) = row {
                next = next.max(place + 1);
                self.rows.push(row);
                places.push(place);
                origins.push(position);
            }
            if ended {
                self.turn_start = self.rows.len();
            }
        }
        let mut ordered: Vec<(u64, usize, Row)> = places
            .into_iter()
            .zip(origins)
            .zip(self.rows.drain(..))
            .map(|((place, origin), row)| (place, origin, row))
            .collect();
        // Stable, so rows with the same place keep their order in the file.
        ordered.sort_by_key(|(place, _, _)| *place);
        // Log gap markers with nothing between them read as one gap, unless a recovery
        // batch logged after the first of them names the later one, or the latest batch
        // to fill the first expects it as it is: a fill expects the row it saw exactly.
        ordered.dedup_by(|(_, logged, later), (_, first, kept)| {
            let (Row::Item(later), Row::Item(kept)) = (later, kept) else {
                return false;
            };
            let (Some(more), Some(count)) = (lost_count(later), lost_count(kept)) else {
                return false;
            };
            fn expected<'a>(batch: &'a RecoveryBatch, id: &str) -> Vec<&'a Item> {
                batch
                    .operations
                    .iter()
                    .filter_map(|operation| match operation {
                        Operation::Replace { expected, .. } if expected.id == id => Some(expected),
                        _ => None,
                    })
                    .collect()
            }
            let from = *logged.min(first);
            let after = batches
                .iter()
                .filter(|(position, _)| *position > from)
                .map(|(_, batch)| batch);
            let named = later.id != kept.id
                && after.clone().any(|batch| {
                    batch.report_after.as_deref() == Some(&later.id)
                        || !expected(batch, &later.id).is_empty()
                });
            let filled = after
                .rev()
                .map(|batch| expected(batch, &kept.id))
                .find(|expected| !expected.is_empty())
                .is_some_and(|expected| expected.contains(&&*kept));
            if named || filled {
                return false;
            }
            // The same marker saved twice, after a write whose outcome was unknown,
            // already counts the earlier copy.
            let total = if later.id == kept.id {
                count.max(more)
            } else {
                count.saturating_add(more)
            };
            with_lost_count(kept, total);
            true
        });
        let (origins, rows): (Vec<usize>, Vec<Row>) = ordered
            .into_iter()
            .map(|(_, origin, row)| (origin, row))
            .unzip();
        self.rows = rows;
        let at: HashMap<usize, usize> = origins
            .iter()
            .enumerate()
            .map(|(index, origin)| (*origin, index))
            .collect();
        // A point whose rows are gone, or out of order, is dropped: forking there offers
        // a handoff instead of guessing. So is a checkpoint, which is then not offered.
        // Recovered rows inserted below move the marks with them.
        self.forks = placed(points, &at)
            .into_iter()
            .map(|(rows, point)| ForkMark { rows, point })
            .collect();
        self.checkpoints = placed(checkpoints, &at)
            .into_iter()
            .map(|(rows, turn)| CheckpointMark { rows, turn })
            .collect();
        // Each batch applies to the rows logged before it, as it did when it was saved.
        self.origins = Some(origins);
        for (position, batch) in &batches {
            self.apply_batch(batch, *position);
        }
        self.origins = None;
        if open_turn {
            self.rows.push(Row::TurnEnded {
                status: TurnStatus::Interrupted,
                error: Some("Keystone closed during this turn".into()),
                limit: None,
            });
            self.closed_marker = Some(self.rows.len() - 1);
        }
        self.uncertain_tail = (open_turn || last_local) && !settled || exited;
        self.recovery = review.map(Recovery::Review);
        self.turn_start = self.rows.len();
    }

    /// The provider exited during a turn: its outcome is unknown until recovery reads
    /// what it saved. Logged, so a restart before that check runs it again. While the
    /// history loads it waits with the turn's other events, so it is logged after them.
    fn provider_exited(&mut self) {
        self.uncertain_tail = true;
        self.write(Entry::ProviderExited, true);
    }

    /// Checks the last turn again, open or not as the earlier check found it.
    pub fn recheck(&mut self, open: bool) {
        self.uncertain_tail = open;
    }

    /// Whether restored history needs a recovery check before the next message.
    pub fn needs_recovery(&self) -> bool {
        self.uncertain_tail && !self.running
    }

    /// Whether Keystone lost track of the last restored turn.
    pub fn open_turn(&self) -> bool {
        self.uncertain_tail
    }

    /// The rows of the turn running now, bounded by the provider's own items, with the
    /// first and last item IDs: a message shown but not echoed may be waiting for the
    /// next turn.
    fn turn_span(&self) -> Option<(Range<usize>, String, String)> {
        let mut items =
            self.rows[self.turn_start..]
                .iter()
                .enumerate()
                .filter_map(|(index, row)| match row {
                    Row::Item(item) if !item.id.starts_with(LOCAL) => {
                        Some((self.turn_start + index, item.id.as_str()))
                    }
                    _ => None,
                });
        let (start, first) = items.next()?;
        let (end, last) = items.last().unwrap_or((start, first));
        Some((start..end + 1, first.to_owned(), last.to_owned()))
    }

    /// While a restore reads the log: the log positions of the rows named `first` and
    /// `last` in the current turn. The turn's rows are the ones since it started, so an
    /// ID reused in another turn never moves the span. A row is named by the log entry
    /// that added it, which the ordering keeps.
    fn logged_span(&self, origins: &[usize], first: &str, last: &str) -> Option<(usize, usize)> {
        let turn = &self.rows[self.turn_start..];
        let named = |row: &Row, id: &str| matches!(row, Row::Item(item) if item.id == id);
        let first = turn.iter().position(|row| named(row, first))?;
        let last = turn.iter().rposition(|row| named(row, last))?;
        Some((
            origins[self.turn_start + first],
            origins[self.turn_start + last],
        ))
    }

    /// Draws again the rows that carry a Restore control, since which checkpoints are
    /// saved changed.
    pub fn show_checkpoints(&mut self) {
        let rows: Vec<usize> = self
            .checkpoints
            .iter()
            .flat_map(|m| m.rows.clone())
            .collect();
        for index in rows {
            self.mark(index);
        }
    }

    /// The checkpoint a turn just given one waits to have saved.
    pub fn take_checkpoint_due(&mut self) -> Option<u64> {
        self.checkpoint_due.take()
    }

    /// Whether a turn ended since the last call. An end held during a load counts once
    /// the load applies it.
    pub fn take_turn_end(&mut self) -> bool {
        std::mem::take(&mut self.turn_ended)
    }

    /// Numbers later turns' checkpoints above `turn`, one saved in the repository, so
    /// a turn whose mark the log lost is never given a checkpoint saved before.
    pub fn checkpoints_saved_through(&mut self, turn: u64) {
        self.last_checkpoint = self.last_checkpoint.max(turn);
    }

    /// The checkpoint whose Restore control the row at `index` carries: the turn's
    /// last reply.
    pub fn checkpoint_at(&self, index: usize) -> Option<u64> {
        let at = self
            .checkpoints
            .partition_point(|mark| mark.rows.end <= index);
        let mark = self.checkpoints.get(at)?;
        (self.last_reply(mark) == Some(index)).then_some(mark.turn)
    }

    /// The row carrying `turn`'s checkpoint, as [`Conversation::checkpoint_at`] finds.
    pub fn checkpoint_row(&self, turn: u64) -> Option<usize> {
        self.last_reply(self.checkpoints.iter().find(|mark| mark.turn == turn)?)
    }

    /// The latest turn with a checkpoint shown.
    pub fn latest_checkpoint(&self) -> Option<u64> {
        self.checkpoints
            .iter()
            .rev()
            .find(|mark| self.last_reply(mark).is_some())
            .map(|mark| mark.turn)
    }

    fn last_reply(&self, mark: &CheckpointMark) -> Option<usize> {
        mark.rows.clone().rev().find(|row| {
            matches!(&self.rows[*row], Row::Item(item) if item.kind == ItemKind::AgentMessage)
        })
    }

    /// How a fork from the row at `index` goes. A native fork keeps the row's whole
    /// turn, with the rows that close it; a handoff keeps rows up to the next message
    /// the user sent.
    pub fn fork_from(&self, index: usize) -> ForkFrom<'_> {
        let closing = |mut end: usize| {
            while matches!(
                self.rows.get(end),
                Some(Row::TurnEnded { .. } | Row::Decision { .. })
            ) {
                end += 1;
            }
            end
        };
        let running = self.running && index >= self.turn_start;
        // A message the provider never echoed may not be in its history at that point.
        let unconfirmed =
            matches!(&self.rows[index], Row::Item(item) if item.id.starts_with(LOCAL));
        let at = self.forks.partition_point(|mark| mark.rows.end <= index);
        if let Some(mark) = self
            .forks
            .get(at)
            .filter(|mark| mark.rows.contains(&index) && !unconfirmed)
        {
            return ForkFrom::Native {
                point: &mark.point,
                end: closing(mark.rows.end),
            };
        }
        if running {
            return ForkFrom::Running;
        }
        let end = self.rows[index + 1..]
            .iter()
            .position(|row| matches!(row, Row::Item(item) if item.kind == ItemKind::UserMessage))
            .map_or(self.rows.len(), |n| index + 1 + n);
        ForkFrom::Handoff { end }
    }

    /// The log a native fork starts with: the rows before `end` as one run, with the
    /// fork points of the turns among them, so the fork can be forked again there. Turn
    /// boundaries go between turns, so a restore never matches an ID reused in another
    /// turn.
    pub fn fork_log(&self, end: usize) -> Vec<Write> {
        let run = Some(Run::default().0);
        let boundary = |sync| Write {
            entry: Entry::TurnEnded {
                status: TurnStatus::Completed,
                error: None,
                limit: None,
                seq: None,
                run: None,
            },
            sync,
        };
        let id = |index: usize| match &self.rows[index] {
            Row::Item(item) => Some(item.id.clone()),
            _ => None,
        };
        let marks: Vec<&ForkMark> = self.forks.iter().filter(|m| m.rows.end <= end).collect();
        let mut writes = Vec::new();
        let mut skipped = false;
        for (index, row) in self.rows[..end].iter().enumerate() {
            let mark = marks.iter().find(|m| m.rows.contains(&index));
            let starts_turn = match mark {
                Some(mark) => mark.rows.start == index,
                None => matches!(row, Row::Item(item) if item.kind == ItemKind::UserMessage),
            };
            if index > 0 && starts_turn {
                writes.push(boundary(false));
            }
            // A message the provider never echoed is not in its history at the cut, nor
            // are the images sent with it.
            let unechoed =
                mark.is_some() && matches!(row, Row::Item(item) if item.id.starts_with(LOCAL));
            if unechoed || (skipped && matches!(row, Row::Attachments(_))) {
                skipped = true;
                continue;
            }
            skipped = false;
            let seq = Some(index as u64);
            let entry = match row.clone() {
                Row::Item(item) => Entry::Item { item, seq, run },
                Row::Decision { action, allowed } => Entry::Approval {
                    action,
                    allowed,
                    seq,
                    run,
                },
                // The usage limit card and its scheduled continue stay with the source.
                Row::TurnEnded { status, error, .. } => Entry::TurnEnded {
                    status,
                    error,
                    limit: None,
                    seq,
                    run,
                },
                Row::Surface(surface) => Entry::Surface { surface, seq, run },
                Row::Attachments(attachments) => Entry::Attachments {
                    attachments,
                    seq,
                    run,
                },
                Row::OlderNotImported => Entry::OlderNotImported,
                Row::Recovery(_) => continue,
            };
            writes.push(Write { entry, sync: false });
            if let Some(mark) = mark.filter(|m| m.rows.end == index + 1)
                && let (Some(first), Some(last)) = (id(mark.rows.start), id(index))
            {
                writes.push(Write {
                    entry: Entry::ForkPoint {
                        point: mark.point.clone(),
                        first,
                        last,
                    },
                    sync: false,
                });
            }
        }
        writes.push(boundary(true));
        writes
    }

    /// The items shown, in order, for recovery to match against.
    pub fn neutral_items(&self) -> Vec<Item> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) => Some(item.clone()),
                _ => None,
            })
            .collect()
    }

    /// Shows a recovery's rows and report, and logs the batch so every restore applies
    /// it the same way.
    pub fn apply_recovery(&mut self, batch: RecoveryBatch) {
        let len = self.rows.len();
        let first = self.apply_batch(&batch, usize::MAX);
        self.uncertain_tail = false;
        if !self.running {
            self.turn_start = self.rows.len();
        }
        if let Some(first) = first {
            self.changed.extend(first..self.rows.len().max(len));
        }
        self.write(Entry::Recovery { batch }, true);
    }

    /// Logs a recovery batch already shown, whose earlier write the log rejected.
    pub fn log_recovery(&mut self, batch: RecoveryBatch) {
        self.write(Entry::Recovery { batch }, true);
    }

    /// Applies a batch's checked operations and places its report. A replacement whose
    /// row no longer holds the expected value, or an insertion whose anchor is gone, is
    /// skipped. Returns the first row changed.
    fn apply_batch(&mut self, batch: &RecoveryBatch, logged_at: usize) -> Option<usize> {
        let mut first: Option<usize> = None;
        let mut touch = |index: usize| first = Some(first.map_or(index, |f| f.min(index)));
        for operation in &batch.operations {
            match operation {
                Operation::Replace { expected, item } => {
                    if let Some(index) = self.find(logged_at, |old| old == expected) {
                        self.rows[index] = Row::Item(item.clone());
                        touch(index);
                    }
                }
                Operation::Insert { after, item } => {
                    if self.find(logged_at, |old| old.id == item.id).is_some() {
                        continue;
                    }
                    if let Some(index) = self.find(logged_at, |old| old.id == *after) {
                        let at = index
                            + 1
                            + usize::from(matches!(
                                self.rows.get(index + 1),
                                Some(Row::Attachments(_))
                            ));
                        self.insert_row(at, Row::Item(item.clone()), logged_at);
                        touch(at);
                    }
                }
            }
        }
        let report = Row::Recovery(batch.report.clone());
        match batch
            .closes_turn
            .then(|| self.closed_marker.take())
            .flatten()
        {
            Some(marker) => {
                self.rows[marker] = report;
                touch(marker);
            }
            None => {
                let at = batch
                    .report_after
                    .as_deref()
                    .and_then(|id| self.find(logged_at, |item| item.id == id))
                    .map_or(self.rows.len(), |index| {
                        let mut at = index + 1;
                        while matches!(
                            self.rows.get(at),
                            Some(
                                Row::TurnEnded { .. } | Row::Decision { .. } | Row::Attachments(_)
                            )
                        ) {
                            at += 1;
                        }
                        at
                    });
                self.insert_row(at, report, logged_at);
                touch(at);
            }
        }
        first
    }

    /// The last item row logged before `logged_at` that `matches`.
    fn find(&self, logged_at: usize, matches: impl Fn(&Item) -> bool) -> Option<usize> {
        self.rows.iter().enumerate().rposition(|(index, row)| {
            matches!(row, Row::Item(item) if matches(item))
                && self
                    .origins
                    .as_ref()
                    .is_none_or(|origins| origins[index] <= logged_at)
        })
    }

    /// Inserts a row, keeping the indexes that point past it on the same rows.
    fn insert_row(&mut self, at: usize, row: Row, logged_at: usize) {
        self.rows.insert(at, row);
        let marks = self.forks.iter_mut().map(|mark| &mut mark.rows);
        for rows in marks.chain(self.checkpoints.iter_mut().map(|mark| &mut mark.rows)) {
            if at <= rows.start {
                rows.start += 1;
            }
            if at < rows.end {
                rows.end += 1;
            }
        }
        if let Some(origins) = &mut self.origins {
            origins.insert(at, logged_at);
        }
        if at < self.run_start {
            self.run_start += 1;
        }
        if at < self.turn_start {
            self.turn_start += 1;
        }
        if let Some(marker) = &mut self.closed_marker
            && at <= *marker
        {
            *marker += 1;
        }
    }

    #[cfg(test)]
    pub fn push_local(&mut self, text: &str) {
        self.push_sent(text, Vec::new());
    }

    /// Shows and logs a sent message at once. Steers are not always echoed back, so
    /// the message is logged now; the provider's copy replaces it if one arrives. The
    /// images sent with it follow in a row of their own, which the log holds as
    /// references, not bytes.
    pub fn push_sent(&mut self, text: &str, attachments: Vec<keystone_core::Attachment>) {
        if let Some(pending) = &mut self.pending {
            pending.push(Pending::Local(text.to_owned(), attachments));
            // The message has already gone to the provider, so the turn is running
            // now, and Stop is offered before the history arrives.
            self.running = true;
            self.error = None;
            return;
        }
        // Unique across restarts: a reloaded log already holds earlier local IDs.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let item = Item {
            id: format!("{LOCAL}{nanos}-{}", self.rows.len()),
            kind: ItemKind::UserMessage,
            text: text.to_owned(),
            status: ItemStatus::Completed,
        };
        if !self.running {
            self.turn_start = self.rows.len();
        }
        let index = self.push(Row::Item(item.clone()));
        self.log_item(index, item);
        if !attachments.is_empty() {
            let index = self.push(Row::Attachments(attachments.clone()));
            let (seq, run) = (Some(self.seq(index)), Some(self.run.0));
            self.write(
                Entry::Attachments {
                    attachments,
                    seq,
                    run,
                },
                false,
            );
        }
        self.running = true;
        self.error = None;
    }

    /// The usage limit that refused the latest turn, while nothing came after it.
    pub fn usage_limit(&self) -> Option<&UsageLimit> {
        if self.running || self.is_loading() {
            return None;
        }
        match self.rows.last()? {
            Row::TurnEnded {
                limit: Some(limit), ..
            } => Some(limit),
            _ => None,
        }
    }

    /// Whether row `index` takes a place in the transcript. The turn end that the usage
    /// limit card stands for does not when the error named the limit and its reset, as
    /// the card then says all it said.
    pub fn shows(&self, index: usize) -> bool {
        self.rows.get(index).is_some_and(|row| {
            visible(row)
                && !(index + 1 == self.rows.len()
                    && self.recovery.is_none()
                    && self.usage_limit().is_some_and(|limit| {
                        !limit.inferred && limit.label.is_some() && limit.resets_at.is_some()
                    }))
        })
    }

    /// Applies one provider event. Returns the thread ID when the provider opened one.
    pub fn apply(&mut self, event: Event) -> Option<String> {
        if let Some(pending) = &mut self.pending {
            // The thread is saved with the session, and the model is shown from memory,
            // not from this log, so neither waits.
            match event {
                Event::ThreadReady { thread_id } => return Some(thread_id),
                Event::ModelReported { model } => self.reported_model = Some(model),
                Event::ModelLost => self.reported_model = None,
                Event::ContextUsage { used, window } => self.context = Some((used, window)),
                event => pending.push(Pending::Event(event)),
            }
            return None;
        }
        match event {
            Event::ThreadReady { thread_id } => return Some(thread_id),
            Event::ModelReported { model } => self.reported_model = Some(model),
            Event::ModelLost => self.reported_model = None,
            Event::ContextUsage { used, window } => self.context = Some((used, window)),
            // Catalogs, commands, account usage and MCP status belong to the provider, not to a
            // conversation.
            Event::Catalog(_) | Event::Commands(_) | Event::Usage(_) | Event::McpStatus(_) => {}
            Event::TurnStarted => {
                self.running = true;
                self.error = None;
                self.write(Entry::TurnStarted, false);
            }
            Event::ItemStarted(item) => {
                self.place(item);
            }
            Event::ItemDelta { item_id, text } => {
                if let Some(index) = self.position(&item_id)
                    && let Row::Item(item) = &mut self.rows[index]
                {
                    item.text.push_str(&text);
                    // Command output can be huge. Cap it while it streams, not only at the
                    // end; trimming at three times the cap keeps the cost occasional.
                    if matches!(item.kind, ItemKind::Command { .. })
                        && item.text.len() > OUTPUT_CAP * 6
                    {
                        item.text = cap_output(&item.text);
                    }
                    self.mark(index);
                }
            }
            Event::ItemCompleted(item) => {
                let index = self.place(item.clone());
                self.log_item(index, item);
            }
            Event::ApprovalRequested(approval) => self.approvals.push(approval),
            Event::ApprovalResolved { request } => self.approvals.retain(|a| a.request != request),
            Event::SurfaceShown { surface, .. } => {
                let index = match self.surface_row(&surface.call) {
                    Some(index) => {
                        self.rows[index] = Row::Surface(surface);
                        self.mark(index);
                        index
                    }
                    None => self.push(Row::Surface(surface)),
                };
                self.log_surface(index);
            }
            Event::SurfaceStopped { call } => {
                if let Some(index) = self.surface_row(&call) {
                    self.stop_surface(index);
                }
            }
            Event::UsageLimit(limit) => self.limit = Some(limit),
            Event::ForkPoint(point) => {
                if let Some((rows, first, last)) = self.turn_span() {
                    let entry = Entry::ForkPoint {
                        point: point.clone(),
                        first,
                        last,
                    };
                    self.forks.retain(|mark| mark.rows.end <= rows.start);
                    self.forks.push(ForkMark { rows, point });
                    self.write(entry, false);
                }
            }
            Event::ForkPointsLost => {
                self.forks.clear();
                self.write(Entry::ForkPointsLost, false);
            }
            Event::TurnCompleted { status, error, .. } => {
                self.running = false;
                self.turn_ended = true;
                self.approvals.clear();
                self.settle_unfinished();
                if let Some((rows, first, last)) = self.turn_span() {
                    let turn = self.last_checkpoint + 1;
                    self.last_checkpoint = turn;
                    self.checkpoint_due = Some(turn);
                    self.checkpoints.retain(|mark| mark.rows.end <= rows.start);
                    self.checkpoints.push(CheckpointMark { rows, turn });
                    self.write(Entry::Checkpoint { turn, first, last }, false);
                }
                let limit = self.limit.take().filter(|_| status == TurnStatus::Failed);
                let shown = status != TurnStatus::Completed;
                let entry = Entry::TurnEnded {
                    status,
                    error: error.clone(),
                    limit: limit.clone(),
                    seq: shown.then(|| self.seq(self.rows.len())),
                    run: shown.then_some(self.run.0),
                };
                self.write(entry, true);
                if shown {
                    self.push(Row::TurnEnded {
                        status,
                        error,
                        limit,
                    });
                }
                self.turn_start = self.rows.len();
            }
            Event::Failed { message } => {
                self.running = false;
                self.limit = None;
                self.settle_unfinished();
                self.error = Some(message);
                self.turn_start = self.rows.len();
            }
            Event::Warning { message } => self.error = Some(message),
            Event::ProviderNotice { message } => self.provider_notice = message,
            // Recovery's control events belong to the app, never to the transcript.
            Event::RecoveryNeeded => self.provider_exited(),
            // An undelivered widget action is the app's to send.
            Event::HistoryRead { .. }
            | Event::ThreadsListed { .. }
            | Event::IdleCandidate(_)
            | Event::ActionUndelivered { .. } => {}
        }
        None
    }

    /// The row of the widget tool call `call` showed.
    pub fn surface_row(&self, call: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Surface(surface) if surface.call == call))
    }

    /// Writes an input's value into the widget `call` showed. It is logged later, with
    /// [`Conversation::save_surfaces`], so typing does not log every key. Returns
    /// whether the widget changed, or why it refused the value.
    pub fn edit_surface(
        &mut self,
        call: &str,
        pointer: &str,
        value: serde_json::Value,
    ) -> Result<bool, String> {
        let Some(index) = self.surface_row(call) else {
            return Ok(false);
        };
        let Row::Surface(surface) = &mut self.rows[index] else {
            return Ok(false);
        };
        if surface.value(pointer) == Some(&value) {
            return Ok(false);
        }
        surface.set_value(pointer, value)?;
        self.mark(index);
        self.unsaved_surfaces.insert(call.to_owned());
        Ok(true)
    }

    /// Logs every widget edited since it was last logged.
    pub fn save_surfaces(&mut self) {
        for call in std::mem::take(&mut self.unsaved_surfaces) {
            if let Some(index) = self.surface_row(&call) {
                self.log_surface(index);
            }
        }
    }

    /// Presses Button `component` on the widget `call` showed: records it and returns
    /// what to send the agent.
    pub fn press(&mut self, call: &str, component: &str, timestamp_ms: u64) -> Option<Press> {
        let index = self.surface_row(call)?;
        let Row::Surface(surface) = &mut self.rows[index] else {
            return None;
        };
        let action = surface.action(component, timestamp_ms)?;
        let waiting = surface.waiting();
        surface.record_action(component);
        self.mark(index);
        self.unsaved_surfaces.remove(call);
        self.log_surface(index);
        Some(Press {
            call: call.to_owned(),
            action,
            waiting,
        })
    }

    /// Saves the widget in row `index` whole, in place for a restore.
    fn log_surface(&mut self, index: usize) {
        let Some(Row::Surface(surface)) = self.rows.get(index) else {
            return;
        };
        // A widget restored from an earlier run keeps its place; this run logs it anew.
        let (seq, run) = if index >= self.run_start {
            (Some(self.seq(index)), Some(self.run.0))
        } else {
            (None, None)
        };
        let entry = Entry::Surface {
            surface: surface.clone(),
            seq,
            run,
        };
        self.write(entry, true);
    }

    /// A turn that ends leaves items the provider never completes. Keep what streamed,
    /// marked interrupted rather than failed, so the partial reply survives a restart
    /// and recovery may complete it. Restored rows finished in an earlier run.
    fn settle_unfinished(&mut self) {
        let start = self.run_start;
        let unfinished: Vec<(usize, Item)> = self.rows[start..]
            .iter_mut()
            .enumerate()
            .map(|(index, row)| (start + index, row))
            .filter_map(|(index, row)| match row {
                Row::Item(item) if item.status == ItemStatus::InProgress => {
                    item.status = ItemStatus::Interrupted;
                    Some((index, item.clone()))
                }
                _ => None,
            })
            .collect();
        for (index, item) in unfinished {
            self.mark(index);
            self.log_item(index, item);
        }
        // The tool call that waited for a widget ends with its turn.
        for index in start..self.rows.len() {
            self.stop_surface(index);
        }
    }

    /// Marks the widget in row `index` Stopped if it still waits for the user: its tool
    /// call no longer takes a press as its result.
    fn stop_surface(&mut self, index: usize) {
        let Some(Row::Surface(surface)) = self.rows.get_mut(index) else {
            return;
        };
        if !surface.waiting() {
            return;
        }
        surface.status = Status::Stopped;
        self.unsaved_surfaces.remove(&surface.call);
        self.mark(index);
        self.log_surface(index);
    }

    pub fn decide(&mut self, request: &RequestId, allowed: bool) -> Option<Approval> {
        let index = self.approvals.iter().position(|a| &a.request == request)?;
        let approval = self.approvals.remove(index);
        let entry = Entry::Approval {
            action: approval.action.clone(),
            allowed,
            seq: Some(self.seq(self.rows.len())),
            run: Some(self.run.0),
        };
        self.write(entry, true);
        self.push(Row::Decision {
            action: approval.action.clone(),
            allowed,
        });
        Some(approval)
    }

    /// Entries to append to the session's log, in order.
    pub fn take_writes(&mut self) -> Vec<Write> {
        std::mem::take(&mut self.writes)
    }

    /// The rows added or changed since the last call, as ascending, separate ranges.
    pub fn take_changes(&mut self) -> Vec<Range<usize>> {
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for index in std::mem::take(&mut self.changed) {
            match ranges.last_mut() {
                Some(range) if range.end == index => range.end += 1,
                _ => ranges.push(index..index + 1),
            }
        }
        ranges
    }

    /// Puts `item` in its existing row, or a new one, and returns the row.
    fn place(&mut self, item: Item) -> usize {
        match self.position_for(&item) {
            Some(index) => {
                self.rows[index] = Row::Item(item);
                self.mark(index);
                index
            }
            None => self.push(Row::Item(item)),
        }
    }

    fn push(&mut self, row: Row) -> usize {
        self.rows.push(row);
        self.mark(self.rows.len() - 1);
        self.rows.len() - 1
    }

    fn mark(&mut self, index: usize) {
        self.changed.insert(index);
    }

    fn seq(&self, index: usize) -> u64 {
        (index - self.run_start) as u64
    }

    fn log_item(&mut self, index: usize, item: Item) {
        let seq = Some(self.seq(index));
        let run = Some(self.run.0);
        self.write(Entry::Item { item, seq, run }, false);
    }

    fn write(&mut self, entry: Entry, sync: bool) {
        self.writes.push(Write { entry, sync });
    }

    /// The row for `item`: the same ID, or the local copy of the same user message.
    fn position_for(&self, item: &Item) -> Option<usize> {
        self.position(&item.id).or_else(|| {
            (item.kind == ItemKind::UserMessage).then(|| {
                self.rows[self.turn_start..]
                    .iter()
                    .position(|row| {
                        matches!(row, Row::Item(local) if local.id.starts_with(LOCAL) && local.text == item.text)
                    })
                    .map(|index| self.turn_start + index)
            })?
        })
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.rows[self.turn_start..]
            .iter()
            .rposition(|row| matches!(row, Row::Item(item) if item.id == id))
            .map(|index| self.turn_start + index)
    }
}

/// Whether a row has anything to show. Reasoning without a summary is skipped.
pub fn visible(row: &Row) -> bool {
    match row {
        Row::Item(item) => match &item.kind {
            // A message of images alone shows only its images' row.
            ItemKind::Reasoning | ItemKind::UserMessage => !item.text.is_empty(),
            // The widget's own row stands for its tool call.
            ItemKind::Tool { name } => name != surface::TOOL,
            _ => true,
        },
        Row::Decision { allowed, .. } => !allowed,
        _ => true,
    }
}

#[cfg(test)]
// Changes are compared as lists of ranges.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    #[test]
    fn provider_notice_survives_turn_events_and_clears_only_when_rechecked() {
        let mut conversation = super::Conversation::default();
        conversation.apply(keystone_agent::Event::ProviderNotice {
            message: Some("Untested CLI; tested range: 2.1.300–2.1.300".into()),
        });
        conversation.apply(keystone_agent::Event::TurnStarted);
        conversation.apply(keystone_agent::Event::TurnCompleted {
            status: keystone_agent::TurnStatus::Completed,
            error: None,
            stopped: false,
        });
        assert!(conversation.provider_notice.is_some());
        assert!(conversation.error.is_none());
        conversation.apply(keystone_agent::Event::ProviderNotice { message: None });
        assert!(conversation.provider_notice.is_none());
    }

    use super::*;

    fn point(at: &str) -> ForkPoint {
        ForkPoint {
            thread: "thread".into(),
            at: at.into(),
        }
    }

    /// Turns of a user message, a command, and a reply, each ending with its fork
    /// point; the last one fails.
    fn forked_turns(conversation: &mut Conversation, turns: &[&str]) {
        for (n, turn) in turns.iter().enumerate() {
            conversation.push_local(turn);
            conversation.apply(Event::TurnStarted);
            let item = |id: &str, kind: ItemKind, text: &str| {
                Event::ItemCompleted(Item {
                    id: format!("{turn}-{id}"),
                    kind,
                    text: text.into(),
                    status: ItemStatus::Completed,
                })
            };
            conversation.apply(item("user", ItemKind::UserMessage, turn));
            conversation.apply(item(
                "command",
                ItemKind::Command {
                    command: "ls".into(),
                    exit_code: Some(0),
                },
                "",
            ));
            conversation.apply(item("reply", ItemKind::AgentMessage, &format!("re {turn}")));
            conversation.apply(Event::ForkPoint(point(turn)));
            let failed = n + 1 == turns.len();
            conversation.apply(Event::TurnCompleted {
                status: if failed {
                    TurnStatus::Failed
                } else {
                    TurnStatus::Completed
                },
                error: failed.then(|| "rate limited".into()),
                stopped: false,
            });
        }
    }

    fn restored(entries: Vec<Entry>) -> Conversation {
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(entries));
        conversation
    }

    #[test]
    fn a_turn_after_saved_checkpoints_is_numbered_above_them() {
        let mut conversation = Conversation::default();
        conversation.checkpoints_saved_through(7);
        forked_turns(&mut conversation, &["one"]);
        assert_eq!(conversation.take_checkpoint_due(), Some(8));
        assert_eq!(conversation.latest_checkpoint(), Some(8));
    }

    #[test]
    fn a_turn_end_held_during_a_load_counts_once_applied() {
        let mut conversation = Conversation::loading();
        conversation.apply(Event::TurnStarted);
        conversation.apply(Event::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
            stopped: false,
        });
        assert!(!conversation.take_turn_end(), "held while loading");
        conversation.restore(Ok(Vec::new()));
        assert!(conversation.take_turn_end());
        assert!(!conversation.take_turn_end());
    }

    #[test]
    fn each_turn_gets_a_checkpoint_on_its_last_reply_numbered_across_restores() {
        let mut live = Conversation::default();
        forked_turns(&mut live, &["one", "two"]);
        assert_eq!(
            live.take_checkpoint_due(),
            Some(2),
            "the latest turn's waits to be saved"
        );
        assert_eq!(live.take_checkpoint_due(), None);
        let restorable: Vec<_> = (0..live.rows.len())
            .filter_map(|index| Some((index, live.checkpoint_at(index)?)))
            .collect();
        assert_eq!(
            restorable,
            [(2, 1), (5, 2)],
            "only each turn's reply carries it"
        );
        let entries: Vec<Entry> = live.take_writes().into_iter().map(|w| w.entry).collect();

        let mut restored = restored(entries.clone());
        assert_eq!(restored.checkpoints, live.checkpoints);
        assert_eq!(
            restored.take_checkpoint_due(),
            None,
            "a restore saves nothing again"
        );
        forked_turns(&mut restored, &["three"]);
        assert_eq!(restored.take_checkpoint_due(), Some(3));
        assert_eq!(
            restored.checkpoint_at(9),
            Some(3),
            "after turn two's failure row"
        );

        let ForkFrom::Native { end, .. } = live.fork_from(2) else {
            panic!("turn one forks natively");
        };
        let fork = live.fork_log(end);
        assert!(
            !fork
                .iter()
                .any(|w| matches!(w.entry, Entry::Checkpoint { .. })),
            "a fork's checkpoints are its own"
        );
    }

    #[test]
    fn a_turn_with_no_reply_has_no_restore_control() {
        let mut live = Conversation::default();
        live.apply(Event::TurnStarted);
        live.apply(Event::ItemCompleted(Item {
            id: "u".into(),
            kind: ItemKind::UserMessage,
            text: "hi".into(),
            status: ItemStatus::Completed,
        }));
        live.apply(Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None,
            stopped: true,
        });
        assert_eq!(
            live.take_checkpoint_due(),
            Some(1),
            "its files are still saved"
        );
        assert!((0..live.rows.len()).all(|index| live.checkpoint_at(index).is_none()));
    }

    #[test]
    fn each_turn_s_fork_point_covers_its_rows_and_survives_a_restore() {
        let mut live = Conversation::default();
        forked_turns(&mut live, &["one", "two", "three"]);
        let marks: Vec<_> = live.forks.iter().map(|m| m.rows.clone()).collect();
        assert_eq!(marks, [0..3, 3..6, 6..9]);
        assert_eq!(live.rows.len(), 10, "the failed turn's end is a row");
        let entries: Vec<Entry> = live.take_writes().into_iter().map(|w| w.entry).collect();
        assert!(entries.contains(&Entry::ForkPoint {
            point: point("two"),
            first: "two-user".into(),
            last: "two-reply".into(),
        }));
        let restored = restored(entries);
        assert_eq!(restored.rows, live.rows);
        assert_eq!(restored.forks, live.forks);

        assert_eq!(
            live.fork_from(4),
            ForkFrom::Native {
                point: &point("two"),
                end: 6
            }
        );
        assert_eq!(
            live.fork_from(8),
            ForkFrom::Native {
                point: &point("three"),
                end: 10
            },
            "a native fork keeps the row that closes its turn"
        );
    }

    #[test]
    fn a_turn_without_a_point_forks_as_a_handoff_and_a_running_one_not_at_all() {
        let mut conversation = Conversation::default();
        forked_turns(&mut conversation, &["one"]);
        conversation.push_local("old");
        conversation.apply(Event::TurnStarted);
        conversation.apply(Event::ItemCompleted(Item {
            id: "old-reply".into(),
            kind: ItemKind::AgentMessage,
            text: "no point".into(),
            status: ItemStatus::Completed,
        }));
        conversation.apply(Event::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
            stopped: false,
        });
        let user = conversation.rows.len();
        conversation.push_local("now");
        conversation.apply(Event::TurnStarted);
        assert_eq!(conversation.fork_from(5), ForkFrom::Handoff { end: user });
        assert_eq!(conversation.fork_from(user), ForkFrom::Running);
        // A point whose rows the log no longer places is not trusted.
        let mut entries: Vec<Entry> = conversation
            .take_writes()
            .into_iter()
            .map(|w| w.entry)
            .collect();
        entries.push(Entry::ForkPoint {
            point: point("lost"),
            first: "gone".into(),
            last: "old-reply".into(),
        });
        assert_eq!(restored(entries).forks.len(), 1);
    }

    #[test]
    fn a_fork_s_log_shows_the_kept_rows_and_keeps_their_fork_points() {
        let mut source = Conversation::default();
        forked_turns(&mut source, &["one", "two", "three"]);
        let ForkFrom::Native { end, .. } = source.fork_from(4) else {
            panic!("a native point");
        };
        let fork = restored(source.fork_log(end).into_iter().map(|w| w.entry).collect());
        assert_eq!(fork.rows, source.rows[..end]);
        assert_eq!(fork.forks, source.forks[..2]);
        assert!(!fork.needs_recovery());
        assert!(!fork.running);
    }

    fn gap_marker(id: &str, count: u64, seq: Option<u64>, run: Option<u64>) -> Entry {
        let mut item = Item {
            id: format!("{}{id}", keystone_agent::log::LOSS_MARKER_PREFIX),
            kind: ItemKind::Other {
                name: String::new(),
            },
            text: String::new(),
            status: ItemStatus::Failed,
        };
        with_lost_count(&mut item, count);
        Entry::Item { item, seq, run }
    }

    fn shown(conversation: &Conversation) -> Vec<String> {
        conversation
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) => Some(item.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn adjacent_log_gap_markers_restore_as_one_gap_and_separate_ones_stay_in_place() {
        let placed = |item: Item, seq| Entry::Item {
            item,
            seq: Some(seq),
            run: Some(7),
        };
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![
            placed(message("kept", "kept at 2", ItemStatus::Completed), 2),
            gap_marker("a", 1, Some(3), Some(7)),
            gap_marker("b", 1, Some(1), Some(7)),
            gap_marker("c", 2, Some(4), Some(7)),
            placed(message("later", "later at 5", ItemStatus::Completed), 5),
        ]));
        assert_eq!(
            shown(&conversation),
            [
                "1 log entry was lost (log budget exceeded).",
                "kept at 2",
                "3 log entries were lost (log budget exceeded).",
                "later at 5",
            ]
        );
    }

    #[test]
    fn a_gap_marker_after_an_unanswered_message_still_asks_for_a_recovery_check() {
        let local = Entry::Item {
            item: Item {
                id: format!("{LOCAL}1"),
                kind: ItemKind::UserMessage,
                text: "hi".into(),
                status: ItemStatus::Completed,
            },
            seq: Some(0),
            run: Some(1),
        };
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![local, gap_marker("reply", 3, Some(1), Some(1))]));
        assert!(conversation.needs_recovery());
        assert_eq!(
            shown(&conversation),
            ["hi", "3 log entries were lost (log budget exceeded)."]
        );
    }

    #[test]
    fn a_gap_marker_saved_again_with_an_earlier_place_moves_before_the_rows_after_it() {
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![
            Entry::Item {
                item: message("kept", "kept at 2", ItemStatus::Completed),
                seq: Some(2),
                run: Some(1),
            },
            gap_marker("same", 1, Some(3), Some(1)),
            gap_marker("same", 2, Some(1), Some(1)),
        ]));
        assert_eq!(
            shown(&conversation),
            [
                "2 log entries were lost (log budget exceeded).",
                "kept at 2"
            ]
        );
    }

    #[test]
    fn the_same_gap_marker_saved_twice_shows_once_with_its_larger_count() {
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![
            gap_marker("same", 1, Some(3), Some(1)),
            gap_marker("same", 2, Some(1), Some(1)),
        ]));
        assert_eq!(
            shown(&conversation),
            ["2 log entries were lost (log budget exceeded)."]
        );
    }

    #[test]
    fn a_gap_marker_without_a_place_merges_with_the_next_runs_marker() {
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![
            Entry::Item {
                item: message("old", "earlier run", ItemStatus::Completed),
                seq: Some(0),
                run: Some(1),
            },
            gap_marker("turn", 1, None, None),
            gap_marker("item", 1, Some(0), Some(100)),
        ]));
        assert_eq!(
            shown(&conversation),
            [
                "earlier run",
                "2 log entries were lost (log budget exceeded).",
            ]
        );
    }

    fn message(id: &str, text: &str, status: ItemStatus) -> Item {
        Item {
            id: id.into(),
            kind: ItemKind::AgentMessage,
            text: text.into(),
            status,
        }
    }

    fn user(id: &str, text: &str) -> Item {
        Item {
            id: id.into(),
            kind: ItemKind::UserMessage,
            text: text.into(),
            status: ItemStatus::Completed,
        }
    }

    fn texts(conversation: &Conversation) -> Vec<&str> {
        conversation
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) => Some(item.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Stands in for the session's log file: keeps what was written, in order.
    #[derive(Default)]
    struct Log(Vec<Entry>);

    impl Log {
        fn save(&mut self, conversation: &mut Conversation) {
            let writes = conversation.take_writes();
            self.0.extend(writes.into_iter().map(|write| write.entry));
        }

        fn restore(&self) -> Conversation {
            let mut conversation = Conversation::loading();
            conversation.restore(Ok(self.0.clone()));
            conversation
        }
    }

    /// Applies each event and saves what it logged, as the app does.
    /// A saved recovery batch replays against the rows logged before it, so a later
    /// turn that reuses an item ID and its partial text is not completed by it.
    #[test]
    fn a_saved_recovery_never_completes_a_later_row_with_the_same_id() {
        use keystone_agent::recovery::{Snapshot, SnapshotTurn, reconcile};
        let mut log = Log(vec![
            Entry::Item {
                item: user("u1", "hi"),
                seq: Some(0),
                run: Some(1),
            },
            Entry::TurnStarted,
            Entry::Item {
                item: message("m", "Hel", ItemStatus::Interrupted),
                seq: Some(1),
                run: Some(1),
            },
        ]);
        let mut live = log.restore();
        assert!(live.needs_recovery());
        let snapshot = Snapshot {
            turns: vec![SnapshotTurn {
                items: vec![
                    user("u1", "hi"),
                    message("m", "Hello", ItemStatus::Completed),
                ],
                end: Some(TurnStatus::Completed),
            }],
            limits: Vec::new(),
        };
        let batch = reconcile(&live.neutral_items(), live.open_turn(), &snapshot).unwrap();
        live.apply_recovery(batch);
        log.save(&mut live);
        live.push_local("again");
        log.save(&mut live);
        feed(
            &mut live,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemStarted(message("m", "Hel", ItemStatus::InProgress)),
                Event::Failed {
                    message: "Claude exited".into(),
                },
            ],
        );
        let statuses = |c: &Conversation| -> Vec<(String, ItemStatus)> {
            c.rows
                .iter()
                .filter_map(|row| match row {
                    Row::Item(item) if item.id == "m" => Some((item.text.clone(), item.status)),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            statuses(&live),
            [
                ("Hello".to_owned(), ItemStatus::Completed),
                ("Hel".to_owned(), ItemStatus::Interrupted)
            ]
        );
        assert_eq!(statuses(&log.restore()), statuses(&live));
    }

    /// A batch whose insertions follow one another restores whole.
    #[test]
    fn a_saved_recovery_restores_insertions_anchored_on_its_own_rows() {
        use keystone_agent::recovery::{Snapshot, SnapshotTurn, reconcile};
        let mut log = Log(vec![
            Entry::Item {
                item: user("u1", "hi"),
                seq: Some(0),
                run: Some(1),
            },
            Entry::TurnStarted,
        ]);
        let mut live = log.restore();
        let snapshot = Snapshot {
            turns: vec![SnapshotTurn {
                items: vec![
                    user("u1", "hi"),
                    message("m1", "one", ItemStatus::Completed),
                    message("m2", "two", ItemStatus::Completed),
                    message("m3", "three", ItemStatus::Completed),
                ],
                end: Some(TurnStatus::Completed),
            }],
            limits: Vec::new(),
        };
        let batch = reconcile(&live.neutral_items(), live.open_turn(), &snapshot).unwrap();
        live.apply_recovery(batch);
        log.save(&mut live);
        assert_eq!(texts(&live), ["hi", "one", "two", "three"]);
        assert_eq!(log.restore().rows, live.rows);
    }

    /// A recovery that adds the provider's rows where the log marked a gap says so on
    /// the marker, now and on every restore.
    #[test]
    fn a_recovery_that_fills_a_marked_gap_stops_claiming_the_loss() {
        use keystone_agent::recovery::{Snapshot, SnapshotTurn, reconcile};
        let mut log = Log(vec![
            Entry::Item {
                item: user("u1", "hi"),
                seq: Some(0),
                run: Some(1),
            },
            Entry::TurnStarted,
            gap_marker("reply", 1, Some(1), Some(1)),
        ]);
        let mut live = log.restore();
        assert!(live.needs_recovery());
        let snapshot = Snapshot {
            turns: vec![SnapshotTurn {
                items: vec![
                    user("u1", "hi"),
                    message("m", "Hello", ItemStatus::Completed),
                ],
                end: Some(TurnStatus::Completed),
            }],
            limits: Vec::new(),
        };
        let batch = reconcile(&live.neutral_items(), live.open_turn(), &snapshot).unwrap();
        assert_eq!(batch.report.extended, 0);
        live.apply_recovery(batch);
        log.save(&mut live);
        let filled = "1 log entry was not saved here (log budget exceeded); recovery has added what the provider recorded.";
        assert_eq!(texts(&live), ["hi", "Hello", filled]);
        assert!(live.rows.iter().all(|row| !matches!(row,
            Row::Item(item) if lost_count(item).is_some())));
        let Some(Row::Item(marker)) = live.rows.iter().find(|row| {
            matches!(row,
            Row::Item(item) if item.id.starts_with(keystone_agent::log::LOSS_MARKER_PREFIX))
        }) else {
            panic!("the marker is still shown");
        };
        let summary = crate::transcript::loss_summary(marker);
        assert!(summary.badges.is_empty() && summary.details.is_empty());
        assert_eq!(summary.headline, filled);
        assert_eq!(log.restore().rows, live.rows);
    }

    #[test]
    fn a_filled_gap_stays_filled_when_a_later_loss_lands_next_to_it() {
        use keystone_agent::recovery::{Snapshot, SnapshotTurn, reconcile};
        let mut log = Log(vec![
            Entry::Item {
                item: user("u1", "hi"),
                seq: Some(0),
                run: Some(1),
            },
            Entry::TurnStarted,
            gap_marker("reply", 1, Some(1), Some(1)),
        ]);
        let mut live = log.restore();
        let snapshot = Snapshot {
            turns: vec![SnapshotTurn {
                items: vec![
                    user("u1", "hi"),
                    message("m", "Hello", ItemStatus::Completed),
                ],
                end: Some(TurnStatus::Completed),
            }],
            limits: Vec::new(),
        };
        let batch = reconcile(&live.neutral_items(), live.open_turn(), &snapshot).unwrap();
        live.apply_recovery(batch);
        log.save(&mut live);
        // The next run's first entry is lost, and its marker sorts right after.
        log.0.push(gap_marker("next", 1, Some(0), Some(2)));
        assert_eq!(
            texts(&log.restore()),
            [
                "hi",
                "Hello",
                "1 log entry was not saved here (log budget exceeded); recovery has added what the provider recorded.",
                "1 log entry was lost (log budget exceeded).",
            ]
        );
    }

    /// The log's own record of a recovery batch it rejected: the history, then what
    /// the log saved for the batch, then anything after it.
    fn rejected_recovery(later: Option<&str>, retried: bool) -> Vec<Entry> {
        use keystone_agent::log::Logs;
        use keystone_agent::recovery::{Outcome, RecoveryReport, TurnEvidence};
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let wake: keystone_agent::Wake = std::sync::Arc::new(|| {});
        let logs = Logs::new(sessions.clone(), wake.clone()).with_budget(800);
        let write = |entry| Write { entry, sync: true };
        logs.append(
            1,
            vec![
                write(Entry::Item {
                    item: user("u1", "hi"),
                    seq: Some(0),
                    run: Some(1),
                }),
                write(Entry::TurnStarted),
            ],
        )
        .unwrap();
        let batch = RecoveryBatch {
            id: "recovery-9".into(),
            operations: vec![Operation::Insert {
                after: "u1".into(),
                item: message(
                    "m",
                    &"A long recovered reply. ".repeat(40),
                    ItemStatus::Completed,
                ),
            }],
            report: RecoveryReport {
                outcome: Outcome::Partial,
                messages: 1,
                tools: 0,
                extended: 0,
                turn: TurnEvidence::Unknown,
                limits: Vec::new(),
            },
            report_after: Some("m".into()),
            closes_turn: true,
        };
        let recovery = || {
            write(Entry::Recovery {
                batch: batch.clone(),
            })
        };
        assert!(logs.append(1, vec![recovery()]).is_err());
        logs.flush_pending().unwrap();
        if retried {
            // Retry once there is room.
            let roomy = Logs::new(sessions.clone(), wake);
            roomy.append(1, vec![recovery()]).unwrap();
            roomy.flush_pending().unwrap();
        }
        if let Some(text) = later {
            logs.append(
                1,
                vec![write(Entry::Item {
                    item: user(&format!("{LOCAL}2"), text),
                    seq: Some(0),
                    run: Some(2),
                })],
            )
            .unwrap();
        }
        logs.flush_pending().unwrap();
        keystone_agent::log::read(&sessions, 1).unwrap()
    }

    #[test]
    fn a_rejected_recovery_batch_restores_in_place_counting_its_rows() {
        let restored = Log(rejected_recovery(Some("next"), false)).restore();
        assert_eq!(
            texts(&restored),
            [
                "hi",
                "2 log entries were lost (log budget exceeded).",
                "next"
            ]
        );
    }

    #[test]
    fn a_rejected_recovery_batch_that_retry_saved_leaves_no_marker() {
        let restored = Log(rejected_recovery(None, true)).restore();
        assert_eq!(
            texts(&restored),
            ["hi", &"A long recovered reply. ".repeat(40)]
        );
        assert!(matches!(restored.rows.last(), Some(Row::Recovery(_))));
    }

    /// How many entries the restored rows say were lost.
    fn restored_lost(conversation: &Conversation) -> u64 {
        conversation
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) => lost_count(item),
                _ => None,
            })
            .sum()
    }

    /// Logs with a small budget whose store cannot be opened until the returned file is
    /// removed, so what they queue waits in the budget.
    fn logs_with_a_broken_store() -> (
        keystone_agent::log::Logs,
        std::path::PathBuf,
        std::path::PathBuf,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let store = keystone_agent::log::path(&sessions, 1);
        std::fs::write(&store, "not a database").unwrap();
        let wake: keystone_agent::Wake = std::sync::Arc::new(|| {});
        let logs = keystone_agent::log::Logs::new(sessions.clone(), wake).with_budget(4096);
        (logs, sessions, store, dir)
    }

    fn saved(entry: Entry) -> Write {
        Write { entry, sync: true }
    }

    #[test]
    fn the_live_loss_count_matches_the_restored_markers_after_a_retry_saves_its_batch() {
        use keystone_agent::recovery::{Outcome, RecoveryReport, TurnEvidence};
        let (logs, sessions, store, _dir) = logs_with_a_broken_store();
        let item = |item, seq| {
            saved(Entry::Item {
                item,
                seq: Some(seq),
                run: Some(1),
            })
        };
        logs.append(1, vec![item(user("u1", "hi"), 0)]).unwrap();
        let mut seq = 1;
        while logs
            .append(
                1,
                vec![item(
                    message(&format!("m{seq}"), &"x".repeat(900), ItemStatus::Completed),
                    seq,
                )],
            )
            .is_ok()
        {
            seq += 1;
        }
        let batch = RecoveryBatch {
            id: "recovery-3".into(),
            operations: vec![Operation::Insert {
                after: "u1".into(),
                item: message(
                    "r",
                    &"A recovered reply. ".repeat(40),
                    ItemStatus::Completed,
                ),
            }],
            report: RecoveryReport {
                outcome: Outcome::Recovered,
                messages: 1,
                tools: 0,
                extended: 0,
                turn: TurnEvidence::Unaffected,
                limits: Vec::new(),
            },
            report_after: Some("r".into()),
            closes_turn: false,
        };
        let recovery = || {
            saved(Entry::Recovery {
                batch: batch.clone(),
            })
        };
        assert!(logs.append(1, vec![recovery()]).is_err());
        assert_eq!(logs.lost_entries(1), 1 + 2);
        std::fs::remove_file(&store).unwrap();
        logs.flush_pending().unwrap();
        // Retry, now that saving freed the budget.
        logs.append(1, vec![recovery()]).unwrap();
        logs.flush_pending().unwrap();
        let restored = Log(keystone_agent::log::read(&sessions, 1).unwrap()).restore();
        assert_eq!(restored_lost(&restored), 1);
        assert_eq!(logs.lost_entries(1), restored_lost(&restored));
    }

    #[test]
    fn the_live_loss_count_matches_the_restored_markers_after_boundaries_collapse() {
        let (logs, sessions, store, _dir) = logs_with_a_broken_store();
        for turn in 0..300 {
            logs.append(1, vec![saved(Entry::TurnStarted)]).unwrap();
            let _ = logs.append(
                1,
                vec![saved(Entry::Item {
                    item: message(&format!("m{turn}"), &"x".repeat(900), ItemStatus::Completed),
                    seq: Some(2 * turn),
                    run: Some(1),
                })],
            );
            logs.append(
                1,
                vec![saved(Entry::TurnEnded {
                    status: TurnStatus::Failed,
                    error: Some("failed".into()),
                    limit: None,
                    seq: Some(2 * turn + 1),
                    run: Some(1),
                })],
            )
            .unwrap();
        }
        logs.flush_pending().unwrap_err();
        std::fs::remove_file(&store).unwrap();
        logs.flush_pending().unwrap();
        let restored = Log(keystone_agent::log::read(&sessions, 1).unwrap()).restore();
        assert!(restored_lost(&restored) > 300, "boundaries were folded in");
        assert_eq!(logs.lost_entries(1), restored_lost(&restored));
    }

    fn report_only(id: &str, operations: Vec<Operation>) -> Entry {
        use keystone_agent::recovery::{Outcome, RecoveryReport, TurnEvidence};
        Entry::Recovery {
            batch: RecoveryBatch {
                id: id.into(),
                operations,
                report: RecoveryReport {
                    outcome: Outcome::Partial,
                    messages: 0,
                    tools: 0,
                    extended: 0,
                    turn: TurnEvidence::Unknown,
                    limits: Vec::new(),
                },
                report_after: None,
                closes_turn: false,
            },
        }
    }

    #[test]
    fn gap_markers_either_side_of_a_batch_that_filled_neither_restore_as_one() {
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![
            Entry::Item {
                item: message("kept", "kept", ItemStatus::Completed),
                seq: Some(0),
                run: Some(1),
            },
            gap_marker("a", 1, Some(1), Some(1)),
            report_only("recovery-1", Vec::new()),
            gap_marker("b", 2, Some(2), Some(1)),
        ]));
        assert_eq!(
            shown(&conversation),
            ["kept", "3 log entries were lost (log budget exceeded)."]
        );
    }

    #[test]
    fn a_gap_marker_a_later_batch_fills_stays_its_own_row() {
        let Entry::Item { item: first, .. } = gap_marker("a", 1, Some(1), Some(1)) else {
            unreachable!()
        };
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![
            Entry::Item {
                item: message("kept", "kept", ItemStatus::Completed),
                seq: Some(0),
                run: Some(1),
            },
            gap_marker("a", 1, Some(1), Some(1)),
            report_only("recovery-1", Vec::new()),
            gap_marker("b", 2, Some(2), Some(1)),
            report_only(
                "recovery-2",
                vec![Operation::Replace {
                    expected: first.clone(),
                    item: keystone_agent::log::filled(&first),
                }],
            ),
        ]));
        assert_eq!(
            shown(&conversation),
            [
                "kept",
                "1 log entry was not saved here (log budget exceeded); recovery has added what the provider recorded.",
                "2 log entries were lost (log budget exceeded).",
            ]
        );
    }

    #[test]
    fn a_later_marker_a_batch_filled_after_merging_it_with_the_next_stays_apart() {
        let mut merged = match gap_marker("b", 1, Some(2), Some(1)) {
            Entry::Item { item, .. } => item,
            _ => unreachable!(),
        };
        with_lost_count(&mut merged, 3);
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![
            Entry::Item {
                item: message("kept", "kept", ItemStatus::Completed),
                seq: Some(0),
                run: Some(1),
            },
            gap_marker("a", 1, Some(1), Some(1)),
            report_only("recovery-1", Vec::new()),
            gap_marker("b", 1, Some(2), Some(1)),
            gap_marker("c", 2, Some(3), Some(1)),
            report_only(
                "recovery-2",
                vec![Operation::Replace {
                    item: keystone_agent::log::filled(&merged),
                    expected: merged,
                }],
            ),
        ]));
        assert_eq!(
            shown(&conversation),
            [
                "kept",
                "1 log entry was lost (log budget exceeded).",
                "3 log entries were not saved here (log budget exceeded); recovery has added what the provider recorded.",
            ]
        );
    }

    #[test]
    fn a_fill_of_a_run_of_merged_gaps_wins_over_an_earlier_fill_of_its_first() {
        let marker = |id, count| match gap_marker(id, count, Some(1), Some(1)) {
            Entry::Item { item, .. } => item,
            _ => unreachable!(),
        };
        let fill = |id: &str, marker: Item| {
            report_only(
                id,
                vec![Operation::Replace {
                    item: keystone_agent::log::filled(&marker),
                    expected: marker,
                }],
            )
        };
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![
            Entry::Item {
                item: message("kept", "kept", ItemStatus::Completed),
                seq: Some(0),
                run: Some(1),
            },
            gap_marker("a", 1, Some(1), Some(1)),
            gap_marker("b", 2, Some(2), Some(1)),
            gap_marker("c", 4, Some(3), Some(1)),
            fill("recovery-1", marker("a", 1)),
            fill("recovery-2", marker("a", 7)),
        ]));
        assert_eq!(
            shown(&conversation),
            [
                "kept",
                "7 log entries were not saved here (log budget exceeded); recovery has added what the provider recorded.",
            ]
        );
    }

    #[test]
    fn a_fill_of_the_merged_gap_wins_over_an_earlier_fill_of_one_marker() {
        let marker = |id, count, seq| {
            let Entry::Item { item, .. } = gap_marker(id, count, Some(seq), Some(1)) else {
                unreachable!()
            };
            item
        };
        let fill = |id: &str, marker: Item| {
            report_only(
                id,
                vec![Operation::Replace {
                    item: keystone_agent::log::filled(&marker),
                    expected: marker,
                }],
            )
        };
        let mut merged = marker("a", 1, 1);
        with_lost_count(&mut merged, 3);
        let mut conversation = Conversation::loading();
        conversation.restore(Ok(vec![
            Entry::Item {
                item: message("kept", "kept", ItemStatus::Completed),
                seq: Some(0),
                run: Some(1),
            },
            gap_marker("a", 1, Some(1), Some(1)),
            gap_marker("b", 2, Some(2), Some(1)),
            fill("recovery-1", marker("a", 1, 1)),
            fill("recovery-2", merged),
        ]));
        assert_eq!(
            shown(&conversation),
            [
                "kept",
                "3 log entries were not saved here (log budget exceeded); recovery has added what the provider recorded.",
            ]
        );
    }

    /// A provider that exits while the history loads is logged after the turn it
    /// ended, so a restart still knows the check is owed.
    #[test]
    fn a_provider_exit_while_history_loads_is_owed_a_check_after_restart() {
        let mut live = Conversation::loading();
        live.push_local("hi");
        for event in [
            Event::TurnStarted,
            Event::ItemStarted(message("m", "Hal", ItemStatus::InProgress)),
            Event::RecoveryNeeded,
            Event::TurnCompleted {
                status: TurnStatus::Failed,
                error: Some("Claude Code stopped".into()),
                stopped: false,
            },
        ] {
            live.apply(event);
        }
        live.restore(Ok(Vec::new()));
        assert!(live.needs_recovery());
        let mut log = Log::default();
        log.save(&mut live);
        let started = log.0.iter().position(|e| *e == Entry::TurnStarted).unwrap();
        let exited = log
            .0
            .iter()
            .position(|e| *e == Entry::ProviderExited)
            .unwrap();
        assert!(started < exited, "{:?}", log.0);
        assert!(log.restore().needs_recovery());
    }

    fn feed(live: &mut Conversation, log: &mut Log, events: impl IntoIterator<Item = Event>) {
        for event in events {
            live.apply(event);
            log.save(live);
        }
    }

    fn completed() -> Event {
        Event::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
            stopped: false,
        }
    }

    #[test]
    fn events_stream_into_rows_and_reload_from_the_log() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        let thread = live.apply(Event::ThreadReady {
            thread_id: "t".into(),
        });
        assert_eq!(thread.as_deref(), Some("t"));
        feed(
            &mut live,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemStarted(message("m", "", ItemStatus::InProgress)),
                Event::ItemDelta {
                    item_id: "m".into(),
                    text: "Hel".into(),
                },
                Event::ItemDelta {
                    item_id: "m".into(),
                    text: "lo".into(),
                },
            ],
        );
        assert_eq!(
            live.rows,
            vec![Row::Item(message("m", "Hello", ItemStatus::InProgress))]
        );
        assert!(live.running);

        live.apply(Event::ApprovalRequested(Approval {
            request: RequestId("1".into()),
            item_id: None,
            action: "rm -rf build".into(),
            detail: None,
        }));
        assert_eq!(
            live.decide(&RequestId("1".into()), false).unwrap().action,
            "rm -rf build"
        );
        log.save(&mut live);
        feed(
            &mut live,
            &mut log,
            [
                Event::ItemCompleted(message("m", "Hello", ItemStatus::Completed)),
                Event::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    error: None,
                    stopped: true,
                },
            ],
        );
        assert!(!live.running);
        // The sync points: the decision and the end of the turn.
        let synced: Vec<_> = live.take_writes();
        assert!(synced.is_empty(), "everything was saved");

        let reloaded = log.restore();
        assert_eq!(reloaded.rows, live.rows, "restored as it was shown");
        assert_eq!(
            reloaded.rows,
            vec![
                Row::Item(message("m", "Hello", ItemStatus::Completed)),
                Row::Decision {
                    action: "rm -rf build".into(),
                    allowed: false
                },
                Row::TurnEnded {
                    status: TurnStatus::Interrupted,
                    error: None,
                    limit: None,
                },
            ]
        );
    }

    #[test]
    fn a_failure_ends_the_turn() {
        let mut live = Conversation::default();
        live.push_local("hello");
        live.apply(Event::Failed {
            message: "Could not start Codex".into(),
        });
        assert!(!live.running);
    }

    /// A failed turn or a refused request leaves the provider where it was, so the
    /// model it reported still holds. Only a lost process or conversation clears it.
    #[test]
    fn a_reported_model_outlives_failures_until_the_provider_is_gone() {
        let mut live = Conversation::default();
        live.apply(Event::ModelReported {
            model: "gpt-6-astra".into(),
        });
        live.apply(Event::TurnCompleted {
            status: TurnStatus::Failed,
            error: Some("turn rejected".into()),
            stopped: false,
        });
        live.apply(Event::Failed {
            message: "Could not steer".into(),
        });
        assert_eq!(live.reported_model.as_deref(), Some("gpt-6-astra"));
        live.apply(Event::ModelLost);
        assert_eq!(live.reported_model, None);

        // Both bypass the history-loading queue in order.
        let mut loading = Conversation::loading();
        loading.apply(Event::ModelReported {
            model: "gpt-6-astra".into(),
        });
        loading.apply(Event::ModelLost);
        assert_eq!(loading.reported_model, None);
        loading.apply(Event::ModelReported {
            model: "gpt-6-sol".into(),
        });
        assert_eq!(loading.reported_model.as_deref(), Some("gpt-6-sol"));
    }

    #[test]
    fn a_failure_settles_the_reply_it_cut_off() {
        let mut live = Conversation::default();
        live.push_local("hello");
        live.apply(Event::ItemStarted(message(
            "m",
            "Half a",
            ItemStatus::InProgress,
        )));
        live.apply(Event::Failed {
            message: "Codex exited".into(),
        });
        assert_eq!(
            live.rows[1],
            Row::Item(message("m", "Half a", ItemStatus::Interrupted))
        );
    }

    #[test]
    fn a_steer_is_restored_after_the_reply_it_interrupted() {
        for echoed in [true, false] {
            let (mut live, mut log) = (Conversation::default(), Log::default());
            live.push_local("Write a poem");
            log.save(&mut live);
            feed(
                &mut live,
                &mut log,
                [
                    Event::TurnStarted,
                    Event::ItemStarted(user("u1", "Write a poem")),
                    Event::ItemCompleted(user("u1", "Write a poem")),
                    Event::ItemStarted(message("m1", "Roses", ItemStatus::InProgress)),
                ],
            );
            // The steer is shown, and logged, while the first reply still streams.
            live.push_local("Make it rhyme");
            log.save(&mut live);
            let mut rest = vec![Event::ItemCompleted(message(
                "m1",
                "Roses are red",
                ItemStatus::Completed,
            ))];
            if echoed {
                rest.push(Event::ItemStarted(user("u2", "Make it rhyme")));
                rest.push(Event::ItemCompleted(user("u2", "Make it rhyme")));
            }
            rest.push(Event::ItemCompleted(message(
                "m2",
                "Violets are blue",
                ItemStatus::Completed,
            )));
            rest.push(completed());
            feed(&mut live, &mut log, rest);

            let shown = [
                "Write a poem",
                "Roses are red",
                "Make it rhyme",
                "Violets are blue",
            ];
            assert_eq!(texts(&live), shown);
            assert_eq!(texts(&log.restore()), shown, "echoed: {echoed}");
        }
    }

    #[test]
    fn an_old_log_keeps_file_order_and_new_rows_follow_it() {
        // Written before entries carried their place.
        let old = [
            r#"{"entry":"item","item":{"id":"local:1-0","kind":{"type":"user_message"},"text":"Old question","status":"completed"}}"#,
            r#"{"entry":"turn_started"}"#,
            r#"{"entry":"item","item":{"id":"local:2-1","kind":{"type":"user_message"},"text":"Old steer","status":"completed"}}"#,
            r#"{"entry":"item","item":{"id":"m","kind":{"type":"agent_message"},"text":"Old reply","status":"completed"}}"#,
            r#"{"entry":"turn_ended","status":"completed","error":null}"#,
        ];
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        std::fs::write(sessions.join("1.jsonl"), old.join("\n")).unwrap();
        keystone_core::Store::open(dir.path().join("workspace.json")).unwrap();
        let mut log = Log(keystone_agent::log::read(&sessions, 1).unwrap());
        assert_eq!(log.0.len(), old.len());
        let mut live = log.restore();
        assert_eq!(texts(&live), ["Old question", "Old steer", "Old reply"]);

        live.push_local("New question");
        log.save(&mut live);
        feed(
            &mut live,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemCompleted(message("m", "New reply", ItemStatus::Completed)),
                completed(),
            ],
        );
        let shown = [
            "Old question",
            "Old steer",
            "Old reply",
            "New question",
            "New reply",
        ];
        assert_eq!(texts(&live), shown);
        let mut again = log.restore();
        assert_eq!(texts(&again), shown);

        // And places keep growing across a second restart.
        again.push_local("Third question");
        log.save(&mut again);
        assert_eq!(texts(&log.restore()).last(), Some(&"Third question"));
    }

    #[test]
    fn what_happens_while_loading_is_applied_after_the_history() {
        let mut log = Log::default();
        let mut first = Conversation::default();
        first.push_local("Earlier");
        log.save(&mut first);
        feed(&mut first, &mut log, [Event::TurnStarted, completed()]);

        let mut live = Conversation::loading();
        live.push_local("Sent while loading");
        let thread = live.apply(Event::ThreadReady {
            thread_id: "t".into(),
        });
        assert_eq!(thread.as_deref(), Some("t"), "the thread is not held back");
        live.apply(Event::TurnStarted);
        assert!(live.rows.is_empty() && live.take_writes().is_empty());
        assert!(live.is_loading());

        live.restore(Ok(log.0.clone()));
        log.save(&mut live);
        assert!(!live.is_loading() && live.running);
        assert_eq!(texts(&live), ["Earlier", "Sent while loading"]);
        assert_eq!(live.take_changes(), [0..2]);
        assert_eq!(texts(&log.restore()), ["Earlier", "Sent while loading"]);
    }

    #[test]
    fn a_message_sent_while_loading_shows_the_turn_running_at_once() {
        // The message has already gone to the provider, so Stop must be offered now,
        // not only once the history arrives.
        let mut live = Conversation::loading();
        live.push_local("Sent while loading");
        assert!(live.running);
        live.restore(Ok(Vec::new()));
        assert!(live.running);
        assert_eq!(texts(&live), ["Sent while loading"]);
    }

    #[test]
    fn changes_report_only_the_rows_that_changed() {
        let mut live = Conversation::default();
        live.push_local("one");
        live.apply(Event::ItemStarted(message("m", "", ItemStatus::InProgress)));
        assert_eq!(live.take_changes(), [0..2]);
        assert!(live.take_changes().is_empty());
        live.apply(Event::ItemDelta {
            item_id: "m".into(),
            text: "streamed".into(),
        });
        assert_eq!(live.take_changes(), [1..2]);
        // A row above changes and one is added: the row between is left alone.
        live.apply(Event::ItemStarted(message("n", "", ItemStatus::InProgress)));
        live.apply(Event::ItemCompleted(user("u", "one")));
        live.apply(Event::ItemStarted(message("o", "", ItemStatus::InProgress)));
        assert_eq!(live.take_changes(), [0..1, 2..4]);
        live.apply(Event::ApprovalRequested(Approval {
            request: RequestId("1".into()),
            item_id: None,
            action: "ls".into(),
            detail: None,
        }));
        assert!(live.take_changes().is_empty(), "approvals are not rows");
    }

    #[test]
    fn a_log_that_cannot_be_read_shows_an_error_and_keeps_what_was_sent() {
        let (mut first, mut log) = (Conversation::default(), Log::default());
        first.push_local("Old question");
        log.save(&mut first);
        feed(
            &mut first,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemCompleted(message("m", "Old reply", ItemStatus::Completed)),
                completed(),
            ],
        );

        // The read fails once, but writing still works.
        let mut live = Conversation::loading();
        live.push_local("New question");
        live.apply(Event::TurnStarted);
        live.restore(Err("permission denied".into()));
        assert!(!live.is_loading() && live.running);
        assert_eq!(texts(&live), ["New question"], "what was sent is shown");
        log.save(&mut live);
        feed(
            &mut live,
            &mut log,
            [
                Event::ItemCompleted(message("m2", "New reply", ItemStatus::Completed)),
                completed(),
            ],
        );
        live.push_local("Another");
        log.save(&mut live);
        let warning = live.load_error.as_deref().expect("the warning stays");
        assert!(warning.contains("permission denied"));

        // Once it can be read, the history comes first and what was sent follows it.
        assert_eq!(
            texts(&log.restore()),
            [
                "Old question",
                "Old reply",
                "New question",
                "New reply",
                "Another"
            ]
        );
    }

    #[test]
    fn the_providers_copy_replaces_a_locally_shown_message() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        live.push_local("Fix the build");
        live.push_local("Never echoed");
        log.save(&mut live);
        let echoed = user("u1", "Fix the build");
        feed(
            &mut live,
            &mut log,
            [
                Event::ItemStarted(echoed.clone()),
                Event::ItemCompleted(echoed.clone()),
            ],
        );
        assert_eq!(live.rows[0], Row::Item(echoed.clone()));

        // After a restart: the echoed message once, and the un-echoed steer kept.
        let mut reloaded = log.restore();
        assert_eq!(reloaded.rows.len(), 2);
        assert_eq!(reloaded.rows[0], Row::Item(echoed));
        assert!(matches!(&reloaded.rows[1], Row::Item(item) if item.text == "Never echoed"));

        // Messages sent after the restart never take the place of earlier ones.
        reloaded.push_local("After restart");
        log.save(&mut reloaded);
        assert_eq!(
            texts(&log.restore()),
            ["Fix the build", "Never echoed", "After restart"]
        );
    }

    /// A sent message's images follow it in a row of their own, which the provider's
    /// echo of the message leaves in place, and which a restart restores after it. The
    /// log holds references, not bytes.
    #[test]
    fn images_sent_with_a_message_follow_it_and_survive_the_echo_and_a_restart() {
        let image = |n: &str| keystone_core::Attachment {
            sha256: n.repeat(64),
            media_type: "image/png".into(),
            name: format!("{n}.png"),
        };
        let images = vec![image("a"), image("b")];
        let (mut live, mut log) = (Conversation::default(), Log::default());
        live.push_sent("What changed?", images.clone());
        assert!(live.writes.iter().any(|write| matches!(&write.entry,
            Entry::Attachments { attachments, seq: Some(1), .. } if *attachments == images)));
        log.save(&mut live);
        let echoed = user("u1", "What changed?");
        feed(
            &mut live,
            &mut log,
            [
                Event::ItemStarted(echoed.clone()),
                Event::ItemCompleted(echoed.clone()),
                Event::ItemCompleted(message("r", "A diagram.", ItemStatus::Completed)),
                completed(),
            ],
        );
        let expected = [
            Row::Item(echoed),
            Row::Attachments(images.clone()),
            Row::Item(message("r", "A diagram.", ItemStatus::Completed)),
        ];
        assert_eq!(live.rows, expected);
        assert_eq!(log.restore().rows, expected);

        // Images alone: the empty message is not shown, its images are.
        let mut alone = Conversation::default();
        alone.push_sent("", vec![image("c")]);
        let shown: Vec<_> = alone.rows.iter().filter(|row| visible(row)).collect();
        assert_eq!(shown, [&Row::Attachments(vec![image("c")])]);
    }

    /// Rows a recovery adds after a message come after the images sent with it, and so
    /// does its report.
    #[test]
    fn a_native_fork_keeps_the_images_sent_in_its_turns() {
        let image = |n: &str| keystone_core::Attachment {
            sha256: n.repeat(64),
            media_type: "image/png".into(),
            name: format!("{n}.png"),
        };
        let mut live = Conversation::default();
        live.push_sent("What changed?", vec![image("a")]);
        let echoed = user("u1", "What changed?");
        for event in [
            Event::TurnStarted,
            Event::ItemCompleted(echoed.clone()),
            Event::ItemCompleted(message("r", "A diagram.", ItemStatus::Completed)),
            Event::ForkPoint(point("one")),
            completed(),
        ] {
            live.apply(event);
        }
        let ForkFrom::Native { end, .. } = live.fork_from(0) else {
            panic!("a native point");
        };
        let fork = Log(live.fork_log(end).into_iter().map(|w| w.entry).collect()).restore();
        assert_eq!(fork.rows, live.rows);
        assert!(matches!(&fork.rows[1], Row::Attachments(images) if *images == [image("a")]));
    }

    #[test]
    fn a_fork_leaves_the_usage_limit_card_with_its_source() {
        let mut live = Conversation::default();
        for event in [
            Event::TurnStarted,
            Event::ItemCompleted(user("u1", "go")),
            Event::ItemCompleted(message("r", "partial", ItemStatus::Completed)),
            Event::UsageLimit(UsageLimit::default()),
            Event::ForkPoint(point("one")),
            Event::TurnCompleted {
                status: TurnStatus::Failed,
                error: Some("You've hit your usage limit.".into()),
                stopped: false,
            },
        ] {
            live.apply(event);
        }
        assert!(live.usage_limit().is_some());
        let ForkFrom::Native { end, .. } = live.fork_from(0) else {
            panic!("a native point");
        };
        let fork = Log(live.fork_log(end).into_iter().map(|w| w.entry).collect()).restore();
        assert_eq!(fork.usage_limit(), None);
        assert!(matches!(
            fork.rows.last(),
            Some(Row::TurnEnded {
                status: TurnStatus::Failed,
                limit: None,
                ..
            })
        ));
    }

    #[test]
    fn a_recovery_keeps_a_message_s_images_under_it() {
        use keystone_agent::recovery::{Snapshot, SnapshotTurn, reconcile};
        let images = vec![keystone_core::Attachment {
            sha256: "a".repeat(64),
            media_type: "image/png".into(),
            name: "a.png".into(),
        }];
        let mut log = Log(vec![
            Entry::Item {
                item: user("u1", "hi"),
                seq: Some(0),
                run: Some(1),
            },
            Entry::Attachments {
                attachments: images.clone(),
                seq: Some(1),
                run: Some(1),
            },
            Entry::TurnStarted,
        ]);
        let mut live = log.restore();
        let snapshot = Snapshot {
            turns: vec![SnapshotTurn {
                items: vec![
                    user("u1", "hi"),
                    message("m1", "one", ItemStatus::Completed),
                ],
                end: Some(TurnStatus::Completed),
            }],
            limits: Vec::new(),
        };
        let batch = reconcile(&live.neutral_items(), live.open_turn(), &snapshot).unwrap();
        live.apply_recovery(batch);
        log.save(&mut live);
        assert!(matches!(&live.rows[1], Row::Attachments(shown) if *shown == images));
        assert!(matches!(&live.rows[2], Row::Item(item) if item.id == "m1"));
        assert_eq!(log.restore().rows, live.rows);
    }

    #[test]
    fn fork_points_follow_rows_a_recovery_inserts_before_them() {
        use keystone_agent::recovery::{Outcome, RecoveryReport, TurnEvidence};
        let item = |item: Item, seq| Entry::Item {
            item,
            seq: Some(seq),
            run: Some(1),
        };
        let batch = RecoveryBatch {
            id: "recovery-1".into(),
            operations: vec![Operation::Insert {
                after: "u1".into(),
                item: message("m1", "recovered", ItemStatus::Completed),
            }],
            report: RecoveryReport {
                outcome: Outcome::Recovered,
                messages: 1,
                tools: 0,
                extended: 0,
                turn: TurnEvidence::Recorded(TurnStatus::Completed),
                limits: Vec::new(),
            },
            report_after: Some("m1".into()),
            closes_turn: true,
        };
        let restored = Log(vec![
            item(user("u1", "one"), 0),
            Entry::TurnStarted,
            Entry::ProviderExited,
            Entry::Recovery { batch },
            Entry::TurnStarted,
            item(user("u2", "two"), 1),
            item(message("r2", "reply two", ItemStatus::Completed), 2),
            Entry::ForkPoint {
                point: point("two"),
                first: "u2".into(),
                last: "r2".into(),
            },
            Entry::TurnEnded {
                status: TurnStatus::Completed,
                error: None,
                limit: None,
                seq: None,
                run: None,
            },
        ])
        .restore();
        let at = |id: &str| {
            restored
                .rows
                .iter()
                .position(|row| matches!(row, Row::Item(item) if item.id == id))
                .unwrap()
        };
        assert!(at("m1") < at("u2"), "{:?}", restored.rows);
        assert_eq!(restored.forks[0].rows, at("u2")..at("r2") + 1);
        let ForkFrom::Native { point: chosen, end } = restored.fork_from(at("u2")) else {
            panic!("a native point");
        };
        assert_eq!(*chosen, point("two"));
        let fork = Log(restored
            .fork_log(end)
            .into_iter()
            .map(|w| w.entry)
            .collect())
        .restore();
        assert_eq!(fork.forks.len(), 1);
    }

    #[test]
    fn a_message_queued_behind_a_turn_is_not_forked_through_that_turn() {
        let mut live = Conversation::default();
        live.push_local("a");
        live.apply(Event::TurnStarted);
        live.apply(Event::ItemCompleted(user("a-user", "a")));
        // Sent while the turn runs, held by the provider for the next turn.
        live.push_local("b");
        let queued = live.rows.len() - 1;
        live.apply(Event::ItemCompleted(message(
            "a-reply",
            "reply a",
            ItemStatus::Completed,
        )));
        live.apply(Event::ForkPoint(point("a")));
        live.apply(completed());
        let ForkFrom::Native { end, .. } = live.fork_from(0) else {
            panic!("a native point");
        };
        let fork = Log(live.fork_log(end).into_iter().map(|w| w.entry).collect()).restore();
        assert_eq!(texts(&fork), ["a", "reply a"]);
        assert_eq!(
            live.fork_from(queued),
            ForkFrom::Handoff {
                end: live.rows.len()
            }
        );
    }

    #[test]
    fn a_compaction_leaves_only_later_turns_forkable() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        for (text, compacts) in [("one", false), ("two", true), ("three", false)] {
            feed(
                &mut live,
                &mut log,
                [
                    Event::TurnStarted,
                    Event::ItemCompleted(user(&format!("{text}-user"), text)),
                ]
                .into_iter()
                .chain(compacts.then_some(Event::ForkPointsLost))
                .chain([
                    Event::ItemCompleted(message(
                        &format!("{text}-reply"),
                        text,
                        ItemStatus::Completed,
                    )),
                    Event::ForkPoint(point(text)),
                    completed(),
                ]),
            );
        }
        for conversation in [&live, &log.restore()] {
            let points: Vec<_> = conversation
                .forks
                .iter()
                .map(|m| m.point.at.as_str())
                .collect();
            assert_eq!(points, ["two", "three"]);
            assert!(matches!(
                conversation.fork_from(0),
                ForkFrom::Handoff { .. }
            ));
            assert!(matches!(conversation.fork_from(2), ForkFrom::Native { .. }));
        }
    }

    #[test]
    fn ids_reused_across_turns_keep_each_turn_s_fork_point() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        for text in ["first", "second"] {
            live.push_local(text);
            log.save(&mut live);
            feed(
                &mut live,
                &mut log,
                [
                    Event::TurnStarted,
                    Event::ItemCompleted(user("prompt", text)),
                    Event::ItemCompleted(message("reused", text, ItemStatus::Completed)),
                    Event::ForkPoint(point(text)),
                    completed(),
                ],
            );
        }
        let ranges = |c: &Conversation| -> Vec<_> {
            c.forks
                .iter()
                .map(|m| (m.rows.clone(), m.point.at.clone()))
                .collect()
        };
        assert_eq!(
            ranges(&live),
            [(0..2, "first".to_owned()), (2..4, "second".to_owned())]
        );
        let restored = log.restore();
        assert_eq!(restored.rows, live.rows);
        assert_eq!(ranges(&restored), ranges(&live));
        let fork = Log(live.fork_log(4).into_iter().map(|w| w.entry).collect()).restore();
        assert_eq!(fork.rows, live.rows);
        assert_eq!(ranges(&fork), ranges(&live));
    }

    #[test]
    fn an_id_reused_in_a_later_turn_adds_a_row() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        for text in ["first", "second"] {
            live.push_local(text);
            log.save(&mut live);
            feed(
                &mut live,
                &mut log,
                [
                    Event::TurnStarted,
                    Event::ItemCompleted(message("reused", text, ItemStatus::Completed)),
                    completed(),
                ],
            );
        }
        for conversation in [&live, &log.restore()] {
            let replies: Vec<_> = conversation
                .rows
                .iter()
                .filter_map(|row| match row {
                    Row::Item(item) if item.kind == ItemKind::AgentMessage => {
                        Some(item.text.as_str())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(replies, ["first", "second"]);
        }
    }

    #[test]
    fn approvals_received_while_loading_still_wait() {
        let approval = |id: &str| Approval {
            request: RequestId(id.into()),
            item_id: None,
            action: format!("cmd {id}"),
            detail: None,
        };
        let mut live = Conversation::loading();
        assert!(!live.awaiting_approval());
        live.apply(Event::ApprovalRequested(approval("1")));
        assert!(live.approvals.is_empty(), "held until the history loads");
        assert!(live.awaiting_approval());
        live.apply(Event::ApprovalResolved {
            request: RequestId("1".into()),
        });
        assert!(!live.awaiting_approval());
        live.apply(Event::ApprovalRequested(approval("2")));
        live.apply(completed());
        assert!(
            !live.awaiting_approval(),
            "a finished turn clears its requests"
        );
    }

    #[test]
    fn approvals_wait_in_order_and_each_is_answered() {
        let mut live = Conversation::default();
        let approval = |id: &str| Approval {
            request: RequestId(id.into()),
            item_id: None,
            action: format!("cmd {id}"),
            detail: None,
        };
        live.apply(Event::ApprovalRequested(approval("1")));
        live.apply(Event::ApprovalRequested(approval("2")));
        assert_eq!(
            live.decide(&RequestId("1".into()), true).unwrap().action,
            "cmd 1"
        );
        assert_eq!(live.approvals, vec![approval("2")]);
        assert!(live.decide(&RequestId("1".into()), true).is_none());
        assert_eq!(live.approvals, vec![approval("2")]);
        assert!(live.take_writes()[0].sync, "a decision is synced");
        live.apply(Event::ApprovalResolved {
            request: RequestId("2".into()),
        });
        assert!(live.approvals.is_empty());
    }

    /// Real codex-cli 0.162.1 traffic through the driver: the third turn was stopped
    /// mid-reply, and Codex never completed that message.
    #[test]
    fn a_real_stopped_codex_reply_survives_reload() {
        use keystone_agent::codex::CodexDriver;
        use keystone_agent::{Command, Outputs};
        const THREAD: &str = "01a1418d-549d-7553-bd08-4183fac44aa7";
        let fixture =
            include_str!("../../keystone-agent/tests/fixtures/codex-0.162.1-thread.jsonl");
        let mut out = Outputs::default();
        let mut driver = CodexDriver::new("test", &mut out);
        driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
        driver.command(
            Command::Open {
                session: 12,
                cwd: "/tmp/repo".into(),
                resume: None,
                model: None,
            },
            &mut out,
        );
        driver.on_line(
            format!(r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}"}}}}}}"#).as_bytes(),
            &mut out,
        );
        for line in fixture.lines() {
            driver.on_line(line.as_bytes(), &mut out);
        }
        let (mut live, mut log) = (Conversation::default(), Log::default());
        feed(&mut live, &mut log, out.events.into_iter().map(|(_, e)| e));
        let last_reply = |c: &Conversation| {
            c.rows.iter().rev().find_map(|row| match row {
                Row::Item(item) if item.kind == ItemKind::AgentMessage => Some(item.text.clone()),
                _ => None,
            })
        };
        let streamed = last_reply(&live).expect("the stopped reply streamed some text");
        assert!(!streamed.is_empty());
        let reloaded = log.restore();
        assert_eq!(last_reply(&reloaded), Some(streamed));
        assert_eq!(reloaded.rows, live.rows);
    }

    #[test]
    fn a_stopped_reply_keeps_its_streamed_text() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        feed(
            &mut live,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemStarted(message("m", "", ItemStatus::InProgress)),
                Event::ItemDelta {
                    item_id: "m".into(),
                    text: "1\n2".into(),
                },
                Event::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    error: None,
                    stopped: true,
                },
            ],
        );
        assert_eq!(
            log.restore().rows[0],
            Row::Item(message("m", "1\n2", ItemStatus::Interrupted))
        );
    }

    #[test]
    fn a_turn_open_at_shutdown_reloads_as_interrupted() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        feed(
            &mut live,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemCompleted(message("m", "partial", ItemStatus::Completed)),
            ],
        );
        assert!(matches!(
            log.restore().rows.last(),
            Some(Row::TurnEnded {
                status: TurnStatus::Interrupted,
                error: Some(_),
                ..
            })
        ));
    }

    fn checklist(call: &str) -> keystone_agent::surface::Surface {
        keystone_agent::surface::parse(call, &keystone_agent::surface::examples::checklist())
            .unwrap()
    }

    fn surface_rows(conversation: &Conversation) -> Vec<&keystone_agent::surface::Surface> {
        conversation
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Surface(surface) => Some(surface),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_widget_waits_until_pressed_and_its_state_survives_a_restore() {
        let mut live = Conversation::default();
        live.apply(Event::TurnStarted);
        live.apply(Event::ItemStarted(Item {
            id: "toolu_1".into(),
            kind: ItemKind::Tool {
                name: keystone_agent::surface::TOOL.into(),
            },
            text: String::new(),
            status: ItemStatus::InProgress,
        }));
        live.apply(Event::SurfaceShown {
            surface: checklist("toolu_1"),
            waiting: true,
        });
        // The widget stands for its tool call, whose own row is not shown.
        assert!(!visible(&live.rows[0]));
        assert!(live.awaiting_widget());
        assert_eq!(
            live.settle_blocker(),
            Some("Answer the widget before settling this session.")
        );
        let shown = live.take_writes();
        assert!(matches!(
            &shown[..],
            [
                _,
                Write {
                    entry: Entry::Surface { .. },
                    sync: true
                }
            ]
        ));

        // Edits wait to be logged, so typing logs the widget once.
        assert_eq!(
            live.edit_surface("toolu_1", "/items/tests", serde_json::json!(true)),
            Ok(true)
        );
        assert_eq!(
            live.edit_surface("toolu_1", "/items/tests", serde_json::json!(true)),
            Ok(false)
        );
        assert!(live.take_writes().is_empty());
        live.save_surfaces();
        assert_eq!(live.take_writes().len(), 1);

        let press = live.press("toolu_1", "send", 1_760_000_000_000).unwrap();
        assert!(press.waiting);
        let action: serde_json::Value = serde_json::from_str(&press.action).unwrap();
        assert_eq!(
            action["action"]["context"]["items"],
            serde_json::json!({"docs": true, "tests": true, "changelog": false})
        );
        assert!(!live.awaiting_widget());
        assert_eq!(live.settle_blocker(), None);
        // A second press is no longer the tool's result.
        assert!(!live.press("toolu_1", "skip", 1).unwrap().waiting);

        let mut entries: Vec<Entry> = shown.into_iter().map(|write| write.entry).collect();
        entries.extend(live.take_writes().into_iter().map(|write| write.entry));
        let mut restored = Conversation::loading();
        restored.restore(Ok(entries));
        let surfaces = surface_rows(&restored);
        assert_eq!(surfaces.len(), 1, "the latest entry replaces earlier ones");
        assert_eq!(
            surfaces[0].status,
            keystone_agent::surface::Status::Answered
        );
        assert_eq!(surfaces[0].last_action.as_deref(), Some("skip"));
        assert_eq!(
            surfaces[0].value("/items/tests"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn a_turn_that_ends_stops_its_waiting_widget_and_so_does_a_restart() {
        let mut live = Conversation::default();
        live.apply(Event::TurnStarted);
        live.apply(Event::SurfaceShown {
            surface: checklist("call-1"),
            waiting: true,
        });
        let mut entries: Vec<Entry> = live.take_writes().into_iter().map(|w| w.entry).collect();
        // Keystone closed with the widget waiting: no live call can take its answer.
        let mut restored = Conversation::loading();
        restored.restore(Ok(entries.clone()));
        assert!(!restored.awaiting_widget());
        assert_eq!(
            surface_rows(&restored)[0].status,
            keystone_agent::surface::Status::Stopped
        );
        assert!(!restored.press("call-1", "send", 1).unwrap().waiting);

        live.apply(Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None,
            stopped: true,
        });
        assert!(!live.awaiting_widget());
        entries.extend(live.take_writes().into_iter().map(|w| w.entry));
        assert!(entries.iter().any(|entry| matches!(entry,
            Entry::Surface { surface, .. } if surface.status == keystone_agent::surface::Status::Stopped)));
    }

    /// A recovery that inserts rows above a widget being edited still logs the edit.
    #[test]
    fn a_recovery_above_an_edited_widget_keeps_its_pending_save() {
        use keystone_agent::recovery::{Snapshot, SnapshotTurn, reconcile};
        let log = Log(vec![
            Entry::Item {
                item: user("u1", "hi"),
                seq: Some(0),
                run: Some(1),
            },
            Entry::TurnStarted,
            Entry::Surface {
                surface: checklist("call-1"),
                seq: Some(1),
                run: Some(1),
            },
        ]);
        let mut live = log.restore();
        assert!(live.needs_recovery());
        assert_eq!(
            live.edit_surface("call-1", "/items/tests", serde_json::json!(true)),
            Ok(true)
        );
        let snapshot = Snapshot {
            turns: vec![SnapshotTurn {
                items: vec![
                    user("u1", "hi"),
                    message("m", "Hello", ItemStatus::Completed),
                ],
                end: Some(TurnStatus::Completed),
            }],
            limits: Vec::new(),
        };
        let batch = reconcile(&live.neutral_items(), live.open_turn(), &snapshot).unwrap();
        live.apply_recovery(batch);
        live.take_writes();
        live.save_surfaces();
        let writes = live.take_writes();
        assert!(
            writes.iter().any(|write| matches!(&write.entry,
                Entry::Surface { surface, .. }
                    if surface.value("/items/tests") == Some(&serde_json::json!(true)))),
            "the edited widget is logged"
        );
    }

    /// A widget that waited during a turn that ended while the history loads no
    /// longer blocks settling.
    #[test]
    fn a_widget_whose_turn_ended_while_history_loads_does_not_block_settling() {
        let mut loading = Conversation::loading();
        loading.apply(Event::TurnStarted);
        loading.apply(Event::SurfaceShown {
            surface: checklist("call-1"),
            waiting: true,
        });
        loading.apply(Event::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
            stopped: false,
        });
        assert!(!loading.awaiting_widget());
        assert_eq!(loading.settle_blocker(), None);
    }

    /// The provider giving up on one widget's call, live or while the history loads,
    /// stops that widget only; another one still waits.
    #[test]
    fn a_withdrawn_widget_call_stops_only_that_widget() {
        for mut conversation in [Conversation::default(), Conversation::loading()] {
            conversation.apply(Event::TurnStarted);
            for call in ["call-1", "call-2"] {
                conversation.apply(Event::SurfaceShown {
                    surface: checklist(call),
                    waiting: true,
                });
            }
            conversation.apply(Event::SurfaceStopped {
                call: "call-1".into(),
            });
            assert!(conversation.awaiting_widget());
            conversation.apply(Event::SurfaceStopped {
                call: "call-2".into(),
            });
            assert!(!conversation.awaiting_widget());
            assert_eq!(conversation.settle_blocker(), None);
        }

        let mut live = Conversation::default();
        live.apply(Event::TurnStarted);
        live.apply(Event::SurfaceShown {
            surface: checklist("call-1"),
            waiting: true,
        });
        live.take_writes();
        live.apply(Event::SurfaceStopped {
            call: "call-1".into(),
        });
        let Some(Row::Surface(surface)) = live.rows.last() else {
            panic!("the widget row stays");
        };
        assert_eq!(surface.status, keystone_agent::surface::Status::Stopped);
        assert!(
            live.take_writes().iter().any(|write| matches!(&write.entry,
                Entry::Surface { surface, .. }
                    if surface.status == keystone_agent::surface::Status::Stopped)),
            "Stopped is saved"
        );
    }

    #[test]
    fn a_widget_shown_while_history_loads_still_needs_the_user() {
        let mut loading = Conversation::loading();
        loading.apply(Event::SurfaceShown {
            surface: checklist("call-1"),
            waiting: true,
        });
        assert!(loading.awaiting_widget());
        loading.restore(Ok(Vec::new()));
        assert!(loading.awaiting_widget());
    }
}
