//! One append-only file per session, holding completed items and turn boundaries.
//!
//! Nothing is written per streamed chunk. Each completed item is appended once, and
//! the file is synced when a turn ends and after each approval decision. [`Logs`] does
//! the reading and writing off the UI thread.

use crate::connection::Wake;
use crate::item::{Item, TurnStatus};
use crate::worker::Worker;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
        Ok(Self { file })
    }

    pub fn append(&mut self, entry: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        self.file.write_all(&line)
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
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
    /// Saving failed with this error, or with `None`, works again after a failure.
    Saved { session: u64, error: Option<String> },
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
    reports: Arc<Mutex<Vec<Report>>>,
    /// How many loads have not reported yet.
    loading: Arc<(Mutex<usize>, Condvar)>,
    wake: Wake,
}

struct Files {
    dir: PathBuf,
    open: HashMap<u64, SessionLog>,
    /// Sessions whose last write failed. A success clears the error once.
    failing: HashSet<u64>,
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
                open: HashMap::new(),
                failing: HashSet::new(),
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

    /// Appends entries in order. A failure is reported, and the next append reopens the file.
    pub fn append(&self, session: u64, writes: Vec<Write>) {
        if writes.is_empty() {
            return;
        }
        let (files, reports, wake) = (self.files.clone(), self.reports.clone(), self.wake.clone());
        self.writer.run(move || {
            let mut files = files.lock().unwrap();
            let report = match files.write(session, &writes) {
                Ok(()) => files.failing.remove(&session).then_some(Report::Saved {
                    session,
                    error: None,
                }),
                Err(error) => {
                    files.open.remove(&session);
                    files.failing.insert(session);
                    Some(Report::Saved {
                        session,
                        error: Some(error.to_string()),
                    })
                }
            };
            drop(files);
            if let Some(report) = report {
                reports.lock().unwrap().push(report);
                wake();
            }
        });
    }

    /// Blocks until everything queued so far is written, such as before quitting. It
    /// does not wait for loads; see [`Logs::wait_for_loads`].
    pub fn flush(&self) {
        self.writer.flush();
    }

    /// Takes every report since the last call.
    pub fn drain(&self) -> Vec<Report> {
        std::mem::take(&mut self.reports.lock().unwrap())
    }
}

impl Files {
    fn write(&mut self, session: u64, writes: &[Write]) -> io::Result<()> {
        if !self.open.contains_key(&session) {
            let log = SessionLog::open(&self.dir, session)?;
            self.open.insert(session, log);
        }
        let log = self.open.get_mut(&session).expect("opened above");
        for write in writes {
            log.append(&write.entry)?;
            if write.sync {
                log.sync()?;
            }
        }
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
            logs.append(1, vec![entry(n)]);
        }
        logs.flush();
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
            logs.append(2, vec![entry(0)]);
            logs.flush();
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
        logs.append(2, write());
        wake.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(
            &logs.drain()[..],
            [Report::Saved {
                session: 2,
                error: Some(_)
            }]
        ));
        std::fs::remove_file(&sessions).unwrap();
        logs.append(2, write());
        wake.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            logs.drain(),
            vec![Report::Saved {
                session: 2,
                error: None
            }]
        );
        // Further successes report nothing.
        logs.append(2, write());
        logs.flush();
        assert!(logs.drain().is_empty());
        assert_eq!(read(&sessions, 2).unwrap().len(), 2);
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
