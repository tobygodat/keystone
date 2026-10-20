//! One append-only file per session, holding completed items and turn boundaries.
//!
//! Nothing is written per streamed chunk. Each completed item is appended once, and
//! the file is synced when a turn ends and after each approval decision. [`Logs`] does
//! the reading and writing off the UI thread.
//!
//! Failed writes are retained in memory up to [`MAX_PENDING_BYTES`]. This supports
//! repair and retry within the running app, not exactly-once recovery after a crash.
//!
//! Recovery means the append, tail repair and requested sync syscalls succeeded.
//! It cannot guarantee durability after a kernel/filesystem writeback failure:
//! some filesystems discard failed dirty pages and a later sync can succeed without
//! rewriting them. A cleared save error is not proof against that storage failure.
//! Open handles are retained to preserve append/sync checkpoints; a stale network
//! or FUSE handle may require restarting, which loses the in-memory retry queue.

use crate::connection::Wake;
use crate::item::{Item, ItemKind, ItemStatus, TurnStatus};
use crate::worker::Worker;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

/// One line of a session log.
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
}

/// An entry to append, and whether to sync the file after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Write {
    pub entry: Entry,
    pub sync: bool,
}

pub struct SessionLog {
    file: File,
    #[cfg(test)]
    faults: VecDeque<Fault>,
}

pub fn path(dir: &Path, session: u64) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

impl SessionLog {
    pub fn open(dir: &Path, session: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path(dir, session))?;
        // A crash can leave a torn last line. End it, so the next entry starts on a
        // line of its own instead of being swallowed by the torn one.
        let len = file.metadata()?.len();
        if len > 0 {
            let mut last = [0];
            file.seek(SeekFrom::Start(len - 1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        Ok(Self {
            file,
            #[cfg(test)]
            faults: VecDeque::new(),
        })
    }

    pub fn append(&mut self, entry: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        self.file.write_all(&line)
    }

    pub fn sync(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if matches!(self.faults.front(), Some(Fault::Sync)) {
            self.faults.pop_front();
            return Err(io::Error::other("injected sync failure"));
        }
        self.file.sync_data()
    }

    fn append_line(&mut self, line: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if let Some(Fault::Append(bytes)) = self.faults.front() {
            let bytes = *bytes;
            self.faults.pop_front();
            self.file.write_all(&line[..bytes.min(line.len())])?;
            return Err(io::Error::other("injected append failure"));
        }
        self.file.write_all(line)
    }

    fn rollback(&mut self, len: u64) -> io::Result<()> {
        #[cfg(test)]
        if matches!(self.faults.front(), Some(Fault::Rollback)) {
            self.faults.pop_front();
            return Err(io::Error::other("injected tail repair failure"));
        }
        self.file.set_len(len)
    }
}

/// Reads a session's entries. A torn or unreadable line, such as the last line after a
/// crash, is skipped rather than failing the whole session.
pub fn read(dir: &Path, session: u64) -> io::Result<Vec<Entry>> {
    let file = match File::open(path(dir, session)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut entries = Vec::new();
    for line in BufReader::new(file).split(b'\n') {
        if let Ok(entry) = serde_json::from_slice(&line?) {
            entries.push(entry);
        }
    }
    Ok(entries)
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
    dir: PathBuf,
    sessions: BTreeMap<u64, PendingSession>,
    failing: BTreeMap<u64, SaveFailure>,
}

#[derive(Clone, PartialEq, Eq)]
struct SaveFailure {
    message: String,
    retryable: bool,
}

/// Maximum serialized bytes retained across the worker mailbox and failed writes.
/// Once exceeded, that session rejects further batches and `flush` stays failed for
/// the lifetime of these Logs, even if earlier entries recover. `flush_pending`
/// saves a gap marker and reports only still-retryable work. Rejected entries are
/// not retained: callers must surface the loss. Queue metadata is additional memory.
pub const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
pub const LOSS_MARKER_PREFIX: &str = "keystone-log-loss-";

#[derive(Default)]
struct Budget {
    bytes: usize,
    rejected: BTreeMap<u64, Loss>,
}

#[derive(Default)]
struct Loss {
    entries: u64,
    marked: u64,
    queued: u64,
}

fn limit_error(session: u64, entries: u64) -> String {
    format!(
        "Session {session}: the 16 MiB log retry buffer limit was exceeded. {entries} entries were rejected and are not saved; further writes are disabled for this session until restart."
    )
}

struct PendingWrite {
    line: Vec<u8>,
    sync: bool,
    loss_marker: Option<u64>,
}

#[derive(Default)]
struct PendingSession {
    log: Option<SessionLog>,
    writes: VecDeque<PendingWrite>,
    progress: Progress,
}

#[derive(Default)]
enum Progress {
    #[default]
    Append,
    Rollback(u64),
    Sync,
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
    Append(usize),
    Sync,
    Rollback,
}

impl Logs {
    /// `wake` runs after a report is queued; then call [`Logs::drain`].
    pub fn new(dir: PathBuf, wake: Wake) -> Self {
        Self::with_reader(dir, wake, Arc::new(read))
    }

    pub fn with_reader(dir: PathBuf, wake: Wake, read: Reader) -> Self {
        Self {
            writer: Worker::new("session-logs"),
            files: Arc::new(Mutex::new(Files {
                dir: dir.clone(),
                sessions: BTreeMap::new(),
                failing: BTreeMap::new(),
            })),
            budget: Arc::default(),
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

    /// Queues entries in order. I/O failures retain the batch for the next append or
    /// flush to retry. Admission is bounded by [`MAX_PENDING_BYTES`]; a rejected
    /// batch returns an error and permanently fails this session's flush result.
    pub fn append(&self, session: u64, writes: Vec<Write>) -> Result<(), String> {
        if writes.is_empty() {
            return Ok(());
        }
        let mut admission = self.budget.lock().unwrap();
        let count = writes.len() as u64;
        if admission.rejected.contains_key(&session) {
            return Err(self.reject(session, count, &mut admission));
        }
        let mut pending = VecDeque::new();
        let mut bytes = 0;
        for write in writes {
            let mut line = LimitedLine {
                bytes: Vec::new(),
                limit: MAX_PENDING_BYTES - admission.bytes - bytes,
            };
            if serde_json::to_writer(&mut line, &write.entry).is_err()
                || line.write_all(b"\n").is_err()
            {
                return Err(self.reject(session, count, &mut admission));
            }
            bytes += line.bytes.len();
            pending.push_back(PendingWrite {
                line: line.bytes,
                sync: write.sync,
                loss_marker: None,
            });
        }
        admission.bytes += bytes;
        let (files, reports, wake) = (self.files.clone(), self.reports.clone(), self.wake.clone());
        let budget = self.budget.clone();
        self.writer.run(move || {
            let mut files = files.lock().unwrap();
            files
                .sessions
                .entry(session)
                .or_default()
                .writes
                .extend(pending);
            let report = files.retry(session, &budget);
            drop(files);
            if let Some(report) = report {
                reports.lock().unwrap().push(report);
                wake();
            }
        });
        drop(admission);
        Ok(())
    }

    fn reject(&self, session: u64, count: u64, budget: &mut Budget) -> String {
        let loss = budget.rejected.entry(session).or_default();
        loss.entries = loss.entries.saturating_add(count);
        let error = limit_error(session, loss.entries);
        let (reports, wake, budget, files) = (
            self.reports.clone(),
            self.wake.clone(),
            self.budget.clone(),
            self.files.clone(),
        );
        self.writer.run(move || {
            let entries = budget.lock().unwrap().rejected[&session].entries;
            let files = files.lock().unwrap();
            let failure = files
                .failing
                .get(&session)
                .filter(|failure| failure.retryable);
            reports.lock().unwrap().push(Report::Saved {
                session,
                error: Some(failure.map_or_else(
                    || limit_error(session, entries),
                    |failure| failure.message.clone(),
                )),
                retryable: failure.is_some(),
                lost: entries,
            });
            wake();
        });
        error
    }

    pub fn lost_entries(&self, session: u64) -> u64 {
        self.budget
            .lock()
            .unwrap()
            .rejected
            .get(&session)
            .map_or(0, |loss| loss.entries)
    }

    /// Retries pending appends and requested syncs, then reports every outstanding
    /// failure, including terminal admission loss. This strict barrier never succeeds
    /// after a rejection; the desktop's Quit uses [`Logs::flush_pending`] instead.
    pub fn flush(&self) -> Result<(), String> {
        self.barrier(false)
    }

    /// Saves recoverable work and a durable loss marker without vetoing on entries
    /// that admission already rejected. Normal writes remain disabled until restart.
    pub fn flush_pending(&self) -> Result<(), String> {
        self.barrier(true)
    }

    fn barrier(&self, record_loss: bool) -> Result<(), String> {
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
            let mut marker_errors = Vec::new();
            if record_loss {
                let (sessions, errors) = files.queue_loss_markers(&budget);
                marker_errors = errors;
                for session in sessions {
                    if let Some(report) = files.retry(session, &budget) {
                        recovered.push(report);
                    }
                }
            }
            let mut errors: BTreeMap<_, _> = files
                .failing
                .iter()
                .filter(|(_, failure)| !record_loss || failure.retryable)
                .map(|(session, failure)| (*session, failure.message.clone()))
                .collect();
            if !record_loss {
                for (session, loss) in &budget.lock().unwrap().rejected {
                    let limit = limit_error(*session, loss.entries);
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
            let errors: Vec<_> = errors.into_values().chain(marker_errors).collect();
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

    /// Takes every report since the last call.
    pub fn drain(&self) -> Vec<Report> {
        std::mem::take(&mut self.reports.lock().unwrap())
    }
}

impl Files {
    fn queue_loss_markers(&mut self, budget: &Mutex<Budget>) -> (Vec<u64>, Vec<String>) {
        let sessions: Vec<_> = budget.lock().unwrap().rejected.keys().copied().collect();
        let mut queued = Vec::new();
        let mut errors = Vec::new();
        for session in sessions {
            if self
                .sessions
                .get(&session)
                .is_some_and(|pending| !pending.writes.is_empty())
            {
                continue;
            }
            let mut budget = budget.lock().unwrap();
            let loss = &budget.rejected[&session];
            if loss.entries == loss.marked || loss.queued > loss.marked {
                continue;
            }
            let total = loss.entries;
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.as_nanos());
            let text = format!(
                "{} log entries were lost (log budget exceeded).",
                total - loss.marked
            );
            let entry = Entry::Item {
                item: Item {
                    id: format!(
                        "{LOSS_MARKER_PREFIX}{}-{nanos}-{session}-{total}",
                        std::process::id()
                    ),
                    kind: ItemKind::Other {
                        name: format!("Keystone log loss: {text}"),
                    },
                    text,
                    status: ItemStatus::Failed,
                },
                seq: None,
                run: None,
            };
            let mut line = serde_json::to_vec(&entry).expect("log loss marker is serializable");
            line.push(b'\n');
            if budget.bytes + line.len() > MAX_PENDING_BYTES {
                errors.push(format!(
                    "Session {session}: the log loss marker is waiting for retry-buffer space"
                ));
                continue;
            }
            budget.bytes += line.len();
            budget.rejected.get_mut(&session).unwrap().queued = total;
            self.sessions
                .entry(session)
                .or_default()
                .writes
                .push_back(PendingWrite {
                    line,
                    sync: true,
                    loss_marker: Some(total),
                });
            queued.push(session);
        }
        (queued, errors)
    }

    fn retry(&mut self, session: u64, budget: &Mutex<Budget>) -> Option<Report> {
        let pending = self.sessions.get_mut(&session).expect("queued above");
        let result = pending.retry(&self.dir, session, budget);
        let failure = result
            .err()
            .map(|error| format!("Session {session}: {error}"));
        let lost = budget
            .lock()
            .unwrap()
            .rejected
            .get(&session)
            .map_or(0, |loss| loss.entries);
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
    fn retry(&mut self, dir: &Path, session: u64, budget: &Mutex<Budget>) -> io::Result<()> {
        while let Some(write) = self.writes.front() {
            if self.log.is_none() {
                self.log = Some(SessionLog::open(dir, session)?);
            }
            let log = self.log.as_mut().expect("opened above");
            if let Progress::Rollback(len) = self.progress {
                log.rollback(len)?;
                self.progress = Progress::Append;
            }
            if matches!(self.progress, Progress::Append) {
                let len = log.file.metadata()?.len();
                self.progress = Progress::Rollback(len);
                log.append_line(&write.line)?;
                self.progress = Progress::Sync;
            }
            if write.sync {
                log.sync()?;
            }
            {
                let mut budget = budget.lock().unwrap();
                budget.bytes -= write.line.len();
                if let Some(marked) = write.loss_marker {
                    budget
                        .rejected
                        .get_mut(&session)
                        .expect("loss marker has a rejection")
                        .marked = marked;
                }
            }
            self.writes.pop_front();
            self.progress = Progress::Append;
        }
        self.writes = VecDeque::new();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{ItemKind, ItemStatus};

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

    /// A read that never returns, here from a FIFO nothing writes to, must not hold up
    /// other sessions' writes and loads, or quitting.
    #[test]
    fn a_read_that_never_returns_holds_up_neither_writes_nor_quitting() {
        use std::time::Duration;
        let (logs, wake, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(path(&sessions, 1))
            .status()
            .unwrap();
        assert!(status.success());
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
    }

    #[test]
    fn a_save_error_is_reported_until_saving_works_again() {
        let (logs, wake, dir) = logs();
        let sessions = dir.path().join("sessions");
        std::fs::write(&sessions, "").unwrap();
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
        std::fs::remove_file(&sessions).unwrap();
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
        fs::write(&sessions, "blocked").unwrap();
        logs.append(1, vec![entry(0), entry(1)]).unwrap();
        logs.append(1, vec![entry(2)]).unwrap();
        logs.append(2, vec![entry(3)]).unwrap();
        let error = logs.flush().unwrap_err();
        assert!(error.contains("Session 1") && error.contains("Session 2"));
        assert!(logs.budget.lock().unwrap().bytes > 0);
        logs.drain();
        fs::remove_file(&sessions).unwrap();
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
        let log = SessionLog::open(&files.dir, session).unwrap();
        let pending = files.sessions.entry(session).or_default();
        pending.log = Some(log);
        pending.log.as_mut().unwrap().faults = faults;
    }

    #[test]
    fn partial_append_and_failed_tail_repair_block_later_entries_until_retry() {
        let (logs, _, dir) = logs();
        inject(
            &logs,
            1,
            VecDeque::from([Fault::Append(13), Fault::Rollback, Fault::Rollback]),
        );
        logs.append(1, vec![entry(0), entry(1)]).unwrap();
        logs.writer.flush();
        let sessions = dir.path().join("sessions");
        let partial = fs::read(path(&sessions, 1)).unwrap();
        assert_eq!(partial.len(), 13);
        assert!(logs.flush().unwrap_err().contains("tail repair"));
        logs.append(1, vec![entry(2)]).unwrap();
        logs.writer.flush();
        assert_eq!(fs::read(path(&sessions, 1)).unwrap(), partial);
        logs.flush().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0, 1, 2]);
        assert_eq!(
            fs::read(path(&sessions, 1))
                .unwrap()
                .iter()
                .filter(|b| **b == b'\n')
                .count(),
            3
        );
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn sync_retry_does_not_append_the_entry_twice_or_clear_failure_early() {
        let (logs, _, dir) = logs();
        inject(
            &logs,
            1,
            VecDeque::from([Fault::Sync, Fault::Sync, Fault::Sync]),
        );
        logs.append(1, vec![entry(0), entry(1)]).unwrap();
        logs.writer.flush();
        let sessions = dir.path().join("sessions");
        let before = fs::read(path(&sessions, 1)).unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0]);
        assert!(logs.flush().unwrap_err().contains("sync failure"));
        logs.append(2, vec![entry(5)]).unwrap();
        logs.writer.flush();
        assert_eq!(fs::read(path(&sessions, 1)).unwrap(), before);
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
    fn zero_byte_append_failure_preserves_the_successful_prefix() {
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
            .push_back(Fault::Append(0));
        logs.append(1, vec![entry(2), entry(3)]).unwrap();
        logs.writer.flush();
        let sessions = dir.path().join("sessions");
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [1]);
        logs.flush().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [1, 2, 3]);
    }

    #[test]
    fn buffer_limit_rejection_is_sticky_after_the_retained_prefix_recovers() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(&sessions, "blocked").unwrap();
        let mut first = entry(0);
        if let Entry::Item { item, .. } = &mut first.entry {
            item.text = "a".repeat(MAX_PENDING_BYTES / 2);
        }
        logs.append(1, vec![first.clone()]).unwrap();
        let before = logs.budget.lock().unwrap().bytes;
        assert!(logs.append(1, vec![first]).unwrap_err().contains("limit"));
        assert_eq!(logs.budget.lock().unwrap().bytes, before);
        assert!(logs.append(1, vec![entry(2)]).is_err());
        assert!(logs.flush().is_err());
        fs::remove_file(&sessions).unwrap();
        assert!(logs.flush().unwrap_err().contains("rejected"));
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0]);
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
        let mut oversized = entry(4);
        if let Entry::Item { item, .. } = &mut oversized.entry {
            item.text = "a".repeat(MAX_PENDING_BYTES);
        }
        assert!(logs.append(3, vec![oversized]).is_err());
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn rejection_report_follows_any_already_queued_recovery() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(&sessions, "blocked").unwrap();
        logs.append(1, vec![entry(0)]).unwrap();
        assert!(logs.flush().is_err());
        logs.drain();
        fs::remove_file(&sessions).unwrap();
        logs.flush().unwrap();
        let mut oversized = entry(1);
        if let Entry::Item { item, .. } = &mut oversized.entry {
            item.text = "a".repeat(MAX_PENDING_BYTES);
        }
        assert!(logs.append(1, vec![oversized]).is_err());
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
        let mut oversized = entry(0);
        if let Entry::Item { item, .. } = &mut oversized.entry {
            item.text = "a".repeat(MAX_PENDING_BYTES);
        }
        assert!(logs.append(1, vec![oversized, entry(1)]).is_err());
        assert_eq!(logs.lost_entries(1), 2);
        logs.flush_pending().unwrap();
        logs.flush_pending().unwrap();
        let sessions = dir.path().join("sessions");
        let entries = read(&sessions, 1).unwrap();
        assert!(
            matches!(&entries[..], [Entry::Item { item, seq: None, run: None }] if
            item.kind == ItemKind::Other { name: "Keystone log loss: 2 log entries were lost (log budget exceeded).".into() }
                && item.text == "2 log entries were lost (log budget exceeded)."
                && item.status == ItemStatus::Failed)
        );
        assert!(logs.flush().unwrap_err().contains("2 entries"));
        assert!(logs.append(1, vec![entry(2)]).is_err());
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(&entries[1], Entry::Item { item, .. } if item.text == "1 log entries were lost (log budget exceeded).")
        );
        assert_eq!(logs.lost_entries(1), 3);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn terminal_loss_does_not_mask_a_retained_write_failure_and_recovery_keeps_order() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(&sessions, "blocked").unwrap();
        let mut large = entry(0);
        if let Entry::Item { item, .. } = &mut large.entry {
            item.text = "a".repeat(MAX_PENDING_BYTES / 2);
        }
        logs.append(1, vec![large.clone()]).unwrap();
        let original = logs.flush().unwrap_err();
        logs.drain();
        assert!(logs.append(1, vec![large]).is_err());
        logs.writer.flush();
        assert!(
            matches!(logs.drain().last(), Some(Report::Saved { error: Some(error), retryable: true, lost: 1, .. }) if error.contains(&original))
        );
        assert!(logs.flush_pending().unwrap_err().contains(&original));
        fs::remove_file(&sessions).unwrap();
        logs.flush_pending().unwrap();
        let entries = read(&sessions, 1).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], Entry::Item { seq: Some(0), .. }));
        assert!(
            matches!(&entries[1], Entry::Item { item, .. } if item.text == "1 log entries were lost (log budget exceeded).")
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
    fn loss_markers_share_the_budget_and_wait_for_other_sessions_to_recover() {
        let (logs, _, dir) = logs();
        let sessions = dir.path().join("sessions");
        fs::write(&sessions, "blocked").unwrap();
        let mut full = entry(0);
        let overhead = serde_json::to_vec(&full.entry).unwrap().len() + 1;
        if let Entry::Item { item, .. } = &mut full.entry {
            item.text = "a".repeat(MAX_PENDING_BYTES - overhead - 1);
        }
        logs.append(1, vec![full]).unwrap();
        assert!(logs.append(2, vec![entry(1)]).is_err());
        let error = logs.flush_pending().unwrap_err();
        assert!(error.contains("Session 1") && error.contains("retry-buffer space"));
        assert!(logs.budget.lock().unwrap().bytes <= MAX_PENDING_BYTES);
        fs::remove_file(&sessions).unwrap();
        logs.flush_pending().unwrap();
        assert_eq!(seqs(&read(&sessions, 1).unwrap()), [0]);
        assert!(
            matches!(&read(&sessions, 2).unwrap()[..], [Entry::Item { item, .. }] if item.text == "1 log entries were lost (log budget exceeded).")
        );
        logs.append(3, vec![entry(2)]).unwrap();
        logs.flush_pending().unwrap();
        assert_eq!(seqs(&read(&sessions, 3).unwrap()), [2]);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn a_loss_marker_sync_failure_is_retryable_without_duplicate_markers() {
        let (logs, _, dir) = logs();
        let mut oversized = entry(0);
        if let Entry::Item { item, .. } = &mut oversized.entry {
            item.text = "a".repeat(MAX_PENDING_BYTES);
        }
        assert!(logs.append(1, vec![oversized]).is_err());
        logs.writer.flush();
        inject(&logs, 1, VecDeque::from([Fault::Sync, Fault::Sync]));
        assert!(logs.flush_pending().unwrap_err().contains("sync failure"));
        let before = fs::read(path(&dir.path().join("sessions"), 1)).unwrap();
        assert!(logs.budget.lock().unwrap().bytes > 0);
        assert!(logs.flush_pending().unwrap_err().contains("sync failure"));
        logs.flush_pending().unwrap();
        assert_eq!(
            fs::read(path(&dir.path().join("sessions"), 1)).unwrap(),
            before
        );
        assert_eq!(read(&dir.path().join("sessions"), 1).unwrap().len(), 1);
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn a_loss_marker_partial_append_repairs_its_tail_before_replay() {
        let (logs, _, dir) = logs();
        let mut oversized = entry(0);
        if let Entry::Item { item, .. } = &mut oversized.entry {
            item.text = "a".repeat(MAX_PENDING_BYTES);
        }
        assert!(logs.append(1, vec![oversized]).is_err());
        logs.writer.flush();
        inject(
            &logs,
            1,
            VecDeque::from([Fault::Append(13), Fault::Rollback]),
        );
        assert!(logs.flush_pending().unwrap_err().contains("append failure"));
        assert!(logs.flush_pending().unwrap_err().contains("tail repair"));
        logs.flush_pending().unwrap();
        let sessions = dir.path().join("sessions");
        assert_eq!(read(&sessions, 1).unwrap().len(), 1);
        assert_eq!(
            fs::read(path(&sessions, 1))
                .unwrap()
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1
        );
        assert_eq!(logs.budget.lock().unwrap().bytes, 0);
    }

    #[test]
    fn entries_round_trip_and_a_torn_last_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let item = item("a");
        let mut log = SessionLog::open(dir.path(), 7).unwrap();
        log.append(&Entry::TurnStarted).unwrap();
        log.append(&Entry::Item {
            item: item.clone(),
            seq: Some(0),
            run: None,
        })
        .unwrap();
        log.sync().unwrap();
        drop(log);
        let mut file = OpenOptions::new()
            .append(true)
            .open(path(dir.path(), 7))
            .unwrap();
        file.write_all(b"{\"entry\":\"turn_en").unwrap();

        assert_eq!(
            read(dir.path(), 7).unwrap(),
            vec![
                Entry::TurnStarted,
                Entry::Item {
                    item: item.clone(),
                    seq: Some(0),
                    run: None,
                }
            ]
        );
        assert!(read(dir.path(), 8).unwrap().is_empty());

        // Reopening after the tear keeps the next entry.
        let mut log = SessionLog::open(dir.path(), 7).unwrap();
        log.append(&Entry::TurnStarted).unwrap();
        assert_eq!(
            read(dir.path(), 7).unwrap(),
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
    }
}
