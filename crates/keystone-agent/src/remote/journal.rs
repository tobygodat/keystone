//! The host's journal: every event it sent or will send each client, numbered, with
//! the `sessions` table beside it and the calls it accepted, with their replies. One
//! SQLite file, written only by the daemon's one writer thread.

use super::Route;
use super::frame::{self, Frame, Outcome, Routed, SessionState};
use crate::{Approval, Event, RequestId, TurnStatus};
use keystone_core::Provider;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Unacknowledged bytes kept per client before item events are dropped.
pub(crate) const CAP: u64 = 64 * 1024 * 1024;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS clients (
    client TEXT PRIMARY KEY,
    latest INTEGER NOT NULL DEFAULT 0,
    acked INTEGER NOT NULL DEFAULT 0,
    size INTEGER NOT NULL DEFAULT 0,
    acked_call INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS events (
    client TEXT NOT NULL,
    seq INTEGER NOT NULL,
    class INTEGER NOT NULL,
    provider TEXT NOT NULL,
    account TEXT,
    session INTEGER NOT NULL,
    frame BLOB NOT NULL,
    PRIMARY KEY (client, seq)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS events_by_class ON events (client, class, seq);
CREATE TABLE IF NOT EXISTS calls (
    client TEXT NOT NULL,
    id INTEGER NOT NULL,
    reply TEXT,
    PRIMARY KEY (client, id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS sessions (
    client TEXT NOT NULL,
    session INTEGER NOT NULL,
    provider TEXT NOT NULL,
    account TEXT,
    thread TEXT,
    running INTEGER NOT NULL DEFAULT 0,
    approvals TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (client, session)
) WITHOUT ROWID;
";

/// The schema version [`SCHEMA`] writes.
const VERSION: i64 = 4;

/// Each brings a journal from the version before it to the next, starting at 1.
const UPGRADES: [&str; 3] = [
    // A call is recorded when accepted, before it has a reply.
    "ALTER TABLE calls RENAME TO calls_1;
     CREATE TABLE calls (
         client TEXT NOT NULL,
         id INTEGER NOT NULL,
         reply TEXT,
         PRIMARY KEY (client, id)
     ) WITHOUT ROWID;
     INSERT INTO calls SELECT client, id, reply FROM calls_1;
     DROP TABLE calls_1;",
    // Each session's provider process, so a daemon started after a crash can end it.
    "ALTER TABLE sessions ADD COLUMN process INTEGER;
     ALTER TABLE sessions ADD COLUMN started INTEGER;",
    // Each provider's watcher holds a lock file instead.
    "ALTER TABLE sessions DROP COLUMN process;
     ALTER TABLE sessions DROP COLUMN started;",
];

/// Which events the cap may drop, oldest first: deltas, then items. Everything else,
/// lifecycle events included, is kept until acknowledged.
const KEPT: i64 = 0;
const DELTA: i64 = 1;
const ITEM: i64 = 2;

fn class(event: &Event) -> i64 {
    match event {
        Event::ItemDelta { .. } => DELTA,
        Event::ItemStarted(_) | Event::ItemCompleted(_) => ITEM,
        _ => KEPT,
    }
}

pub(crate) struct Journal {
    db: Connection,
    cap: u64,
}

/// What the journal knew of a call when it arrived.
pub(crate) enum Accepted {
    /// Never seen: it is now recorded as accepted, so it must run.
    New,
    /// It ran, and this was its reply.
    Replied(Outcome),
    /// It was accepted and has no reply, so it may have run. It must not run again.
    Unanswered,
}

pub(crate) struct Cursor {
    pub oldest: u64,
    pub latest: u64,
    pub acked: u64,
}

/// A session whose turn was running or whose approvals waited when the host lost its
/// process.
pub(crate) struct Interrupted {
    pub client: String,
    pub route: Route,
    pub session: u64,
    pub approvals: Vec<Approval>,
}

fn sql<T>(result: rusqlite::Result<T>) -> Result<T, String> {
    result.map_err(|e| format!("journal: {e}"))
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
    }
}

fn parse_provider(name: &str) -> Option<Provider> {
    match name {
        "Claude" => Some(Provider::Claude),
        "Codex" => Some(Provider::Codex),
        _ => None,
    }
}

impl Journal {
    pub fn open(path: &Path, cap: u64) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        }
        // SQLite gives the -wal and -shm files the database's own mode.
        std::fs::File::options()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("Could not open {}: {e}", path.display()))?;
        for suffix in ["", "-wal", "-shm"] {
            let file = format!("{}{suffix}", path.display());
            match std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(format!("Could not make {file} private: {error}"));
                }
                _ => {}
            }
        }
        let mut db = sql(Connection::open(path))?;
        let version: i64 = sql(db.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
        if version > VERSION {
            return Err(format!(
                "{} was written by a newer keystone-host",
                path.display()
            ));
        }
        sql(db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(())))?;
        sql(db.execute_batch("PRAGMA synchronous = NORMAL;"))?;
        let tx = sql(db.transaction())?;
        if version > 0 {
            for upgrade in &UPGRADES[version as usize - 1..] {
                sql(tx.execute_batch(upgrade))?;
            }
        }
        sql(tx.execute_batch(SCHEMA))?;
        sql(tx.pragma_update(None, "user_version", VERSION))?;
        sql(tx.commit())?;
        Ok(Self { db, cap })
    }

    /// Numbers and stores `events` for `client`, updating each session's row in the same
    /// transaction, and returns them as frames to send. Past the cap, it drops what
    /// [`class`] allows, oldest first. On an error nothing is stored.
    pub fn append(
        &mut self,
        client: &str,
        route: &Route,
        events: &[(u64, Event)],
    ) -> Result<Vec<Vec<u8>>, String> {
        let cap = self.cap;
        let tx = sql(self.db.transaction())?;
        sql(tx.execute(
            "INSERT OR IGNORE INTO clients (client) VALUES (?1)",
            [client],
        ))?;
        let (mut latest, mut size): (i64, i64) = sql(tx.query_row(
            "SELECT latest, size FROM clients WHERE client = ?1",
            [client],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ))?;
        let mut lines = Vec::with_capacity(events.len());
        for (session, event) in events {
            let session = *session;
            latest += 1;
            track(&tx, client, route, session, event)?;
            let class = class(event);
            let line = encode_event(latest as u64, route, session, event.clone());
            sql(tx.execute(
                "INSERT INTO events (client, seq, class, provider, account, session, frame)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    client,
                    latest,
                    class,
                    provider_name(route.provider),
                    route.account,
                    session as i64,
                    line
                ],
            ))?;
            size += line.len() as i64;
            lines.push(line);
        }
        if size > cap as i64 {
            size = sql(trim(&tx, client, size, cap as i64))?;
        }
        sql(tx.execute(
            "UPDATE clients SET latest = ?2, size = ?3 WHERE client = ?1",
            params![client, latest, size],
        ))?;
        sql(tx.commit())?;
        Ok(lines)
    }

    /// Records a gap for `sessions` at the next number: item events of theirs that
    /// were dropped before the journal could take them. Returns it as a frame to send.
    pub fn gap(
        &mut self,
        client: &str,
        route: &Route,
        sessions: &BTreeSet<u64>,
    ) -> Result<Vec<u8>, String> {
        let tx = sql(self.db.transaction())?;
        sql(tx.execute(
            "INSERT OR IGNORE INTO clients (client) VALUES (?1)",
            [client],
        ))?;
        let seq: i64 = sql(tx.query_row(
            "SELECT latest + 1 FROM clients WHERE client = ?1",
            [client],
            |row| row.get(0),
        ))?;
        let line = frame::encode(&Frame::Gap {
            provider: route.provider,
            account: route.account.clone(),
            from: seq as u64,
            to: seq as u64,
            sessions: sessions.iter().copied().collect(),
        })?;
        sql(tx.execute(
            "INSERT INTO events (client, seq, class, provider, account, session, frame)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![
                client,
                seq,
                KEPT,
                provider_name(route.provider),
                route.account,
                line
            ],
        ))?;
        sql(tx.execute(
            "UPDATE clients SET latest = ?2, size = size + ?3 WHERE client = ?1",
            params![client, seq, line.len() as i64],
        ))?;
        sql(tx.commit())?;
        Ok(line)
    }

    pub fn cursor(&self, client: &str) -> Result<Cursor, String> {
        let (latest, acked): (i64, i64) = sql(self
            .db
            .query_row(
                "SELECT latest, acked FROM clients WHERE client = ?1",
                [client],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional())?
        .unwrap_or_default();
        let oldest: Option<i64> = sql(self.db.query_row(
            "SELECT MIN(seq) FROM events WHERE client = ?1",
            [client],
            |row| row.get(0),
        ))?;
        Ok(Cursor {
            oldest: oldest.unwrap_or(latest + 1) as u64,
            latest: latest as u64,
            acked: acked as u64,
        })
    }

    /// The frames after `after`, through `upto`, in order.
    pub fn replay(&self, client: &str, after: u64, upto: u64) -> Result<Vec<Vec<u8>>, String> {
        let mut statement = sql(self.db.prepare(
            "SELECT frame FROM events WHERE client = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq",
        ))?;
        let rows =
            sql(statement.query_map(params![client, after as i64, upto as i64], |row| row.get(0)))?;
        sql(rows.collect())
    }

    /// The app drained every event through `seq` and has every reply through `calls`.
    pub fn ack(&mut self, client: &str, seq: u64, calls: u64) -> Result<(), String> {
        let (seq, calls) = (
            seq.min(i64::MAX as u64) as i64,
            calls.min(i64::MAX as u64) as i64,
        );
        let tx = sql(self.db.transaction())?;
        sql(tx.execute(
            "INSERT OR IGNORE INTO clients (client) VALUES (?1)",
            [client],
        ))?;
        let freed: i64 = sql(tx.query_row(
            "SELECT COALESCE(SUM(length(frame)), 0) FROM events WHERE client = ?1 AND seq <= ?2",
            params![client, seq],
            |row| row.get(0),
        ))?;
        sql(tx.execute(
            "DELETE FROM events WHERE client = ?1 AND seq <= ?2",
            params![client, seq],
        ))?;
        sql(tx.execute(
            "DELETE FROM calls WHERE client = ?1 AND id <= ?2",
            params![client, calls],
        ))?;
        sql(tx.execute(
            "UPDATE clients SET acked = MAX(acked, MIN(?2, latest)), size = MAX(size - ?3, 0),
             acked_call = MAX(acked_call, ?4) WHERE client = ?1",
            params![client, seq, freed, calls],
        ))?;
        sql(tx.commit())
    }

    /// Records call `id` as accepted if it is new, in one transaction, so that a call
    /// sent again never runs twice.
    pub fn accept(&mut self, client: &str, id: u64) -> Result<Accepted, String> {
        let tx = sql(self.db.transaction())?;
        let acked: Option<i64> = sql(tx
            .query_row(
                "SELECT acked_call FROM clients WHERE client = ?1",
                [client],
                |row| row.get(0),
            )
            .optional())?;
        if acked.is_some_and(|acked| id <= acked as u64) {
            return Ok(Accepted::Replied(Outcome::Err(format!(
                "call {id} was already answered"
            ))));
        }
        let row: Option<Option<String>> = sql(tx
            .query_row(
                "SELECT reply FROM calls WHERE client = ?1 AND id = ?2",
                params![client, id as i64],
                |row| row.get(0),
            )
            .optional())?;
        let accepted = match row {
            None => {
                sql(tx.execute(
                    "INSERT INTO calls (client, id) VALUES (?1, ?2)",
                    params![client, id as i64],
                ))?;
                Accepted::New
            }
            Some(None) => Accepted::Unanswered,
            Some(Some(text)) => Accepted::Replied(
                serde_json::from_str(&text).map_err(|e| format!("journal: call {id}: {e}"))?,
            ),
        };
        sql(tx.commit())?;
        Ok(accepted)
    }

    /// Records the reply of accepted call `id`.
    pub fn reply(&mut self, client: &str, id: u64, outcome: &Outcome) -> Result<(), String> {
        let reply = serde_json::to_string(outcome).map_err(|e| e.to_string())?;
        sql(self.db.execute(
            "UPDATE calls SET reply = ?3 WHERE client = ?1 AND id = ?2",
            params![client, id as i64, reply],
        ))
        .map(drop)
    }

    /// A new run of the app numbers its calls from 1 again.
    pub fn forget_calls(&mut self, client: &str) -> Result<(), String> {
        let tx = sql(self.db.transaction())?;
        sql(tx.execute("DELETE FROM calls WHERE client = ?1", [client]))?;
        sql(tx.execute(
            "UPDATE clients SET acked_call = 0 WHERE client = ?1",
            [client],
        ))?;
        sql(tx.commit())
    }

    /// The app answered `request`, so the provider no longer waits on it. Claude
    /// reports no event for an answer.
    pub fn answered(
        &mut self,
        client: &str,
        session: u64,
        request: &RequestId,
    ) -> Result<(), String> {
        let approvals: Option<String> = sql(self
            .db
            .query_row(
                "SELECT approvals FROM sessions WHERE client = ?1 AND session = ?2",
                params![client, session as i64],
                |row| row.get(0),
            )
            .optional())?;
        let Some(approvals) = approvals else {
            return Ok(());
        };
        let mut approvals = parse_approvals(&approvals);
        approvals.retain(|approval| approval.request != *request);
        let approvals = serde_json::to_string(&approvals).map_err(|e| e.to_string())?;
        sql(self.db.execute(
            "UPDATE sessions SET approvals = ?3 WHERE client = ?1 AND session = ?2",
            params![client, session as i64, approvals],
        ))
        .map(drop)
    }

    pub fn sessions(&self, client: &str) -> Result<Vec<SessionState>, String> {
        let mut statement = sql(self.db.prepare(
            "SELECT session, provider, account, running, approvals FROM sessions
             WHERE client = ?1 ORDER BY session",
        ))?;
        let rows = sql(statement.query_map([client], |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, String>(4)?,
            ))
        }))?;
        let mut sessions = Vec::new();
        for row in rows {
            let (session, provider, account, running, approvals) = sql(row)?;
            let Some(provider) = parse_provider(&provider) else {
                continue;
            };
            sessions.push(SessionState {
                provider,
                account,
                session,
                running,
                approvals: parse_approvals(&approvals),
            });
        }
        Ok(sessions)
    }

    /// Sessions of `client`, or of every client, with a turn running or approvals
    /// waiting.
    pub fn interrupted(&self, client: Option<&str>) -> Result<Vec<Interrupted>, String> {
        let mut statement = sql(self.db.prepare(
            "SELECT client, session, provider, account, approvals FROM sessions
             WHERE (running = 1 OR approvals != '[]') AND (?1 IS NULL OR client = ?1)
             ORDER BY client, session",
        ))?;
        let rows = sql(statement.query_map([client], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        }))?;
        let mut interrupted = Vec::new();
        for row in rows {
            let (client, session, provider, account, approvals) = sql(row)?;
            let Some(provider) = parse_provider(&provider) else {
                continue;
            };
            interrupted.push(Interrupted {
                client,
                route: Route { provider, account },
                session,
                approvals: parse_approvals(&approvals),
            });
        }
        Ok(interrupted)
    }
}

fn parse_approvals(text: &str) -> Vec<Approval> {
    serde_json::from_str(text).unwrap_or_else(|e| {
        eprintln!("keystone-host: journal: unreadable approvals ({e}), treated as none");
        Vec::new()
    })
}

fn encode_event(seq: u64, route: &Route, session: u64, event: Event) -> Vec<u8> {
    let routed = |event| {
        Frame::Event(Routed {
            seq,
            provider: route.provider,
            account: route.account.clone(),
            session,
            event,
        })
    };
    frame::encode(&routed(event)).unwrap_or_else(|reason| {
        let message = format!("keystone-host left out an event it couldn't send: {reason}");
        frame::encode(&routed(Event::Warning { message })).expect("a warning fits a frame")
    })
}

/// Keeps the session's row in step with `event`: its thread, whether a turn runs, and
/// the approvals waiting.
fn track(
    tx: &Transaction,
    client: &str,
    route: &Route,
    session: u64,
    event: &Event,
) -> Result<(), String> {
    let thread = match event {
        Event::ThreadReady { thread_id } => Some(thread_id.as_str()),
        _ => None,
    };
    let row: Option<(Option<String>, bool, String)> = sql(tx
        .query_row(
            "SELECT thread, running, approvals FROM sessions WHERE client = ?1 AND session = ?2",
            params![client, session as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional())?;
    let (old_thread, running, approvals) = row.unwrap_or((None, false, "[]".into()));
    let mut state = SessionState {
        provider: route.provider,
        account: route.account.clone(),
        session,
        running,
        approvals: parse_approvals(&approvals),
    };
    if !state.apply(event) && thread.is_none() {
        return Ok(());
    }
    let approvals = serde_json::to_string(&state.approvals).map_err(|e| e.to_string())?;
    sql(tx.execute(
        "INSERT INTO sessions (client, session, provider, account, thread, running, approvals)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (client, session) DO UPDATE SET provider = excluded.provider,
         account = excluded.account, thread = excluded.thread, running = excluded.running,
         approvals = excluded.approvals",
        params![
            client,
            session as i64,
            provider_name(route.provider),
            route.account,
            thread.map(str::to_owned).or(old_thread),
            state.running,
            approvals
        ],
    ))?;
    Ok(())
}

/// Drops deltas, then items, oldest first, until `client` holds at most `cap` bytes,
/// and records a gap for each connection whose items went. Returns the new size.
fn trim(tx: &Transaction, client: &str, size: i64, cap: i64) -> rusqlite::Result<i64> {
    let mut size = drop_oldest(tx, client, DELTA, size, cap, &mut BTreeMap::new())?;
    if size <= cap {
        return Ok(size);
    }
    let mut gaps = BTreeMap::new();
    size = drop_oldest(tx, client, ITEM, size, cap, &mut gaps)?;
    for ((name, account), (from, to, sessions)) in gaps {
        let Some(provider) = parse_provider(&name) else {
            continue;
        };
        let mut from = from as u64;
        let mut sessions: BTreeSet<u64> = sessions.into_iter().map(|s| s as u64).collect();
        // One run of lost items is one gap: when nothing was kept for the connection
        // between the last gap and these items, that gap grows to cover them.
        let newest: Option<(i64, Vec<u8>)> = tx
            .query_row(
                "SELECT seq, frame FROM events WHERE client = ?1 AND class = ?2
                 AND provider = ?3 AND account IS ?4 AND seq < ?5 ORDER BY seq DESC LIMIT 1",
                params![client, KEPT, name, account, from as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((seq, line)) = newest
            && let Ok(Frame::Gap {
                from: earlier,
                sessions: named,
                ..
            }) = frame::parse(line.strip_suffix(b"\n").unwrap_or(&line))
        {
            tx.execute(
                "DELETE FROM events WHERE client = ?1 AND seq = ?2",
                params![client, seq],
            )?;
            size -= line.len() as i64;
            from = from.min(earlier);
            sessions.extend(named);
        }
        let gap = Frame::Gap {
            provider,
            account: account.clone(),
            from,
            to: to as u64,
            sessions: sessions.into_iter().collect(),
        };
        let line = frame::encode(&gap).expect("a gap fits a frame");
        tx.execute(
            "INSERT INTO events (client, seq, class, provider, account, session, frame)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![client, to, KEPT, name, account, line],
        )?;
        size += line.len() as i64;
    }
    Ok(size)
}

type Gaps = BTreeMap<(String, Option<String>), (i64, i64, BTreeSet<i64>)>;

fn drop_oldest(
    tx: &Transaction,
    client: &str,
    class: i64,
    size: i64,
    cap: i64,
    gaps: &mut Gaps,
) -> rusqlite::Result<i64> {
    let mut freed = 0;
    let mut last = None;
    {
        let mut statement = tx.prepare(
            "SELECT seq, length(frame), provider, account, session FROM events
             WHERE client = ?1 AND class = ?2 ORDER BY seq",
        )?;
        let mut rows = statement.query(params![client, class])?;
        while size - freed > cap {
            let Some(row) = rows.next()? else { break };
            let seq: i64 = row.get(0)?;
            freed += row.get::<_, i64>(1)?;
            last = Some(seq);
            let (from, to, sessions) =
                gaps.entry((row.get(2)?, row.get(3)?))
                    .or_insert((seq, seq, BTreeSet::new()));
            *from = (*from).min(seq);
            *to = (*to).max(seq);
            sessions.insert(row.get(4)?);
        }
    }
    if let Some(last) = last {
        tx.execute(
            "DELETE FROM events WHERE client = ?1 AND class = ?2 AND seq <= ?3",
            params![client, class, last],
        )?;
    }
    Ok(size - freed)
}

/// What a Claude driver reports when its process exits during a turn
/// (`ClaudeDriver::on_exit`), for a turn the host lost when it restarted.
pub(crate) fn restarted(approvals: &[Approval]) -> Vec<Event> {
    let mut events = vec![Event::RecoveryNeeded];
    events.extend(resolved(approvals));
    events.push(Event::TurnCompleted {
        status: TurnStatus::Failed,
        error: Some("keystone-host restarted, so the turn ended".into()),
        stopped: false,
    });
    events.push(Event::ModelLost);
    events
}

/// The end of a turn whose process the app asked the host to end when it quit.
pub(crate) fn quit(approvals: &[Approval]) -> Vec<Event> {
    let mut events: Vec<Event> = resolved(approvals).collect();
    events.push(Event::TurnCompleted {
        status: TurnStatus::Interrupted,
        error: None,
        stopped: true,
    });
    events.push(Event::ModelLost);
    events
}

fn resolved(approvals: &[Approval]) -> impl Iterator<Item = Event> + '_ {
    approvals.iter().map(|approval| Event::ApprovalResolved {
        request: approval.request.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema keystone-host wrote at version 1.
    const VERSION_1: &str = "
        CREATE TABLE clients (client TEXT PRIMARY KEY, latest INTEGER NOT NULL DEFAULT 0,
            acked INTEGER NOT NULL DEFAULT 0, size INTEGER NOT NULL DEFAULT 0,
            acked_call INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
        CREATE TABLE events (client TEXT NOT NULL, seq INTEGER NOT NULL,
            class INTEGER NOT NULL, provider TEXT NOT NULL, account TEXT,
            session INTEGER NOT NULL, frame BLOB NOT NULL,
            PRIMARY KEY (client, seq)) WITHOUT ROWID;
        CREATE INDEX events_by_class ON events (client, class, seq);
        CREATE TABLE calls (client TEXT NOT NULL, id INTEGER NOT NULL, reply TEXT NOT NULL,
            PRIMARY KEY (client, id)) WITHOUT ROWID;
        CREATE TABLE sessions (client TEXT NOT NULL, session INTEGER NOT NULL,
            provider TEXT NOT NULL, account TEXT, thread TEXT,
            running INTEGER NOT NULL DEFAULT 0, approvals TEXT NOT NULL DEFAULT '[]',
            PRIMARY KEY (client, session)) WITHOUT ROWID;
        PRAGMA user_version = 1;";

    /// What version 3 added to version 1: a call accepted before its reply, and each
    /// session's provider process.
    const VERSION_3: &str = "
        ALTER TABLE calls RENAME TO calls_1;
        CREATE TABLE calls (client TEXT NOT NULL, id INTEGER NOT NULL, reply TEXT,
            PRIMARY KEY (client, id)) WITHOUT ROWID;
        INSERT INTO calls SELECT client, id, reply FROM calls_1;
        DROP TABLE calls_1;
        ALTER TABLE sessions ADD COLUMN process INTEGER;
        ALTER TABLE sessions ADD COLUMN started INTEGER;
        PRAGMA user_version = 3;";

    /// Opens a journal written at an older version, holding a replied call and a
    /// session with a running turn, and checks it kept both and works as new.
    fn upgrades(old: &[&str]) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let db = Connection::open(&path).unwrap();
        for step in old {
            db.execute_batch(step).unwrap();
        }
        db.execute_batch(
            "INSERT INTO calls VALUES ('app', 1, '{\"ok\":null}');
             INSERT INTO sessions (client, session, provider, running)
                 VALUES ('app', 12, 'Claude', 1);",
        )
        .unwrap();
        drop(db);
        let mut journal = Journal::open(&path, CAP).unwrap();
        assert!(matches!(
            journal.accept("app", 1).unwrap(),
            Accepted::Replied(Outcome::Ok(()))
        ));
        assert!(matches!(journal.accept("app", 2).unwrap(), Accepted::New));
        assert!(matches!(
            journal.accept("app", 2).unwrap(),
            Accepted::Unanswered
        ));
        journal.reply("app", 2, &Outcome::Err("no".into())).unwrap();
        assert!(matches!(
            journal.accept("app", 2).unwrap(),
            Accepted::Replied(Outcome::Err(reason)) if reason == "no"
        ));
        assert_eq!(journal.interrupted(None).unwrap().len(), 1);
        let route = Route {
            provider: Provider::Claude,
            account: None,
        };
        journal
            .append("app", &route, &[(12, Event::RecoveryNeeded)])
            .unwrap();
        let columns: Vec<String> = journal
            .db
            .prepare("SELECT name FROM pragma_table_info('sessions')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!columns.iter().any(|c| c == "process" || c == "started"));
        drop(journal);
        let journal = Journal::open(&path, CAP).unwrap();
        assert_eq!(journal.interrupted(None).unwrap().len(), 1, "opening again");
    }

    #[test]
    fn a_version_1_journal_keeps_what_it_held_and_gains_what_is_new() {
        upgrades(&[VERSION_1]);
    }

    #[test]
    fn a_version_3_journal_keeps_what_it_held_and_loses_its_process_columns() {
        upgrades(&[VERSION_1, VERSION_3]);
    }

    #[test]
    fn a_gap_takes_the_next_number_and_names_its_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&dir.path().join("journal.sqlite3"), CAP).unwrap();
        let route = Route {
            provider: Provider::Claude,
            account: None,
        };
        journal
            .append("app", &route, &[(12, Event::TurnStarted)])
            .unwrap();
        let line = journal
            .gap("app", &route, &BTreeSet::from([12, 15]))
            .unwrap();
        assert_eq!(
            frame::parse(line.strip_suffix(b"\n").unwrap()).unwrap(),
            Frame::Gap {
                provider: Provider::Claude,
                account: None,
                from: 2,
                to: 2,
                sessions: vec![12, 15],
            }
        );
        assert_eq!(journal.cursor("app").unwrap().latest, 2);
        assert_eq!(journal.replay("app", 1, 2).unwrap(), [line]);
    }
}
