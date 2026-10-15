//! One append-only file per session, holding completed items and turn boundaries.
//!
//! Nothing is written per streamed chunk. Each completed item is appended once, and
//! the file is synced when a turn ends and after each approval decision.

use crate::item::{Item, TurnStatus};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum Entry {
    TurnStarted,
    Item {
        item: Item,
    },
    Approval {
        action: String,
        allowed: bool,
    },
    TurnEnded {
        status: TurnStatus,
        error: Option<String>,
    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{ItemKind, ItemStatus};

    #[test]
    fn entries_round_trip_and_a_torn_last_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let item = Item {
            id: "a".into(),
            kind: ItemKind::AgentMessage,
            text: "Hello".into(),
            status: ItemStatus::Completed,
        };
        let mut log = SessionLog::open(dir.path(), 7).unwrap();
        log.append(&Entry::TurnStarted).unwrap();
        log.append(&Entry::Item { item: item.clone() }).unwrap();
        log.sync().unwrap();
        drop(log);
        let mut file = OpenOptions::new()
            .append(true)
            .open(path(dir.path(), 7))
            .unwrap();
        file.write_all(b"{\"entry\":\"turn_en").unwrap();

        assert_eq!(
            read(dir.path(), 7).unwrap(),
            vec![Entry::TurnStarted, Entry::Item { item: item.clone() }]
        );
        assert!(read(dir.path(), 8).unwrap().is_empty());

        // Reopening after the tear keeps the next entry.
        let mut log = SessionLog::open(dir.path(), 7).unwrap();
        log.append(&Entry::TurnStarted).unwrap();
        assert_eq!(
            read(dir.path(), 7).unwrap(),
            vec![Entry::TurnStarted, Entry::Item { item }, Entry::TurnStarted]
        );
    }
}
