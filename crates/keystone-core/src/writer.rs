//! The one connection this process writes a database file through, and the thread that
//! owns it. Every write runs on that thread in the order it was queued, so writes never
//! contend with each other, and the connection stays open while the database is in use,
//! so readers never find the file without a connection and rebuild its WAL index.

use crate::database::{BUSY_TIMEOUT, configure, read_only, require_writable, sql_error};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::Duration;

/// How long a write waits for the writes queued before it before reporting the writer
/// busy: a hang guard, not a latency budget. A write that has started is waited for.
pub const WRITER_WAIT: Duration = Duration::from_secs(2);

/// Why a write that waited [`WRITER_WAIT`] did not run.
pub(crate) const BUSY: &str = "SQLite writer is busy; retry saving";

/// What answers a queued write's caller, which the writer runs once the database file is
/// settled.
pub(crate) type Reply = Box<dyn FnOnce() + Send>;

/// A queued write. It runs on the writer's connection and returns its [`Reply`].
type Job = Box<dyn FnOnce(&mut Slot) -> Reply + Send>;

/// How long an idle writer waits before trying again to empty a WAL a reader held.
const SETTLE_RETRY: Duration = Duration::from_millis(100);

/// This process's live writers, by the path each writes and the file it found there, so
/// a second open of a file joins its writer instead of starting another.
static OPEN: LazyLock<Mutex<HashMap<PathBuf, (Weak<Writer>, Option<FileId>)>>> =
    LazyLock::new(Mutex::default);

pub(crate) struct Writer {
    jobs: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    id: ThreadId,
}

/// The connection a write runs on: the writer's to the file at `path`, with the identity
/// of the file it opened, or an in-memory database's only connection.
pub(crate) enum Slot {
    Disk {
        path: PathBuf,
        open: Option<(Connection, Option<FileId>)>,
    },
    Memory(Arc<Mutex<Connection>>),
}

pub(crate) type FileId = (u64, u64);

/// The identity of the file at `path` now, taken before a connection opens it.
pub(crate) fn identify(path: &Path) -> Option<FileId> {
    fs::metadata(path).ok().as_ref().and_then(file_id)
}

fn file_id(metadata: &fs::Metadata) -> Option<FileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

/// The key a file's writer is registered under, and the file now at `path`.
fn key(path: &Path) -> (PathBuf, Option<FileId>) {
    (
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        identify(path),
    )
}

/// Whether this process may write the file at `path`, asked without opening it: closing
/// a file outside SQLite drops this process's locks on it, and SQLite reuses a descriptor
/// it opened before the permissions changed.
fn writable(path: &Path, metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let _ = metadata;
        std::ffi::CString::new(path.as_os_str().as_bytes())
            .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        !metadata.permissions().readonly()
    }
}

/// Whether this process may create files beside `path`: a fresh connection opens a WAL
/// database read-only when it cannot create the WAL and shared-memory files, which this
/// writer's open connection keeps in place.
fn folder_writable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let folder = match path.parent() {
            Some(folder) if !folder.as_os_str().is_empty() => folder,
            _ => Path::new("."),
        };
        std::ffi::CString::new(folder.as_os_str().as_bytes()).is_ok_and(
            |folder| unsafe { libc::access(folder.as_ptr(), libc::W_OK | libc::X_OK) } == 0,
        )
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

impl Slot {
    /// Copies the WAL into the database file and empties it, as closing the last
    /// connection did, so the file is complete on its own whenever no write is queued, and
    /// a file that replaces it is never read through a WAL that belongs to this one.
    /// Returns whether it finished; it never waits for a reader, so a write is never held
    /// up by one, and an idle writer tries again.
    fn checkpoint(&self) -> bool {
        let Self::Disk {
            open: Some((connection, _)),
            ..
        } = self
        else {
            return true;
        };
        let _ = connection.busy_timeout(Duration::ZERO);
        let done = connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            })
            .is_ok_and(|busy| busy == 0);
        let _ = connection.busy_timeout(BUSY_TIMEOUT);
        done
    }

    /// Runs `f` on the connection. A disk write is refused while this process cannot write
    /// the database's folder, and the connection is reopened first when the file at its
    /// path was replaced or removed, or this process can no longer write it, so such a
    /// write fails as it would on a fresh connection. It is closed after any failure other
    /// than busy.
    pub(crate) fn run<T>(
        &mut self,
        f: impl FnOnce(&mut Connection) -> io::Result<T>,
    ) -> io::Result<T> {
        let (path, open) = match self {
            Self::Memory(connection) => {
                let mut connection = connection
                    .lock()
                    .map_err(|_| io::Error::other("SQLite connection lock poisoned"))?;
                return f(&mut connection);
            }
            Self::Disk { path, open } => (path, open),
        };
        if !folder_writable(path) {
            return Err(read_only());
        }
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            let current = fs::metadata(&*path);
            let unchanged = |opened: &Option<FileId>| match &current {
                Ok(metadata) => {
                    opened.is_some() && *opened == file_id(metadata) && writable(path, metadata)
                }
                Err(_) => false,
            };
            if open.as_ref().is_some_and(|(_, id)| !unchanged(id)) {
                *open = None;
            }
            if open.is_none() {
                let connection =
                    Connection::open_with_flags(&*path, OpenFlags::SQLITE_OPEN_READ_WRITE)
                        .map_err(sql_error)?;
                require_writable(&connection)?;
                configure(&connection, BUSY_TIMEOUT)?;
                let id = current.ok().as_ref().and_then(file_id);
                *open = Some((connection, id));
            }
            f(&mut open.as_mut().expect("opened above").0)
        }))
        .unwrap_or_else(|_| Err(io::Error::other("SQLite write stopped unexpectedly")));
        if outcome
            .as_ref()
            .is_err_and(|error| error.kind() != io::ErrorKind::WouldBlock)
        {
            *open = None;
        }
        outcome
    }
}

impl Writer {
    /// The live writer for the file at `path`, or a new one on the connection `open`
    /// returns. Opens wait for each other, so a file never gets two writers here; a file
    /// that was just created, or replaced the one a writer opened, gets its own.
    pub(crate) fn shared(
        path: &Path,
        created: bool,
        open: impl FnOnce() -> io::Result<Connection>,
    ) -> io::Result<Arc<Self>> {
        let mut writers = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(writer) = Self::find(&writers, path).filter(|_| !created) {
            return Ok(writer);
        }
        let id = identify(path);
        let writer = Self::start(path, open()?, id)?;
        let (key, _) = key(path);
        writers.retain(|_, (writer, _)| writer.strong_count() > 0);
        writers.insert(key, (Arc::downgrade(&writer), id));
        Ok(writer)
    }

    /// The live writer for the file at `path`, if this process has one.
    pub(crate) fn live(path: &Path) -> Option<Arc<Self>> {
        Self::find(&OPEN.lock().unwrap_or_else(PoisonError::into_inner), path)
    }

    fn find(
        writers: &HashMap<PathBuf, (Weak<Writer>, Option<FileId>)>,
        path: &Path,
    ) -> Option<Arc<Self>> {
        let (key, id) = key(path);
        writers
            .get(&key)
            .filter(|(_, opened)| opened.is_some() && *opened == id)
            .and_then(|(writer, _)| writer.upgrade())
    }

    /// Starts the writer for `path` with `connection`, already open and configured on it,
    /// and `id`, the identity of the file taken before it was opened.
    fn start(path: &Path, connection: Connection, id: Option<FileId>) -> io::Result<Arc<Self>> {
        let mut slot = Slot::Disk {
            path: path.to_path_buf(),
            open: Some((connection, id)),
        };
        let settled = slot.checkpoint();
        let (jobs, rx) = mpsc::channel::<Job>();
        let thread = thread::Builder::new()
            .name("keystone-sqlite-writer".into())
            .spawn(move || {
                let mut next = None;
                let mut unsettled = !settled;
                loop {
                    let job = match next.take() {
                        Some(job) => job,
                        None if unsettled => match rx.recv_timeout(SETTLE_RETRY) {
                            Ok(job) => job,
                            Err(RecvTimeoutError::Timeout) => {
                                unsettled = !slot.checkpoint();
                                continue;
                            }
                            Err(RecvTimeoutError::Disconnected) => return,
                        },
                        None => match rx.recv() {
                            Ok(job) => job,
                            Err(_) => return,
                        },
                    };
                    let reply = job(&mut slot);
                    match rx.try_recv() {
                        Ok(job) => {
                            next = Some(job);
                            unsettled = true;
                        }
                        Err(_) => unsettled = !slot.checkpoint(),
                    }
                    reply();
                }
            })?;
        Ok(Arc::new(Self {
            id: thread.thread().id(),
            jobs: Some(jobs),
            thread: Some(thread),
        }))
    }

    /// Queues `job` after every job queued before it, without waiting.
    pub(crate) fn submit(
        &self,
        job: impl FnOnce(&mut Slot) -> Reply + Send + 'static,
    ) -> io::Result<()> {
        self.jobs
            .as_ref()
            .and_then(|jobs| jobs.send(Box::new(job)).ok())
            .ok_or_else(|| io::Error::other("SQLite writer has stopped"))
    }

    /// Waits for every job queued before this call.
    pub(crate) fn flush(&self) {
        debug_assert_ne!(thread::current().id(), self.id, "a write job cannot wait");
        let (done, finished) = mpsc::sync_channel(1);
        let queued = self.submit(move |_| {
            Box::new(move || {
                let _ = done.send(());
            })
        });
        if queued.is_ok() {
            let _ = finished.recv();
        }
    }

    /// Runs `f` on the writer's connection after every write queued before it, and returns
    /// its result. Waits up to [`WRITER_WAIT`] for `f` to start; one that has not
    /// started by then is withdrawn and reported busy, so it never runs later.
    pub(crate) fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        debug_assert_ne!(thread::current().id(), self.id, "a write job cannot write");
        let (reply, result) = mpsc::sync_channel(1);
        let claimed = Arc::new(AtomicBool::new(false));
        let claim = claimed.clone();
        self.submit(move |slot| {
            if claim.swap(true, Ordering::AcqRel) {
                return Box::new(|| ());
            }
            let outcome = slot.run(f);
            Box::new(move || {
                let _ = reply.send(outcome);
            })
        })?;
        let stopped = || io::Error::other("SQLite writer has stopped");
        match result.recv_timeout(WRITER_WAIT) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) if !claimed.swap(true, Ordering::AcqRel) => {
                Err(io::Error::new(io::ErrorKind::WouldBlock, BUSY))
            }
            Err(RecvTimeoutError::Timeout) => result.recv().unwrap_or_else(|_| Err(stopped())),
            Err(RecvTimeoutError::Disconnected) => Err(stopped()),
        }
    }
}

impl Drop for Writer {
    /// Finishes the queued writes and closes the connection, unless the last handle is
    /// dropped by a write job itself. A write stuck in a filesystem call holds this up
    /// until it returns.
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != thread::current().id()
        {
            let _ = thread.join();
        }
    }
}
