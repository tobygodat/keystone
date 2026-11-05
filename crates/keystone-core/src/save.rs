//! The Store's saves. A change applies in memory at once and its snapshot goes to the
//! writer, which commits each one in order as its own transaction. When a commit fails,
//! the newest snapshot stays queued, and the next save, [`Store::retry`] or
//! [`Store::flush`] tries it again. Saves queued behind a failure are tried once, by
//! the last of them. A commit another writer made stale is never tried again.
//!
//! [`Store::retry`]: crate::Store::retry
//! [`Store::flush`]: crate::Store::flush

use crate::database::{self, Database};
use crate::writer::{BUSY, Slot};
use crate::{Revision, Snapshot, Workspace};
use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Asks the app, from any thread, to look at what changed.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Why changes are not saved yet. Shown, it reads as its reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// A commit failed, so the newest changes are not in the database.
    Unsaved(String),
    /// Every change is committed, and the folder sync that confirms them failed.
    Unconfirmed(String),
    /// Another writer changed the database, so no change from this store can be saved.
    /// Nothing tries them again, and the error stays until the store is opened again.
    Stale(String),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Unsaved(reason) | Self::Unconfirmed(reason) | Self::Stale(reason)) = self;
        f.write_str(reason)
    }
}

pub(crate) struct Saves {
    database: Database,
    /// The newest revision issued.
    issued: u64,
    /// Touched only by jobs on the write connection, which run one at a time.
    saver: Arc<Mutex<Saver>>,
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    progress: Mutex<Progress>,
    changed: Condvar,
    wake: Mutex<Option<Wake>>,
}

/// What the writer has done with the issued revisions. It is locked only to publish,
/// never across I/O.
#[derive(Default)]
struct Progress {
    /// The newest revision committed and confirmed by a directory sync.
    confirmed: u64,
    /// Transactions committed.
    writes: u64,
    /// Why the last drain left revisions unsaved.
    error: Option<SaveError>,
    /// Drains queued and not finished.
    draining: usize,
    /// The newest revision whose save wakes the app.
    watched: u64,
}

struct Saver {
    /// Issued and not committed, oldest first.
    queued: VecDeque<(u64, Snapshot)>,
    /// What the database holds, which each commit is a change from.
    committed: Workspace,
    /// The database's workspace revision, which a commit must still find there.
    revision: i64,
    /// The folder holding the database file, synced to confirm a commit. None in memory.
    folder: Option<PathBuf>,
    /// The newest revision committed and not confirmed yet, with the folder to sync.
    unconfirmed: Option<(u64, File)>,
    confirmed: u64,
    sync_directory: fn(&File) -> io::Result<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Saves {
    pub(crate) fn new(database: Database, committed: Workspace, revision: i64) -> Self {
        let folder = (!database.is_memory()).then(|| {
            database
                .path()
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf()
        });
        Self {
            database,
            issued: 0,
            saver: Arc::new(Mutex::new(Saver {
                queued: VecDeque::new(),
                committed,
                revision,
                folder,
                unconfirmed: None,
                confirmed: 0,
                sync_directory: File::sync_all,
            })),
            shared: Arc::default(),
        }
    }

    pub(crate) fn issued(&self) -> Revision {
        Revision(self.issued)
    }

    /// Queues `snapshot` as the next revision, and returns that revision.
    pub(crate) fn save(&mut self, snapshot: Snapshot) -> Revision {
        self.issued += 1;
        self.drain(Some((self.issued, snapshot)));
        Revision(self.issued)
    }

    /// Queues another try when the last one failed, none is queued, and another try
    /// could save it.
    pub(crate) fn retry(&self) {
        let failing = {
            let progress = lock(&self.shared.progress);
            progress
                .error
                .as_ref()
                .is_some_and(|error| !matches!(error, SaveError::Stale(_)))
                && progress.draining == 0
        };
        if failing {
            self.drain(None);
        }
    }

    fn drain(&self, snapshot: Option<(u64, Snapshot)>) {
        lock(&self.shared.progress).draining += 1;
        let (saver, shared) = (self.saver.clone(), self.shared.clone());
        let queued = self.database.submit(move |slot| {
            let mut saver = lock(&saver);
            saver.queued.extend(snapshot);
            // After a failure, a drain with another queued behind it leaves the try to
            // that one, so a backlog of saves costs one try rather than one each.
            let failed = {
                let progress = lock(&shared.progress);
                progress.error.clone().filter(|_| progress.draining > 1)
            };
            let (writes, outcome) = match failed {
                Some(error) => {
                    let older = saver.queued.len().saturating_sub(1);
                    saver.queued.drain(..older);
                    (0, Err(error))
                }
                None => saver.drain(slot),
            };
            let confirmed = saver.confirmed;
            Box::new(move || shared.finish(writes, confirmed, outcome.err()))
        });
        if let Err(error) = queued {
            let confirmed = lock(&self.shared.progress).confirmed;
            self.shared
                .finish(0, confirmed, Some(SaveError::Unsaved(error.to_string())));
        }
    }

    pub(crate) fn is_durable(&self, revision: Revision) -> bool {
        lock(&self.shared.progress).confirmed >= revision.0
    }

    /// Wakes the app once `revision` is saved, at once when it already is.
    pub(crate) fn wake_when_durable(&self, revision: Revision) {
        let durable = {
            let mut progress = lock(&self.shared.progress);
            progress.watched = progress.watched.max(revision.0);
            progress.confirmed >= revision.0
        };
        if durable && let Some(wake) = lock(&self.shared.wake).clone() {
            wake();
        }
    }

    pub(crate) fn error(&self) -> Option<SaveError> {
        lock(&self.shared.progress).error.clone()
    }

    pub(crate) fn writes(&self) -> u64 {
        self.database.flush();
        lock(&self.shared.progress).writes
    }

    /// Waits up to `timeout` for `revision` to be saved, and says why it is not, or
    /// nothing when it is still being saved.
    pub(crate) fn wait(
        &self,
        revision: Revision,
        timeout: Duration,
    ) -> Result<(), Option<SaveError>> {
        let deadline = Instant::now() + timeout;
        let mut progress = lock(&self.shared.progress);
        loop {
            if progress.confirmed >= revision.0 {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if progress.draining == 0 || left.is_zero() {
                return Err(progress.error.clone());
            }
            progress = self
                .shared
                .changed
                .wait_timeout(progress, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Tries again, waits for the writer, then says why anything issued is not saved.
    /// Changes another writer made stale do not fail it: [`Saves::error`] keeps them.
    pub(crate) fn flush(&self) -> Result<(), String> {
        self.retry();
        self.database.flush();
        let progress = lock(&self.shared.progress);
        if progress.confirmed >= self.issued || matches!(progress.error, Some(SaveError::Stale(_)))
        {
            return Ok(());
        }
        Err(progress
            .error
            .as_ref()
            .map_or_else(|| BUSY.into(), ToString::to_string))
    }

    pub(crate) fn set_wake(&self, wake: Wake) {
        *lock(&self.shared.wake) = Some(wake);
    }

    /// Replaces the folder sync from the next save on.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn set_sync_directory(&self, sync: fn(&File) -> io::Result<()>) {
        let saver = self.saver.clone();
        let _ = self.database.submit(move |_| {
            lock(&saver).sync_directory = sync;
            Box::new(|| ())
        });
    }
}

impl Shared {
    /// Publishes what a drain did, and wakes the app when the save status changed or a
    /// watched revision was saved.
    fn finish(&self, writes: u64, confirmed: u64, error: Option<SaveError>) {
        let changed = {
            let mut progress = lock(&self.progress);
            progress.draining -= 1;
            progress.writes += writes;
            let saved = progress.confirmed < progress.watched && confirmed >= progress.watched;
            progress.confirmed = progress.confirmed.max(confirmed);
            let changed = progress.error != error || saved;
            progress.error = error;
            changed
        };
        self.changed.notify_all();
        if changed && let Some(wake) = lock(&self.wake).clone() {
            wake();
        }
    }
}

impl Saver {
    /// Commits each queued snapshot in order, then confirms them with one folder sync.
    /// After a failure only the newest snapshot stays queued: its change from what is
    /// saved holds every queued change. None stays after another writer made them
    /// stale. Returns the transactions committed.
    fn drain(&mut self, slot: &mut Slot) -> (u64, Result<(), SaveError>) {
        let mut writes = 0;
        let mut failure = None;
        while let Some((revision, snapshot)) = self.queued.front() {
            let revision = *revision;
            let committed = self
                .folder
                .as_ref()
                .map(File::open)
                .transpose()
                .and_then(|folder| {
                    let next = slot.run(|connection| {
                        database::commit(connection, snapshot, &self.committed, self.revision)
                    })?;
                    Ok((next, folder))
                });
            match committed {
                Ok((next, folder)) => {
                    let (_, snapshot) = self.queued.pop_front().expect("committed above");
                    self.committed = snapshot.workspace;
                    self.revision = next;
                    writes += 1;
                    match folder {
                        Some(folder) => self.unconfirmed = Some((revision, folder)),
                        None => self.confirmed = revision,
                    }
                }
                Err(error) => {
                    let stale = error
                        .get_ref()
                        .is_some_and(|error| error.is::<database::Stale>());
                    failure = Some(if stale {
                        SaveError::Stale(error.to_string())
                    } else {
                        SaveError::Unsaved(error.to_string())
                    });
                    break;
                }
            }
        }
        match failure {
            Some(SaveError::Stale(_)) => self.queued.clear(),
            Some(_) => {
                self.queued.drain(..self.queued.len().saturating_sub(1));
            }
            None => {}
        }
        if let Some((revision, folder)) = &self.unconfirmed {
            match (self.sync_directory)(folder) {
                Ok(()) => {
                    self.confirmed = *revision;
                    self.unconfirmed = None;
                }
                Err(error) => {
                    failure.get_or_insert(SaveError::Unconfirmed(error.to_string()));
                }
            }
        }
        (writes, failure.map_or(Ok(()), Err))
    }
}
