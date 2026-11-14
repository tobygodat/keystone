use crate::writer::{Reply, Slot, Writer};
use crate::{AccountUsage, Durability, Snapshot, Workspace, invalid, validate};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where a copy is remembered: by its copy, and by its project when either end is a
/// `Local` server, which every project keeps in the same file.
fn copy_key(pair: &crate::integrations::copy::Pair) -> io::Result<String> {
    let local = [&pair.from, &pair.to]
        .iter()
        .any(|end| end.scope == crate::integrations::Scope::Local);
    let project = local.then_some(&pair.project);
    Ok(format!(
        "{COPY_PREFIX}{}",
        serde_json::to_string(&(&pair.to, project))?
    ))
}

pub(crate) const SCHEMA_VERSION: u32 = 8;
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_millis(100);
/// The `metadata` keys that hold each account's usage readings.
const USAGE_PREFIX: &str = "usage/";
/// The `metadata` keys that hold each folder's unfinished commit message.
const COMMIT_DRAFT_PREFIX: &str = "commit_draft/";
/// The `metadata` keys that hold the address each folder's browser page was on.
const BROWSER_PAGE_PREFIX: &str = "browser_page/";
/// The `metadata` keys that hold each copy Settings → Integrations made, by its copy.
const COPY_PREFIX: &str = "integration_copy/";
/// The `metadata` keys that hold each scheduled task, by its ID.
const SCHEDULED_TASK_PREFIX: &str = "scheduled_task/";
const SCHEMA: &str = "
CREATE TABLE events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    event_key TEXT UNIQUE
);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;
CREATE TABLE workspace (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    revision INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    position INTEGER NOT NULL UNIQUE,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
CREATE TABLE log_entries (
    sequence INTEGER PRIMARY KEY REFERENCES events(sequence),
    session_id TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
CREATE INDEX log_entries_session ON log_entries(session_id, sequence);
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";
/// Forward migrations, one per schema version after 1, each run in the opening
/// IMMEDIATE transaction that also records the new `user_version`. A build that
/// knows fewer versions refuses a newer database.
const MIGRATIONS: [&str; 7] = [
    "
CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
",
    "
CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
",
    "
CREATE TABLE prompts (
    id TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
",
    "
CREATE TABLE deliveries (
    child TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
",
    "
CREATE TABLE notes (
    project TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
",
    "
CREATE TABLE machines (
    id TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
",
    "
CREATE TABLE remote_cursors (
    session_id TEXT PRIMARY KEY,
    journal TEXT NOT NULL,
    seq INTEGER NOT NULL
);
",
];

#[derive(Clone)]
pub struct Database {
    path: PathBuf,
    memory: Option<Arc<Mutex<Connection>>>,
    read_only: bool,
    writer: Option<Arc<Writer>>,
}

#[derive(Debug)]
pub struct StoredEvent {
    pub sequence: u64,
    pub kind: String,
    pub entity_id: String,
    pub payload: String,
}

pub(crate) fn sql_error(error: rusqlite::Error) -> io::Error {
    let kind = match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            io::ErrorKind::WouldBlock
        }
        Some(rusqlite::ErrorCode::DiskFull) => io::ErrorKind::StorageFull,
        Some(rusqlite::ErrorCode::ReadOnly) => {
            return io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{error}; check write permissions on the database, its -wal/-shm files and parent directory"
                ),
            );
        }
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error)
}

pub(crate) fn configure(connection: &Connection, busy_timeout: Duration) -> io::Result<()> {
    connection.busy_timeout(busy_timeout).map_err(sql_error)?;
    connection
        .execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")
        .map_err(sql_error)
}

pub fn database_path(path: &Path) -> PathBuf {
    let mut header = [0; 16];
    let count = fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .and_then(|_| File::open(path).ok())
        .and_then(|mut file| file.read(&mut header).ok())
        .unwrap_or(0);
    if count == header.len() && &header == b"SQLite format 3\0" {
        return path.to_path_buf();
    }
    let mut first = header[..count]
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace());
    if first.is_none() && count > 0 {
        if let Ok(file) = File::open(path) {
            first = BufReader::new(file).bytes().find_map(|byte| match byte {
                Ok(byte) if byte.is_ascii_whitespace() => None,
                Ok(byte) => Some(byte),
                Err(_) => Some(0),
            });
        }
    }
    let mut name = path.as_os_str().to_os_string();
    name.push(".sqlite3");
    let sibling = PathBuf::from(name);
    if path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        path.with_extension("sqlite3")
    } else if path
        .extension()
        .is_some_and(|extension| matches!(extension.to_str(), Some("sqlite3" | "sqlite" | "db")))
        && first != Some(b'{')
        && !(fs::metadata(path).map_or_else(
            |error| error.kind() == io::ErrorKind::NotFound,
            |metadata| metadata.len() == 0,
        ) && sibling.exists())
    {
        path.to_path_buf()
    } else {
        sibling
    }
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let supplied = path.as_ref();
        let path = database_path(supplied);
        let legacy = if path == supplied {
            path.with_extension("json")
        } else {
            supplied.to_path_buf()
        };
        let mut file = fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file.mode(0o600);
        }
        let created = match file.open(&path) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error),
        };
        let writer = Writer::shared(&path, created, || {
            let mut connection = Connection::open(&path).map_err(sql_error)?;
            require_writable(&connection)?;
            configure(&connection, BUSY_TIMEOUT)?;
            let version: u32 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(sql_error)?;
            if version > SCHEMA_VERSION {
                return Err(invalid(format!(
                    "unsupported SQLite schema version {version} (expected at most {SCHEMA_VERSION})"
                )));
            }
            let journal: String = connection
                .query_one("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .map_err(sql_error)?;
            if journal != "wal" {
                return Err(io::Error::other("SQLite WAL mode is required"));
            }
            let initialized = version == SCHEMA_VERSION
                && connection
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='legacy_import_complete')",
                        [],
                        |row| row.get::<_, bool>(0),
                    )
                    .map_err(sql_error)?;
            if !initialized {
                Self::initialize(&mut connection, Some(&legacy))?;
            }
            Ok(connection)
        })?;
        File::open(
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;
        Ok(Self {
            path,
            memory: None,
            read_only: false,
            writer: Some(writer),
        })
    }

    pub fn open_read_only(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = database_path(path.as_ref());
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sql_error)?;
        configure(&connection, BUSY_TIMEOUT)?;
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql_error)?;
        if version != SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported SQLite schema version {version}"
            )));
        }
        let initialized: bool = connection
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='legacy_import_complete')",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if !initialized {
            return Err(invalid("SQLite workspace import is incomplete"));
        }
        Ok(Self {
            writer: Writer::live(&path),
            path,
            memory: None,
            read_only: true,
        })
    }

    pub fn in_memory() -> io::Result<Self> {
        let mut connection = Connection::open_in_memory().map_err(sql_error)?;
        configure(&connection, BUSY_TIMEOUT)?;
        Self::initialize(&mut connection, None)?;
        Ok(Self {
            path: PathBuf::from(":memory:"),
            memory: Some(Arc::new(Mutex::new(connection))),
            read_only: false,
            writer: None,
        })
    }

    fn initialize(connection: &mut Connection, legacy: Option<&Path>) -> io::Result<()> {
        Self::prepare_import(connection, legacy)?
            .commit()
            .map_err(sql_error)
    }

    fn prepare_import<'a>(
        connection: &'a mut Connection,
        legacy: Option<&Path>,
    ) -> io::Result<rusqlite::Transaction<'a>> {
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let version: u32 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql_error)?;
        if version > SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported SQLite schema version {version}"
            )));
        }
        if version == 0 {
            tx.execute_batch(SCHEMA).map_err(sql_error)?;
        }
        for migration in &MIGRATIONS[version.saturating_sub(1) as usize..] {
            tx.execute_batch(migration).map_err(sql_error)?;
        }
        if version != SCHEMA_VERSION {
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(sql_error)?;
        }
        let imported: bool = tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='legacy_import_complete')",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if !imported {
            let snapshot = match legacy.map(File::open).transpose() {
                Ok(Some(file)) => serde_json::from_reader(file)?,
                Ok(None) => empty_snapshot(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => empty_snapshot(),
                Err(error) => return Err(error),
            };
            validate(&snapshot).map_err(|error| invalid(error.to_string()))?;
            write_projection(&tx, &snapshot, 0, None)?;
            if let Some(legacy) = legacy {
                import_logs(
                    &tx,
                    legacy.parent().unwrap_or(Path::new(".")).join("sessions"),
                )?;
            }
            tx.execute(
                "INSERT INTO metadata VALUES ('legacy_import_complete', '1')",
                [],
            )
            .map_err(sql_error)?;
        }
        Ok(tx)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn is_memory(&self) -> bool {
        self.memory.is_some()
    }

    fn memory<T>(
        memory: &Mutex<Connection>,
        f: impl FnOnce(&mut Connection) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut connection = memory
            .lock()
            .map_err(|_| io::Error::other("SQLite connection lock poisoned"))?;
        f(&mut connection)
    }

    /// Waits for the writes this process queued before it, then runs `f` on an
    /// independent read-only connection, so a read sees every save made before it.
    fn read<T>(&self, f: impl FnOnce(&mut Connection) -> io::Result<T>) -> io::Result<T> {
        if let Some(memory) = &self.memory {
            return Self::memory(memory, f);
        }
        self.flush();
        let mut connection =
            Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(sql_error)?;
        configure(&connection, BUSY_TIMEOUT)?;
        f(&mut connection)
    }

    /// Runs `f` on the writer's connection, after the writes queued before it.
    fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        if let Some(memory) = &self.memory {
            return Self::memory(memory, f);
        }
        match &self.writer {
            Some(writer) if !self.read_only => writer.write(f),
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SQLite database is read-only",
            )),
        }
    }

    /// Runs `job` on the write connection after every write queued before it, without
    /// waiting: on the writer's thread, or at once for an in-memory database.
    pub(crate) fn submit(
        &self,
        job: impl FnOnce(&mut Slot) -> Reply + Send + 'static,
    ) -> io::Result<()> {
        if let Some(memory) = &self.memory {
            job(&mut Slot::Memory(memory.clone()))();
            return Ok(());
        }
        match &self.writer {
            Some(writer) if !self.read_only => writer.submit(job),
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SQLite database is read-only",
            )),
        }
    }

    /// Waits for every write this process queued on the database before this call.
    pub(crate) fn flush(&self) {
        if let Some(writer) = &self.writer {
            writer.flush();
        }
    }

    pub(crate) fn snapshot(&self) -> io::Result<(Snapshot, i64)> {
        self.read(|connection| {
            let tx = connection.transaction().map_err(sql_error)?;
            let (payload, revision): (String, i64) = tx
                .query_one(
                    "SELECT payload, revision FROM workspace WHERE singleton=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(sql_error)?;
            let mut snapshot: Snapshot = serde_json::from_str(&payload)?;
            let mut statement = tx
                .prepare("SELECT payload FROM sessions ORDER BY position")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            for row in rows {
                snapshot
                    .workspace
                    .sessions
                    .push(serde_json::from_str(&row.map_err(sql_error)?)?);
            }
            let mut statement = tx
                .prepare("SELECT payload FROM accounts ORDER BY position")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            for row in rows {
                snapshot
                    .workspace
                    .accounts
                    .push(serde_json::from_str(&row.map_err(sql_error)?)?);
            }
            let mut statement = tx
                .prepare("SELECT payload FROM machines ORDER BY position")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            for row in rows {
                snapshot
                    .workspace
                    .machines
                    .push(serde_json::from_str(&row.map_err(sql_error)?)?);
            }
            let mut statement = tx
                .prepare("SELECT payload FROM projects ORDER BY position")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            for row in rows {
                snapshot
                    .workspace
                    .projects
                    .push(serde_json::from_str(&row.map_err(sql_error)?)?);
            }
            let mut statement = tx
                .prepare("SELECT payload FROM prompts ORDER BY position")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            for row in rows {
                snapshot
                    .workspace
                    .prompts
                    .push(serde_json::from_str(&row.map_err(sql_error)?)?);
            }
            let mut statement = tx
                .prepare("SELECT payload FROM deliveries ORDER BY position")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            for row in rows {
                snapshot
                    .workspace
                    .deliveries
                    .push(serde_json::from_str(&row.map_err(sql_error)?)?);
            }
            let mut statement = tx
                .prepare("SELECT payload FROM notes ORDER BY position")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            for row in rows {
                snapshot
                    .workspace
                    .notes
                    .push(serde_json::from_str(&row.map_err(sql_error)?)?);
            }
            let mut statement = tx
                .prepare("SELECT key, value FROM metadata WHERE substr(key, 1, length(?1)) = ?1")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([COMMIT_DRAFT_PREFIX], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(sql_error)?;
            for row in rows {
                let (key, text) = row.map_err(sql_error)?;
                if let Some(folder) = key.strip_prefix(COMMIT_DRAFT_PREFIX) {
                    snapshot
                        .workspace
                        .commit_drafts
                        .insert(folder.to_owned(), text);
                }
            }
            let mut statement = tx
                .prepare("SELECT key, value FROM metadata WHERE substr(key, 1, length(?1)) = ?1")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([BROWSER_PAGE_PREFIX], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(sql_error)?;
            for row in rows {
                let (key, url) = row.map_err(sql_error)?;
                if let Some(folder) = key.strip_prefix(BROWSER_PAGE_PREFIX) {
                    snapshot
                        .workspace
                        .browser_pages
                        .insert(folder.to_owned(), url);
                }
            }
            let mut statement = tx
                .prepare("SELECT value FROM metadata WHERE substr(key, 1, length(?1)) = ?1")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([SCHEDULED_TASK_PREFIX], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            for row in rows {
                if let Ok(task) = serde_json::from_str(&row.map_err(sql_error)?) {
                    snapshot.workspace.scheduled_tasks.push(task);
                }
            }
            snapshot
                .workspace
                .scheduled_tasks
                .sort_by_key(crate::ScheduledTask::id);
            validate(&snapshot).map_err(|error| invalid(error.to_string()))?;
            Ok((snapshot, revision))
        })
    }

    /// Appends a session's log records in one transaction. With `through`, the
    /// session is on another machine and the records hold every row its events up to
    /// that place added: the journal ID and the number in it. Its cursor becomes that
    /// place in the same transaction, never ahead of the rows. Places come in the order
    /// the session's events were applied, so the newest replaces the last, including
    /// one in another journal after the machine's journal was replaced.
    pub fn append_logs(
        &self,
        session: u64,
        records: &[(String, String)],
        through: Option<(&str, u64)>,
    ) -> io::Result<Durability> {
        let records = records.to_vec();
        let through = through.map(|(journal, seq)| (journal.to_owned(), seq));
        self.write(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let session = session.to_string();
            for (key, payload) in &records {
                let existing: Option<(String, String, String)> = tx.query_row("SELECT kind, entity_id, payload FROM events WHERE event_key=?1", [key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional().map_err(sql_error)?;
                if let Some(existing) = existing {
                    if existing != ("log.appended".into(), session.clone(), payload.clone()) {
                        return Err(invalid("log event key reused for a different entry"));
                    }
                    continue;
                }
                tx.execute("INSERT INTO events(kind, entity_id, payload, event_key) VALUES ('log.appended', ?1, ?2, ?3)", params![session, payload, key]).map_err(sql_error)?;
                tx.execute("INSERT INTO log_entries(sequence, session_id, payload) VALUES (?1, ?2, ?3)", params![tx.last_insert_rowid(), session, payload]).map_err(sql_error)?;
            }
            if let Some((journal, seq)) = &through {
                tx.execute(
                    "INSERT INTO remote_cursors(session_id, journal, seq) VALUES (?1, ?2, ?3)
                     ON CONFLICT(session_id) DO UPDATE SET journal = excluded.journal, seq = excluded.seq",
                    params![session, journal, (*seq).min(i64::MAX as u64) as i64],
                )
                .map_err(sql_error)?;
            }
            tx.commit().map_err(sql_error)
        })?;
        Ok(match self.sync_directory() {
            Ok(()) => Durability::Synced,
            Err(error) => Durability::DirectorySyncPending(error.to_string()),
        })
    }

    /// Whether a log entry is saved under each of `keys`.
    pub fn logs_saved<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> io::Result<bool> {
        self.read(|connection| {
            let mut statement = connection
                .prepare("SELECT 1 FROM events WHERE event_key=?1 AND kind='log.appended'")
                .map_err(sql_error)?;
            for key in keys {
                if !statement.exists([key]).map_err(sql_error)? {
                    return Ok(false);
                }
            }
            Ok(true)
        })
    }

    /// Each session on another machine with its saved cursor: the host journal it
    /// counts in and the place there through which every row its events added is
    /// saved.
    pub fn remote_cursors(&self) -> io::Result<Vec<(u64, String, u64)>> {
        self.read(|connection| {
            let mut statement = connection
                .prepare("SELECT session_id, journal, seq FROM remote_cursors")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(sql_error)?;
            let mut cursors = Vec::new();
            for row in rows {
                let (session, journal, seq) = row.map_err(sql_error)?;
                if let Ok(session) = session.parse() {
                    cursors.push((session, journal, seq.max(0) as u64));
                }
            }
            Ok(cursors)
        })
    }

    pub fn read_logs(&self, session: u64) -> io::Result<Vec<String>> {
        self.read(|connection| {
            let mut statement = connection
                .prepare("SELECT payload FROM log_entries WHERE session_id=?1 ORDER BY sequence")
                .map_err(sql_error)?;
            statement
                .query_map([session.to_string()], |row| row.get(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)
        })
    }

    /// Calls `visit` with every saved log entry of every session, newest first: the
    /// session's ID and the entry's payload. Stops early when `visit` returns `false`.
    /// It runs on a read-only connection of its own, so writes go on meanwhile.
    pub fn scan_logs(&self, mut visit: impl FnMut(u64, &str) -> bool) -> io::Result<()> {
        self.read(|connection| {
            let mut statement = connection
                .prepare("SELECT session_id, payload FROM log_entries ORDER BY sequence DESC")
                .map_err(sql_error)?;
            let mut rows = statement.query([]).map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                let session = row
                    .get_ref(0)
                    .map_err(sql_error)?
                    .as_str()
                    .map_err(|error| invalid(error.to_string()))?;
                let Ok(session) = session.parse() else {
                    continue;
                };
                let payload = row
                    .get_ref(1)
                    .map_err(sql_error)?
                    .as_str()
                    .map_err(|error| invalid(error.to_string()))?;
                if !visit(session, payload) {
                    break;
                }
            }
            Ok(())
        })
    }

    /// The name this workspace's checkpoints are saved under in each repository:
    /// `fresh` the first time it is asked for, and the same name from then on.
    pub fn checkpoint_store(&self, fresh: &str) -> io::Result<String> {
        let fresh = fresh.to_owned();
        self.write(move |connection| {
            connection
                .execute(
                    "INSERT OR IGNORE INTO metadata(key, value) VALUES ('checkpoint_store', ?1)",
                    [&fresh],
                )
                .map_err(sql_error)?;
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='checkpoint_store'",
                    [],
                    |row| row.get(0),
                )
                .map_err(sql_error)
        })
    }

    /// Saves an account's usage readings under `account`: a `usage.recorded` event
    /// and its projection in `metadata`, which older builds leave alone. Saved windows
    /// `usage` does not carry are kept, so a reading this process never loaded is not
    /// lost.
    pub fn record_usage(&self, account: &str, usage: &AccountUsage) -> io::Result<()> {
        let (account, usage) = (account.to_owned(), usage.clone());
        self.write(move |connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let key = format!("{USAGE_PREFIX}{account}");
            let saved: Option<String> = tx
                .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |row| row.get(0))
                .optional()
                .map_err(sql_error)?;
            let mut merged = saved
                .and_then(|saved| serde_json::from_str::<AccountUsage>(&saved).ok())
                .unwrap_or_default();
            for reading in &usage.readings {
                match merged
                    .readings
                    .iter_mut()
                    .find(|old| old.window.id == reading.window.id)
                {
                    Some(old) => *old = reading.clone(),
                    None => merged.readings.push(reading.clone()),
                }
            }
            if let Some(newest) = usage.readings.iter().map(|r| r.observed_at).max() {
                merged.prune(newest);
            }
            let payload = serde_json::to_string(&merged)?;
            tx.execute(
                "INSERT INTO events(kind, entity_id, payload) VALUES ('usage.recorded', ?1, ?2)",
                params![account, payload],
            )
            .map_err(sql_error)?;
            tx.execute(
                "INSERT INTO metadata(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, payload],
            )
            .map_err(sql_error)?;
            tx.commit().map_err(sql_error)
        })
    }

    /// Every account's saved usage, by the key [`Database::record_usage`] took. A
    /// reading that no longer parses is skipped.
    pub fn usage(&self) -> io::Result<Vec<(String, AccountUsage)>> {
        self.read(|connection| {
            let mut statement = connection
                .prepare("SELECT key, value FROM metadata WHERE substr(key, 1, length(?1)) = ?1 ORDER BY key")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([USAGE_PREFIX], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            Ok(rows
                .into_iter()
                .filter_map(|(key, value)| {
                    Some((
                        key.strip_prefix(USAGE_PREFIX)?.to_owned(),
                        serde_json::from_str(&value).ok()?,
                    ))
                })
                .collect())
        })
    }

    /// Remembers a copy of an MCP server or skill: an `integration.copied` event and
    /// its projection in `metadata`, keyed by the copy, so copying over it again
    /// replaces it.
    pub fn record_copy(&self, pair: &crate::integrations::copy::Pair) -> io::Result<()> {
        let key = copy_key(pair)?;
        let payload = serde_json::to_string(pair)?;
        self.write(move |connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            tx.execute(
                "INSERT INTO events(kind, entity_id, payload) VALUES ('integration.copied', ?1, ?2)",
                params![key, payload],
            )
            .map_err(sql_error)?;
            tx.execute(
                "INSERT INTO metadata(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, payload],
            )
            .map_err(sql_error)?;
            tx.commit().map_err(sql_error)
        })
    }

    /// Forgets a copy [`Database::record_copy`] remembered.
    pub fn forget_copy(&self, pair: &crate::integrations::copy::Pair) -> io::Result<()> {
        let key = copy_key(pair)?;
        self.write(move |connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            tx.execute(
                "INSERT INTO events(kind, entity_id, payload) VALUES ('integration.copy_forgotten', ?1, '{}')",
                [&key],
            )
            .map_err(sql_error)?;
            tx.execute("DELETE FROM metadata WHERE key=?1", [&key])
                .map_err(sql_error)?;
            tx.commit().map_err(sql_error)
        })
    }

    /// Every copy [`Database::record_copy`] remembered. One that no longer parses is
    /// skipped.
    pub fn copies(&self) -> io::Result<Vec<crate::integrations::copy::Pair>> {
        self.read(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT value FROM metadata WHERE substr(key, 1, length(?1)) = ?1 ORDER BY key",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map([COPY_PREFIX], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            Ok(rows
                .iter()
                .filter_map(|value| serde_json::from_str(value).ok())
                .collect())
        })
    }

    pub fn events_after(&self, sequence: u64, limit: u32) -> io::Result<Vec<StoredEvent>> {
        let sequence = i64::try_from(sequence)
            .map_err(|_| invalid("event cursor exceeds SQLite sequence range"))?;
        self.read(|connection| {
            let mut statement = connection.prepare("SELECT sequence, kind, entity_id, payload FROM events WHERE sequence>?1 ORDER BY sequence LIMIT ?2").map_err(sql_error)?;
            statement.query_map(params![sequence, limit.min(1000)], |row| Ok(StoredEvent { sequence: row.get::<_, i64>(0)? as u64, kind: row.get(1)?, entity_id: row.get(2)?, payload: row.get(3)? })).map_err(sql_error)?.collect::<Result<Vec<_>, _>>().map_err(sql_error)
        })
    }

    pub fn sync_directory(&self) -> io::Result<()> {
        if self.memory.is_none() {
            File::open(
                self.path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?
            .sync_all()?;
        }
        Ok(())
    }
}

/// The database holds a workspace revision this store did not commit: another writer
/// changed it, so no commit from this store can follow.
#[derive(Debug)]
pub(crate) struct Stale;

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The workspace changed in another writer. Reload Keystone before saving.")
    }
}

impl std::error::Error for Stale {}

/// Commits `snapshot` over `previous` as the revision after `revision`, and returns
/// that revision. Fails with [`Stale`], without writing, when the database holds another
/// revision.
pub(crate) fn commit(
    connection: &mut Connection,
    snapshot: &Snapshot,
    previous: &Workspace,
    revision: i64,
) -> io::Result<i64> {
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let actual: i64 = tx
        .query_one(
            "SELECT revision FROM workspace WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if actual != revision {
        return Err(io::Error::other(Stale));
    }
    let next = revision
        .checked_add(1)
        .ok_or_else(|| invalid("workspace revision exhausted"))?;
    write_projection(&tx, snapshot, next, Some(previous))?;
    tx.commit().map_err(sql_error)?;
    Ok(next)
}

pub(crate) fn require_writable(connection: &Connection) -> io::Result<()> {
    if connection.is_readonly("main").map_err(sql_error)? {
        return Err(read_only());
    }
    Ok(())
}

pub(crate) fn read_only() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "SQLite workspace is read-only; restore write permission before retrying",
    )
}

fn empty_snapshot() -> Snapshot {
    Snapshot {
        version: crate::FORMAT_VERSION,
        next_id: 1,
        workspace: Workspace::default(),
        unknown: Map::new(),
    }
}

fn write_projection(
    tx: &rusqlite::Transaction<'_>,
    snapshot: &Snapshot,
    revision: i64,
    previous: Option<&Workspace>,
) -> io::Result<()> {
    for (position, session) in snapshot.workspace.sessions.iter().enumerate() {
        if previous.is_some_and(|workspace| workspace.sessions.get(position) == Some(session)) {
            continue;
        }
        let payload = serde_json::to_string(session)?;
        let id = session.id.to_string();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('session.upserted', ?1, ?2)",
            params![id, payload],
        )
        .map_err(sql_error)?;
        tx.execute("INSERT INTO sessions(id, position, payload) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET position=excluded.position, payload=excluded.payload", params![id, position as i64, payload]).map_err(sql_error)?;
    }
    let accounts = &snapshot.workspace.accounts;
    for removed in previous
        .into_iter()
        .flat_map(|workspace| &workspace.accounts)
        .filter(|old| !accounts.iter().any(|account| account.id() == old.id()))
    {
        let id = removed.id().as_str();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('account.removed', ?1, ?2)",
            params![id, serde_json::to_string(removed)?],
        )
        .map_err(sql_error)?;
        tx.execute("DELETE FROM accounts WHERE id=?1", [id])
            .map_err(sql_error)?;
        clear_usage(tx, id)?;
    }
    for (position, account) in accounts.iter().enumerate() {
        if previous.is_some_and(|workspace| workspace.accounts.get(position) == Some(account)) {
            continue;
        }
        // A new account may reuse a removed one's ID; it starts with no readings.
        if previous.is_some_and(|workspace| {
            !workspace
                .accounts
                .iter()
                .any(|old| old.id() == account.id())
        }) {
            clear_usage(tx, account.id().as_str())?;
        }
        let payload = serde_json::to_string(account)?;
        let id = account.id().as_str();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('account.upserted', ?1, ?2)",
            params![id, payload],
        )
        .map_err(sql_error)?;
        tx.execute("INSERT INTO accounts(id, position, payload) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET position=excluded.position, payload=excluded.payload", params![id, position as i64, payload]).map_err(sql_error)?;
    }
    let machines = &snapshot.workspace.machines;
    for removed in previous
        .into_iter()
        .flat_map(|workspace| &workspace.machines)
        .filter(|old| !machines.iter().any(|machine| machine.id() == old.id()))
    {
        let id = removed.id().as_str();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('machine.removed', ?1, ?2)",
            params![id, serde_json::to_string(removed)?],
        )
        .map_err(sql_error)?;
        tx.execute("DELETE FROM machines WHERE id=?1", [id])
            .map_err(sql_error)?;
    }
    for (position, machine) in machines.iter().enumerate() {
        if previous.is_some_and(|workspace| workspace.machines.get(position) == Some(machine)) {
            continue;
        }
        let payload = serde_json::to_string(machine)?;
        let id = machine.id().as_str();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('machine.upserted', ?1, ?2)",
            params![id, payload],
        )
        .map_err(sql_error)?;
        tx.execute("INSERT INTO machines(id, position, payload) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET position=excluded.position, payload=excluded.payload", params![id, position as i64, payload]).map_err(sql_error)?;
    }
    let projects = &snapshot.workspace.projects;
    for removed in previous
        .into_iter()
        .flat_map(|workspace| &workspace.projects)
        .filter(|old| !projects.iter().any(|project| project.id() == old.id()))
    {
        let id = removed.id();
        let id = id.path().to_string_lossy();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('project.removed', ?1, ?2)",
            params![id, serde_json::to_string(removed)?],
        )
        .map_err(sql_error)?;
        tx.execute("DELETE FROM projects WHERE id=?1", [id])
            .map_err(sql_error)?;
    }
    for (position, project) in projects.iter().enumerate() {
        if previous.is_some_and(|workspace| workspace.projects.get(position) == Some(project)) {
            continue;
        }
        let payload = serde_json::to_string(project)?;
        let id = project.id();
        let id = id.path().to_string_lossy();
        let kind = if project.removed() {
            "project.removed"
        } else {
            "project.upserted"
        };
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES (?1, ?2, ?3)",
            params![kind, id, payload],
        )
        .map_err(sql_error)?;
        tx.execute("INSERT INTO projects(id, position, payload) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET position=excluded.position, payload=excluded.payload", params![id, position as i64, payload]).map_err(sql_error)?;
    }
    let prompts = &snapshot.workspace.prompts;
    for removed in previous
        .into_iter()
        .flat_map(|workspace| &workspace.prompts)
        .filter(|old| !prompts.iter().any(|prompt| prompt.id() == old.id()))
    {
        let id = removed.id().to_string();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('prompt.removed', ?1, ?2)",
            params![id, serde_json::to_string(removed)?],
        )
        .map_err(sql_error)?;
        tx.execute("DELETE FROM prompts WHERE id=?1", [id])
            .map_err(sql_error)?;
    }
    for (position, prompt) in prompts.iter().enumerate() {
        if previous.is_some_and(|workspace| workspace.prompts.get(position) == Some(prompt)) {
            continue;
        }
        let payload = serde_json::to_string(prompt)?;
        let id = prompt.id().to_string();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('prompt.upserted', ?1, ?2)",
            params![id, payload],
        )
        .map_err(sql_error)?;
        tx.execute("INSERT INTO prompts(id, position, payload) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET position=excluded.position, payload=excluded.payload", params![id, position as i64, payload]).map_err(sql_error)?;
    }
    // Deliveries are only added and changed, never removed.
    for (position, delivery) in snapshot.workspace.deliveries.iter().enumerate() {
        if previous.is_some_and(|workspace| workspace.deliveries.get(position) == Some(delivery)) {
            continue;
        }
        let payload = serde_json::to_string(delivery)?;
        let child = delivery.child().to_string();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('delivery.upserted', ?1, ?2)",
            params![child, payload],
        )
        .map_err(sql_error)?;
        tx.execute("INSERT INTO deliveries(child, position, payload) VALUES (?1, ?2, ?3) ON CONFLICT(child) DO UPDATE SET position=excluded.position, payload=excluded.payload", params![child, position as i64, payload]).map_err(sql_error)?;
    }
    let notes = &snapshot.workspace.notes;
    for removed in previous
        .into_iter()
        .flat_map(|workspace| &workspace.notes)
        .filter(|old| !notes.iter().any(|note| note.project() == old.project()))
    {
        let project = removed.project();
        let project = project.path().to_string_lossy();
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('note.removed', ?1, ?2)",
            params![project, serde_json::to_string(removed)?],
        )
        .map_err(sql_error)?;
        tx.execute("DELETE FROM notes WHERE project=?1", [project])
            .map_err(sql_error)?;
    }
    for (position, note) in notes.iter().enumerate() {
        if previous.is_some_and(|workspace| workspace.notes.get(position) == Some(note)) {
            continue;
        }
        let project = note.project();
        let project = project.path().to_string_lossy();
        // A note that only moved up, as another was emptied, keeps its text and needs
        // no event.
        if previous.is_some_and(|workspace| workspace.notes.contains(note)) {
            tx.execute(
                "UPDATE notes SET position=?1 WHERE project=?2",
                params![position as i64, project],
            )
            .map_err(sql_error)?;
            continue;
        }
        let payload = serde_json::to_string(note)?;
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('note.upserted', ?1, ?2)",
            params![project, payload],
        )
        .map_err(sql_error)?;
        tx.execute("INSERT INTO notes(project, position, payload) VALUES (?1, ?2, ?3) ON CONFLICT(project) DO UPDATE SET position=excluded.position, payload=excluded.payload", params![project, position as i64, payload]).map_err(sql_error)?;
    }
    let drafts = &snapshot.workspace.commit_drafts;
    let saved = previous.map(|workspace| &workspace.commit_drafts);
    for folder in saved
        .into_iter()
        .flat_map(|saved| saved.keys())
        .filter(|folder| !drafts.contains_key(*folder))
    {
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('commit_draft.cleared', ?1, '\"\"')",
            [folder],
        )
        .map_err(sql_error)?;
        tx.execute(
            "DELETE FROM metadata WHERE key=?1",
            [format!("{COMMIT_DRAFT_PREFIX}{folder}")],
        )
        .map_err(sql_error)?;
    }
    for (folder, text) in drafts {
        if saved.and_then(|saved| saved.get(folder)) == Some(text) {
            continue;
        }
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('commit_draft.saved', ?1, ?2)",
            params![folder, serde_json::to_string(text)?],
        )
        .map_err(sql_error)?;
        tx.execute(
            "INSERT INTO metadata(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![format!("{COMMIT_DRAFT_PREFIX}{folder}"), text],
        )
        .map_err(sql_error)?;
    }
    let pages = &snapshot.workspace.browser_pages;
    let saved = previous.map(|workspace| &workspace.browser_pages);
    for (folder, url) in pages {
        if saved.and_then(|saved| saved.get(folder)) == Some(url) {
            continue;
        }
        tx.execute(
            "INSERT INTO events(kind, entity_id, payload) VALUES ('browser_page.saved', ?1, ?2)",
            params![folder, serde_json::to_string(url)?],
        )
        .map_err(sql_error)?;
        tx.execute(
            "INSERT INTO metadata(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![format!("{BROWSER_PAGE_PREFIX}{folder}"), url],
        )
        .map_err(sql_error)?;
    }
    let tasks = &snapshot.workspace.scheduled_tasks;
    for removed in previous
        .into_iter()
        .flat_map(|workspace| &workspace.scheduled_tasks)
        .filter(|old| !tasks.iter().any(|task| task.id() == old.id()))
    {
        let id = removed.id().to_string();
        tx.execute("INSERT INTO events(kind, entity_id, payload) VALUES ('scheduled_task.removed', ?1, '{}')", [&id]).map_err(sql_error)?;
        tx.execute(
            "DELETE FROM metadata WHERE key=?1",
            [format!("{SCHEDULED_TASK_PREFIX}{id}")],
        )
        .map_err(sql_error)?;
    }
    for task in tasks {
        if previous.is_some_and(|workspace| workspace.scheduled_tasks.iter().any(|old| old == task))
        {
            continue;
        }
        let id = task.id().to_string();
        let payload = serde_json::to_string(task)?;
        tx.execute("INSERT INTO events(kind, entity_id, payload) VALUES ('scheduled_task.upserted', ?1, ?2)", params![id, payload]).map_err(sql_error)?;
        tx.execute("INSERT INTO metadata(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![format!("{SCHEDULED_TASK_PREFIX}{id}"), payload]).map_err(sql_error)?;
    }
    let mut metadata = Snapshot {
        version: snapshot.version,
        next_id: snapshot.next_id,
        workspace: snapshot.workspace.clone(),
        unknown: snapshot.unknown.clone(),
    };
    metadata.workspace.sessions.clear();
    metadata.workspace.accounts.clear();
    metadata.workspace.machines.clear();
    metadata.workspace.projects.clear();
    metadata.workspace.prompts.clear();
    metadata.workspace.deliveries.clear();
    metadata.workspace.notes.clear();
    metadata.workspace.commit_drafts.clear();
    metadata.workspace.browser_pages.clear();
    metadata.workspace.scheduled_tasks.clear();
    let payload = serde_json::to_string(&metadata)?;
    let previous: Option<String> = tx
        .query_row(
            "SELECT payload FROM workspace WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if previous.as_deref() != Some(payload.as_str()) {
        tx.execute("INSERT INTO events(kind, entity_id, payload) VALUES ('workspace.upserted', 'workspace', ?1)", [&payload]).map_err(sql_error)?;
    }
    tx.execute("INSERT INTO workspace(singleton, revision, payload) VALUES (1, ?1, ?2) ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision, payload=excluded.payload", params![revision, payload]).map_err(sql_error)?;
    Ok(())
}

/// Forgets every provider's usage readings for account `id`.
fn clear_usage(tx: &rusqlite::Transaction<'_>, id: &str) -> io::Result<()> {
    for provider in ["claude", "codex"] {
        tx.execute(
            "DELETE FROM metadata WHERE key=?1",
            [format!("{USAGE_PREFIX}{provider}/{id}")],
        )
        .map_err(sql_error)?;
    }
    Ok(())
}

fn import_logs(tx: &rusqlite::Transaction<'_>, directory: PathBuf) -> io::Result<()> {
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(import_error(&directory, error)),
    };
    let mut paths = entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| import_error(&directory, error))
        })
        .collect::<io::Result<Vec<_>>>()?;
    paths.sort();
    for path in paths {
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let Some(session) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u64>().ok())
        else {
            continue;
        };
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(import_error(&path, error)),
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(import_error(&path, error)),
        };
        for line in BufReader::new(file).split(b'\n') {
            let line = line.map_err(|error| import_error(&path, error))?;
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if !matches!(
                value.get("entry").and_then(Value::as_str),
                Some("turn_started" | "item" | "approval" | "turn_ended")
            ) {
                continue;
            }
            let payload = serde_json::to_string(&value)?;
            tx.execute(
                "INSERT INTO events(kind, entity_id, payload) VALUES ('log.appended', ?1, ?2)",
                params![session.to_string(), payload],
            )
            .map_err(sql_error)?;
            tx.execute(
                "INSERT INTO log_entries(sequence, session_id, payload) VALUES (?1, ?2, ?3)",
                params![tx.last_insert_rowid(), session.to_string(), payload],
            )
            .map_err(sql_error)?;
        }
    }
    Ok(())
}

fn import_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("Could not import {}: {error}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Provider, Store, WorkingDirectory};
    use std::time::Instant;

    fn directory() -> WorkingDirectory {
        WorkingDirectory::new(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn legacy(path: &Path) {
        fs::write(path, r#"{"version":1,"next_id":2,"future":{"kept":true},"workspace":{"selected":1,"sessions":[{"id":1,"title":"Imported","provider":"Codex","settled":false,"draft":"saved draft","model_locked":false,"future_session":42}],"preferences":{"future_preference":"kept"}}}"#).unwrap();
        fs::create_dir(path.parent().unwrap().join("sessions")).unwrap();
        fs::write(path.parent().unwrap().join("sessions/1.jsonl"), "{\"entry\":\"turn_started\"}\n{\"entry\":\"turn_ended\",\"status\":\"completed\",\"error\":null}\n{torn").unwrap();
    }

    #[test]
    fn the_checkpoint_store_name_is_kept_across_reopening() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let first = Database::open(&path)
            .unwrap()
            .checkpoint_store("a1")
            .unwrap();
        let again = Database::open(&path)
            .unwrap()
            .checkpoint_store("b2")
            .unwrap();
        assert_eq!((first.as_str(), again.as_str()), ("a1", "a1"));
    }

    #[test]
    fn usage_readings_survive_reopening_and_append_an_event() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let database = Database::open(&path).unwrap();
        let before = database.events_after(0, 1000).unwrap().len();
        let mut usage = AccountUsage::default();
        usage.record(
            vec![crate::LimitWindow {
                id: "five_hour".into(),
                label: "5-hour".into(),
                used_percent: 85.0,
                resets_at: Some(1_800_000_000),
            }],
            42,
        );
        database.record_usage("claude/default", &usage).unwrap();
        usage.readings[0].window.used_percent = 90.0;
        database.record_usage("claude/default", &usage).unwrap();
        database
            .record_usage("codex/codex-work", &AccountUsage::default())
            .unwrap();
        // A report that carries fewer windows keeps the saved ones it leaves out.
        let mut weekly = AccountUsage::default();
        weekly.record(
            vec![crate::LimitWindow {
                id: "seven_day".into(),
                label: "Weekly".into(),
                used_percent: 20.0,
                resets_at: None,
            }],
            43,
        );
        database.record_usage("claude/default", &weekly).unwrap();
        usage.readings.push(weekly.readings[0].clone());
        drop(database);
        let reopened = Database::open(&path).unwrap();
        let saved = reopened.usage().unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].0, "claude/default");
        assert_eq!(saved[0].1, usage);
        assert_eq!(saved[1].0, "codex/codex-work");
        let events = reopened.events_after(0, 1000).unwrap();
        assert_eq!(events.len(), before + 4);
        assert!(events[before..].iter().all(|e| e.kind == "usage.recorded"));
        Database::open_read_only(&path).unwrap().usage().unwrap();
    }

    #[test]
    fn a_saved_window_that_reset_a_day_before_the_next_report_is_dropped() {
        let database = Database::in_memory().unwrap();
        let window = |id: &str, resets_at| crate::LimitWindow {
            id: id.into(),
            label: id.into(),
            used_percent: 100.0,
            resets_at,
        };
        let mut once = AccountUsage::default();
        once.record(vec![window("seven_day_opus", Some(1))], 1_000);
        database.record_usage("claude/default", &once).unwrap();
        let mut later = AccountUsage::default();
        later.record(vec![window("five_hour", None)], 2_000 + 24 * 60 * 60_000);
        database.record_usage("claude/default", &later).unwrap();
        assert_eq!(
            database.usage().unwrap(),
            [("claude/default".into(), later)]
        );
    }

    #[test]
    fn in_memory_store_has_atomic_monotone_events_and_shared_logs() {
        let mut store = Store::in_memory().unwrap();
        let database = store.database();
        store.create(Provider::Codex, &directory(), 1).unwrap();
        database
            .append_logs(
                1,
                &[("entry-1".into(), r#"{"entry":"turn_started"}"#.into())],
                None,
            )
            .unwrap();
        store.set_draft(1, "keep exactly".into()).unwrap();
        let events = database.events_after(0, 1000).unwrap();
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "log.appended")
                .count(),
            1
        );
        let last = events.last().unwrap().sequence;
        assert!(database.events_after(last, 1000).unwrap().is_empty());
        let restored = Store::from_database(database).unwrap();
        assert_eq!(restored.workspace(), store.workspace());
        assert_eq!(restored.workspace().sessions()[0].draft(), "keep exactly");
    }

    #[test]
    fn events_are_append_only_and_projection_failure_rolls_back_everything() {
        let mut store = Store::in_memory().unwrap();
        let database = store.database();
        let before = store.saved_snapshot().unwrap();
        let count = database.events_after(0, 1000).unwrap().len();
        database.write(|connection| {
            assert!(connection.execute("UPDATE events SET kind='changed'", []).is_err());
            assert!(connection.execute("DELETE FROM events", []).is_err());
            connection.execute_batch("CREATE TRIGGER reject_workspace BEFORE UPDATE ON workspace BEGIN SELECT RAISE(ABORT, 'injected projection failure'); END;").map_err(sql_error)
        }).unwrap();
        let created = store.create(Provider::Claude, &directory(), 1).unwrap();
        assert!(store.flush().is_err());
        assert_eq!(store.workspace().sessions().len(), 1);
        assert_eq!(store.saved_snapshot().unwrap(), before);
        assert_eq!(database.events_after(0, 1000).unwrap().len(), count);
        database
            .write(|connection| {
                connection
                    .execute_batch("DROP TRIGGER reject_workspace")
                    .map_err(sql_error)
            })
            .unwrap();
        store.flush().unwrap();
        assert_eq!(created.value, 1);
        assert_eq!(
            Store::from_database(database)
                .unwrap()
                .workspace()
                .sessions()[0]
                .id(),
            1
        );
    }

    #[test]
    fn stale_workspace_writer_cannot_overwrite_newer_projection() {
        let database = Database::in_memory().unwrap();
        let mut first = Store::from_database(database.clone()).unwrap();
        let mut stale = Store::from_database(database.clone()).unwrap();
        first.create(Provider::Claude, &directory(), 1).unwrap();
        let count = database.events_after(0, 1000).unwrap().len();
        stale.create(Provider::Codex, &directory(), 2).unwrap();
        stale.flush().unwrap();
        let error = stale.save_error();
        assert!(
            matches!(&error, Some(crate::SaveError::Stale(reason)) if reason.contains("another writer")),
            "{error:?}"
        );
        assert_eq!(stale.workspace().sessions().len(), 1);
        assert_eq!(database.events_after(0, 1000).unwrap().len(), count);
        assert_eq!(
            Store::from_database(database).unwrap().workspace(),
            first.workspace()
        );
    }

    #[test]
    fn sqlite_full_retains_unapplied_draft_and_recovers_without_duplicate_events() {
        let mut store = Store::in_memory().unwrap();
        store.create(Provider::Claude, &directory(), 1).unwrap();
        let database = store.database();
        let count = database.events_after(0, 1000).unwrap().len();
        database
            .write(|connection| {
                connection
                    .execute_batch("PRAGMA max_page_count=1")
                    .map_err(sql_error)
            })
            .unwrap();
        let draft = "full storage retry ".repeat(4096);
        store.set_draft(1, draft.clone()).unwrap();
        assert!(store.flush().unwrap_err().contains("full"));
        assert_eq!(store.workspace().sessions()[0].draft(), draft);
        assert_eq!(database.events_after(0, 1000).unwrap().len(), count);
        database
            .write(|connection| {
                connection
                    .execute_batch("PRAGMA max_page_count=1073741823")
                    .map_err(sql_error)
            })
            .unwrap();
        store.set_draft(1, draft.clone()).unwrap();
        assert_eq!(
            store.saved_snapshot().unwrap()["workspace"]["sessions"][0]["draft"],
            draft
        );
        assert_eq!(database.events_after(0, 1000).unwrap().len(), count + 1);
    }

    #[test]
    fn import_is_atomic_idempotent_and_never_reopens_legacy_backups() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        legacy(&path);
        let old = fs::read(&path).unwrap();
        let logs = fs::read(dir.path().join("sessions/1.jsonl")).unwrap();
        let unreadable = dir.path().join("sessions/2.jsonl");
        fs::write(&unreadable, "{\"entry\":\"turn_started\"}\n").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let error = Store::open(&path).err().unwrap().to_string();
        assert!(error.contains(unreadable.to_str().unwrap()), "{error}");
        let connection = Connection::open(database_path(&path)).unwrap();
        assert_eq!(
            connection
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            0
        );
        drop(connection);
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(unreadable).unwrap();
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.workspace().sessions()[0].draft(), "saved draft");
        assert_eq!(store.saved_snapshot().unwrap()["future"]["kept"], true);
        assert_eq!(
            store.saved_snapshot().unwrap()["workspace"]["sessions"][0]["future_session"],
            42
        );
        assert_eq!(store.database().read_logs(1).unwrap().len(), 2);
        assert_eq!(fs::read(&path).unwrap(), old);
        assert_eq!(fs::read(dir.path().join("sessions/1.jsonl")).unwrap(), logs);
        store
            .set_draft(1, "SQLite is authoritative".into())
            .unwrap();
        let count = store.database().events_after(0, 1000).unwrap().len();
        fs::write(&path, "old build changed its backup").unwrap();
        fs::write(dir.path().join("sessions/1.jsonl"), "old build appended").unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.workspace().sessions()[0].draft(),
            "SQLite is authoritative"
        );
        assert_eq!(reopened.database().read_logs(1).unwrap().len(), 2);
        assert_eq!(
            reopened.database().events_after(0, 1000).unwrap().len(),
            count
        );
    }

    #[test]
    fn newer_schema_is_rejected_without_changing_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(connection);
        let before = fs::read(&path).unwrap();
        assert!(
            Database::open(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("unsupported SQLite schema")
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn read_only_load_does_not_wait_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.sqlite3");
        let database = Database::open(&path).unwrap();
        database
            .append_logs(1, &[("saved".into(), "{}".into())], None)
            .unwrap();
        let lock = Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let reader = Database::open_read_only(&path).unwrap();
        assert_eq!(reader.read_logs(1).unwrap(), vec!["{}"]);
        assert!(
            reader
                .append_logs(1, &[("forbidden".into(), "{}".into())], None)
                .is_err()
        );
        lock.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn arbitrary_legacy_filenames_keep_their_backups_and_reopen_the_database() {
        for name in ["state", "state.backup", "state.sqlite3", "state.json"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(name);
            legacy(&path);
            let before = fs::read(&path).unwrap();
            let mut store = Store::open(&path).unwrap();
            assert_ne!(store.path(), path);
            assert_eq!(store.workspace().sessions()[0].draft(), "saved draft");
            assert_eq!(store.database().read_logs(1).unwrap().len(), 2);
            store
                .set_draft(1, "new authoritative draft".into())
                .unwrap();
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(
                Store::open(&path).unwrap().workspace().sessions()[0].draft(),
                "new authoritative draft"
            );
            assert_eq!(
                Store::open(store.path()).unwrap().workspace(),
                store.workspace()
            );
            fs::write(&path, "").unwrap();
            assert_eq!(
                Store::open(&path).unwrap().workspace(),
                store.workspace(),
                "emptying the legacy backup must not select another database: {name}"
            );
            fs::remove_file(&path).unwrap();
            assert_eq!(
                Store::open(&path).unwrap().workspace(),
                store.workspace(),
                "removing the legacy backup must not select another database: {name}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_and_nonregular_logs_are_skipped_but_regular_symlinks_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        legacy(&path);
        let sessions = dir.path().join("sessions");
        std::os::unix::fs::symlink(sessions.join("missing"), sessions.join("9.jsonl")).unwrap();
        std::os::unix::fs::symlink(sessions.join("1.jsonl"), sessions.join("6.jsonl")).unwrap();
        fs::create_dir(sessions.join("8.jsonl")).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(sessions.join("7.jsonl"))
                .status()
                .unwrap()
                .success()
        );
        let store = Store::open(&path).unwrap();
        assert_eq!(store.database().read_logs(1).unwrap().len(), 2);
        assert_eq!(store.database().read_logs(6).unwrap().len(), 2);
        for session in [7, 8, 9] {
            assert!(store.database().read_logs(session).unwrap().is_empty());
        }
    }

    #[test]
    fn session_only_events_still_advance_the_workspace_writer_guard() {
        let mut store = Store::in_memory().unwrap();
        store.create(Provider::Claude, &directory(), 1).unwrap();
        let database = store.database();
        let mut stale = Store::from_database(database.clone()).unwrap();
        let cursor = database
            .events_after(0, 1000)
            .unwrap()
            .last()
            .unwrap()
            .sequence;
        store
            .set_draft(1, "only this entity changed".into())
            .unwrap();
        let events = database.events_after(cursor, 1000).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "session.upserted");
        stale.set_draft(1, "do not overwrite".into()).unwrap();
        stale.flush().unwrap();
        assert!(matches!(
            stale.save_error(),
            Some(crate::SaveError::Stale(_))
        ));
        assert_eq!(
            Store::from_database(database).unwrap().workspace(),
            store.workspace()
        );
    }

    /// Holds the write gate for `hold`, as a write whose commit stalls does, and returns
    /// once it holds it.
    fn slow_write(database: &Database, hold: Duration) -> std::thread::JoinHandle<()> {
        let (held, wait) = std::sync::mpsc::channel();
        let database = database.clone();
        let writer = std::thread::spawn(move || {
            database
                .write(move |_| {
                    held.send(()).unwrap();
                    std::thread::sleep(hold);
                    Ok(())
                })
                .unwrap();
        });
        wait.recv().unwrap();
        writer
    }

    #[test]
    fn a_save_waits_for_a_slow_write_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Claude, &directory(), 1).unwrap();
        let writer = slow_write(&store.database(), Duration::from_millis(300));
        store.set_draft(1, "saved after it".into()).unwrap();
        writer.join().unwrap();
        assert_eq!(store.workspace().sessions()[0].draft(), "saved after it");
    }

    #[test]
    fn a_gate_wait_leaves_sqlite_its_whole_busy_retry() {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("workspace.sqlite3")).unwrap();
        let writer = slow_write(&database, Duration::from_millis(150));
        let busy: i64 = database
            .write(|connection| {
                connection
                    .pragma_query_value(None, "busy_timeout", |row| row.get(0))
                    .map_err(sql_error)
            })
            .unwrap();
        writer.join().unwrap();
        assert_eq!(busy, BUSY_TIMEOUT.as_millis() as i64);
    }

    #[test]
    fn a_repeated_log_event_key_is_stored_once_and_cannot_change_its_entry() {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("workspace.sqlite3")).unwrap();
        let record = (
            "same-key".to_string(),
            r#"{"entry":"turn_started"}"#.to_string(),
        );
        database
            .append_logs(1, std::slice::from_ref(&record), None)
            .unwrap();
        database
            .append_logs(1, &[record.clone(), record.clone()], None)
            .unwrap();
        assert_eq!(database.read_logs(1).unwrap(), [record.1.clone()]);
        let changed = (
            "same-key".to_string(),
            r#"{"entry":"turn_ended"}"#.to_string(),
        );
        assert!(database.append_logs(1, &[changed], None).is_err());
        assert!(database.append_logs(2, &[record.clone()], None).is_err());
        assert_eq!(database.read_logs(1).unwrap(), [record.1]);
        assert!(database.read_logs(2).unwrap().is_empty());
    }

    #[test]
    fn a_remote_sessions_cursor_moves_only_with_its_rows_and_names_its_journal() {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("workspace.sqlite3")).unwrap();
        let row = |key: &str| (key.to_string(), r#"{"entry":"turn_started"}"#.to_string());
        let cursors = || database.remote_cursors().unwrap();
        database
            .append_logs(7, &[row("a")], Some(("old", 12)))
            .unwrap();
        assert_eq!(cursors(), [(7, "old".to_owned(), 12)]);
        let torn = (String::from("b"), String::from("{"));
        assert!(
            database
                .append_logs(7, &[row("c"), torn], Some(("old", 20)))
                .is_err()
        );
        assert_eq!(
            cursors(),
            [(7, "old".to_owned(), 12)],
            "a transaction that fails leaves the cursor with the rows it had"
        );
        assert_eq!(database.read_logs(7).unwrap().len(), 1);
        database
            .append_logs(7, &[row("d")], Some(("new", 3)))
            .unwrap();
        assert_eq!(
            cursors(),
            [(7, "new".to_owned(), 3)],
            "a place in the machine's new journal replaces one in its old journal"
        );
        database.append_logs(8, &[row("e")], None).unwrap();
        assert_eq!(cursors(), [(7, "new".to_owned(), 3)]);
    }

    #[test]
    fn a_save_returns_at_once_while_the_writer_is_held_and_lands_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Claude, &directory(), 1).unwrap();
        let writer = slow_write(&store.database(), Duration::from_secs(2));
        let start = Instant::now();
        let saved = store.set_draft(1, "saved after the hold".into()).unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(
            store.workspace().sessions()[0].draft(),
            "saved after the hold"
        );
        assert!(!store.is_durable(saved.revision));
        writer.join().unwrap();
        store.flush().unwrap();
        assert!(store.is_durable(saved.revision));
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            "saved after the hold"
        );
    }

    #[test]
    fn a_reopen_in_the_same_process_sees_saves_issued_just_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Claude, &directory(), 1).unwrap();
        let writer = slow_write(&store.database(), Duration::from_millis(300));
        for index in 0..20 {
            store.set_draft(1, format!("draft {index}")).unwrap();
        }
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.workspace().sessions()[0].draft(), "draft 19");
        let logs = Database::open(&path).unwrap();
        logs.append_logs(1, &[("after".into(), "{}".into())], None)
            .unwrap();
        assert_eq!(reopened.database().read_logs(1).unwrap(), ["{}"]);
        writer.join().unwrap();
    }

    #[test]
    fn a_write_withdrawn_as_busy_never_runs_later() {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("workspace.sqlite3")).unwrap();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (started, wait) = std::sync::mpsc::channel();
        let blocker = {
            let database = database.clone();
            std::thread::spawn(move || {
                database
                    .write(move |_| {
                        started.send(()).unwrap();
                        held.recv().unwrap();
                        Ok(())
                    })
                    .unwrap();
            })
        };
        wait.recv().unwrap();
        let error = database
            .append_logs(
                1,
                &[("withdrawn".into(), r#"{"n":"withdrawn"}"#.into())],
                None,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        release.send(()).unwrap();
        blocker.join().unwrap();
        database
            .append_logs(1, &[("after".into(), r#"{"n":"after"}"#.into())], None)
            .unwrap();
        assert_eq!(database.read_logs(1).unwrap(), [r#"{"n":"after"}"#]);
    }

    #[test]
    fn a_write_that_panics_fails_and_the_next_write_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("workspace.sqlite3")).unwrap();
        let error = database
            .write(|_| -> io::Result<()> { panic!("a write job panicked") })
            .unwrap_err();
        assert_eq!(error.to_string(), "SQLite write stopped unexpectedly");
        database
            .append_logs(1, &[("after".into(), r#"{"n":"after"}"#.into())], None)
            .unwrap();
        assert_eq!(database.read_logs(1).unwrap(), [r#"{"n":"after"}"#]);
    }

    #[test]
    fn concurrent_workspace_and_log_writers_share_a_gate_with_independent_reads() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Claude, &directory(), 1).unwrap();
        let database = store.database();
        let stop = Arc::new(AtomicBool::new(false));
        let read_db = database.clone();
        let stopped = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut reads = 0;
            loop {
                read_db.read_logs(1).unwrap();
                reads += 1;
                if stopped.load(Ordering::Relaxed) {
                    return reads;
                }
            }
        });
        let logger = std::thread::spawn(move || {
            for index in 0..300 {
                assert_eq!(
                    database
                        .append_logs(1, &[(format!("concurrent-{index}"), "{}".into())], None)
                        .unwrap(),
                    Durability::Synced
                );
            }
        });
        for index in 0..300 {
            store.set_draft(1, format!("draft-{index}")).unwrap();
        }
        logger.join().unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
        assert_eq!(store.database().read_logs(1).unwrap().len(), 300);
        assert_eq!(store.workspace().sessions()[0].draft(), "draft-299");
    }

    #[cfg(unix)]
    #[test]
    fn a_new_database_is_private_and_permission_failure_is_retryable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        store.create(Provider::Claude, &directory(), 1).unwrap();
        let before = store.saved_snapshot().unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o400)).unwrap();
        store.set_draft(1, "keep for retry".into()).unwrap();
        assert!(store.flush().is_err());
        assert_eq!(store.workspace().sessions()[0].draft(), "keep for retry");
        assert_eq!(store.saved_snapshot().unwrap(), before);
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).unwrap();
        for suffix in ["-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{suffix}", store.path().display()));
            if path.exists() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        store.set_draft(1, "keep for retry".into()).unwrap();
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            "keep for retry"
        );
    }

    #[test]
    fn a_file_that_replaces_an_idle_database_is_read_and_written_as_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.sqlite3");
        let database = Database::open(&path).unwrap();
        database
            .append_logs(1, &[("old".into(), r#"{"origin":"old"}"#.into())], None)
            .unwrap();
        let other = dir.path().join("replacement.sqlite3");
        let replacement = Database::open(&other).unwrap();
        replacement
            .append_logs(1, &[("new".into(), r#"{"origin":"new"}"#.into())], None)
            .unwrap();
        drop(replacement);
        fs::rename(&other, &path).unwrap();
        assert_eq!(database.read_logs(1).unwrap(), [r#"{"origin":"new"}"#]);
        database
            .append_logs(1, &[("after".into(), r#"{"origin":"after"}"#.into())], None)
            .unwrap();
        drop(database);
        assert_eq!(
            Database::open_read_only(&path)
                .unwrap()
                .read_logs(1)
                .unwrap(),
            [r#"{"origin":"new"}"#, r#"{"origin":"after"}"#]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_write_fails_as_a_fresh_connection_would_once_this_process_cannot_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.sqlite3");
        let database = Database::open(&path).unwrap();
        let append = |key: &str| database.append_logs(1, &[(key.into(), "{}".into())], None);
        append("first").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o460)).unwrap();
        assert_eq!(
            append("denied").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        append("repaired").unwrap();
        assert_eq!(database.read_logs(1).unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_write_fails_as_a_fresh_connection_would_once_its_folder_is_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.sqlite3");
        let database = Database::open(&path).unwrap();
        let append = |key: &str| database.append_logs(1, &[(key.into(), "{}".into())], None);
        append("first").unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let denied = append("denied");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(denied.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        append("repaired").unwrap();
        assert_eq!(database.read_logs(1).unwrap().len(), 2);
    }

    #[test]
    fn a_file_that_replaces_a_database_just_opened_is_read_and_written_as_itself() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("replacement.sqlite3");
        let replacement = Database::open(&other).unwrap();
        replacement
            .append_logs(1, &[("new".into(), r#"{"origin":"new"}"#.into())], None)
            .unwrap();
        drop(replacement);
        let path = dir.path().join("workspace.sqlite3");
        let database = Database::open(&path).unwrap();
        fs::rename(&other, &path).unwrap();
        database
            .append_logs(1, &[("after".into(), r#"{"origin":"after"}"#.into())], None)
            .unwrap();
        drop(database);
        assert_eq!(
            Database::open_read_only(&path)
                .unwrap()
                .read_logs(1)
                .unwrap(),
            [r#"{"origin":"new"}"#, r#"{"origin":"after"}"#]
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore]
    fn exclusive_writer_child() {
        let path = std::env::var_os("KEYSTONE_TEST_EXCLUSIVE_WRITER").expect("child database path");
        let path = PathBuf::from(path);
        let connection = Connection::open(&path).unwrap();
        connection.busy_timeout(BUSY_TIMEOUT).unwrap();
        let outcome = connection.execute_batch(
            r#"PRAGMA locking_mode=EXCLUSIVE;
            BEGIN EXCLUSIVE;
            INSERT INTO events(kind, entity_id, payload, event_key)
                VALUES ('log.appended', '1', '{"origin":"child"}', 'child');
            INSERT INTO log_entries(sequence, session_id, payload)
                VALUES (last_insert_rowid(), '1', '{"origin":"child"}');
            COMMIT;"#,
        );
        let outcome = if outcome.is_ok() {
            "committed"
        } else {
            "refused"
        };
        fs::write(path.with_extension("outcome"), outcome).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_write_another_process_commits_between_ours_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.sqlite3");
        let database = Database::open(&path).unwrap();
        database
            .append_logs(1, &[("seed".into(), r#"{"origin":"seed"}"#.into())], None)
            .unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "database::tests::exclusive_writer_child",
                "--ignored",
            ])
            .env("KEYSTONE_TEST_EXCLUSIVE_WRITER", &path)
            .output()
            .unwrap();
        assert!(child.status.success(), "{child:?}");
        let committed = fs::read_to_string(path.with_extension("outcome")).unwrap() == "committed";
        database
            .append_logs(1, &[("after".into(), r#"{"origin":"after"}"#.into())], None)
            .unwrap();
        drop(database);
        let mut expected = vec![r#"{"origin":"seed"}"#];
        if committed {
            expected.push(r#"{"origin":"child"}"#);
        }
        expected.push(r#"{"origin":"after"}"#);
        assert_eq!(
            Database::open_read_only(&path)
                .unwrap()
                .read_logs(1)
                .unwrap(),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore]
    fn import_crash_child() {
        let path = std::env::var_os("KEYSTONE_TEST_CRASH_IMPORT").expect("child import path");
        let path = PathBuf::from(path);
        let mut connection = Connection::open(database_path(&path)).unwrap();
        configure(&connection, BUSY_TIMEOUT).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        let tx = Database::prepare_import(&mut connection, Some(&path)).unwrap();
        let count: i64 = tx
            .query_one("SELECT count(*) FROM log_entries", [], |row| row.get(0))
            .unwrap();
        let staged = path.with_extension("ready.partial");
        fs::write(&staged, count.to_string()).unwrap();
        fs::rename(staged, path.with_extension("ready")).unwrap();
        loop {
            std::thread::park();
        }
    }

    #[cfg(unix)]
    struct LiveChild(std::process::Child);

    #[cfg(unix)]
    impl LiveChild {
        fn running(&mut self) -> bool {
            self.0.try_wait().unwrap().is_none()
        }

        fn kill(mut self) -> std::process::ExitStatus {
            self.0.kill().unwrap();
            self.0.wait().unwrap()
        }
    }

    #[cfg(unix)]
    impl Drop for LiveChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(unix)]
    #[test]
    fn killed_mid_import_restarts_from_no_partial_state_and_imports_once() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        legacy(&path);
        let old = fs::read(&path).unwrap();
        let ready = path.with_extension("ready");
        let mut child = LiveChild(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "database::tests::import_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("KEYSTONE_TEST_CRASH_IMPORT", &path)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline && child.running() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(ready).expect("import reached uncommitted projections"),
            "2"
        );
        let connection = Connection::open(database_path(&path)).unwrap();
        assert_eq!(
            connection
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            0
        );
        assert!(!child.kill().success());
        drop(connection);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.workspace().sessions()[0].draft(), "saved draft");
        assert_eq!(store.database().read_logs(1).unwrap().len(), 2);
        let events = store.database().events_after(0, 1000).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .database()
                .events_after(0, 1000)
                .unwrap()
                .len(),
            events.len()
        );
        assert_eq!(fs::read(&path).unwrap(), old);
    }
}
