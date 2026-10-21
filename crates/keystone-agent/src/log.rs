//! Durable neutral transcript entries, holding completed items and turn boundaries.
//!
//! Nothing is written per streamed chunk. Each queued batch commits atomically to
//! SQLite with synchronous=FULL. [`Logs`] does the reading and writing off the UI
//! thread, retaining stable event keys until a successful commit is confirmed.
//!
//! Failed writes are retained in memory up to [`MAX_PENDING_BYTES`]. This supports
//! repair and retry within the running app, not exactly-once recovery after a crash.
//! Entries that do not fit are lost, and a gap marker saved in their place says so.

use crate::connection::Wake;
use crate::item::{Item, ItemKind, ItemStatus, TurnStatus};
use crate::worker::Worker;
use keystone_core::{Database, Durability};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

/// One entry in a session log.
///
/// `seq` is the row's place in the transcript as it was shown: the order in which rows
/// first appeared, not the order they completed. A steer appears before the reply it
/// interrupted completes, so completion order would put it first. Logs written before
/// `seq` existed have none and are shown in file order.
///
/// `run` identifies the run of Keystone that wrote the entry, and `seq` counts from the
/// first row that run added. A restore places each run after every row before it, so
/// a run can log without knowing the saved history, such as when reading it failed.
/// Entries without a `run` count from the start of the log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum Entry {
    TurnStarted,
    Item {
        item: Item,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<u64>,
    },
    Approval {
        action: String,
        allowed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<u64>,
    },
    TurnEnded {
        status: TurnStatus,
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<u64>,
    },
    /// Rows recovered from the provider's store after Keystone lost track of a turn,
    /// with the report shown for them. Applied whole on every restore; builds before
    /// recovery skip it, as they skip any entry they cannot read.
    Recovery {
        batch: crate::recovery::RecoveryBatch,
    },
    /// The provider exited during a turn, so what it saved must be checked. A later
    /// `Recovery` or a new turn settles it; until then every restore checks again.
    ProviderExited,
    /// An agent-built widget, whole, saved each time it changes. The latest entry for
    /// a `call` replaces earlier ones; `seq` and `run` place its row as for an item.
    Surface {
        surface: crate::surface::Surface,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<u64>,
    },
}

/// An entry to append. Every SQLite write syncs, even when `sync` is false.
#[derive(Clone, Debug, PartialEq)]
pub struct Write {
    pub entry: Entry,
    pub sync: bool,
}

pub struct SessionLog {
    database: Database,
    session: u64,
    #[cfg(test)]
    faults: VecDeque<Fault>,
}

/// The workspace database beside the supplied sessions directory.
pub fn path(dir: &Path, _session: u64) -> PathBuf {
    dir.parent().unwrap_or(dir).join("workspace.sqlite3")
}

fn open_database(path: &Path) -> io::Result<Database> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    Database::open(path)
}

impl SessionLog {
    pub fn open(dir: &Path, session: u64) -> io::Result<Self> {
        Ok(Self::with_database(
            open_database(&path(dir, session))?,
            session,
        ))
    }

    fn with_database(database: Database, session: u64) -> Self {
        Self {
            database,
            session,
            #[cfg(test)]
            faults: VecDeque::new(),
        }
    }

    pub fn append(&mut self, entry: &Entry) -> io::Result<()> {
        match self.append_records(&[(event_key(), serde_json::to_string(entry)?)])? {
            Durability::Synced => Ok(()),
            Durability::DirectorySyncPending(error) => Err(io::Error::other(error)),
        }
    }

    pub fn sync(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if matches!(self.faults.front(), Some(Fault::Committed)) {
            self.faults.pop_front();
            return Err(io::Error::other("injected creation sync failure"));
        }
        self.database.sync_directory()
    }

    fn append_records(&mut self, records: &[(String, String)]) -> io::Result<Durability> {
        #[cfg(test)]
        if matches!(self.faults.front(), Some(Fault::Transaction)) {
            self.faults.pop_front();
            let mut invalid = records.to_vec();
            invalid.push((event_key(), "{".into()));
            let error = self
                .database
                .append_logs(self.session, &invalid)
                .expect_err("SQLite must reject malformed JSON and roll back the batch");
            return Err(io::Error::other(format!(
                "injected transaction failure: {error}"
            )));
        }
        let durability = self.database.append_logs(self.session, records)?;
        #[cfg(test)]
        if matches!(self.faults.front(), Some(Fault::CommittedThenFailed)) {
            self.faults.pop_front();
            return Err(io::Error::other("injected failure after commit"));
        }
        #[cfg(test)]
        if matches!(self.faults.front(), Some(Fault::Committed)) {
            self.faults.pop_front();
            return Ok(Durability::DirectorySyncPending(
                "injected creation sync failure".into(),
            ));
        }
        Ok(durability)
    }
}

fn event_key() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    format!(
        "log-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn read_database(database: &Database, session: u64) -> io::Result<Vec<Entry>> {
    Ok(database
        .read_logs(session)?
        .into_iter()
        .filter_map(|payload| serde_json::from_str(&payload).ok())
        .collect())
}

/// Reads a session's entries without creating a missing database. Unsupported
/// payloads are skipped, as unreadable neutral entries were in legacy logs.
pub fn read(dir: &Path, session: u64) -> io::Result<Vec<Entry>> {
    let path = path(dir, session);
    match std::fs::metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    read_database(&Database::open_read_only(path)?, session)
}

/// What [`Logs`] finished, for the UI to apply.
#[derive(Debug, PartialEq)]
pub enum Report {
    Loaded {
        session: u64,
        entries: Result<Vec<Entry>, String>,
    },
    /// `retryable` distinguishes outstanding I/O from terminal admission loss.
    /// `lost` counts rejected entries, including ones whose gap marker is saved.
    Saved {
        session: u64,
        error: Option<String>,
        retryable: bool,
        lost: u64,
    },
    /// Everything queued for `session` before [`Logs::checkpoint`] with `ticket` is
    /// saved, or `error` says why not.
    /// `error` names only work a retry can still save; `lost` counts the session's
    /// rejected entries so far, whose markers are saved like any entry.
    Checkpoint {
        session: u64,
        ticket: u64,
        error: Option<String>,
        lost: u64,
    },
}

/// Reads a session's saved entries. [`read`], unless a test needs a slower disk.
pub type Reader = Arc<dyn Fn(&Path, u64) -> io::Result<Vec<Entry>> + Send + Sync>;

/// Every session's log, read and written off the UI thread so the UI never waits on
/// the disk.
///
/// Writes run on one worker thread in the order they were queued, so a session's
/// entries are written in order. Each load reads on a thread of its own, so a read that
/// never returns, such as from a stalled network disk, holds up neither the writes nor
/// other sessions' loads, and quitting need not wait for it. A session is loaded once,
/// before anything is written for it, so a load never races that session's writes.
pub struct Logs {
    writer: Worker,
    dir: PathBuf,
    read: Reader,
    files: Arc<Mutex<Files>>,
    budget: Arc<Mutex<Budget>>,
    reports: Arc<Mutex<Vec<Report>>>,
    /// How many loads have not reported yet.
    loading: Arc<(Mutex<usize>, Condvar)>,
    wake: Wake,
}

struct Files {
    path: PathBuf,
    database: Option<Database>,
    sessions: BTreeMap<u64, PendingSession>,
    failing: BTreeMap<u64, SaveFailure>,
}

#[derive(Clone, PartialEq, Eq)]
struct SaveFailure {
    message: String,
    retryable: bool,
}

/// The default for the serialized bytes retained across the worker mailbox and failed
/// writes, for every session together. An item or approval over a quarter of the budget
/// keeps both ends of its text. An entry that still does not fit the room left is rejected, and
/// the session's log gets a gap marker in its place: an item with the
/// [`LOSS_MARKER_PREFIX`] that counts the missing entries and is placed where the
/// first of them was. A rejected recovery batch gets a marker of its own, named after
/// the batch and counting its rows, which a restore drops if a Retry saved the batch.
/// Later entries are admitted again once there is room, and the marker is saved like
/// any entry, not only at Quit. Turn boundaries and provider exits are never rejected:
/// they may use a small reserve beyond the budget, with a turn error cut to fit, and past
/// it a turn's error is not kept. Boundaries that do not fit even then are kept outside
/// the budget: after a failed save a session keeps one turn of them, and the earlier
/// ones join the gap marker as lost; once committed, they no longer wait in memory for
/// a failing directory barrier. Markers are not counted either: rejections with no
/// admitted entry between them share one marker, saved again with each new count, even
/// behind a failing barrier, so a crash never leaves it undercounting, and
/// a session's queue holds at most one marker more than the entries it holds, plus
/// one per rejected recovery batch. A
/// session's strict [`Logs::flush`] stays failed for the lifetime of these Logs;
/// [`Logs::flush_pending`] reports only retryable work. Queue metadata is additional
/// memory.
pub const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
pub const LOSS_MARKER_PREFIX: &str = "keystone-log-loss-";

/// A gap marker's text for `count` lost entries.
pub fn loss_text(count: u64) -> String {
    if count == 1 {
        "1 log entry was lost (log budget exceeded).".to_owned()
    } else {
        format!("{count} log entries were lost (log budget exceeded).")
    }
}

/// How many entries a gap marker says were lost, or `None` for any other item,
/// including a marker a recovery has since [`filled`].
pub fn lost_count(item: &Item) -> Option<u64> {
    if !item.id.starts_with(LOSS_MARKER_PREFIX) || item.status != ItemStatus::Failed {
        return None;
    }
    let digits = item.text.split(' ').next()?;
    digits.parse().ok()
}

/// `marker` standing for `count` lost entries.
pub fn with_lost_count(marker: &mut Item, count: u64) {
    marker.text = loss_text(count);
    marker.kind = ItemKind::Other {
        name: format!("Keystone log loss: {}", marker.text),
    };
}

/// `marker` once a recovery has added what the provider recorded where its entries
/// were lost, so it no longer says they are missing.
pub fn filled(marker: &Item) -> Item {
    let count = lost_count(marker).unwrap_or(1);
    let entries = if count == 1 {
        "1 log entry was".to_owned()
    } else {
        format!("{count} log entries were")
    };
    let text = format!(
        "{entries} not saved here (log budget exceeded); recovery has added what the provider recorded."
    );
    Item {
        id: marker.id.clone(),
        kind: ItemKind::Other {
            name: format!("Keystone log loss: {text}"),
        },
        text,
        status: ItemStatus::Completed,
    }
}

struct Budget {
    bytes: usize,
    limit: usize,
    /// Entries rejected for each session.
    rejected: BTreeMap<u64, u64>,
    /// Recovery batches rejected so far, so a batch rejected again on Retry is counted
    /// once.
    batches: BTreeSet<String>,
}

fn limit_error(session: u64, entries: u64) -> String {
    let entries = if entries == 1 {
        "1 entry was".to_owned()
    } else {
        format!("{entries} entries were")
    };
    format!(
        "Session {session}: the log retry buffer limit was exceeded. {entries} rejected and not saved; a marker in the conversation shows where, and later entries are still saved."
    )
}

/// Rejected entries in a row, which one marker stands for.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Gap {
    count: u64,
    /// The place of the first of them that was shown, so the marker is shown there.
    seq: Option<u64>,
    run: Option<u64>,
}

impl Gap {
    /// A recovery batch counts the rows it would have added: one per insertion and its
    /// report.
    fn of(entry: &Entry) -> Self {
        let (seq, run) = match entry {
            Entry::Item { seq, run, .. }
            | Entry::Approval { seq, run, .. }
            | Entry::TurnEnded { seq, run, .. }
            | Entry::Surface { seq, run, .. } => (*seq, *run),
            _ => (None, None),
        };
        let count = match entry {
            Entry::Recovery { batch } => {
                let inserts = batch.operations.iter().filter(|operation| {
                    matches!(operation, crate::recovery::Operation::Insert { .. })
                });
                inserts.count() as u64 + 1
            }
            _ => 1,
        };
        Self { count, seq, run }
    }

    fn merge(self, later: Gap) -> Gap {
        let first = match (self.seq, later.seq) {
            (None, Some(_)) => later,
            (Some(a), Some(b)) if b < a && self.run == later.run => later,
            _ => self,
        };
        Gap {
            count: self.count.saturating_add(later.count),
            ..first
        }
    }

    fn record(&self, id: &str) -> String {
        let mut item = Item {
            id: id.to_owned(),
            kind: ItemKind::Other {
                name: String::new(),
            },
            text: String::new(),
            status: ItemStatus::Failed,
        };
        with_lost_count(&mut item, self.count);
        let entry = Entry::Item {
            item,
            seq: self.seq,
            run: self.run,
        };
        let mut line = serde_json::to_string(&entry).expect("log loss marker is serializable");
        line.push('\n');
        line
    }
}

struct PendingWrite {
    record: (String, String),
    kind: Kind,
}

enum Kind {
    /// An entry counted in the budget.
    Entry,
    /// A gap marker, not counted in the budget: its item ID, the gap it stands for, and
    /// whether it stands for one recovery batch alone, under that batch's ID.
    Marker { id: String, gap: Gap, batch: bool },
    /// A turn boundary admitted past the reserve, not counted in the budget: which
    /// boundary, and the gap it leaves when a later one collapses it.
    Spare {
        boundary: std::mem::Discriminant<Entry>,
        gap: Gap,
    },
}

/// What admission passes to the writer, in order.
enum Queued {
    Write(PendingWrite),
    /// Rejected entries in a row, or one rejected recovery batch with its ID.
    Gap {
        gap: Gap,
        batch: Option<String>,
    },
}

#[derive(Default)]
struct PendingSession {
    log: Option<SessionLog>,
    writes: VecDeque<PendingWrite>,
    committed: usize,
}

struct LimitedLine {
    bytes: Vec<u8>,
    limit: usize,
}

impl io::Write for LimitedLine {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("log retry buffer limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
enum Fault {
    Transaction,
    Committed,
    /// The transaction commits, but the append still reports an error.
    CommittedThenFailed,
}

impl Logs {
    /// `wake` runs after a report is queued; then call [`Logs::drain`].
    pub fn new(dir: PathBuf, wake: Wake) -> Self {
        Self::with_reader(dir, wake, Arc::new(read))
    }

    pub fn with_reader(dir: PathBuf, wake: Wake, read: Reader) -> Self {
        Self::create(dir.clone(), None, wake, read)
    }

    pub fn with_database(database: Database, wake: Wake) -> Self {
        let dir = database
            .path()
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join("sessions");
        let reader = database.clone();
        Self::create(
            dir,
            Some(database),
            wake,
            Arc::new(move |_, session| read_database(&reader, session)),
        )
    }

    fn create(dir: PathBuf, database: Option<Database>, wake: Wake, read: Reader) -> Self {
        Self {
            writer: Worker::new("session-logs"),
            files: Arc::new(Mutex::new(Files {
                path: path(&dir, 0),
                database,
                sessions: BTreeMap::new(),
                failing: BTreeMap::new(),
            })),
            budget: Arc::new(Mutex::new(Budget {
                bytes: 0,
                limit: MAX_PENDING_BYTES,
                rejected: BTreeMap::new(),
                batches: BTreeSet::new(),
            })),
            dir,
            read,
            reports: Arc::default(),
            loading: Arc::default(),
            wake,
        }
    }

    /// Reads the session's entries and reports them as [`Report::Loaded`].
    pub fn load(&self, session: u64) {
        let (dir, read) = (self.dir.clone(), self.read.clone());
        let (reports, loading, wake) = (
            self.reports.clone(),
            self.loading.clone(),
            self.wake.clone(),
        );
        *loading.0.lock().unwrap() += 1;
        let finish = move |entries: Result<Vec<Entry>, String>| {
            // Reported before the count drops, so a finished wait finds the report.
            reports
                .lock()
                .unwrap()
                .push(Report::Loaded { session, entries });
            *loading.0.lock().unwrap() -= 1;
            loading.1.notify_all();
            wake();
        };
        let on_thread = finish.clone();
        let spawned = thread::Builder::new()
            .name("session-log-read".into())
            .spawn(move || on_thread(read(&dir, session).map_err(|e| e.to_string())));
        if let Err(error) = spawned {
            finish(Err(error.to_string()));
        }
    }

    /// Waits until every load has reported, or `deadline` passes. Returns whether
    /// they all reported.
    pub fn wait_for_loads(&self, deadline: Instant) -> bool {
        let (count, reported) = &*self.loading;
        let mut count = count.lock().unwrap();
        while *count > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            count = reported.wait_timeout(count, deadline - now).unwrap().0;
        }
        true
    }

    /// Bounds retained bytes at `bytes` instead of [`MAX_PENDING_BYTES`].
    pub fn with_budget(self, bytes: usize) -> Self {
        self.budget.lock().unwrap().limit = bytes;
        self
    }

    /// Queues entries in order. I/O failures retain them for the next append or flush
    /// to retry. An item or approval larger than a quarter of the budget keeps both ends
    /// of its text and notes what was left out. An entry that still does not fit the room
    /// left is rejected: the session's log gets a marker in its place, and this
    /// returns an error. Later entries are admitted whenever they fit.
    pub fn append(&self, session: u64, writes: Vec<Write>) -> Result<(), String> {
        if writes.is_empty() {
            return Ok(());
        }
        let mut admission = self.budget.lock().unwrap();
        let mut queued = Vec::new();
        let mut bytes = 0;
        // Entries refused in this call, and how many of them are newly lost.
        let mut rejected = 0;
        let mut newly_lost = 0;
        for write in writes {
            let used = admission.bytes + bytes;
            let room = admission.limit.saturating_sub(used);
            // Turn boundaries and provider exits are small and shape how a restore reads
            // the rows around them, so they may use a reserve beyond the budget.
            let boundary = matches!(
                write.entry,
                Entry::TurnStarted | Entry::TurnEnded { .. } | Entry::ProviderExited
            );
            let mut spare = false;
            let (entry, line) = if boundary {
                let mut entry = write.entry;
                let mut line = serialize(&entry, room);
                let reserve = (admission.limit / 64).max(BOUNDARY_BYTES);
                let reserved = (admission.limit + reserve).saturating_sub(used);
                if line.is_none() {
                    entry = fit(entry, BOUNDARY_BYTES);
                    line = serialize(&entry, reserved);
                }
                // Past the reserve a boundary is still kept, without its error. One that
                // still does not fit is kept outside the budget, and the writer collapses
                // a run of them.
                if line.is_none() {
                    if let Entry::TurnEnded {
                        error: Some(error), ..
                    } = &mut entry
                    {
                        *error = "The error was not saved (log budget exceeded).".into();
                    }
                    line = serialize(&entry, reserved);
                }
                if line.is_none() {
                    line = serialize(&entry, usize::MAX);
                    spare = true;
                }
                (entry, line)
            } else {
                // A fixed cap, so a message and the provider's copy of it save the same.
                let entry = fit(write.entry, admission.limit / 4);
                let line = serialize(&entry, room);
                (entry, line)
            };
            if let Some(line) = line {
                let kind = if spare {
                    Kind::Spare {
                        boundary: std::mem::discriminant(&entry),
                        gap: Gap::of(&entry),
                    }
                } else {
                    bytes += line.len();
                    Kind::Entry
                };
                queued.push(Queued::Write(PendingWrite {
                    record: (event_key(), line),
                    kind,
                }));
                continue;
            }
            rejected += 1;
            let gap = Gap::of(&entry);
            let batch = match &entry {
                Entry::Recovery { batch } => Some(batch.id.clone()),
                _ => None,
            };
            if batch
                .as_ref()
                .is_none_or(|id| admission.batches.insert(id.clone()))
            {
                newly_lost += gap.count;
            }
            match (queued.last_mut(), &batch) {
                (
                    Some(Queued::Gap {
                        gap: held,
                        batch: None,
                    }),
                    None,
                ) => *held = held.merge(gap),
                _ => queued.push(Queued::Gap { gap, batch }),
            }
        }
        admission.bytes += bytes;
        let lost = if rejected > 0 {
            let lost = admission.rejected.entry(session).or_default();
            *lost = lost.saturating_add(newly_lost);
            *lost
        } else {
            0
        };
        let (files, reports, wake) = (self.files.clone(), self.reports.clone(), self.wake.clone());
        let budget = self.budget.clone();
        self.writer.run(move || {
            let mut files = files.lock().unwrap();
            let pending = files.sessions.entry(session).or_default();
            for queued in queued {
                pending.queue(queued);
            }
            let report = files.retry(session, &budget).or_else(|| {
                // The loss is news even while an earlier failure is unchanged.
                (rejected > 0).then(|| {
                    let lost = budget.lock().unwrap().rejected[&session];
                    let failure = files
                        .failing
                        .get(&session)
                        .filter(|failure| failure.retryable);
                    Report::Saved {
                        session,
                        error: Some(failure.map_or_else(
                            || limit_error(session, lost),
                            |failure| failure.message.clone(),
                        )),
                        retryable: failure.is_some(),
                        lost,
                    }
                })
            });
            drop(files);
            if let Some(report) = report {
                reports.lock().unwrap().push(report);
                wake();
            }
        });
        drop(admission);
        if rejected > 0 {
            Err(limit_error(session, lost))
        } else {
            Ok(())
        }
    }

    pub fn lost_entries(&self, session: u64) -> u64 {
        self.budget
            .lock()
            .unwrap()
            .rejected
            .get(&session)
            .copied()
            .unwrap_or(0)
    }

    /// Retries pending transactions, then reports every outstanding
    /// failure, including entries admission rejected. This strict barrier never
    /// succeeds after a rejection; the desktop's Quit uses [`Logs::flush_pending`].
    pub fn flush(&self) -> Result<(), String> {
        self.barrier(true)
    }

    /// Saves retained entries and gap markers, failing only on work a retry can still
    /// save: entries admission rejected never veto.
    pub fn flush_pending(&self) -> Result<(), String> {
        self.barrier(false)
    }

    fn barrier(&self, strict: bool) -> Result<(), String> {
        let (files, budget, reports, wake) = (
            self.files.clone(),
            self.budget.clone(),
            self.reports.clone(),
            self.wake.clone(),
        );
        let (done, wait) = std::sync::mpsc::channel();
        self.writer.run(move || {
            let mut files = files.lock().unwrap();
            let sessions: Vec<_> = files.sessions.keys().copied().collect();
            let mut recovered = Vec::new();
            for session in sessions {
                if let Some(report) = files.retry(session, &budget) {
                    recovered.push(report);
                }
            }
            let mut errors: BTreeMap<_, _> = files
                .failing
                .iter()
                .filter(|(_, failure)| strict || failure.retryable)
                .map(|(session, failure)| (*session, failure.message.clone()))
                .collect();
            if strict {
                for (session, lost) in &budget.lock().unwrap().rejected {
                    let limit = limit_error(*session, *lost);
                    errors
                        .entry(*session)
                        .and_modify(|error| {
                            if *error != limit {
                                *error = format!("{error}; {limit}");
                            }
                        })
                        .or_insert(limit);
                }
            }
            drop(files);
            if !recovered.is_empty() {
                reports.lock().unwrap().extend(recovered);
                wake();
            }
            let errors: Vec<_> = errors.into_values().collect();
            let result = if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            };
            let _ = done.send(result);
        });
        wait.recv()
            .unwrap_or_else(|_| Err("The log writer stopped before saving completed".into()))
    }

    /// Retries the session's failed writes, then reports [`Report::Checkpoint`]: saved
    /// once every entry queued before this call is committed. Runs on the writer, so
    /// it never blocks the caller, and another session's failure does not fail it.
    pub fn checkpoint(&self, session: u64, ticket: u64) {
        let (files, budget, reports, wake) = (
            self.files.clone(),
            self.budget.clone(),
            self.reports.clone(),
            self.wake.clone(),
        );
        self.writer.run(move || {
            let mut files = files.lock().unwrap();
            let saved = files
                .sessions
                .contains_key(&session)
                .then(|| files.retry(session, &budget))
                .flatten();
            let error = files
                .failing
                .get(&session)
                .filter(|failure| failure.retryable)
                .map(|failure| failure.message.clone());
            let lost = budget
                .lock()
                .unwrap()
                .rejected
                .get(&session)
                .copied()
                .unwrap_or(0);
            drop(files);
            let mut reports = reports.lock().unwrap();
            reports.extend(saved);
            reports.push(Report::Checkpoint {
                session,
                ticket,
                error,
                lost,
            });
            drop(reports);
            wake();
        });
    }

    /// Takes every report since the last call.
    pub fn drain(&self) -> Vec<Report> {
        std::mem::take(&mut self.reports.lock().unwrap())
    }
}

impl Files {
    fn retry(&mut self, session: u64, budget: &Mutex<Budget>) -> Option<Report> {
        let pending = self.sessions.get_mut(&session).expect("queued above");
        let result: io::Result<()> = (|| {
            if pending.writes.is_empty() {
                return Ok(());
            }
            if pending.log.is_none() {
                if self.database.is_none() {
                    self.database = Some(open_database(&self.path)?);
                }
                pending.log = Some(SessionLog::with_database(
                    self.database.as_ref().unwrap().clone(),
                    session,
                ));
            }
            pending.retry(budget)
        })();
        if result.is_err() {
            self.sessions.get_mut(&session).unwrap().compact();
        }
        let failure = result
            .err()
            .map(|error| format!("Session {session}: {error}"));
        let lost = budget
            .lock()
            .unwrap()
            .rejected
            .get(&session)
            .copied()
            .unwrap_or(0);
        let retryable = failure.is_some();
        let error = failure
            .or_else(|| (lost > 0).then(|| limit_error(session, lost)))
            .map(|message| SaveFailure { message, retryable });
        match error {
            Some(error) => {
                if self.failing.get(&session) == Some(&error) {
                    return None;
                }
                self.failing.insert(session, error.clone());
                Some(Report::Saved {
                    session,
                    error: Some(error.message),
                    retryable,
                    lost,
                })
            }
            None => self.failing.remove(&session).map(|_| Report::Saved {
                session,
                error: None,
                retryable: false,
                lost: 0,
            }),
        }
    }
}

impl PendingSession {
    /// Queues a write, or the marker for rejected entries.
    fn queue(&mut self, queued: Queued) {
        match queued {
            Queued::Write(write) => self.writes.push_back(write),
            Queued::Gap { gap, batch: None } => self.mark_gap(gap),
            Queued::Gap {
                gap,
                batch: Some(batch),
            } => {
                let id = format!("{LOSS_MARKER_PREFIX}{batch}");
                // A batch rejected again on Retry is marked already.
                if !self.writes.iter().any(
                    |write| matches!(&write.kind, Kind::Marker { id: held, .. } if *held == id),
                ) {
                    self.writes.push_back(PendingWrite {
                        record: (event_key(), gap.record(&id)),
                        kind: Kind::Marker {
                            id,
                            gap,
                            batch: true,
                        },
                    });
                }
            }
        }
    }

    /// Queues the marker for rejected entries. Rejections with nothing queued between
    /// them share one marker, saved again with the new count under a fresh event key,
    /// since a failed append may still have committed the old payload. A marker already
    /// committed and awaiting its directory barrier is saved again too, so a crash
    /// while the barrier keeps failing never leaves it undercounting: a restore keeps
    /// the larger count of a marker saved twice, and memory holds only the latest. So a
    /// session never queues more markers than entries plus one, and markers that end
    /// up adjacent are merged when a log is restored.
    fn mark_gap(&mut self, gap: Gap) {
        if let Some(index) = self.mergeable() {
            let PendingWrite {
                record,
                kind: Kind::Marker { id, gap: held, .. },
            } = &mut self.writes[index]
            else {
                unreachable!("a mergeable write is a marker");
            };
            *held = held.merge(gap);
            *record = (event_key(), held.record(id));
            self.committed = self.committed.min(index);
            return;
        }
        let id = format!("{LOSS_MARKER_PREFIX}{}", event_key());
        self.writes.push_back(PendingWrite {
            record: (event_key(), gap.record(&id)),
            kind: Kind::Marker {
                id,
                gap,
                batch: false,
            },
        });
    }

    /// The marker a rejection now joins: the last write, or the last before a run of
    /// rejected recovery batches' markers, which keep their own.
    fn mergeable(&self) -> Option<usize> {
        let index = self
            .writes
            .iter()
            .rposition(|write| !matches!(write.kind, Kind::Marker { batch: true, .. }))?;
        matches!(self.writes[index].kind, Kind::Marker { batch: false, .. }).then_some(index)
    }

    /// After a failed save, so a store that works again saves every boundary. Past the
    /// reserve a session keeps one turn of boundaries while the store cannot write.
    fn compact(&mut self) {
        let unsent: Vec<_> = self.writes.drain(self.committed..).collect();
        for write in unsent {
            if let Kind::Spare { boundary, .. } = write.kind {
                self.collapse(boundary);
            }
            self.writes.push_back(write);
        }
    }

    /// A boundary past the reserve drops the unsent ones from the last turn start, when
    /// it starts a turn itself, or else from an earlier boundary of its kind since that
    /// start. What it drops joins the gap marker, as entries lost, so only a few
    /// boundaries wait however many turns go by. A rejected recovery batch's marker is
    /// kept as it is.
    fn collapse(&mut self, boundary: std::mem::Discriminant<Entry>) {
        let started = std::mem::discriminant(&Entry::TurnStarted);
        let tail = self.writes.len()
            - self
                .writes
                .iter()
                .skip(self.committed)
                .rev()
                .take_while(|write| matches!(write.kind, Kind::Spare { .. } | Kind::Marker { .. }))
                .count();
        let at = |index: usize, kind| matches!(self.writes[index].kind, Kind::Spare { boundary, .. } if boundary == kind);
        let turn = (tail..self.writes.len())
            .rev()
            .find(|&index| at(index, started));
        let from = if boundary == started {
            turn
        } else {
            (turn.unwrap_or(tail)..self.writes.len()).find(|&index| at(index, boundary))
        };
        let Some(from) = from else {
            return;
        };
        let mut kept = None;
        let mut batches = Vec::new();
        let mut dropped: Option<Gap> = None;
        for write in self.writes.drain(from..).collect::<Vec<_>>() {
            let gap = match write.kind {
                Kind::Marker { batch: true, .. } => {
                    batches.push(write);
                    continue;
                }
                Kind::Marker { id, gap, .. } => {
                    kept.get_or_insert(id);
                    gap
                }
                Kind::Spare { gap, .. } => gap,
                Kind::Entry => unreachable!("a collapse never reaches a budgeted entry"),
            };
            dropped = Some(dropped.map_or(gap, |held| held.merge(gap)));
        }
        self.writes.extend(batches);
        let Some(gap) = dropped else {
            return;
        };
        match kept {
            // A dropped marker keeps its ID, so a copy whose append outcome was unknown
            // restores as the same marker.
            Some(id) if self.mergeable().is_none() => {
                self.writes.push_back(PendingWrite {
                    record: (event_key(), gap.record(&id)),
                    kind: Kind::Marker {
                        id,
                        gap,
                        batch: false,
                    },
                });
            }
            _ => self.mark_gap(gap),
        }
    }

    fn retry(&mut self, budget: &Mutex<Budget>) -> io::Result<()> {
        while !self.writes.is_empty() {
            let log = self.log.as_mut().expect("opened above");
            let barrier = if self.committed > 0 {
                log.sync()
            } else {
                Ok(())
            };
            if self.committed == 0 || barrier.is_err() && self.committed < self.writes.len() {
                let records: Vec<_> = self
                    .writes
                    .iter()
                    .skip(self.committed)
                    .map(|write| write.record.clone())
                    .collect();
                let durability = log.append_records(&records)?;
                self.committed = self.writes.len();
                if let Durability::DirectorySyncPending(error) = durability {
                    // Committed rows are never sent again, so only entries counted in
                    // the budget wait here for the barrier. The last write stays, so a
                    // rejection after it is marked after it.
                    let last = self.writes.len() - 1;
                    let mut index = 0;
                    self.writes.retain(|write| {
                        index += 1;
                        index > last || matches!(write.kind, Kind::Entry)
                    });
                    self.committed = self.writes.len();
                    return Err(io::Error::other(error));
                }
            } else {
                barrier?;
            }
            let mut budget = budget.lock().unwrap();
            for write in self.writes.drain(..self.committed) {
                if matches!(write.kind, Kind::Entry) {
                    budget.bytes -= write.record.1.len();
                }
            }
            self.committed = 0;
        }
        self.writes = VecDeque::new();
        Ok(())
    }
}

/// What a turn boundary that needs the reserve is cut to: its error keeps at most
/// 1 KiB from each end, and less when escaping makes that too long.
const BOUNDARY_BYTES: usize = 4096;

/// `entry` as one log line, or `None` when it is longer than `limit` bytes.
fn serialize(entry: &Entry, limit: usize) -> Option<String> {
    let mut line = LimitedLine {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut line, entry).ok()?;
    line.write_all(b"\n").ok()?;
    Some(String::from_utf8(line.bytes).expect("serialized JSON is UTF-8"))
}

/// `entry`, keeping both ends of its text with a note of what was left out when it
/// serializes to more than `limit` bytes. Only message, reasoning and output text,
/// approval actions and turn errors shrink. Items and approvals are cut to a quarter of
/// the budget and keep at least 1 KiB from each end; a turn error is cut only when its
/// boundary needs the reserve, as far as it takes to fit.
fn fit(mut entry: Entry, limit: usize) -> Entry {
    let fits = |entry: &Entry| {
        serde_json::to_writer(
            &mut LimitedLine {
                bytes: Vec::new(),
                limit,
            },
            entry,
        )
        .is_ok()
    };
    fn text(entry: &mut Entry) -> Option<&mut String> {
        match entry {
            Entry::Item { item, .. } => Some(&mut item.text),
            Entry::Approval { action, .. } => Some(action),
            Entry::TurnEnded { error, .. } => error.as_mut(),
            _ => None,
        }
    }
    if fits(&entry) {
        return entry;
    }
    let Some(original) = text(&mut entry).map(std::mem::take) else {
        return entry;
    };
    // Escaping can make even 1 KiB from each end too long for a turn error that needs
    // the reserve, so that is cut as far as it takes.
    let least = if matches!(entry, Entry::TurnEnded { .. }) {
        1
    } else {
        1024
    };
    let mut keep = limit / 4;
    while keep >= least {
        if original.len() > keep * 2 {
            let head = original.floor_char_boundary(keep);
            let tail = original.ceil_char_boundary(original.len() - keep);
            *text(&mut entry).unwrap() = format!(
                "{}\n… {} bytes not saved (log budget exceeded) …\n{}",
                &original[..head],
                tail - head,
                &original[tail..]
            );
            if fits(&entry) {
                return entry;
            }
        }
        keep /= 2;
    }
    *text(&mut entry).unwrap() = original;
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{ItemKind, ItemStatus};
    use std::fs;

    fn item(id: &str) -> Item {
        Item {
            id: id.into(),
            kind: ItemKind::AgentMessage,
            text: "Hello".into(),
            status: ItemStatus::Completed,
        }
    }

    #[test]
    fn a_log_written_before_seq_existed_still_reads() {
        let old = r#"{"entry":"item","item":{"id":"a","kind":{"type":"agent_message"},"text":"Hello","status":"completed"}}"#;
        let entry: Entry = serde_json::from_str(old).unwrap();
        assert_eq!(
            entry,
            Entry::Item {
                item: item("a"),
                seq: None,
                run: None,
            }
        );
        // And an entry without a place is written the old way.
        assert_eq!(serde_json::to_string(&entry).unwrap(), old);
    }

    fn logs() -> (Logs, std::sync::mpsc::Receiver<()>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let wake: Wake = Arc::new(move || {
            let _ = tx.send(());
        });
        (Logs::new(dir.path().join("sessions"), wake), rx, dir)
    }

    #[test]
    fn a_fully_saved_log_barrier_does_not_invent_recoverable_work() {
        let (logs, _, dir) = logs();
        logs.append(1, vec![entry(1)]).unwrap();
        logs.flush_pending().unwrap();
        let database = path(&dir.path().join("sessions"), 1);
        let backup = dir.path().join("backup.sqlite3");
        fs::rename(&database, &backup).unwrap();
        fs::create_dir(&database).unwrap();
        logs.flush_pending().unwrap();
        logs.flush().unwrap();
        fs::remove_dir(database).unwrap();
        assert_eq!(
            Database::open_read_only(backup)
                .unwrap()
                .read_logs(1)
                .unwrap()
                .len(),
            1
        );
    }

    fn checkpoints(rx: &std::sync::mpsc::Receiver<()>, logs: &Logs) -> Vec<(u64, Option<String>)> {
        loop {
            let found: Vec<_> = logs
                .drain()
                .into_iter()
                .filter_map(|report| match report {
                    Report::Checkpoint { ticket, error, .. } => Some((ticket, error)),
                    _ => None,
                })
                .collect();
            if !found.is_empty() {
                return found;
            }
            rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        }
    }

    #[test]
    fn a_checkpoint_reports_a_recovery_batch_saved_only_once_it_commits() {
        let (logs, rx, dir) = logs();
        logs.append(1, vec![entry(0)]).unwrap();
        logs.flush().unwrap();
        let database = path(&dir.path().join("sessions"), 1);
        let writer = rusqlite::Connection::open(&database).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let batch = crate::recovery::RecoveryBatch {
            id: "recovery-1".into(),
            operations: Vec::new(),
            report: crate::recovery::RecoveryReport {
                outcome: crate::recovery::Outcome::Partial,
                messages: 0,
                tools: 0,
                extended: 0,
                turn: crate::recovery::TurnEvidence::Unknown,
                limits: vec![crate::recovery::Limit::TornTail],
            },
            report_after: None,
            closes_turn: true,
        };
        logs.append(
            1,
            vec![Write {
                entry: Entry::Recovery {
                    batch: batch.clone(),
                },
                sync: true,
            }],
        )
        .unwrap();
        logs.checkpoint(1, 7);
        let failed = checkpoints(&rx, &logs);
        assert!(matches!(&failed[..], [(7, Some(_))]), "{failed:?}");
        // Another session saving does not clear this session's failure.
        drop(writer);
        logs.append(2, vec![entry(1)]).unwrap();
        logs.checkpoint(2, 8);
        assert_eq!(checkpoints(&rx, &logs), [(8, None)]);
        logs.checkpoint(1, 9);
        assert_eq!(checkpoints(&rx, &logs), [(9, None)]);
        let entries = read(&dir.path().join("sessions"), 1).unwrap();
        assert_eq!(entries.last(), Some(&Entry::Recovery { batch }));
        // A build that predates recovery skips the entry and keeps the rest.
        let old: Result<Entry, _> = serde_json::from_str(
            &serde_json::to_string(entries.last().unwrap())
                .unwrap()
                .replace("\"recovery\"", "\"unknown\""),
        );
        assert!(old.is_err());
    }

    fn entry(n: u64) -> Write {
        Write {
            entry: Entry::Item {
                item: item(&n.to_string()),
                seq: Some(n),
                run: Some(1),
            },
            sync: n.is_multiple_of(10),
        }
    }

    fn seqs(entries: &[Entry]) -> Vec<u64> {
        entries
            .iter()
            .map(|e| match e {
                Entry::Item { seq, .. } => seq.unwrap(),
                _ => unreachable!(),
            })
            .collect()
    }

    /// An entry of about `bytes` that capping cannot shorten: the bytes are in its
    /// kind's name, not its text.
    fn bulky(n: u64, bytes: usize) -> Write {
        let mut write = entry(n);
        if let Entry::Item { item, .. } = &mut write.entry {
            item.kind = ItemKind::Other {
                name: "a".repeat(bytes),
            };
        }
        write
    }

    /// The marker's text and place, when `entry` is a gap marker.
    fn marker(entry: &Entry) -> Option<(&str, Option<u64>)> {
        match entry {
            Entry::Item { item, seq, .. } if item.id.starts_with(LOSS_MARKER_PREFIX) => {
                Some((item.text.as_str(), *seq))
            }
            _ => None,
        }
    }

    #[test]
    fn writes_land_in_order_and_a_load_reports_them() {
        let (logs, wake, dir) = logs();
        for n in 0..50 {
            logs.append(1, vec![entry(n)]).unwrap();
        }
        logs.flush().unwrap();
        let sessions = dir.path().join("sessions");
        assert_eq!(
            seqs(&read(&sessions, 1).unwrap()),
            (0..50).collect::<Vec<_>>()
        );
        assert!(path(&sessions, 1).is_file());
        assert!(!sessions.exists());
        logs.load(1);
        assert!(logs.wait_for_loads(Instant::now() + std::time::Duration::from_secs(5)));
        wake.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let [
            Report::Loaded {
                session: 1,
                entries,
            },
        ] = &logs.drain()[..]
        else {
            panic!("expected one load report");
        };
        assert_eq!(seqs(entries.as_ref().unwrap()), (0..50).collect::<Vec<_>>());
    }

    /// A read that never returns must not hold up
    /// other sessions' writes and loads, or quitting.
    #[test]
    fn a_read_that_never_returns_holds_up_neither_writes_nor_quitting() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        Database::open(path(&sessions, 0)).unwrap();
        let (woken, wake) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = Mutex::new(blocked);
        let logs = Logs::with_reader(
            sessions.clone(),
            Arc::new(move || {
                let _ = woken.send(());
            }),
            Arc::new(move |dir, session| {
                if session == 1 {
                    blocked.lock().unwrap().recv().unwrap();
                }
                read(dir, session)
            }),
        );
        let (done, finished) = std::sync::mpsc::channel();
        thread::spawn(move || {
            logs.load(1);
            logs.load(3);
            logs.append(2, vec![entry(0)]).unwrap();
            logs.flush().unwrap();
            // Session 3's load reports although session 1's never does.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut reports = Vec::new();
            while !reports
                .iter()
                .any(|r| matches!(r, Report::Loaded { session: 3, .. }))
                && Instant::now() < deadline
            {
                let _ = wake.recv_timeout(Duration::from_millis(100));
                reports.extend(logs.drain());
            }
            let waited = Instant::now();
            let all = logs.wait_for_loads(waited + Duration::from_millis(200));
            let bounded = waited.elapsed() < Duration::from_secs(2);
            // Dropping, as quitting does, must not wait for the read.
            drop(logs);
            let _ = done.send((reports, all, bounded));
        });
        let (reports, all, bounded) = finished
            .recv_timeout(Duration::from_secs(10))
            .expect("writes and quitting waited for a read that never returns");
        assert!(
            reports.iter().any(
                |r| matches!(r, Report::Loaded { session: 3, entries: Ok(e) } if e.is_empty())
            ),
            "{reports:?}"
        );
        assert!(
            !all && bounded,
            "the wait for loads gives up at its deadline"
        );
        assert_eq!(seqs(&read(&sessions, 2).unwrap()), [0]);
        release.send(()).unwrap();
    }

    #[test]
    fn a_save_error_is_reported_until_saving_works_again() {
        let (logs, wake, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        let write = || {
            vec![Write {
                entry: Entry::TurnStarted,
                sync: true,
            }]
        };
        logs.append(2, write()).unwrap();
        wake.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(
            &logs.drain()[..],
            [Report::Saved {
                session: 2,
                error: Some(_),
                retryable: true,
                lost: 0,
            }]
        ));
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.append(2, write()).unwrap();
        wake.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            logs.drain(),
            vec![Report::Saved {
                session: 2,
                error: None,
                retryable: false,
                lost: 0,
            }]
        );
        // Further successes report nothing.
        logs.append(2, write()).unwrap();
        logs.flush().unwrap();
        assert!(logs.drain().is_empty());
        assert_eq!(read(&sessions, 2).unwrap().len(), 3);
    }

    #[test]
    fn failed_batches_retry_without_a_new_message_in_original_order() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        logs.append(1, vec![entry(0), entry(1)]).unwrap();
        logs.append(1, vec![entry(2)]).unwrap();
        logs.append(2, vec![entry(3)]).unwrap();
        let error = logs.flush().unwrap_err();
        assert!(error.contains("Session 1") && error.contains("Session 2"));
        assert!(logs.budget.lock().unwrap().bytes > 0);
        logs.drain();
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0, 1, 2]);
        assert_eq!(seqs(&read(&sessions, 2).unwrap()), [3]);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
        assert_eq!(
            logs.drain(),
            vec![
                Report::Saved {
                    session: 1,
                    error: None,
                    retryable: false,
                    lost: 0,
                },
                Report::Saved {
                    session: 2,
                    error: None,
                    retryable: false,
                    lost: 0,
                },
            ]
        );
        logs.flush().unwrap();
        assert!(logs.drain().is_empty());
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0, 1, 2]);
    }

    fn inject(logs: &Logs, session: u64, faults: VecDeque<Fault>) {
        let mut files = logs.files.lock().unwrap();
        if files.database.is_none() {
            files.database = Some(Database::open(&files.path).unwrap());
        }
        let mut log = SessionLog::with_database(files.database.as_ref().unwrap().clone(), session);
        log.faults = faults;
        let pending = files.sessions.entry(session).or_default();
        pending.log = Some(log);
    }

    #[test]
    fn failed_transactions_block_later_entries_until_atomic_retry() {
        let (logs, _, dir) = logs();
        inject(
            &logs,
            1,
            VecDeque::from([Fault::Transaction, Fault::Transaction, Fault::Transaction]),
        );
        logs.append(1, vec![entry(0), entry(1)]).unwrap();
        logs.writer.flush();
        let sessions = dir.path().join("sessions");
        assert!(read(&sessions, 1).unwrap().is_empty());
        assert!(logs.flush().unwrap_err().contains("transaction failure"));
        logs.append(1, vec![entry(2)]).unwrap();
        logs.writer.flush();
        assert!(read(&sessions, 1).unwrap().is_empty());
        logs.flush().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0, 1, 2]);
        logs.flush().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0, 1, 2]);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn sync_retry_does_not_append_the_entry_twice_or_clear_failure_early() {
        let (logs, _, dir) = logs();
        inject(
            &logs,
            1,
            VecDeque::from([Fault::Committed, Fault::Committed, Fault::Committed]),
        );
        logs.append(1, vec![entry(0), entry(1)]).unwrap();
        logs.writer.flush();
        let sessions = dir.path().join("sessions");
        let before = read(&sessions, 1).unwrap();
        assert_eq!(seqs(&before), [0, 1]);
        assert!(logs.flush().unwrap_err().contains("sync failure"));
        logs.append(2, vec![entry(5)]).unwrap();
        logs.writer.flush();
        assert_eq!(read(&sessions, 1).unwrap(), before);
        assert!(logs.flush().is_err());
        assert!(!logs.drain().iter().any(|report| matches!(
            report,
            Report::Saved {
                session: 1,
                error: None,
                ..
            }
        )));
        logs.flush().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0, 1]);
        assert_eq!(seqs(&read(&sessions, 2).unwrap()), [5]);
        assert_eq!(
            logs.drain(),
            vec![Report::Saved {
                session: 1,
                error: None,
                retryable: false,
                lost: 0,
            }]
        );
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn committed_log_barrier_repair_does_not_need_sql_writer_access() {
        let (logs, _, dir) = logs();
        inject(&logs, 1, VecDeque::from([Fault::Committed]));
        logs.append(1, vec![entry(0)]).unwrap();
        logs.writer.flush();
        let sessions = dir.path().join("sessions");
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0]);
        let writer = rusqlite::Connection::open(path(&sessions, 1)).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        logs.flush_pending().unwrap();
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
        logs.append(1, vec![entry(1)]).unwrap();
        assert!(logs.flush_pending().unwrap_err().contains("locked"));
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0]);
        writer.execute_batch("ROLLBACK").unwrap();
        logs.flush_pending().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0, 1]);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn committed_log_barrier_repair_does_not_need_writable_database_files() {
        use std::os::unix::fs::PermissionsExt;
        let (logs, _, dir) = logs();
        inject(&logs, 1, VecDeque::from([Fault::Committed]));
        logs.append(1, vec![entry(0)]).unwrap();
        logs.writer.flush();
        let database = path(&dir.path().join("sessions"), 1);
        fs::set_permissions(&database, fs::Permissions::from_mode(0o400)).unwrap();
        logs.flush_pending().unwrap();
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
        fs::set_permissions(database, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(seqs(&read(&dir.path().join("sessions"), 1).unwrap()), [0]);
    }

    #[test]
    fn committed_loss_notice_barrier_does_not_need_another_sql_write() {
        let (logs, _, dir) = logs();
        inject(
            &logs,
            1,
            VecDeque::from([Fault::Committed, Fault::Committed]),
        );
        assert!(logs.append(1, vec![bulky(0, MAX_PENDING_BYTES)]).is_err());
        logs.writer.flush();
        assert!(logs.flush_pending().unwrap_err().contains("sync failure"));
        let sessions = dir.path().join("sessions");
        let before = read(&sessions, 1).unwrap();
        assert_eq!(before.len(), 1);
        let writer = rusqlite::Connection::open(path(&sessions, 1)).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        logs.flush_pending().unwrap();
        logs.flush_pending().unwrap();
        assert_eq!(read(&sessions, 1).unwrap(), before);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
        assert!(logs.flush().unwrap_err().contains("rejected"));
        writer.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn uncertain_commit_keeps_event_keys_when_later_entries_join_the_retry() {
        let (logs, _, dir) = logs();
        inject(
            &logs,
            1,
            VecDeque::from([Fault::Committed, Fault::Committed, Fault::Committed]),
        );
        logs.append(1, vec![entry(0), entry(1)]).unwrap();
        logs.writer.flush();
        let keys: Vec<_> = logs.files.lock().unwrap().sessions[&1]
            .writes
            .iter()
            .map(|write| write.record.0.clone())
            .collect();
        assert_eq!(
            seqs(&read(&dir.path().join("sessions"), 1).unwrap()),
            [0, 1]
        );
        logs.append(1, vec![entry(2)]).unwrap();
        logs.writer.flush();
        let pending_keys: Vec<_> = logs.files.lock().unwrap().sessions[&1]
            .writes
            .iter()
            .map(|write| write.record.0.clone())
            .collect();
        assert_eq!(&pending_keys[..2], &keys);
        assert_eq!(
            seqs(&read(&dir.path().join("sessions"), 1).unwrap()),
            [0, 1, 2]
        );
        assert!(logs.budget.lock().unwrap().bytes > 0);
        logs.flush().unwrap();
        logs.flush().unwrap();
        assert_eq!(
            seqs(&read(&dir.path().join("sessions"), 1).unwrap()),
            [0, 1, 2]
        );
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn an_in_memory_database_shares_writes_and_async_loads() {
        let database = Database::in_memory().unwrap();
        let logs = Logs::with_database(database.clone(), Arc::new(|| {}));
        logs.append(7, vec![entry(1), entry(1)]).unwrap();
        logs.flush().unwrap();
        assert_eq!(seqs(&read_database(&database, 7).unwrap()), [1, 1]);
        assert!(read_database(&database, 8).unwrap().is_empty());
        logs.load(7);
        assert!(logs.wait_for_loads(Instant::now() + std::time::Duration::from_secs(5)));
        assert!(
            matches!(&logs.drain()[..], [Report::Loaded { session: 7, entries: Ok(entries) }] if seqs(entries) == [1, 1])
        );
    }

    #[test]
    fn a_database_load_does_not_wait_for_the_file_worker() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("workspace.sqlite3")).unwrap();
        let logs = Logs::with_database(database, Arc::new(|| {}));
        logs.append(1, vec![entry(0)]).unwrap();
        logs.flush().unwrap();
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let files = logs.files.clone();
        logs.writer.run(move || {
            let _files = files.lock().unwrap();
            entered.send(()).unwrap();
            blocked.recv().unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        logs.load(1);
        let loaded = logs.wait_for_loads(Instant::now() + Duration::from_secs(2));
        let reports = logs.drain();
        release.send(()).unwrap();
        logs.flush().unwrap();
        assert!(loaded, "the database load waited behind the file worker");
        assert!(
            matches!(&reports[..], [Report::Loaded { session: 1, entries: Ok(entries) }] if seqs(entries) == [0])
        );
    }

    #[test]
    fn a_read_failure_reports_and_finishes_the_async_load() {
        let dir = tempfile::tempdir().unwrap();
        let logs = Logs::with_reader(
            dir.path().join("sessions"),
            Arc::new(|| {}),
            Arc::new(|_, _| Err(io::Error::other("database read failure"))),
        );
        logs.load(1);
        assert!(logs.wait_for_loads(Instant::now() + std::time::Duration::from_secs(5)));
        assert_eq!(
            logs.drain(),
            vec![Report::Loaded {
                session: 1,
                entries: Err("database read failure".into())
            }]
        );
    }

    #[test]
    fn a_missing_database_read_does_not_create_anything() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        assert!(read(&sessions, 1).unwrap().is_empty());
        assert!(!path(&sessions, 1).exists());
        assert!(!sessions.exists());
    }

    #[test]
    fn opening_a_log_creates_missing_workspace_directories() {
        let dir = tempfile::tempdir().unwrap();
        let direct = dir.path().join("direct/workspace/sessions");
        let mut log = SessionLog::open(&direct, 1).unwrap();
        log.append(&entry(0).entry).unwrap();
        log.sync().unwrap();
        assert_eq!(seqs(&read(&direct, 1).unwrap()), [0]);
        let queued = dir.path().join("queued/workspace/sessions");
        let logs = Logs::new(queued.clone(), Arc::new(|| {}));
        logs.append(2, vec![entry(1)]).unwrap();
        logs.flush().unwrap();
        assert_eq!(seqs(&read(&queued, 2).unwrap()), [1]);
        assert!(!direct.exists());
        assert!(!queued.exists());
    }

    #[test]
    fn unsupported_neutral_entries_are_skipped_without_hiding_database_errors() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let database = Database::open(path(&sessions, 1)).unwrap();
        database
            .append_logs(
                1,
                &[
                    (event_key(), serde_json::to_string(&entry(0).entry).unwrap()),
                    (
                        event_key(),
                        r#"{"entry":"item","item":"unreadable"}"#.into(),
                    ),
                    (event_key(), serde_json::to_string(&entry(1).entry).unwrap()),
                ],
            )
            .unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0, 1]);
        drop(database);
        fs::remove_file(path(&sessions, 1)).unwrap();
        fs::write(path(&sessions, 1), "not a database").unwrap();
        assert!(read(&sessions, 1).is_err());
    }

    #[test]
    fn writes_without_a_sync_flag_are_committed_before_retiring_the_queue() {
        let (logs, _, dir) = logs();
        let mut write = entry(0);
        write.sync = false;
        logs.append(1, vec![write]).unwrap();
        logs.writer.flush();
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
        drop(logs);
        assert_eq!(seqs(&read(&dir.path().join("sessions"), 1).unwrap()), [0]);
    }

    #[test]
    fn transaction_failure_preserves_the_successful_prefix() {
        let (logs, _, dir) = logs();
        logs.append(1, vec![entry(1)]).unwrap();
        logs.flush().unwrap();
        logs.files
            .lock()
            .unwrap()
            .sessions
            .get_mut(&1)
            .unwrap()
            .log
            .as_mut()
            .unwrap()
            .faults
            .push_back(Fault::Transaction);
        logs.append(1, vec![entry(2), entry(3)]).unwrap();
        logs.writer.flush();
        let sessions = dir.path().join("sessions");
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [1]);
        logs.flush().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [1, 2, 3]);
    }

    #[test]
    fn buffer_limit_rejection_marks_the_gap_and_later_entries_save_after_it() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        logs.append(1, vec![bulky(0, MAX_PENDING_BYTES / 2)])
            .unwrap();
        let before = logs.budget.lock().unwrap().bytes;
        assert!(
            logs.append(1, vec![bulky(1, MAX_PENDING_BYTES / 2)])
                .unwrap_err()
                .contains("limit")
        );
        assert_eq!(logs.budget.lock().unwrap().bytes, before);
        logs.append(1, vec![entry(2)]).unwrap();
        assert!(logs.flush().is_err());
        fs::remove_file(path(&sessions, 0)).unwrap();
        assert!(logs.flush().unwrap_err().contains("rejected"));
        let entries = read(&sessions, 1).unwrap();
        assert_eq!(seqs(&entries), [0, 1, 2]);
        assert_eq!(
            marker(&entries[1]),
            Some(("1 log entry was lost (log budget exceeded).", Some(1)))
        );
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
        assert!(!logs.drain().iter().any(|report| matches!(
            report,
            Report::Saved {
                session: 1,
                error: None,
                ..
            }
        )));
        logs.append(2, vec![entry(3)]).unwrap();
        assert!(logs.flush().is_err());
        assert_eq!(seqs(&read(&sessions, 2).unwrap()), [3]);
        assert!(logs.append(3, vec![bulky(4, MAX_PENDING_BYTES)]).is_err());
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn rejection_report_follows_any_already_queued_recovery() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        logs.append(1, vec![entry(0)]).unwrap();
        assert!(logs.flush().is_err());
        logs.drain();
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush().unwrap();
        assert!(logs.append(1, vec![bulky(1, MAX_PENDING_BYTES)]).is_err());
        logs.writer.flush();
        let reports = logs.drain();
        assert!(matches!(
            reports.first(),
            Some(Report::Saved { error: None, .. })
        ));
        assert!(
            matches!(reports.last(), Some(Report::Saved { error: Some(error), .. }) if error.contains("rejected"))
        );
    }

    #[test]
    fn pending_barrier_records_oversized_batch_loss_once_without_resetting_rejection() {
        let (logs, _, dir) = logs();
        assert!(
            logs.append(1, vec![bulky(0, MAX_PENDING_BYTES), entry(1)])
                .is_err()
        );
        assert_eq!(logs.lost_entries(1), 1);
        logs.flush_pending().unwrap();
        logs.flush_pending().unwrap();
        let sessions = dir.path().join("sessions");
        let entries = read(&sessions, 1).unwrap();
        assert!(
            matches!(&entries[0], Entry::Item { item, seq: Some(0), run: Some(1) } if
            item.kind == ItemKind::Other { name: "Keystone log loss: 1 log entry was lost (log budget exceeded).".into() }
                && item.text == "1 log entry was lost (log budget exceeded)."
                && item.status == ItemStatus::Failed)
        );
        assert_eq!(seqs(&entries), [0, 1]);
        assert!(logs.flush().unwrap_err().contains("1 entry was rejected"));
        assert!(logs.append(1, vec![bulky(2, MAX_PENDING_BYTES)]).is_err());
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            marker(&entries[2]),
            Some(("1 log entry was lost (log budget exceeded).", Some(2)))
        );
        assert_eq!(logs.lost_entries(1), 2);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn terminal_loss_does_not_mask_a_retained_write_failure_and_recovery_keeps_order() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        logs.append(1, vec![bulky(0, MAX_PENDING_BYTES / 2)])
            .unwrap();
        let original = logs.flush().unwrap_err();
        logs.drain();
        assert!(
            logs.append(1, vec![bulky(1, MAX_PENDING_BYTES / 2)])
                .is_err()
        );
        logs.writer.flush();
        assert!(
            matches!(logs.drain().last(), Some(Report::Saved { error: Some(error), retryable: true, lost: 1, .. }) if error.contains(&original))
        );
        assert!(logs.flush_pending().unwrap_err().contains(&original));
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], Entry::Item { seq: Some(0), .. }));
        assert_eq!(
            marker(&entries[1]),
            Some(("1 log entry was lost (log budget exceeded).", Some(1)))
        );
        assert!(matches!(
            logs.drain().last(),
            Some(Report::Saved {
                retryable: false,
                lost: 1,
                ..
            })
        ));
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn a_full_budget_still_queues_the_loss_marker_outside_it() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        let overhead = serde_json::to_vec(&bulky(0, 0).entry).unwrap().len() + 1;
        logs.append(1, vec![bulky(0, MAX_PENDING_BYTES - overhead)])
            .unwrap();
        assert_eq!(logs.budget.lock().unwrap().bytes, MAX_PENDING_BYTES);
        assert!(logs.append(2, vec![entry(1)]).is_err());
        let error = logs.flush_pending().unwrap_err();
        assert!(error.contains("Session 1") && error.contains("Session 2"));
        assert_eq!(logs.budget.lock().unwrap().bytes, MAX_PENDING_BYTES);
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush_pending().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0]);
        assert_eq!(
            read(&sessions, 2)
                .unwrap()
                .iter()
                .map(marker)
                .collect::<Vec<_>>(),
            [Some((
                "1 log entry was lost (log budget exceeded).",
                Some(1)
            ))]
        );
        logs.append(3, vec![entry(2)]).unwrap();
        logs.flush_pending().unwrap();
        assert_eq!(seqs(&read(&sessions, 3).unwrap()), [2]);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn a_loss_marker_sync_failure_is_retryable_without_duplicate_markers() {
        let (logs, _, dir) = logs();
        inject(
            &logs,
            1,
            VecDeque::from([Fault::Committed, Fault::Committed, Fault::Committed]),
        );
        assert!(logs.append(1, vec![bulky(0, MAX_PENDING_BYTES)]).is_err());
        logs.writer.flush();
        assert!(logs.flush_pending().unwrap_err().contains("sync failure"));
        let before = read(&dir.path().join("sessions"), 1).unwrap();
        assert_eq!(before.len(), 1);
        assert!(logs.flush_pending().unwrap_err().contains("sync failure"));
        logs.flush_pending().unwrap();
        assert_eq!(read(&dir.path().join("sessions"), 1).unwrap(), before);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn a_loss_marker_transaction_failure_retries_atomically() {
        let (logs, _, dir) = logs();
        inject(
            &logs,
            1,
            VecDeque::from([Fault::Transaction, Fault::Transaction]),
        );
        assert!(logs.append(1, vec![bulky(0, MAX_PENDING_BYTES)]).is_err());
        logs.writer.flush();
        let sessions = dir.path().join("sessions");
        assert!(read(&sessions, 1).unwrap().is_empty());
        assert!(
            logs.flush_pending()
                .unwrap_err()
                .contains("transaction failure")
        );
        assert!(read(&sessions, 1).unwrap().is_empty());
        logs.flush_pending().unwrap();
        assert_eq!(read(&sessions, 1).unwrap().len(), 1);
        logs.flush_pending().unwrap();
        assert_eq!(read(&sessions, 1).unwrap().len(), 1);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn an_entry_too_large_for_the_budget_keeps_both_ends_and_says_what_was_left_out() {
        let (logs, _, dir) = logs();
        let mut huge = entry(0);
        if let Entry::Item { item, .. } = &mut huge.entry {
            item.text = format!("start{}end", "é".repeat(MAX_PENDING_BYTES / 2));
        }
        logs.append(1, vec![huge]).unwrap();
        logs.flush().unwrap();
        let entries = read(&dir.path().join("sessions"), 1).unwrap();
        let [
            Entry::Item {
                item, seq: Some(0), ..
            },
        ] = &entries[..]
        else {
            panic!("expected the capped item: {entries:?}");
        };
        assert!(item.text.starts_with("start") && item.text.ends_with("end"));
        assert!(item.text.contains("bytes not saved (log budget exceeded)"));
        assert!(item.text.len() <= MAX_PENDING_BYTES / 4);
        assert_eq!(logs.lost_entries(1), 0);
    }

    #[test]
    fn a_rejection_saves_its_marker_without_a_barrier_and_logging_goes_on() {
        let (logs, _, dir) = logs();
        logs.append(1, vec![entry(0)]).unwrap();
        assert!(logs.append(1, vec![bulky(1, MAX_PENDING_BYTES)]).is_err());
        logs.append(1, vec![entry(2)]).unwrap();
        logs.writer.flush();
        // No flush or Quit: what a crash now would leave.
        let entries = read(&dir.path().join("sessions"), 1).unwrap();
        assert_eq!(seqs(&entries), [0, 1, 2]);
        assert_eq!(
            marker(&entries[1]),
            Some(("1 log entry was lost (log budget exceeded).", Some(1)))
        );
        assert_eq!(logs.lost_entries(1), 1);
        assert!(logs.flush_pending().is_ok());
        assert_eq!(read(&dir.path().join("sessions"), 1).unwrap(), entries);
    }

    #[test]
    fn a_batch_admits_each_entry_that_fits_and_marks_the_rest_in_place() {
        let (logs, _, dir) = logs();
        let error = logs
            .append(
                1,
                vec![
                    entry(0),
                    bulky(1, MAX_PENDING_BYTES),
                    bulky(2, MAX_PENDING_BYTES),
                    entry(3),
                ],
            )
            .unwrap_err();
        assert!(error.contains("2 entries"));
        logs.flush_pending().unwrap();
        let entries = read(&dir.path().join("sessions"), 1).unwrap();
        assert_eq!(seqs(&entries), [0, 1, 3]);
        assert_eq!(
            marker(&entries[1]),
            Some(("2 log entries were lost (log budget exceeded).", Some(1)))
        );
    }

    #[test]
    fn rejections_in_a_row_share_one_marker_while_it_waits() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        let overhead = serde_json::to_vec(&bulky(0, 0).entry).unwrap().len() + 1;
        logs.append(1, vec![bulky(0, MAX_PENDING_BYTES - overhead)])
            .unwrap();
        for n in [3, 1, 2] {
            assert!(logs.append(1, vec![entry(n)]).is_err());
        }
        assert!(logs.flush_pending().is_err());
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush_pending().unwrap();
        logs.append(1, vec![entry(4)]).unwrap();
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        assert_eq!(seqs(&entries), [0, 1, 4]);
        assert_eq!(
            marker(&entries[1]),
            Some(("3 log entries were lost (log budget exceeded).", Some(1)))
        );
        assert_eq!(logs.lost_entries(1), 3);
    }

    #[test]
    fn rejections_behind_a_committed_marker_save_their_count_in_bounded_memory() {
        let (logs, _, dir) = logs();
        let logs = logs.with_budget(4096);
        inject(&logs, 1, (0..101).map(|_| Fault::Committed).collect());
        for n in 0..100 {
            assert!(logs.append(1, vec![bulky(n, 8192)]).is_err());
        }
        logs.writer.flush();
        // The barrier never succeeded: what a crash now would leave.
        let sessions = dir.path().join("sessions");
        let entries = read(&sessions, 1).unwrap();
        let ids: BTreeSet<_> = entries
            .iter()
            .map(|entry| match entry {
                Entry::Item { item, .. } => item.id.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids.len(), 1, "one marker, saved again with each count");
        assert_eq!(
            entries.last().and_then(marker),
            Some(("100 log entries were lost (log budget exceeded).", Some(0)))
        );
        assert_eq!(
            logs.files.lock().unwrap().sessions[&1].writes.len(),
            1,
            "only the latest copy waits for the barrier"
        );
        assert!(logs.flush_pending().is_err());
        logs.flush_pending().unwrap();
        assert_eq!(read(&sessions, 1).unwrap(), entries);
        assert_eq!(logs.lost_entries(1), 100);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn a_rejection_after_a_saved_marker_gets_its_own_marker_in_place() {
        let (logs, _, dir) = logs();
        logs.append(1, vec![entry(2)]).unwrap();
        assert!(logs.append(1, vec![bulky(3, MAX_PENDING_BYTES)]).is_err());
        logs.flush_pending().unwrap();
        assert!(logs.append(1, vec![bulky(1, MAX_PENDING_BYTES)]).is_err());
        logs.flush_pending().unwrap();
        let entries = read(&dir.path().join("sessions"), 1).unwrap();
        assert_eq!(seqs(&entries), [2, 3, 1]);
        assert!(marker(&entries[1]).is_some() && marker(&entries[2]).is_some());
        assert_eq!(
            lost_count(match &entries[2] {
                Entry::Item { item, .. } => item,
                _ => unreachable!(),
            }),
            Some(1)
        );
    }

    #[test]
    fn rewriting_a_marker_after_an_unknown_append_outcome_does_not_wedge_quit() {
        let (logs, _, dir) = logs();
        inject(&logs, 1, VecDeque::from([Fault::CommittedThenFailed]));
        assert!(logs.append(1, vec![bulky(0, MAX_PENDING_BYTES)]).is_err());
        logs.writer.flush();
        let sessions = dir.path().join("sessions");
        assert_eq!(read(&sessions, 1).unwrap().len(), 1);
        // The marker counts as unsent, so this rejection rewrites it.
        assert!(logs.append(1, vec![bulky(1, MAX_PENDING_BYTES)]).is_err());
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        assert_eq!(
            entries.iter().map(marker).collect::<Vec<_>>(),
            [
                Some(("1 log entry was lost (log budget exceeded).", Some(0))),
                Some(("2 log entries were lost (log budget exceeded).", Some(0))),
            ]
        );
        let [
            Entry::Item { item: first, .. },
            Entry::Item { item: second, .. },
        ] = &entries[..]
        else {
            unreachable!()
        };
        assert_eq!(first.id, second.id, "a restore shows the same marker once");
        logs.append(1, vec![entry(2)]).unwrap();
        logs.flush_pending().unwrap();
        assert_eq!(read(&sessions, 1).unwrap().len(), 3);
    }

    #[test]
    fn a_checkpoint_after_an_earlier_loss_succeeds_and_reports_the_loss() {
        let (logs, rx, _dir) = logs();
        assert!(logs.append(1, vec![bulky(0, MAX_PENDING_BYTES)]).is_err());
        logs.append(1, vec![entry(1)]).unwrap();
        logs.checkpoint(1, 7);
        let mut found = None;
        while found.is_none() {
            rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            found = logs.drain().into_iter().find_map(|report| match report {
                Report::Checkpoint {
                    ticket: 7,
                    error,
                    lost,
                    ..
                } => Some((error, lost)),
                _ => None,
            });
        }
        assert_eq!(found, Some((None, 1)));
    }

    #[test]
    fn a_turn_end_past_the_reserve_is_kept_without_its_error() {
        let (logs, _, dir) = logs();
        let logs = logs.with_budget(8192);
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        let ended = |n| Write {
            entry: Entry::TurnEnded {
                status: TurnStatus::Failed,
                error: Some("\"".repeat(8192)),
                seq: Some(n),
                run: Some(1),
            },
            sync: true,
        };
        // The cut errors fill the budget and the reserve; past it each turn end is kept
        // without its error and stands in for the ones before it.
        for n in 0..120 {
            logs.append(1, vec![ended(n)]).unwrap();
        }
        // A stopped turn has no error to replace.
        logs.append(
            2,
            vec![Write {
                entry: Entry::TurnEnded {
                    status: TurnStatus::Interrupted,
                    error: None,
                    seq: Some(0),
                    run: Some(1),
                },
                sync: true,
            }],
        )
        .unwrap();
        logs.writer.flush();
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        let note = "The error was not saved (log budget exceeded).";
        let cut = entries
            .iter()
            .take_while(|entry| {
                matches!(entry, Entry::TurnEnded { error: Some(error), .. }
                if error.contains("bytes not saved (log budget exceeded)"))
            })
            .count();
        let kept = entries
            .iter()
            .position(|entry| marker(entry).is_some())
            .unwrap();
        assert!(cut >= 2 && kept > cut, "{entries:?}");
        assert!(entries[cut..kept].iter().all(|entry| matches!(entry,
            Entry::TurnEnded { error: Some(error), .. } if error == note)));
        assert_eq!(entries.len(), kept + 2);
        let collapsed = loss_text(119 - kept as u64);
        assert_eq!(
            marker(&entries[kept]),
            Some((collapsed.as_str(), Some(kept as u64)))
        );
        assert!(
            matches!(&entries[kept + 1], Entry::TurnEnded { error: Some(error), seq: Some(119), .. }
            if error == note)
        );
        assert!(matches!(
            &read(&sessions, 2).unwrap()[..],
            [Entry::TurnEnded {
                status: TurnStatus::Interrupted,
                error: None,
                ..
            }]
        ));
        assert_eq!(logs.lost_entries(1), 0);
    }

    /// Fills the budget, then its reserve with turn starts, and returns how many were
    /// counted in it. The last is past the reserve.
    fn fill_budget_and_reserve(logs: &Logs, budget: usize) -> usize {
        let overhead = serde_json::to_vec(&bulky(0, 0).entry).unwrap().len() + 1;
        logs.append(1, vec![bulky(0, budget - overhead)]).unwrap();
        let started = Write {
            entry: Entry::TurnStarted,
            sync: false,
        };
        for counted in 0..budget {
            let before = logs.budget.lock().unwrap().bytes;
            logs.append(1, vec![started.clone()]).unwrap();
            if logs.budget.lock().unwrap().bytes == before {
                return counted;
            }
        }
        panic!("turn starts past the reserve still grow the budget");
    }

    /// A turn past the reserve: its start, an item the budget rejects, and its end.
    fn spare_turn(logs: &Logs, n: u64) {
        logs.append(
            1,
            vec![Write {
                entry: Entry::TurnStarted,
                sync: false,
            }],
        )
        .unwrap();
        assert!(logs.append(1, vec![entry(2 * n)]).is_err());
        logs.append(
            1,
            vec![Write {
                entry: Entry::TurnEnded {
                    status: TurnStatus::Failed,
                    error: Some("failed".into()),
                    seq: Some(2 * n + 1),
                    run: Some(1),
                },
                sync: true,
            }],
        )
        .unwrap();
    }

    #[test]
    fn boundaries_past_the_reserve_stay_bounded_while_the_store_cannot_write() {
        let (logs, _, dir) = logs();
        let logs = logs.with_budget(8192);
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        let counted = fill_budget_and_reserve(&logs, 8192);
        let bytes = logs.budget.lock().unwrap().bytes;
        for n in 1..=1000 {
            spare_turn(&logs, n);
        }
        logs.writer.flush();
        assert!(
            logs.files.lock().unwrap().sessions[&1].writes.len() <= 1 + counted + 5,
            "a few writes wait, however many turns go by"
        );
        assert_eq!(logs.budget.lock().unwrap().bytes, bytes);
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        // One marker counts the start past the reserve and every turn but the last: its
        // start, item and end.
        let kept = 1 + counted;
        assert_eq!(entries.len(), kept + 4, "{entries:?}");
        assert_eq!(
            marker(&entries[kept]),
            Some((loss_text(1 + 3 * 999).as_str(), Some(2)))
        );
        assert_eq!(entries[kept + 1], Entry::TurnStarted);
        assert_eq!(
            marker(&entries[kept + 2]),
            Some((loss_text(1).as_str(), Some(2000)))
        );
        assert!(matches!(
            entries[kept + 3],
            Entry::TurnEnded {
                seq: Some(2001),
                ..
            }
        ));
        assert_eq!(logs.lost_entries(1), 1000);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn boundaries_past_the_reserve_are_all_saved_once_the_store_works_again() {
        let (logs, _, dir) = logs();
        let logs = logs.with_budget(8192);
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        fill_budget_and_reserve(&logs, 8192);
        spare_turn(&logs, 1);
        logs.writer.flush();
        fs::remove_file(path(&sessions, 0)).unwrap();
        // The next turn starts in the append that finds the store working.
        logs.append(
            1,
            vec![Write {
                entry: Entry::TurnStarted,
                sync: false,
            }],
        )
        .unwrap();
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        assert!(
            matches!(
                &entries[entries.len() - 2],
                Entry::TurnEnded { seq: Some(3), .. }
            ),
            "{entries:?}"
        );
        assert_eq!(entries.last(), Some(&Entry::TurnStarted));
    }

    #[test]
    fn rejected_recovery_batches_do_not_stop_boundaries_collapsing() {
        let (logs, _, dir) = logs();
        let logs = logs.with_budget(8192);
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        let counted = fill_budget_and_reserve(&logs, 8192);
        for n in 1..=200 {
            spare_turn(&logs, n);
            let recovery = Write {
                entry: Entry::Recovery {
                    batch: batch(&format!("recovery-{n}"), 20),
                },
                sync: true,
            };
            assert!(logs.append(1, vec![recovery]).is_err());
        }
        logs.writer.flush();
        let waiting = logs.files.lock().unwrap().sessions[&1].writes.len();
        // Each rejected batch keeps its own marker; the boundaries around them collapse.
        assert!(waiting <= 1 + counted + 200 + 6, "{waiting}");
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        let batches = entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Item { item, .. } if item.id.starts_with(&format!("{LOSS_MARKER_PREFIX}recovery-"))))
            .count();
        assert_eq!(batches, 200);
    }

    #[test]
    fn boundaries_past_the_reserve_do_not_wait_in_memory_for_a_failing_barrier() {
        let (logs, _, dir) = logs();
        let logs = logs.with_budget(8192);
        inject(&logs, 1, (0..10_000).map(|_| Fault::Committed).collect());
        let counted = fill_budget_and_reserve(&logs, 8192);
        for n in 1..=1000 {
            spare_turn(&logs, n);
        }
        logs.writer.flush();
        {
            let mut files = logs.files.lock().unwrap();
            let pending = files.sessions.get_mut(&1).unwrap();
            assert_eq!(
                pending.writes.len(),
                1 + counted + 1,
                "only budgeted entries and the last write wait for the barrier"
            );
            pending.log.as_mut().unwrap().faults.clear();
        }
        // Every row was committed while the barrier failed.
        let committed = read(&dir.path().join("sessions"), 1).unwrap();
        assert_eq!(committed.len(), 1 + counted + 1 + 3 * 1000);
        logs.flush_pending().unwrap();
        assert_eq!(read(&dir.path().join("sessions"), 1).unwrap(), committed);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn an_escaped_turn_error_that_needs_the_reserve_is_cut_not_dropped() {
        let (logs, _, dir) = logs();
        let logs = logs.with_budget(8192);
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        let overhead = serde_json::to_vec(&bulky(0, 0).entry).unwrap().len() + 1;
        logs.append(1, vec![bulky(0, 8192 - overhead)]).unwrap();
        let error = format!("start{}end", "\"\\\u{1}".repeat(4096));
        logs.append(
            1,
            vec![Write {
                entry: Entry::TurnEnded {
                    status: TurnStatus::Failed,
                    error: Some(error),
                    seq: Some(1),
                    run: Some(1),
                },
                sync: true,
            }],
        )
        .unwrap();
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        let ended = &entries[1];
        assert!(
            matches!(ended, Entry::TurnEnded { error: Some(error), .. }
            if error.starts_with("start\"\\\u{1}") && error.ends_with("\"\\\u{1}end")
                && error.contains("bytes not saved (log budget exceeded)")),
            "{ended:?}"
        );
        assert!(serde_json::to_vec(ended).unwrap().len() <= BOUNDARY_BYTES);
        // An item still keeps at least 1 KiB from each end, so under a small budget an
        // escaped one is rejected rather than cut further.
        let mut escaped = entry(2);
        if let Entry::Item { item, .. } = &mut escaped.entry {
            item.text = "\"".repeat(4096);
        }
        assert!(logs.append(1, vec![escaped]).is_err());
        logs.flush_pending().unwrap();
        assert_eq!(
            marker(read(&sessions, 1).unwrap().last().unwrap()),
            Some((loss_text(1).as_str(), Some(2)))
        );
    }

    fn batch(id: &str, rows: usize) -> crate::recovery::RecoveryBatch {
        crate::recovery::RecoveryBatch {
            id: id.into(),
            operations: (0..rows)
                .map(|n| crate::recovery::Operation::Insert {
                    after: "u1".into(),
                    item: Item {
                        text: "recovered ".repeat(100),
                        ..item(&format!("r{n}"))
                    },
                })
                .collect(),
            report: crate::recovery::RecoveryReport {
                outcome: crate::recovery::Outcome::Recovered,
                messages: rows as u32,
                tools: 0,
                extended: 0,
                turn: crate::recovery::TurnEvidence::Unaffected,
                limits: Vec::new(),
            },
            report_after: None,
            closes_turn: false,
        }
    }

    #[test]
    fn a_rejected_batch_counts_only_the_rows_it_would_have_added() {
        let mut recovery = batch("recovery-y", 2);
        recovery
            .operations
            .push(crate::recovery::Operation::Replace {
                expected: item("r0"),
                item: item("r0"),
            });
        assert_eq!(Gap::of(&Entry::Recovery { batch: recovery }).count, 3);
    }

    #[test]
    fn a_batch_rejected_again_after_other_writes_is_marked_once() {
        let (logs, _, dir) = logs();
        let logs = logs.with_budget(8192);
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        fill_budget_and_reserve(&logs, 8192);
        for _ in 0..50 {
            let recovery = Write {
                entry: Entry::Recovery {
                    batch: batch("recovery-x", 20),
                },
                sync: true,
            };
            assert!(logs.append(1, vec![recovery]).is_err());
            logs.append(
                1,
                vec![Write {
                    entry: Entry::ProviderExited,
                    sync: true,
                }],
            )
            .unwrap();
        }
        logs.writer.flush();
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush_pending().unwrap();
        let copies = read(&sessions, 1)
            .unwrap()
            .iter()
            .filter(|entry| matches!(entry, Entry::Item { item, .. } if item.id == format!("{LOSS_MARKER_PREFIX}recovery-x")))
            .count();
        assert_eq!(copies, 1);
        assert_eq!(logs.lost_entries(1), 21);
    }

    #[test]
    fn a_rejected_recovery_batch_is_marked_under_its_id_with_its_rows() {
        let (logs, _, dir) = logs();
        let logs = logs.with_budget(1000);
        let recovery = || Write {
            entry: Entry::Recovery {
                batch: batch("recovery-7", 3),
            },
            sync: true,
        };
        assert!(logs.append(1, vec![recovery()]).is_err());
        assert_eq!(logs.lost_entries(1), 4);
        // Retry while there is still no room: the same batch, counted and marked once.
        assert!(logs.append(1, vec![recovery()]).is_err());
        assert!(logs.append(1, vec![bulky(5, 2000)]).is_err());
        logs.flush_pending().unwrap();
        // A copy saved again after the first committed restores as the same marker.
        let entries = read(&dir.path().join("sessions"), 1).unwrap();
        let (later, batch_markers) = entries.split_last().unwrap();
        assert!(!batch_markers.is_empty());
        for entry in batch_markers {
            let Entry::Item {
                item, seq: None, ..
            } = entry
            else {
                panic!("{entries:?}");
            };
            assert_eq!(item.id, format!("{LOSS_MARKER_PREFIX}recovery-7"));
            assert_eq!(lost_count(item), Some(4));
        }
        assert_eq!(marker(later), Some((loss_text(1).as_str(), Some(5))));
        assert_eq!(logs.lost_entries(1), 5);
    }

    #[test]
    fn a_long_turn_error_is_saved_whole_while_it_fits() {
        let (logs, _, dir) = logs();
        let error = "e".repeat(10_000);
        logs.append(
            1,
            vec![Write {
                entry: Entry::TurnEnded {
                    status: TurnStatus::Failed,
                    error: Some(error.clone()),
                    seq: Some(0),
                    run: Some(1),
                },
                sync: true,
            }],
        )
        .unwrap();
        logs.flush().unwrap();
        assert!(matches!(
            &read(&dir.path().join("sessions"), 1).unwrap()[..],
            [Entry::TurnEnded { error: Some(saved), .. }] if *saved == error
        ));
    }

    #[test]
    fn turn_boundaries_use_a_reserve_when_the_budget_is_full() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(path(&sessions, 0), "not a database").unwrap();
        let overhead = serde_json::to_vec(&bulky(0, 0).entry).unwrap().len() + 1;
        logs.append(1, vec![bulky(0, MAX_PENDING_BYTES - overhead)])
            .unwrap();
        let ended = Write {
            entry: Entry::TurnEnded {
                status: TurnStatus::Failed,
                error: Some("e".repeat(1024 * 1024)),
                seq: Some(2),
                run: Some(1),
            },
            sync: true,
        };
        assert!(logs.append(1, vec![entry(1)]).is_err());
        logs.append(
            1,
            vec![
                Write {
                    entry: Entry::TurnStarted,
                    sync: false,
                },
                ended,
                Write {
                    entry: Entry::ProviderExited,
                    sync: true,
                },
            ],
        )
        .unwrap();
        let reserve = (MAX_PENDING_BYTES / 64).max(BOUNDARY_BYTES);
        assert!(logs.budget.lock().unwrap().bytes <= MAX_PENDING_BYTES + reserve);
        fs::remove_file(path(&sessions, 0)).unwrap();
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        assert!(marker(&entries[1]).is_some());
        assert!(matches!(entries[2], Entry::TurnStarted));
        assert!(
            matches!(&entries[3], Entry::TurnEnded { error: Some(error), .. } if error.len() < BOUNDARY_BYTES && error.contains("bytes not saved"))
        );
        assert!(matches!(entries[4], Entry::ProviderExited));
    }

    #[test]
    fn entries_round_trip_and_a_torn_last_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        fs::write(
            sessions.join("7.jsonl"),
            b"{\"entry\":\"turn_started\"}\n{\"entry\":\"turn_en",
        )
        .unwrap();
        let item = item("a");
        let mut log = SessionLog::open(&sessions, 7).unwrap();
        log.append(&Entry::Item {
            item: item.clone(),
            seq: Some(0),
            run: None,
        })
        .unwrap();
        log.sync().unwrap();
        drop(log);
        assert_eq!(
            read(&sessions, 7).unwrap(),
            vec![
                Entry::TurnStarted,
                Entry::Item {
                    item: item.clone(),
                    seq: Some(0),
                    run: None,
                }
            ]
        );
        assert!(read(&sessions, 8).unwrap().is_empty());

        // Reopening after the tear keeps the next entry.
        let mut log = SessionLog::open(&sessions, 7).unwrap();
        log.append(&Entry::TurnStarted).unwrap();
        assert_eq!(
            read(&sessions, 7).unwrap(),
            vec![
                Entry::TurnStarted,
                Entry::Item {
                    item,
                    seq: Some(0),
                    run: None
                },
                Entry::TurnStarted
            ]
        );
        assert_eq!(
            fs::read(sessions.join("7.jsonl")).unwrap(),
            b"{\"entry\":\"turn_started\"}\n{\"entry\":\"turn_en"
        );
    }
}
