//! `keystone-host serve`: the daemon that runs provider processes for the apps linked
//! to this machine. One writer thread owns the journal, the attachments and the
//! connections, so nothing is appended between the steps of a replay.

use super::cache::Cache;
use super::frame::{
    self, Attached, Call, Found, Frame, Login, Op, Outcome, PROTOCOL, SessionState, Welcome,
};
use super::journal::{self, Accepted, Journal};
use super::paths::{Paths, private_dir};
use super::reap::{self, Watching};
use super::{Route, fault};
use crate::connection::command_session;
use crate::{ClaudeConnection, CodexConnection, Command, Connection, Event, LaunchProfile};
use keystone_core::Provider;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(250);
const PING_EVERY: Duration = Duration::from_secs(5);
const SILENCE: Duration = Duration::from_secs(15);
/// How long the daemon stays with no app attached and no provider process.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// How long a new daemon waits for the lock, which a daemon that is exiting still holds.
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// How long a new daemon waits for the last one's watchers before it serves anyone.
const RELEASE_WAIT: Duration = Duration::from_secs(3);
/// The least `KEYSTONE_HOST_QUEUE_LIMIT` may be. A replay never counts against the
/// limit, so this is room for any reply, warning or ordinary event sent live; an event
/// past it costs the reader a reconnect, and the replay brings it.
const QUEUE_FLOOR: u64 = 64 * 1024;
/// How long a session may stay held for its last run's processes before it says so.
const HOLD: Duration = Duration::from_secs(60);

const NO_LOGIN: &str = "The app didn't say where this account keeps its files";
const NO_IMAGES: &str = "An image went with this message as a file here, so it wasn't sent";
const UNSAVED: &str = "The machine couldn't save this session's latest events, so they wait there and arrive once it can";
const UNANSWERED: &str =
    "The machine lost track of this call when keystone-host stopped, so it may or may not have run";
const STOPPING: &str = "The machine is still stopping the processes of this session's last run, so it can't start another turn yet";
const BACKLOGGED: &str = "The machine can't save events, and so many wait to be saved that it starts no more turns until it can";

macro_rules! log {
    ($($arg:tt)*) => { eprintln!("keystone-host: {}", format_args!($($arg)*)) };
}

/// Runs the daemon until it has been idle long enough. Returns at once when another
/// daemon already serves this user.
pub fn serve() -> Result<(), String> {
    let paths = Paths::from_env()?;
    private_dir(&paths.run)?;
    private_dir(&paths.state)?;
    let Some(_lock) = lock(&paths.lock())? else {
        return Ok(());
    };
    let cap = setting("KEYSTONE_HOST_JOURNAL_CAP", journal::CAP)?;
    // As much live output as a full journal holds, and two frames beyond it.
    let queue_limit = setting(
        "KEYSTONE_HOST_QUEUE_LIMIT",
        cap.saturating_add(2 * frame::MAX_FRAME as u64),
    )?;
    if queue_limit < QUEUE_FLOOR {
        return Err(format!(
            "KEYSTONE_HOST_QUEUE_LIMIT must be at least {QUEUE_FLOOR} bytes, not {queue_limit}"
        ));
    }
    let idle =
        setting("KEYSTONE_HOST_IDLE_MS", IDLE.as_millis() as u64).map(Duration::from_millis)?;
    let hold =
        setting("KEYSTONE_HOST_HOLD_MS", HOLD.as_millis() as u64).map(Duration::from_millis)?;
    let journal = Journal::open(&paths.journal(), cap)?;
    let cache = Cache::open(paths.attachments())?;
    let program = watcher_program()?;
    // Listening first: an `attach` that started this daemon waits only so long for the
    // socket, and the last daemon's watchers may take longer.
    let socket = paths.socket();
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .map_err(|e| format!("Could not listen on {}: {e}", socket.display()))?;
    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        thread::Builder::new()
            .name("host-accept".into())
            .spawn(move || accept(listener, tx))
            .map_err(|e| e.to_string())?;
    }
    let mut daemon = Daemon {
        journal,
        tx,
        welcome: welcome(),
        claude: std::env::var_os("KEYSTONE_CLAUDE_PATH").map(PathBuf::from),
        codex: std::env::var_os("KEYSTONE_CODEX_PATH").map(PathBuf::from),
        cache,
        program,
        run: paths.run.clone(),
        backlog_bound: usize::try_from(cap).unwrap_or(usize::MAX),
        backlog_limit: usize::try_from(cap.saturating_mul(2)).unwrap_or(usize::MAX),
        refusing: false,
        idle,
        queue_limit,
        hold,
        started: Instant::now(),
        sockets: HashMap::new(),
        attached: HashMap::new(),
        conns: HashMap::new(),
        held: HashMap::new(),
        backlog: HashMap::new(),
        quiet_since: None,
        names: HashMap::new(),
    };
    daemon.recover(reap::left(&paths.run, RELEASE_WAIT))?;
    daemon.run(rx);
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// The binary each watcher runs: this daemon's own. Linux keeps it reachable through
/// `/proc/self/exe` after an install replaces or deletes the file. macOS can't run a
/// binary through a descriptor, so there it is the path this daemon started from,
/// which an install replaces by renaming a new binary into place. A binary that can't
/// watch fails the handshake, and the start fails with it.
fn watcher_program() -> Result<PathBuf, String> {
    if cfg!(target_os = "linux") {
        Ok(PathBuf::from("/proc/self/exe"))
    } else {
        std::env::current_exe().map_err(|e| format!("Could not find keystone-host itself: {e}"))
    }
}

/// Takes the lock beside the socket and writes this process's ID into it, or returns
/// `None` when another daemon still holds it after [`LOCK_WAIT`].
fn lock(path: &Path) -> Result<Option<File>, String> {
    let mut file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("Could not open {}: {e}", path.display()))?;
    let deadline = Instant::now() + LOCK_WAIT;
    // SAFETY: flock only reads the descriptor, which `file` keeps open.
    while unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(25));
    }
    file.set_len(0).map_err(|e| e.to_string())?;
    write!(file, "{}", std::process::id()).map_err(|e| e.to_string())?;
    Ok(Some(file))
}

/// The whole number in variable `name`, or `default` when it is unset or empty.
fn setting(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => value
            .parse()
            .map_err(|_| format!("{name} must be a whole number, not {value:?}")),
        _ => Ok(default),
    }
}

fn welcome() -> Welcome {
    let found = [
        (Provider::Claude, "KEYSTONE_CLAUDE_PATH", "claude"),
        (Provider::Codex, "KEYSTONE_CODEX_PATH", "codex"),
    ]
    .map(|(provider, var, name)| {
        let program = std::env::var_os(var)
            .map(PathBuf::from)
            .or_else(|| crate::find::program(name));
        thread::spawn(move || {
            let result = match program {
                Some(program) => version(&program),
                None => Err("not on PATH".to_owned()),
            };
            let (version, missing) = match result {
                Ok(version) => (Some(version), None),
                Err(missing) => (None, Some(missing)),
            };
            Found {
                provider,
                version,
                missing,
            }
        })
    });
    let mut hostname = [0u8; 256];
    // SAFETY: gethostname writes at most the buffer's length.
    let ok = unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) } == 0;
    let end = hostname.iter().position(|b| *b == 0).unwrap_or(0);
    Welcome {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        hostname: if ok {
            String::from_utf8_lossy(&hostname[..end]).into_owned()
        } else {
            String::new()
        },
        providers: found
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect(),
        oldest: 0,
        latest: 0,
        journal: String::new(),
    }
}

/// The first line `program --version` prints, within 2 s.
fn version(program: &Path) -> Result<String, String> {
    let (tx, rx) = mpsc::channel();
    let program = program.to_owned();
    thread::spawn(move || {
        let _ = tx.send(
            std::process::Command::new(&program)
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output(),
        );
    });
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(output)) if output.status.success() => Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect()),
        Ok(Ok(output)) => Err(format!("--version failed with {}", output.status)),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("--version did not answer".into()),
    }
}

type Key = (String, Route);

enum Msg {
    Opened(u64, UnixStream),
    Frame(u64, Box<Frame>),
    Closed(u64),
    Wake(Key),
}

fn accept(listener: UnixListener, tx: Sender<Msg>) {
    for (id, stream) in (0u64..).zip(listener.incoming()) {
        let Ok(stream) = stream else { continue };
        let Ok(reading) = stream.try_clone() else {
            continue;
        };
        if tx.send(Msg::Opened(id, stream)).is_err() {
            return;
        }
        let tx = tx.clone();
        let _ = thread::Builder::new()
            .name("host-read".into())
            .spawn(move || read(id, reading, tx));
    }
}

fn read(id: u64, stream: UnixStream, tx: Sender<Msg>) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        match frame::read(&mut reader, &mut line) {
            Ok(Some(frame)) => {
                if tx.send(Msg::Frame(id, Box::new(frame))).is_err() {
                    return;
                }
            }
            Ok(None) => break,
            Err(error) => {
                log!("closing a link: {error}");
                break;
            }
        }
    }
    let _ = tx.send(Msg::Closed(id));
}

/// Writes what the daemon queues, in order, adding it to `written`. Once the daemon
/// drops the sender, it writes what is left and closes the socket, which ends that
/// `attach`.
fn write(mut stream: UnixStream, rx: Receiver<Vec<u8>>, written: Arc<AtomicU64>) {
    for line in rx {
        if stream.write_all(&line).is_err() {
            break;
        }
        written.fetch_add(line.len() as u64, Ordering::Relaxed);
    }
    end(&stream);
}

/// Shuts the socket down, which ends a writer blocked on it and the `attach` on the
/// other side. The halves go one at a time: once the other side has closed its own,
/// macOS refuses both together, though the write half alone still works.
fn end(stream: &UnixStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.shutdown(std::net::Shutdown::Read);
}

struct Socket {
    out: Sender<Vec<u8>>,
    /// Bytes queued for the socket since it opened.
    queued: u64,
    /// Bytes its writer has written since it opened.
    written: Arc<AtomicU64>,
    /// Shut down to end the socket at once, even while its writer is blocked.
    stream: UnixStream,
    client: Option<String>,
    /// `queued` once its replay was queued. Events appended before then reach it
    /// through the replay, and only events appended after go to it live. Only what
    /// is queued after counts against the limit: a reader that keeps up with live
    /// events is never disconnected over the size of its replay.
    live: Option<u64>,
    heard: Instant,
    pinged: Instant,
}

/// Whether the journal has `state`'s session on `route`, the provider and account its
/// turns run with.
fn on_route(state: &SessionState, route: &Route) -> bool {
    state.provider == route.provider && state.account == route.account
}

/// Drops `conn` on a thread of its own: dropping one waits for a provider that is
/// still starting, and the daemon's thread serves every client.
fn release(conn: Option<Conn>) {
    if let Some(conn) = conn {
        let _ = thread::Builder::new()
            .name("host-release".into())
            .spawn(move || drop(conn));
    }
}

/// One provider's connection: a Claude process per session, or one Codex app-server
/// for every session of the account.
enum Runner {
    Claude(ClaudeConnection),
    Codex(CodexConnection),
}

impl Runner {
    fn connection(&self) -> &dyn Connection {
        match self {
            Runner::Claude(claude) => claude,
            Runner::Codex(codex) => codex,
        }
    }
}

struct Conn {
    runner: Runner,
    /// The login it runs as, `None` for the default; a call with another, after the
    /// app changed the account's folder, gets a new connection.
    login: Option<Login>,
    /// Every session this connection served, for ending idle processes.
    sessions: BTreeSet<u64>,
}

impl Conn {
    /// Whether a process of this connection still has work for a session: a turn, an
    /// approval or a message waiting.
    fn is_busy(&self) -> bool {
        match &self.runner {
            Runner::Claude(claude) => claude.is_busy(),
            Runner::Codex(codex) => self.sessions.iter().any(|s| codex.has_work(*s)),
        }
    }

    /// Ends the session's process once it is idle. Codex's one app-server serves every
    /// session, so it ends only with the connection, once no session is busy.
    fn end_idle(&self, session: u64) {
        if let Runner::Claude(claude) = &self.runner {
            claude.end_idle(session);
        }
    }
}

/// Why the host refused a call before it ran: for the call itself, or for the account's
/// login, which the app then says how to fix.
enum Refused {
    Call(String),
    Login(String),
}

impl From<String> for Refused {
    fn from(reason: String) -> Self {
        Refused::Call(reason)
    }
}

impl From<&str> for Refused {
    fn from(reason: &str) -> Self {
        Refused::Call(reason.to_owned())
    }
}

/// The CLI's name, for what the host says about its processes.
fn cli_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude Code",
        Provider::Codex => "Codex",
    }
}

/// How the host runs `login`'s CLI on `machine`, the app's name for this machine:
/// with its config home, which must be a full path with no `.` or `..` part, made
/// private to this user when it doesn't exist yet, as the app makes a local one. It
/// must overlap neither provider's default home here, checked before anything is made
/// and again on the folder's real path once it exists, and the CLI is given that real
/// path, so a symlink can't lead it into a default home.
fn login_profile(
    provider: Provider,
    login: &Login,
    machine: &str,
) -> Result<LaunchProfile, String> {
    let label = &login.label;
    let home = Path::new(&login.home);
    let dotted = login
        .home
        .split('/')
        .any(|part| part == "." || part == "..");
    if !home.is_absolute() || dotted {
        return Err(format!(
            "The account \u{201c}{label}\u{201d} needs a full path on {machine} without . or .. in it, not {}",
            login.home
        ));
    }
    if home.parent().is_none() {
        return Err(format!(
            "The account \u{201c}{label}\u{201d} can't use {}, which is the whole of {machine}'s disk",
            login.home
        ));
    }
    let overlap = |real: &Path| -> Result<(), String> {
        let defaults = [Provider::Claude, Provider::Codex].map(crate::default_home);
        match defaults
            .iter()
            .flatten()
            .find(|default| keystone_core::homes_overlap(&resolved(default), real))
        {
            Some(default) => Err(format!(
                "The account \u{201c}{label}\u{201d} uses {}, which overlaps {machine}'s default home {}",
                login.home,
                default.display()
            )),
            None => Ok(()),
        }
    };
    overlap(&resolved(home))?;
    if !home.exists() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(home)
            .map_err(|e| format!("Could not make {} on {machine}: {e}", login.home))?;
    }
    let real = home
        .canonicalize()
        .map_err(|e| format!("Could not open {} on {machine}: {e}", login.home))?;
    if !real.is_dir() {
        return Err(format!("{} isn't a folder on {machine}", login.home));
    }
    overlap(&real)?;
    Ok(LaunchProfile::account(provider, label, &real))
}

/// `path` with its deepest existing folder resolved through its symlinks, and the
/// rest as written, so a home that doesn't exist yet compares as it will. Callers
/// refuse `.` and `..` first, so the rest is taken as it is.
fn resolved(path: &Path) -> PathBuf {
    for base in path.ancestors() {
        if let Ok(real) = base.canonicalize() {
            let rest = path.strip_prefix(base).unwrap_or(Path::new(""));
            return real.join(rest);
        }
    }
    path.to_path_buf()
}

/// A session of the last daemon whose watchers still end its processes. It starts no
/// turn until they are gone. A Codex app-server serves every session of its account,
/// so its watchers hold all of them: `others` are the ones whose turns end with it.
struct Held {
    route: Route,
    left: Vec<reap::Left>,
    others: BTreeSet<u64>,
    /// Set once the session was told the processes haven't stopped.
    warned: bool,
    /// The app quit asking to end its sessions, so the turn ends as Quit ends it.
    quit: bool,
}

/// The most of a backlog one append tries at once, so a journal that keeps failing
/// costs each retry only this much.
const BATCH: usize = 256;

/// What a connection reported that the journal hasn't taken yet, oldest first.
#[derive(Default)]
struct Backlog {
    waiting: VecDeque<Waiting>,
    /// What `waiting` holds, as JSON.
    bytes: usize,
    /// The attachment already warned about it.
    warned: Option<u64>,
    /// Why the journal last refused it.
    error: String,
}

enum Waiting {
    Event {
        session: u64,
        event: Event,
        bytes: usize,
    },
    /// Item events of these sessions the backlog dropped to stay within its bound.
    Gap(BTreeSet<u64>),
}

/// The oldest of a backlog, to append as one.
enum Batch {
    Events(Vec<(u64, Event)>),
    Gap(BTreeSet<u64>),
}

impl Batch {
    fn len(&self) -> usize {
        match self {
            Batch::Events(events) => events.len(),
            Batch::Gap(_) => 1,
        }
    }

    fn sessions(&self) -> BTreeSet<u64> {
        match self {
            Batch::Events(events) => events.iter().map(|(session, _)| *session).collect(),
            Batch::Gap(sessions) => sessions.clone(),
        }
    }
}

impl Backlog {
    /// Adds `events` behind what waits, then drops what the journal would drop past
    /// its cap until the backlog fits in `bound`: the oldest deltas first, then the
    /// oldest items, leaving a gap where they were. Lifecycle events always stay.
    fn push(&mut self, events: Vec<(u64, Event)>, bound: usize) {
        for (session, event) in events {
            let bytes = serde_json::to_vec(&event).map_or(0, |json| json.len());
            self.bytes += bytes;
            self.waiting.push_back(Waiting::Event {
                session,
                event,
                bytes,
            });
        }
        self.drop_oldest(bound, |event| matches!(event, Event::ItemDelta { .. }));
        let (kept, dropped) = self.drop_oldest(bound, |event| {
            matches!(event, Event::ItemStarted(_) | Event::ItemCompleted(_))
        });
        if dropped.is_empty() {
            return;
        }
        // One run of lost items is one gap.
        if let Some(Waiting::Gap(sessions)) =
            kept.checked_sub(1).and_then(|at| self.waiting.get_mut(at))
        {
            sessions.extend(dropped);
        } else {
            self.waiting.insert(kept, Waiting::Gap(dropped));
        }
    }

    /// Drops the oldest events `droppable` allows while over `bound`. Returns how many
    /// entries were kept before the last one dropped, and the sessions dropped from.
    fn drop_oldest(
        &mut self,
        bound: usize,
        droppable: impl Fn(&Event) -> bool,
    ) -> (usize, BTreeSet<u64>) {
        let mut excess = self.bytes.saturating_sub(bound);
        let (mut seen, mut gone, mut kept) = (0, 0, 0);
        let mut dropped = BTreeSet::new();
        self.waiting.retain(|waiting| {
            seen += 1;
            match waiting {
                Waiting::Event {
                    session,
                    event,
                    bytes,
                } if excess > 0 && droppable(event) => {
                    excess = excess.saturating_sub(*bytes);
                    self.bytes -= bytes;
                    dropped.insert(*session);
                    kept = seen - 1 - gone;
                    gone += 1;
                    false
                }
                _ => true,
            }
        });
        (kept, dropped)
    }

    /// The oldest of what waits: a gap, or up to [`BATCH`] events.
    fn batch(&self) -> Option<Batch> {
        match self.waiting.front()? {
            Waiting::Gap(sessions) => Some(Batch::Gap(sessions.clone())),
            Waiting::Event { .. } => Some(Batch::Events(
                self.waiting
                    .iter()
                    .take(BATCH)
                    .map_while(|waiting| match waiting {
                        Waiting::Event { session, event, .. } => Some((*session, event.clone())),
                        Waiting::Gap(_) => None,
                    })
                    .collect(),
            )),
        }
    }

    /// The journal took the oldest `count`.
    fn taken(&mut self, count: usize) {
        for waiting in self.waiting.drain(..count) {
            if let Waiting::Event { bytes, .. } = waiting {
                self.bytes -= bytes;
            }
        }
    }

    fn events(&self) -> impl Iterator<Item = (u64, &Event)> {
        self.waiting.iter().filter_map(|waiting| match waiting {
            Waiting::Event { session, event, .. } => Some((*session, event)),
            Waiting::Gap(_) => None,
        })
    }
}

struct Daemon {
    journal: Journal,
    tx: Sender<Msg>,
    welcome: Welcome,
    claude: Option<PathBuf>,
    codex: Option<PathBuf>,
    /// The images the app sent, by hash.
    cache: Cache,
    /// The binary each watcher runs.
    program: PathBuf,
    /// The folder each watcher's lock file goes in.
    run: PathBuf,
    /// The most a connection's backlog holds, as the journal's cap.
    backlog_bound: usize,
    /// Past this many bytes in every backlog together, no turn starts. Twice the cap:
    /// the drop rule keeps a backlog at or under the cap while it holds transcript to
    /// drop, so only lifecycle events, catalogs and warnings carry it past, and another
    /// cap's worth of those is hours of busy sessions. The daemon's memory for events
    /// that wait stays within twice the cap, plus what running turns still add.
    backlog_limit: usize,
    /// Set while new turns are refused for the backlog, so the log says so once.
    refusing: bool,
    idle: Duration,
    /// The most an attachment may have queued. One that falls further behind is
    /// disconnected and catches up from the journal when it reconnects.
    queue_limit: u64,
    /// How long a session stays held before it is told why.
    hold: Duration,
    started: Instant,
    sockets: HashMap<u64, Socket>,
    /// Each client's current attachment.
    attached: HashMap<String, u64>,
    conns: HashMap<Key, Conn>,
    held: HashMap<(String, u64), Held>,
    /// Nothing of a connection goes out live past what waits here.
    backlog: HashMap<Key, Backlog>,
    quiet_since: Option<Instant>,
    /// What each client calls this machine, from its last `hello`.
    names: HashMap<String, String>,
}

impl Daemon {
    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if self.tick() {
                return;
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Opened(id, stream) => {
                let Ok(writing) = stream.try_clone() else {
                    return;
                };
                let (out, rx) = mpsc::channel();
                let written = Arc::new(AtomicU64::new(0));
                let counted = written.clone();
                let _ = thread::Builder::new()
                    .name("host-write".into())
                    .spawn(move || write(writing, rx, counted));
                let now = Instant::now();
                self.sockets.insert(
                    id,
                    Socket {
                        out,
                        queued: 0,
                        written,
                        stream,
                        client: None,
                        live: None,
                        heard: now,
                        pinged: now,
                    },
                );
            }
            Msg::Frame(id, frame) => self.frame(id, *frame),
            Msg::Closed(id) => self.disconnect(id),
            Msg::Wake(key) => self.pump(&key),
        }
    }

    /// Holds each session whose provider processes a watcher of the last daemon is
    /// still ending, and ends the running turns of the rest, as a local provider crash
    /// does. A held session's turn ends once its watchers let go.
    fn recover(&mut self, left: Vec<reap::Left>) -> Result<(), String> {
        for left in left {
            let key = (left.watching.client.clone(), left.watching.session);
            log!(
                "session {} waits for the last run's processes to end",
                key.1
            );
            self.held
                .entry(key)
                .or_insert_with(|| Held {
                    route: left.watching.route(),
                    left: Vec::new(),
                    others: BTreeSet::new(),
                    warned: false,
                    quit: false,
                })
                .left
                .push(left);
        }
        for lost in self.journal.interrupted(None)? {
            if self.held.contains_key(&(lost.client.clone(), lost.session)) {
                continue;
            }
            let shared = self.held.iter_mut().find(|((client, _), held)| {
                *client == lost.client
                    && held.route == lost.route
                    && lost.route.provider == Provider::Codex
            });
            if let Some((_, held)) = shared {
                held.others.insert(lost.session);
                continue;
            }
            let events = journal::restarted(&lost.approvals);
            self.enqueue(&(lost.client, lost.route), lost.session, events);
        }
        self.release();
        Ok(())
    }

    /// Lets each held session go once its watchers have, ending its turn if one ran.
    /// One still held after [`Daemon::hold`] is told which processes haven't stopped.
    /// A watcher that stopped before its tree was gone is replaced while its provider
    /// runs, and otherwise its session is told what may still run.
    fn release(&mut self) {
        let mut released = Vec::new();
        let mut stuck = Vec::new();
        let mut lost = Vec::new();
        let late = self.started.elapsed() >= self.hold;
        for (key, held) in &mut self.held {
            for left in std::mem::take(&mut held.left) {
                match left.watcher() {
                    reap::Watcher::Holds => held.left.push(left),
                    reap::Watcher::Finished => {}
                    reap::Watcher::Lost => match left.rewatch(&self.program, &self.run) {
                        Ok(left) => held.left.push(left),
                        Err(pid) => lost.push((key.clone(), held.route.clone(), pid)),
                    },
                }
            }
            if held.left.is_empty() {
                released.push(key.clone());
            } else if late && !held.warned {
                held.warned = true;
                let pids: Vec<u32> = held.left.iter().flat_map(reap::Left::stuck).collect();
                stuck.push((key.clone(), held.route.clone(), pids));
            }
        }
        for ((client, session), route, pid) in lost {
            log!(
                "session {session}'s last watcher stopped before provider {pid}'s processes were gone; they may still run"
            );
            let message = format!(
                "The watcher of this session's last run stopped unexpectedly, so processes its {} (process {pid}) started may still be running",
                cli_name(route.provider)
            );
            self.enqueue(&(client, route), session, vec![Event::Warning { message }]);
        }
        for ((client, session), route, pids) in stuck {
            log!("session {session} still waits for processes {pids:?} of its last run");
            let listed: Vec<String> = pids.iter().map(u32::to_string).collect();
            let message = if listed.is_empty() {
                "The machine can't stop the processes of this session's last run, so the session can't go on until they exit".to_owned()
            } else {
                format!(
                    "The machine can't stop processes {} of this session's last run, so the session can't go on until they exit",
                    listed.join(", ")
                )
            };
            self.enqueue(&(client, route), session, vec![Event::Warning { message }]);
        }
        for (client, session) in released {
            let mut states = match self.states(&client) {
                Ok(states) => states,
                // Still held, so the next tick tries again.
                Err(error) => {
                    log!("{error}");
                    continue;
                }
            };
            let Some(held) = self.held.remove(&(client.clone(), session)) else {
                continue;
            };
            log!("session {session}'s last run has ended");
            for session in std::iter::once(session).chain(held.others.iter().copied()) {
                let Some(state) = states
                    .remove(&session)
                    .filter(|s| s.running || !s.approvals.is_empty())
                else {
                    continue;
                };
                let events = if held.quit {
                    journal::quit(&state.approvals)
                } else {
                    journal::restarted(&state.approvals)
                };
                self.enqueue(&(client.clone(), held.route.clone()), session, events);
            }
        }
    }

    /// Each of `client`'s sessions as the journal has it, followed by the events that
    /// still wait for the journal.
    fn states(&self, client: &str) -> Result<BTreeMap<u64, SessionState>, String> {
        let mut states: BTreeMap<u64, SessionState> = self
            .journal
            .sessions(client)?
            .into_iter()
            .map(|state| (state.session, state))
            .collect();
        for ((owner, route), backlog) in &self.backlog {
            if owner != client {
                continue;
            }
            for (session, event) in backlog.events() {
                states
                    .entry(session)
                    .or_insert_with(|| SessionState {
                        provider: route.provider,
                        account: route.account.clone(),
                        session,
                        running: false,
                        approvals: Vec::new(),
                    })
                    .apply(event);
            }
        }
        Ok(states)
    }

    /// Journals `events` for `session` after whatever of its connection still waits.
    fn enqueue(&mut self, key: &Key, session: u64, events: Vec<Event>) {
        let events = events.into_iter().map(|event| (session, event)).collect();
        self.journal_events(key, events);
    }

    /// Pings attachments, drops silent ones, lets go of connections no detached client
    /// needs, and says whether the daemon has been idle long enough to exit.
    fn tick(&mut self) -> bool {
        let now = Instant::now();
        let mut silent = Vec::new();
        let mut ping = Vec::new();
        for (id, socket) in &mut self.sockets {
            if now.duration_since(socket.heard) >= SILENCE {
                silent.push(*id);
            } else if socket.client.is_some() && now.duration_since(socket.pinged) >= PING_EVERY {
                socket.pinged = now;
                ping.push(*id);
            }
        }
        for id in silent {
            log!("closing a link that went silent");
            self.disconnect(id);
        }
        for id in ping {
            self.send(id, &Frame::Ping);
        }
        let unneeded: Vec<Key> = self
            .conns
            .iter()
            .filter(|((client, _), conn)| !self.attached.contains_key(client) && !conn.is_busy())
            .map(|(key, _)| key.clone())
            .collect();
        for key in unneeded {
            self.let_go(
                &key,
                "keystone-host let go of its provider, so the turn ended",
            );
        }
        self.release();
        let waiting: Vec<Key> = self.backlog.keys().cloned().collect();
        for key in waiting {
            self.pump(&key);
        }
        if !self.sockets.is_empty() || !self.conns.is_empty() {
            self.quiet_since = None;
            return false;
        }
        let idle = now.duration_since(*self.quiet_since.get_or_insert(now)) >= self.idle;
        let unsaved: usize = self.backlog.values().map(|b| b.waiting.len()).sum();
        if idle && unsaved > 0 {
            log!(
                "exiting idle with {unsaved} events the journal never took; the next daemon ends any turn they leave running"
            );
        }
        idle
    }

    fn send(&mut self, id: u64, frame: &Frame) {
        match frame::encode(frame) {
            Ok(line) => self.send_line(id, line),
            Err(error) => log!("could not send a frame: {error}"),
        }
    }

    /// Queues `line` for the socket, or disconnects it if that would put it over the
    /// limit: nothing is lost, because the journal still holds what it missed.
    fn send_line(&mut self, id: u64, line: Vec<u8>) {
        let Some(socket) = self.sockets.get_mut(&id) else {
            return;
        };
        if let Some(live) = socket.live {
            let written = socket.written.load(Ordering::Relaxed).max(live);
            let behind = socket.queued.saturating_sub(written);
            if behind + line.len() as u64 > self.queue_limit {
                log!(
                    "disconnecting a link that fell {behind} bytes behind; it catches up from the journal"
                );
                return self.disconnect(id);
            }
        }
        socket.queued += line.len() as u64;
        let _ = socket.out.send(line);
    }

    /// Ends the socket at once, with what was queued for it, even while its writer is
    /// blocked on a reader that stopped.
    fn disconnect(&mut self, id: u64) {
        if let Some(socket) = self.sockets.get(&id) {
            end(&socket.stream);
        }
        self.close(id);
    }

    /// Forgets the socket, so a frame it sends later is ignored, and closes it once
    /// what was queued for it is written. Only a `refused` reply is sent this way: a
    /// reader that stopped would hold the writer, so everything else disconnects.
    fn close(&mut self, id: u64) {
        let Some(socket) = self.sockets.remove(&id) else {
            return;
        };
        if let Some(client) = socket.client
            && self.attached.get(&client) == Some(&id)
        {
            self.attached.remove(&client);
            self.detached(&client);
        }
    }

    fn frame(&mut self, id: u64, frame: Frame) {
        let Some(socket) = self.sockets.get_mut(&id) else {
            return;
        };
        socket.heard = Instant::now();
        let client = socket.client.clone();
        match (frame, client) {
            (
                Frame::Hello {
                    protocol,
                    client,
                    after,
                    journal,
                    seen,
                    name,
                },
                None,
            ) => {
                if let Some(name) = name.filter(|name| !name.is_empty() && name.len() <= 256) {
                    self.names.insert(client.clone(), name);
                }
                let places = after.zip(journal.clone()).into_iter();
                self.went_back(&client, places.chain(seen.into_iter().map(|(j, s)| (s, j))));
                // A place in another journal is no place in this one.
                let after = after.filter(|_| journal.as_deref() == Some(self.journal.id()));
                self.hello(id, protocol, client, after)
            }
            (Frame::Call(call), Some(client)) => self.call(id, &client, call),
            (Frame::Ack { seq, calls }, Some(client)) => {
                if let Err(error) = self.journal.ack(&client, seq, calls) {
                    log!("{error}");
                }
            }
            (Frame::Quit { keep_running }, Some(client)) => {
                if !keep_running {
                    self.end_sessions(&client);
                }
            }
            (Frame::Ping, _) => self.send(id, &Frame::Pong),
            (Frame::Pong, _) => {}
            (frame, _) => {
                log!("closing a link that sent an unexpected frame: {frame:?}");
                self.disconnect(id);
            }
        }
    }

    /// Takes a new journal identity when the app has a place in this journal past its
    /// latest event for `client`: the journal went back, as a restored backup or a
    /// damaged file would, so its numbers past that point may name other events. With
    /// a new identity, every place the app had counts for nothing, as after any other
    /// journal replacement, and that lasts, since the old identity is gone.
    fn went_back(&mut self, client: &str, places: impl Iterator<Item = (u64, String)>) {
        let places: Vec<(u64, String)> = places.collect();
        let latest = match self.journal.cursor(client) {
            Ok(cursor) => cursor.latest,
            Err(error) => {
                log!("{error}");
                return;
            }
        };
        let own = self.journal.id().to_owned();
        if places
            .iter()
            .any(|(seq, journal)| *journal == own && *seq > latest)
        {
            match self.journal.rotate() {
                Ok(()) => {
                    log!("the journal went back past what the app saw, so it has a new identity")
                }
                Err(error) => log!("{error}"),
            }
        }
    }

    /// Attaches the socket for `client`, replacing any attachment it had, then sends
    /// every journaled event after `after` through the journal's end, and `synced`.
    /// Only then does the attachment go live, so every event reaches it once: through
    /// the replay if it was appended before that point, live if after.
    fn hello(&mut self, id: u64, protocol: u32, client: String, after: Option<u64>) {
        let refusal = if protocol != PROTOCOL {
            Some(format!(
                "protocol {protocol} is needed, this host speaks {PROTOCOL} (keystone-host {})",
                env!("CARGO_PKG_VERSION")
            ))
        } else if client.is_empty() || client.len() > 256 {
            Some("a client id must be 1 to 256 bytes".into())
        } else {
            None
        };
        if let Some(reason) = refusal {
            self.send(id, &Frame::Refused { reason });
            self.close(id);
            return;
        }
        if let Some(old) = self.attached.insert(client.clone(), id) {
            self.disconnect(old);
        }
        if let Some(socket) = self.sockets.get_mut(&id) {
            socket.client = Some(client.clone());
        }
        if let Err(error) = self.replay(id, &client, after) {
            log!("{error}");
            self.disconnect(id);
        }
    }

    fn replay(&mut self, id: u64, client: &str, after: Option<u64>) -> Result<(), String> {
        if after.is_none() {
            self.journal.forget_calls(client)?;
        }
        let cursor = self.journal.cursor(client)?;
        self.send(
            id,
            &Frame::Welcome(Welcome {
                oldest: cursor.oldest,
                latest: cursor.latest,
                journal: self.journal.id().to_owned(),
                ..self.welcome.clone()
            }),
        );
        if fault("append-during-replay") {
            thread::sleep(Duration::from_millis(200));
            let keys: Vec<Key> = self
                .conns
                .keys()
                .filter(|(owner, _)| owner == client)
                .cloned()
                .collect();
            for key in keys {
                self.pump(&key);
            }
        }
        let from = after.unwrap_or(0).max(cursor.acked);
        let end = self.journal.cursor(client)?.latest;
        for line in self.journal.replay(client, from, end)? {
            self.send_line(id, line);
        }
        self.send(id, &Frame::Synced { seq: end });
        if after.is_none() {
            let sessions = self.journal.sessions(client)?;
            self.send(id, &Frame::Sessions { sessions });
        }
        if let Some(socket) = self.sockets.get_mut(&id) {
            socket.live = Some(socket.queued);
        }
        Ok(())
    }

    /// Runs a call at most once: it runs only once the journal holds it as accepted.
    /// Sent again, it gets its recorded reply, or says it may have run when it was
    /// accepted and never answered.
    fn call(&mut self, id: u64, client: &str, call: Call) {
        let call_id = call.id;
        // Only reads the journal, so it may run any number of times.
        if let Op::Recall { send, .. } = &call.op {
            let outcome = match self.journal.recall(client, send) {
                Ok(None) => Outcome::Unrecorded,
                Ok(Some(Accepted::Replied(outcome))) => outcome,
                Ok(Some(_)) => Outcome::Unknown(UNANSWERED.into()),
                Err(error) => {
                    log!("{error}");
                    Outcome::Unknown(format!(
                        "keystone-host couldn't read what became of this message: {error}"
                    ))
                }
            };
            return self.send(
                id,
                &Frame::Reply {
                    id: call_id,
                    outcome,
                },
            );
        }
        let send = match &call.op {
            Op::Send { send, .. } => Some(send.clone()),
            _ => None,
        };
        let outcome = match self.journal.accept(client, call_id, send.as_deref()) {
            Ok(Accepted::New) => {
                let outcome = match self.perform(client, call) {
                    Ok(()) => Outcome::Ok(()),
                    Err(Refused::Call(reason)) => Outcome::Err(reason),
                    Err(Refused::Login(reason)) => Outcome::Login(reason),
                };
                if fault("crash-before-reply") {
                    thread::sleep(Duration::from_secs(2));
                    // SAFETY: kill only signals this process.
                    unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                }
                if let Err(error) = self
                    .journal
                    .reply(client, call_id, send.as_deref(), &outcome)
                {
                    log!("{error}");
                }
                outcome
            }
            Ok(Accepted::Replied(outcome)) => outcome,
            Ok(Accepted::Unanswered) => Outcome::Unknown(UNANSWERED.into()),
            Err(error) => {
                log!("{error}");
                Outcome::Err(format!(
                    "keystone-host couldn't record this call, so it didn't run it: {error}"
                ))
            }
        };
        self.send(
            id,
            &Frame::Reply {
                id: call_id,
                outcome,
            },
        );
    }

    fn perform(&mut self, client: &str, call: Call) -> Result<(), Refused> {
        let Call {
            provider,
            account,
            login,
            op,
            ..
        } = call;
        if let Op::PutAttachment {
            sha256,
            media_type,
            size,
            offset,
            data,
        } = &op
        {
            let attached = Attached {
                media_type: media_type.clone(),
                sha256: sha256.clone(),
            };
            return Ok(self.cache.put(&attached, *size, *offset, data)?);
        }
        let route = Route { provider, account };
        let key = (client.to_owned(), route);
        let session = match &op {
            Op::Send { session, .. }
            | Op::RequestCatalog { session, .. }
            | Op::SetChoice { session, .. }
            | Op::SetApproval { session, .. }
            | Op::HoldFollowUps { session }
            | Op::ReleaseFollowUps { session }
            | Op::EvictIdle { session, .. }
            | Op::DeclineIdle { session, .. }
            | Op::ReadHistory { session, .. }
            | Op::Recall { session, .. } => *session,
            Op::ListThreads { .. } | Op::ReadImport { .. } | Op::PutAttachment { .. } => 0,
            Op::Command(command) => command_session(command),
        };
        let starts = matches!(
            op,
            Op::Send { .. } | Op::Command(Command::Send { .. } | Command::Open { .. })
        );
        let reaches = matches!(
            op,
            Op::Command(Command::Answer { .. } | Command::Act { .. } | Command::Interrupt { .. })
        );
        if (starts || reaches) && self.holds(&key, session) {
            return Err(STOPPING.into());
        }
        if starts && let Some(error) = self.backlogged() {
            return Err(format!("{BACKLOGGED} ({error})").into());
        }
        if matches!(op, Op::Command(Command::Interrupt { .. })) && self.end_orphan(&key, session) {
            return Ok(());
        }
        let machine = self.machine_name(client);
        let images = match &op {
            Op::Send { images, .. } => images
                .iter()
                .map(|image| {
                    self.cache.image(image).map_err(|error| match error.as_str() {
                        super::cache::GONE => format!(
                            "An image in this message is no longer on {machine}. Send it again to upload it"
                        ),
                        _ => error,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => Vec::new(),
        };
        let taken = match &op {
            Op::Send { send, .. } => Some(send.clone()),
            _ => None,
        };
        let conn = self
            .connection(&key, login.as_ref(), &machine)
            .map_err(Refused::Login)?;
        if session != 0 {
            conn.sessions.insert(session);
        }
        let runner = &conn.runner;
        let connection = runner.connection();
        match op {
            Op::Send {
                session,
                cwd,
                resume,
                choice,
                approval,
                text,
                ..
            } => {
                if !std::path::Path::new(&cwd).is_dir() {
                    return Err(format!("{cwd} isn't a folder on {machine}").into());
                }
                connection
                    .send(
                        session,
                        &cwd,
                        resume.as_deref(),
                        &choice,
                        approval,
                        text,
                        images,
                    )
                    .map_err(str::to_owned)?
            }
            // An image here is a path on the app's machine.
            Op::Command(Command::Send { images, .. }) if !images.is_empty() => {
                return Err(NO_IMAGES.into());
            }
            Op::Command(command) => {
                let answered = match &command {
                    Command::Answer { request, .. } => Some(request.clone()),
                    _ => None,
                };
                connection.command(command);
                if let Some(request) = answered
                    && let Err(error) = self.journal.answered(client, session, &request)
                {
                    log!("{error}");
                }
            }
            Op::RequestCatalog { session, cwd } => connection.request_catalog(session, &cwd),
            Op::SetChoice { session, choice } => connection.set_choice(session, choice),
            Op::SetApproval { session, approval } => connection.set_approval(session, approval),
            Op::HoldFollowUps { session } => connection.hold_follow_ups(session),
            Op::ReleaseFollowUps { session } => connection.release_follow_ups(session),
            Op::EvictIdle { session, candidate } => connection.evict_idle(session, candidate),
            Op::DeclineIdle {
                session,
                candidate,
                reason,
            } => connection.decline_idle(session, candidate, reason),
            Op::ReadHistory {
                session,
                thread,
                ticket,
            } => codex(runner)?.read_history(session, thread, ticket),
            Op::ListThreads { cwd, ticket } => codex(runner)?.list_threads(&cwd, ticket),
            Op::ReadImport { thread, ticket } => codex(runner)?.read_import(thread, ticket),
            Op::PutAttachment { .. } | Op::Recall { .. } => unreachable!("handled above"),
        }
        // The journal says which message the turn after it is, before any of that
        // turn's events, so an app that was away learns it from the replay.
        if let Some(send) = taken {
            self.enqueue(&key, session, vec![Event::Taken { send }]);
        }
        Ok(())
    }

    /// Whether the session waits for the last daemon's watchers: its own, or for
    /// Codex, those of its account's app-server, which ran every session of it.
    fn holds(&self, (client, route): &Key, session: u64) -> bool {
        self.held.iter().any(|((owner, held_session), held)| {
            owner == client
                && (*held_session == session
                    || held.others.contains(&session)
                    || (route.provider == Provider::Codex && held.route == *route))
        })
    }

    /// Why the journal last failed, while every backlog together holds more than
    /// [`Daemon::backlog_limit`].
    fn backlogged(&mut self) -> Option<String> {
        let bytes: usize = self.backlog.values().map(|backlog| backlog.bytes).sum();
        if bytes <= self.backlog_limit {
            self.refusing = false;
            return None;
        }
        let error = self
            .backlog
            .values()
            .map(|backlog| backlog.error.clone())
            .find(|error| !error.is_empty())
            .unwrap_or_default();
        if !self.refusing {
            self.refusing = true;
            log!(
                "{bytes} bytes of events wait for the journal, so no turn starts until it takes them ({error})"
            );
        }
        Some(error)
    }

    /// The connection for `key`, made on its first call. One for an account other
    /// than the default login runs with `login`'s profile.
    /// What `client` calls this machine, or its hostname when it said nothing.
    fn machine_name(&self, client: &str) -> String {
        self.names
            .get(client)
            .cloned()
            .unwrap_or_else(|| self.welcome.hostname.clone())
    }

    fn connection(
        &mut self,
        key: &Key,
        login: Option<&Login>,
        machine: &str,
    ) -> Result<&mut Conn, String> {
        // The account is the route's, so its connection is its folder's: a new name
        // for it changes nothing that runs.
        let home = |login: Option<&Login>| login.map(|login| login.home.clone());
        match self.conns.get_mut(key) {
            Some(conn) if login.is_none() || home(conn.login.as_ref()) == home(login) => {
                if login.is_some() {
                    conn.login = login.cloned();
                }
                return Ok(self.conns.get_mut(key).expect("checked above"));
            }
            // The account's folder changed, which the app allows only while none of
            // its sessions has a message the host took. Whatever still ran there ends.
            Some(_) => self.let_go(
                key,
                "This login's folder changed, so the turn on the old one ended",
            ),
            None => {}
        }
        let (client, route) = key.clone();
        let profile = match (&route.account, login) {
            (None, _) => LaunchProfile::inherited(),
            (Some(_), Some(login)) => login_profile(route.provider, login, machine)?,
            (Some(_), None) => return Err(NO_LOGIN.into()),
        };
        let tx = self.tx.clone();
        let woken = key.clone();
        let wake = Arc::new(move || {
            let _ = tx.send(Msg::Wake(woken.clone()));
        });
        let (watcher, run) = (self.program.clone(), self.run.clone());
        let provider = route.provider;
        let hook: crate::SpawnHook = Arc::new(move |session, pid| {
            let watching = Watching {
                client: client.clone(),
                session,
                provider: route.provider,
                account: route.account.clone(),
            };
            reap::watch(&watcher, &run, &watching, pid).map_err(|e| {
                format!(
                    "keystone-host couldn't watch {}, so it stopped it: {e}",
                    cli_name(route.provider)
                )
            })
        });
        let runner = match provider {
            Provider::Claude => {
                let claude = ClaudeConnection::with_profile(self.claude.clone(), profile, wake);
                claude.on_spawn(hook);
                Runner::Claude(claude)
            }
            Provider::Codex => {
                let codex = CodexConnection::with_profile(
                    self.codex.clone(),
                    env!("CARGO_PKG_VERSION"),
                    profile,
                    wake,
                );
                codex.on_spawn(hook);
                Runner::Codex(codex)
            }
        };
        Ok(self.conns.entry(key.clone()).or_insert(Conn {
            runner,
            login: login.cloned(),
            sessions: BTreeSet::new(),
        }))
    }

    /// Lets go of `key`'s connection once what it reported is journaled. Each turn it
    /// still ran, or approval it still waited on, ends there failed with `reason`, so
    /// no session waits on a process that is gone. A session it served only for a call
    /// on its route, such as the model menu's catalog before its login was chosen,
    /// runs its turn on another route and keeps it.
    fn let_go(&mut self, key: &Key, reason: &str) {
        self.pump(key);
        let Some(conn) = self.conns.remove(key) else {
            return;
        };
        // A test's stand-in for a connection lost without a word, which Stop then ends.
        if fault("let-go-silently") {
            return release(Some(conn));
        }
        let reason = format!("{reason}.");
        match self.states(&key.0) {
            Ok(states) => {
                for session in &conn.sessions {
                    if let Some(state) = states.get(session)
                        && on_route(state, &key.1)
                        && (state.running || !state.approvals.is_empty())
                    {
                        let events = journal::lost(&state.approvals, &reason);
                        self.enqueue(key, *session, events);
                    }
                }
            }
            Err(error) => log!("{error}"),
        }
        release(Some(conn));
    }

    /// Ends `session`'s turn when the journal has one running that no connection of
    /// `key` runs, as after its process was let go of without a word, and says whether
    /// it did. Stop then ends it rather than reaching nothing.
    fn end_orphan(&mut self, key: &Key, session: u64) -> bool {
        self.pump(key);
        if self
            .conns
            .get(key)
            .is_some_and(|conn| conn.runner.connection().has_work(session))
        {
            return false;
        }
        let state = match self.states(&key.0) {
            Ok(mut states) => states.remove(&session),
            Err(error) => {
                log!("{error}");
                None
            }
        };
        let Some(state) = state.filter(|state| {
            on_route(state, &key.1) && (state.running || !state.approvals.is_empty())
        }) else {
            return false;
        };
        self.enqueue(key, session, journal::quit(&state.approvals));
        true
    }

    /// Journals what the connection reported, after anything of it still waiting for
    /// the journal.
    fn pump(&mut self, key: &Key) {
        let fresh = match self.conns.get(key) {
            Some(conn) => conn.runner.connection().drain(),
            None => Vec::new(),
        };
        self.journal_events(key, fresh);
    }

    /// Appends what waits for the journal, oldest first and in bounded batches, then
    /// `fresh`, and sends each batch the journal takes to the client if attached. With
    /// no client attached, each session a batch concerned ends its process once idle.
    /// When the journal fails, what is left waits in order, within the backlog's
    /// bound, and is tried again each tick.
    fn journal_events(&mut self, key: &Key, fresh: Vec<(u64, Event)>) {
        let mut backlog = self.backlog.remove(key);
        if let Some(waiting) = &mut backlog {
            while let Some(batch) = waiting.batch() {
                match self.take(key, &batch) {
                    Ok(lines) => {
                        waiting.taken(batch.len());
                        self.deliver(key, lines, batch.sessions());
                    }
                    Err(error) => {
                        waiting.push(fresh, self.backlog_bound);
                        self.failed(key, waiting, &error);
                        self.backlog
                            .insert(key.clone(), backlog.expect("checked above"));
                        return;
                    }
                }
            }
            log!("the journal took the events that waited");
        }
        if fresh.is_empty() {
            return;
        }
        let batch = Batch::Events(fresh);
        match self.take(key, &batch) {
            Ok(lines) => self.deliver(key, lines, batch.sessions()),
            Err(error) => {
                let Batch::Events(fresh) = batch else {
                    unreachable!()
                };
                log!("{error}; events wait for the journal");
                let mut waiting = Backlog::default();
                waiting.push(fresh, self.backlog_bound);
                self.failed(key, &mut waiting, &error);
                self.backlog.insert(key.clone(), waiting);
            }
        }
    }

    fn take(&mut self, (client, route): &Key, batch: &Batch) -> Result<Vec<Vec<u8>>, String> {
        if fault("append") {
            return Err("journal: appends fail (test fault)".to_owned());
        }
        match batch {
            Batch::Events(events) => self.journal.append(client, route, events),
            Batch::Gap(sessions) => self
                .journal
                .gap(client, route, sessions)
                .map(|line| vec![line]),
        }
    }

    fn deliver(&mut self, key: &Key, lines: Vec<Vec<u8>>, sessions: BTreeSet<u64>) {
        match self.attached.get(&key.0) {
            Some(&id) => {
                if self
                    .sockets
                    .get(&id)
                    .is_some_and(|socket| socket.live.is_some())
                {
                    for line in lines {
                        self.send_line(id, line);
                    }
                }
            }
            None => {
                if let Some(conn) = self.conns.get(key) {
                    for session in sessions {
                        conn.end_idle(session);
                    }
                }
            }
        }
    }

    /// Warns the attached app, once per attachment, in each session whose events wait.
    fn failed(&mut self, (client, route): &Key, backlog: &mut Backlog, error: &str) {
        error.clone_into(&mut backlog.error);
        let Some(&id) = self.attached.get(client) else {
            return;
        };
        if backlog.warned == Some(id) {
            return;
        }
        backlog.warned = Some(id);
        let sessions: BTreeSet<u64> = backlog.events().map(|(session, _)| session).collect();
        for session in sessions {
            let warning = Frame::Warning {
                provider: route.provider,
                account: route.account.clone(),
                session,
                message: format!("{UNSAVED} ({error})"),
            };
            self.send(id, &warning);
        }
    }

    fn detached(&mut self, client: &str) {
        for ((owner, _), conn) in &self.conns {
            if owner == client {
                for session in &conn.sessions {
                    conn.end_idle(*session);
                }
            }
        }
    }

    /// Ends every provider process of `client`, as Quit ends local ones, and the turns
    /// that ran in them, after whatever of theirs still waits for the journal. A held
    /// session's turn ends that way once its last run's watchers let go.
    fn end_sessions(&mut self, client: &str) {
        let keys: Vec<Key> = self
            .conns
            .keys()
            .filter(|(owner, _)| owner == client)
            .cloned()
            .collect();
        for key in keys {
            self.pump(&key);
            release(self.conns.remove(&key));
        }
        let states = match self.states(client) {
            Ok(states) => states,
            Err(error) => return log!("{error}"),
        };
        for state in states.into_values() {
            if let Some((_, held)) = self.held.iter_mut().find(|((owner, session), held)| {
                owner == client
                    && (*session == state.session || held.others.contains(&state.session))
            }) {
                held.quit = true;
                continue;
            }
            if state.running || !state.approvals.is_empty() {
                let route = Route {
                    provider: state.provider,
                    account: state.account,
                };
                let events = journal::quit(&state.approvals);
                self.enqueue(&(client.to_owned(), route), state.session, events);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Item, ItemKind, ItemStatus};

    fn item(id: &str) -> Item {
        Item {
            id: id.into(),
            kind: ItemKind::AgentMessage,
            text: "word ".repeat(20),
            status: ItemStatus::Completed,
        }
    }

    /// A turn in session 1: a start, two messages streamed in deltas, and an end.
    fn turn() -> Vec<(u64, Event)> {
        let mut events = vec![(1, Event::TurnStarted)];
        for id in ["a", "b"] {
            events.push((1, Event::ItemStarted(item(id))));
            for _ in 0..20 {
                let text = "word ".into();
                let item_id = id.into();
                events.push((1, Event::ItemDelta { item_id, text }));
            }
            events.push((1, Event::ItemCompleted(item(id))));
        }
        events.push((
            1,
            Event::TurnCompleted {
                status: crate::TurnStatus::Completed,
                error: None,
                stopped: false,
            },
        ));
        events
    }

    fn kinds(backlog: &Backlog) -> Vec<&'static str> {
        backlog
            .waiting
            .iter()
            .map(|waiting| match waiting {
                Waiting::Gap(_) => "gap",
                Waiting::Event { event, .. } => match event {
                    Event::TurnStarted => "start",
                    Event::ItemStarted(_) => "item",
                    Event::ItemDelta { .. } => "delta",
                    Event::ItemCompleted(_) => "done",
                    Event::TurnCompleted { .. } => "end",
                    _ => "other",
                },
            })
            .collect()
    }

    fn size(events: &[(u64, Event)], keep: impl Fn(&Event) -> bool) -> usize {
        events
            .iter()
            .filter(|(_, event)| keep(event))
            .map(|(_, event)| serde_json::to_vec(event).unwrap().len())
            .sum()
    }

    #[test]
    fn a_home_that_is_the_whole_disk_or_has_dots_says_which() {
        let refused = |home: &str| {
            let login = Login {
                label: "Work".into(),
                home: home.into(),
            };
            login_profile(Provider::Codex, &login, "desk").unwrap_err()
        };
        for home in ["/", "//"] {
            let reason = refused(home);
            assert!(
                reason.contains("the whole of desk's disk"),
                "{home}: {reason}"
            );
        }
        for home in ["/a/../b", "/a/./b", "relative"] {
            let reason = refused(home);
            assert!(reason.contains("without . or .."), "{home}: {reason}");
        }
    }

    #[test]
    fn a_backlog_within_its_bound_keeps_everything() {
        let mut backlog = Backlog::default();
        backlog.push(turn(), usize::MAX);
        assert_eq!(backlog.waiting.len(), turn().len());
        assert_eq!(backlog.bytes, size(&turn(), |_| true));
    }

    #[test]
    fn a_backlog_past_its_bound_drops_the_oldest_deltas_first() {
        let deltas = size(&turn(), |e| matches!(e, Event::ItemDelta { .. }));
        let bound = size(&turn(), |_| true) - deltas / 2;
        let mut backlog = Backlog::default();
        backlog.push(turn(), bound);
        let kinds = kinds(&backlog);
        assert!(backlog.bytes <= bound);
        assert_eq!(kinds.iter().filter(|k| **k == "delta").count(), 20);
        assert_eq!(
            &kinds[..4],
            ["start", "item", "done", "item"],
            "the first message's deltas went"
        );
        assert!(!kinds.contains(&"gap"));
    }

    #[test]
    fn a_backlog_far_past_its_bound_keeps_only_lifecycle_events_and_a_gap() {
        let mut backlog = Backlog::default();
        backlog.push(turn(), 0);
        assert_eq!(kinds(&backlog), ["start", "gap", "end"]);
        assert!(
            matches!(&backlog.waiting[1], Waiting::Gap(sessions) if *sessions == BTreeSet::from([1]))
        );
        backlog.push(turn(), 0);
        assert_eq!(
            kinds(&backlog),
            ["start", "gap", "end", "start", "gap", "end"],
            "each run of lost items has a gap of its own"
        );
        let Some(Batch::Events(first)) = backlog.batch() else {
            panic!("events come first");
        };
        assert_eq!(first.len(), 1);
        backlog.taken(1);
        assert!(matches!(backlog.batch(), Some(Batch::Gap(_))));
    }
}

/// The Codex connection a history or import call needs.
fn codex(runner: &Runner) -> Result<&CodexConnection, String> {
    match runner {
        Runner::Codex(codex) => Ok(codex),
        Runner::Claude(_) => {
            Err("Claude Code's history is read from its files, not asked of the host".into())
        }
    }
}
