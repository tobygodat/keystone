//! One append-only file per session, holding completed items and turn boundaries.
//!
//! Nothing is written per streamed chunk. Each completed item is appended once, and
//! the file is synced when a turn ends and after each approval decision. [`Logs`] does
//! the reading and writing on a worker thread.

use crate::connection::Wake;
use crate::item::{Item, TurnStatus};
use crate::worker::Worker;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One line of a session log.
///
/// `seq` is the row's place in the transcript as it was shown: the order in which rows
/// first appeared, not the order they completed. A steer appears before the reply it
/// interrupted completes, so completion order would put it first. Logs written before
/// `seq` existed have none and are shown in file order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum Entry {
    TurnStarted,
    Item {
        item: Item,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
    Approval {
        action: String,
        allowed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
    TurnEnded {
        status: TurnStatus,
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
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

/// Every session's log, read and written on one worker thread so the UI never waits on
/// the disk. Jobs run in the order they were queued: a session's entries are written in
/// order, and a load sees every entry queued before it.
pub struct Logs {
    worker: Worker,
    files: Arc<Mutex<Files>>,
    reports: Arc<Mutex<Vec<Report>>>,
    wake: Wake,
}

struct Files {
    dir: PathBuf,
    open: HashMap<u64, SessionLog>,
    /// Sessions whose last write failed. A success clears the error once.
    failing: HashSet<u64>,
}

impl Logs {
    /// `wake` runs on the worker after a report is queued; then call [`Logs::drain`].
    pub fn new(dir: PathBuf, wake: Wake) -> Self {
        Self {
            worker: Worker::new("session-logs"),
            files: Arc::new(Mutex::new(Files {
                dir,
                open: HashMap::new(),
                failing: HashSet::new(),
            })),
            reports: Arc::default(),
            wake,
        }
    }

    /// Reads the session's entries and reports them as [`Report::Loaded`].
    pub fn load(&self, session: u64) {
        let (files, reports, wake) = self.parts();
        self.worker.run(move || {
            let dir = files.lock().unwrap().dir.clone();
            let entries = read(&dir, session).map_err(|e| e.to_string());
            reports
                .lock()
                .unwrap()
                .push(Report::Loaded { session, entries });
            wake();
        });
    }

    /// Appends entries in order. A failure is reported, and the next append reopens the file.
    pub fn append(&self, session: u64, writes: Vec<Write>) {
        if writes.is_empty() {
            return;
        }
        let (files, reports, wake) = self.parts();
        self.worker.run(move || {
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

    /// Blocks until everything queued so far is written, such as before quitting.
    pub fn flush(&self) {
        self.worker.flush();
    }

    /// Takes every report since the last call.
    pub fn drain(&self) -> Vec<Report> {
        std::mem::take(&mut self.reports.lock().unwrap())
    }

    fn parts(&self) -> (Arc<Mutex<Files>>, Arc<Mutex<Vec<Report>>>, Wake) {
        (self.files.clone(), self.reports.clone(), self.wake.clone())
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
                seq: None
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
    fn a_load_sees_every_write_queued_before_it() {
        let (logs, wake, _dir) = logs();
        for n in 0..50 {
            logs.append(
                1,
                vec![Write {
                    entry: Entry::Item {
                        item: item(&n.to_string()),
                        seq: Some(n),
                    },
                    sync: n % 10 == 0,
                }],
            );
        }
        logs.load(1);
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
        let seqs: Vec<_> = entries
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| match e {
                Entry::Item { seq, .. } => seq.unwrap(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(seqs, (0..50).collect::<Vec<_>>());
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
                Entry::Item { item, seq: Some(0) },
                Entry::TurnStarted
            ]
        );
    }
}
