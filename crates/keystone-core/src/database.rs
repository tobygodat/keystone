use crate::{Durability, Snapshot, Workspace, invalid, validate};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SCHEMA_VERSION: u32 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_millis(100);
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
/// IMMEDIATE transaction that also records the new `user_version`.
const MIGRATIONS: [&str; 1] = ["
CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload))
);
"];

#[derive(Clone)]
pub struct Database {
    path: PathBuf,
    memory: Option<Arc<Mutex<Connection>>>,
    read_only: bool,
    write_gate: Arc<parking_lot::Mutex<()>>,
}

#[derive(Debug)]
pub struct StoredEvent {
    pub sequence: u64,
    pub kind: String,
    pub entity_id: String,
    pub payload: String,
}

fn sql_error(error: rusqlite::Error) -> io::Error {
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

fn configure(connection: &Connection, busy_timeout: Duration) -> io::Result<()> {
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
        let write_gate = Arc::new(parking_lot::Mutex::new(()));
        let started = Instant::now();
        let guard = write_gate
            .try_lock_for(BUSY_TIMEOUT)
            .ok_or_else(writer_busy)?;
        let mut file = fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file.mode(0o600);
        }
        match file.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let mut connection = Connection::open(&path).map_err(sql_error)?;
        require_writable(&connection)?;
        configure(&connection, busy_remaining(started))?;
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
            connection
                .busy_timeout(busy_remaining(started))
                .map_err(sql_error)?;
            Self::initialize(&mut connection, Some(&legacy))?;
        }
        drop(connection);
        drop(guard);
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
            write_gate,
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
            path,
            memory: None,
            read_only: true,
            write_gate: Arc::default(),
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
            write_gate: Arc::default(),
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

    fn connection<T>(
        &self,
        read_only: bool,
        f: impl FnOnce(&mut Connection) -> io::Result<T>,
    ) -> io::Result<T> {
        if self.read_only && !read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SQLite database is read-only",
            ));
        }
        if let Some(memory) = &self.memory {
            let mut connection = memory
                .lock()
                .map_err(|_| io::Error::other("SQLite connection lock poisoned"))?;
            return f(&mut connection);
        }
        let started = Instant::now();
        let _writer = if read_only {
            None
        } else {
            Some(
                self.write_gate
                    .try_lock_for(BUSY_TIMEOUT)
                    .ok_or_else(writer_busy)?,
            )
        };
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let mut connection = Connection::open_with_flags(&self.path, flags).map_err(sql_error)?;
        if !read_only {
            require_writable(&connection)?;
        }
        configure(&connection, busy_remaining(started))?;
        if !read_only {
            connection
                .busy_timeout(busy_remaining(started))
                .map_err(sql_error)?;
        }
        f(&mut connection)
    }

    pub(crate) fn snapshot(&self) -> io::Result<(Snapshot, i64)> {
        self.connection(true, |connection| {
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
            validate(&snapshot).map_err(|error| invalid(error.to_string()))?;
            Ok((snapshot, revision))
        })
    }

    pub(crate) fn commit(
        &self,
        snapshot: &Snapshot,
        previous: &Workspace,
        revision: i64,
    ) -> io::Result<i64> {
        self.connection(false, |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let actual: i64 = tx.query_one("SELECT revision FROM workspace WHERE singleton=1", [], |row| row.get(0)).map_err(sql_error)?;
            if actual != revision {
                return Err(io::Error::other("The workspace changed in another writer. Reload Keystone before saving; no changes were overwritten."));
            }
            let next = revision.checked_add(1).ok_or_else(|| invalid("workspace revision exhausted"))?;
            write_projection(&tx, snapshot, next, Some(previous))?;
            tx.commit().map_err(sql_error)?;
            Ok(next)
        })
    }

    pub fn append_logs(
        &self,
        session: u64,
        records: &[(String, String)],
    ) -> io::Result<Durability> {
        self.connection(false, |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let session = session.to_string();
            for (key, payload) in records {
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
            tx.commit().map_err(sql_error)
        })?;
        Ok(match self.sync_directory() {
            Ok(()) => Durability::Synced,
            Err(error) => Durability::DirectorySyncPending(error.to_string()),
        })
    }

    pub fn read_logs(&self, session: u64) -> io::Result<Vec<String>> {
        self.connection(true, |connection| {
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

    pub fn events_after(&self, sequence: u64, limit: u32) -> io::Result<Vec<StoredEvent>> {
        let sequence = i64::try_from(sequence)
            .map_err(|_| invalid("event cursor exceeds SQLite sequence range"))?;
        self.connection(true, |connection| {
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

fn busy_remaining(started: Instant) -> Duration {
    BUSY_TIMEOUT
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(5))
}

fn writer_busy() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "SQLite writer is busy; retry saving",
    )
}

fn require_writable(connection: &Connection) -> io::Result<()> {
    if connection.is_readonly("main").map_err(sql_error)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SQLite workspace is read-only; restore write permission before retrying",
        ));
    }
    Ok(())
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
    }
    for (position, account) in accounts.iter().enumerate() {
        if previous.is_some_and(|workspace| workspace.accounts.get(position) == Some(account)) {
            continue;
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
    let mut metadata = Snapshot {
        version: snapshot.version,
        next_id: snapshot.next_id,
        workspace: snapshot.workspace.clone(),
        unknown: snapshot.unknown.clone(),
    };
    metadata.workspace.sessions.clear();
    metadata.workspace.accounts.clear();
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

    fn directory() -> WorkingDirectory {
        WorkingDirectory::new(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn legacy(path: &Path) {
        fs::write(path, r#"{"version":1,"next_id":2,"future":{"kept":true},"workspace":{"selected":1,"sessions":[{"id":1,"title":"Imported","provider":"Codex","settled":false,"draft":"saved draft","model_locked":false,"future_session":42}],"preferences":{"future_preference":"kept"}}}"#).unwrap();
        fs::create_dir(path.parent().unwrap().join("sessions")).unwrap();
        fs::write(path.parent().unwrap().join("sessions/1.jsonl"), "{\"entry\":\"turn_started\"}\n{\"entry\":\"turn_ended\",\"status\":\"completed\",\"error\":null}\n{torn").unwrap();
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
        database.connection(false, |connection| {
            assert!(connection.execute("UPDATE events SET kind='changed'", []).is_err());
            assert!(connection.execute("DELETE FROM events", []).is_err());
            connection.execute_batch("CREATE TRIGGER reject_workspace BEFORE UPDATE ON workspace BEGIN SELECT RAISE(ABORT, 'injected projection failure'); END;").map_err(sql_error)
        }).unwrap();
        assert!(store.create(Provider::Claude, &directory(), 1).is_err());
        assert!(store.workspace().sessions().is_empty());
        assert_eq!(store.saved_snapshot().unwrap(), before);
        assert_eq!(database.events_after(0, 1000).unwrap().len(), count);
        database
            .connection(false, |connection| {
                connection
                    .execute_batch("DROP TRIGGER reject_workspace")
                    .map_err(sql_error)
            })
            .unwrap();
        assert_eq!(
            store
                .create(Provider::Claude, &directory(), 1)
                .unwrap()
                .value,
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
        let error = stale.create(Provider::Codex, &directory(), 2).unwrap_err();
        assert!(error.to_string().contains("another writer"));
        assert!(stale.workspace().sessions().is_empty());
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
            .connection(false, |connection| {
                connection
                    .execute_batch("PRAGMA max_page_count=1")
                    .map_err(sql_error)
            })
            .unwrap();
        let draft = "full storage retry ".repeat(4096);
        assert!(
            store
                .set_draft(1, draft.clone())
                .unwrap_err()
                .to_string()
                .contains("full")
        );
        assert_eq!(store.workspace().sessions()[0].draft(), "");
        assert_eq!(database.events_after(0, 1000).unwrap().len(), count);
        database
            .connection(false, |connection| {
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
            .append_logs(1, &[("saved".into(), "{}".into())])
            .unwrap();
        let lock = Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let reader = Database::open_read_only(&path).unwrap();
        assert_eq!(reader.read_logs(1).unwrap(), vec!["{}"]);
        assert!(
            reader
                .append_logs(1, &[("forbidden".into(), "{}".into())])
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
        assert!(stale.set_draft(1, "do not overwrite".into()).is_err());
        assert_eq!(
            Store::from_database(database).unwrap().workspace(),
            store.workspace()
        );
    }

    #[test]
    fn an_exhausted_gate_wait_keeps_a_sqlite_busy_retry() {
        let started = Instant::now() - Duration::from_secs(1);
        assert!(busy_remaining(started) >= Duration::from_millis(1));
        assert!(busy_remaining(Instant::now()) <= BUSY_TIMEOUT);
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
            .append_logs(1, std::slice::from_ref(&record))
            .unwrap();
        database
            .append_logs(1, &[record.clone(), record.clone()])
            .unwrap();
        assert_eq!(database.read_logs(1).unwrap(), [record.1.clone()]);
        let changed = (
            "same-key".to_string(),
            r#"{"entry":"turn_ended"}"#.to_string(),
        );
        assert!(database.append_logs(1, &[changed]).is_err());
        assert!(database.append_logs(2, &[record.clone()]).is_err());
        assert_eq!(database.read_logs(1).unwrap(), [record.1]);
        assert!(database.read_logs(2).unwrap().is_empty());
    }

    #[test]
    fn disk_writer_gate_is_bounded_and_does_not_block_readers() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Claude, &directory(), 1).unwrap();
        let database = store.database();
        let gate = database.write_gate.lock();
        assert!(database.read_logs(1).unwrap().is_empty());
        let start = Instant::now();
        let error = store.set_draft(1, "retained for retry".into()).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(store.workspace().sessions()[0].draft(), "");
        drop(gate);
        store.set_draft(1, "retained for retry".into()).unwrap();
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
                        .append_logs(1, &[(format!("concurrent-{index}"), "{}".into())])
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
        assert!(store.set_draft(1, "keep for retry".into()).is_err());
        assert_eq!(store.workspace().sessions()[0].draft(), "");
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
        fs::write(path.with_extension("ready"), count.to_string()).unwrap();
        loop {
            std::thread::park();
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
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "database::tests::import_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env("KEYSTONE_TEST_CRASH_IMPORT", &path)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline && child.try_wait().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !ready.exists() {
            let _ = child.kill();
            let _ = child.wait();
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
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
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
