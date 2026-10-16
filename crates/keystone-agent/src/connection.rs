//! Running provider connections: drivers, their processes, and an inbox of events
//! for the UI.

use crate::claude::{self, ClaudeDriver};
use crate::codex::CodexDriver;
use crate::driver::{Command, Outputs};
use crate::host::{Exit, Process};
use crate::item::Event;
use crate::worker::Worker;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Called from a reader thread when the inbox goes from empty to non-empty. The UI
/// drains everything in one pass, so a burst of streamed text costs one update.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

type Inbox = Arc<Mutex<Vec<(u64, Event)>>>;

/// What the UI needs from any provider. Commands run on the connection's worker in the
/// order they were given, and a provider that cannot start reports [`Event::Failed`]
/// for the session that needed it.
pub trait Connection {
    /// Opens the session's conversation if needed, then sends `text`: a new turn, or a
    /// steer for the running one. `resume` is the saved provider conversation ID.
    /// Returns the reason, without sending, when the provider cannot take a steer now,
    /// so the caller can keep the text.
    fn send(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        text: String,
    ) -> Result<(), &'static str>;
    /// Stops or answers within a session.
    fn command(&self, command: Command);
    /// Takes every event that arrived since the last call.
    fn drain(&self) -> Vec<(u64, Event)>;
}

/// Describes why a provider process ended.
fn exit_reason(name: &str, exit: &Exit) -> String {
    match (exit.code, exit.stderr_tail.is_empty()) {
        (_, false) => format!("{name} stopped: {}", exit.stderr_tail),
        (Some(code), true) => format!("{name} stopped with exit code {code}"),
        (None, true) => format!("{name} stopped"),
    }
}

/// Reports that `session`'s provider could not start, and wakes the UI.
fn fail(inbox: &Inbox, wake: &Wake, session: u64, message: String) {
    inbox
        .lock()
        .unwrap()
        .push((session, Event::Failed { message }));
    wake();
}

fn command_session(command: &Command) -> u64 {
    match command {
        Command::Open { session, .. }
        | Command::Send { session, .. }
        | Command::Interrupt { session }
        | Command::Answer { session, .. } => *session,
    }
}

/// One shared `codex app-server` for every Codex session. The process starts on the
/// first command and starts again after it exits.
///
/// Commands run on a worker thread in the order they were given, so finding and
/// starting Codex never blocks the caller. A failure to start arrives as
/// [`Event::Failed`] for the session that needed it.
pub struct CodexConnection {
    client_version: String,
    wake: Wake,
    shared: Arc<Mutex<Shared>>,
    /// Kept apart from `shared`, so draining never waits for a process to start.
    inbox: Inbox,
    worker: Option<Worker>,
}

#[derive(Default)]
struct Shared {
    /// The `codex` program, found on first use when not given.
    program: Option<PathBuf>,
    driver: Option<CodexDriver>,
    process: Option<Process>,
    /// The latest thread per session, kept across process restarts. A thread opened
    /// just before a crash may not have reached the caller's saved state yet.
    threads: HashMap<u64, String>,
    /// Incremented per process, so a late exit from an old one is ignored.
    generation: u64,
    /// Set when the connection is dropped, so queued commands start nothing.
    closed: bool,
}

impl CodexConnection {
    /// `program` is the `codex` binary to run, or `None` to look for it on first use.
    pub fn new(program: Option<PathBuf>, client_version: &str, wake: Wake) -> Self {
        Self {
            client_version: client_version.to_owned(),
            wake,
            shared: Arc::new(Mutex::new(Shared {
                program,
                ..Shared::default()
            })),
            inbox: Arc::default(),
            worker: Some(Worker::new("codex-commands")),
        }
    }

    /// The app-server's process ID while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.shared
            .lock()
            .unwrap()
            .process
            .as_ref()
            .map(Process::pid)
    }

    /// Whether the session's conversation is open in the current process.
    pub fn is_open(&self, session: u64) -> bool {
        let shared = self.shared.lock().unwrap();
        shared.driver.as_ref().is_some_and(|d| d.is_open(session))
    }

    pub fn is_running(&self, session: u64) -> bool {
        let shared = self.shared.lock().unwrap();
        shared
            .driver
            .as_ref()
            .is_some_and(|d| d.is_running(session))
    }

    /// Opens the session's conversation if this process has not, then sends `text`,
    /// all under one lock so a process restart cannot fall between the two.
    pub fn send(&self, session: u64, cwd: &str, resume: Option<&str>, text: String) {
        let cwd = cwd.to_owned();
        let resume = resume.map(str::to_owned);
        self.with_driver(session, move |driver, threads, out| {
            if !driver.is_open(session) {
                let resume = threads.get(&session).cloned().or(resume);
                driver.command(
                    Command::Open {
                        session,
                        cwd,
                        resume,
                    },
                    out,
                );
            }
            driver.command(Command::Send { session, text }, out);
        });
    }

    /// Sends a command, starting the app-server first if it is not running.
    pub fn command(&self, command: Command) {
        let session = command_session(&command);
        self.with_driver(session, move |driver, _, out| driver.command(command, out));
    }

    fn with_driver(
        &self,
        session: u64,
        act: impl FnOnce(&mut CodexDriver, &HashMap<u64, String>, &mut Outputs) + Send + 'static,
    ) {
        let starter = self.starter();
        let Some(worker) = &self.worker else { return };
        worker.run(move || {
            let mut shared = starter.shared.lock().unwrap();
            if shared.closed {
                return;
            }
            if shared.process.is_none()
                && let Err(message) = starter.start(&mut shared)
            {
                drop(shared);
                fail(&starter.inbox, &starter.wake, session, message);
                return;
            }
            let mut out = Outputs::default();
            let shared_ref = &mut *shared;
            act(
                shared_ref
                    .driver
                    .as_mut()
                    .expect("driver exists while the process runs"),
                &shared_ref.threads,
                &mut out,
            );
            let wake = deliver(&mut shared, &starter.inbox, out);
            drop(shared);
            if wake {
                (starter.wake)();
            }
        });
    }

    /// Takes every event that arrived since the last call.
    pub fn drain(&self) -> Vec<(u64, Event)> {
        std::mem::take(&mut self.inbox.lock().unwrap())
    }

    fn starter(&self) -> Starter {
        Starter {
            client_version: self.client_version.clone(),
            wake: self.wake.clone(),
            shared: self.shared.clone(),
            inbox: self.inbox.clone(),
        }
    }
}

/// What the worker needs to start the app-server.
struct Starter {
    client_version: String,
    wake: Wake,
    shared: Arc<Mutex<Shared>>,
    inbox: Inbox,
}

impl Starter {
    fn start(&self, shared: &mut Shared) -> Result<(), String> {
        if shared.program.is_none() {
            shared.program = crate::find::program("codex");
        }
        let program = shared
            .program
            .clone()
            .ok_or("Codex is not installed. Install the codex CLI and run `codex login`.")?;
        shared.generation += 1;
        let generation = shared.generation;
        let mut out = Outputs::default();
        shared.driver = Some(CodexDriver::new(&self.client_version, &mut out));
        let on_line = {
            let shared = self.shared.clone();
            let inbox = self.inbox.clone();
            let wake = self.wake.clone();
            move |line: &[u8]| {
                let mut guard = shared.lock().unwrap();
                // Output that arrives after this process was replaced is not for the
                // new driver.
                if guard.generation != generation {
                    return;
                }
                let mut out = Outputs::default();
                if let Some(driver) = guard.driver.as_mut() {
                    driver.on_line(line, &mut out);
                }
                let wake_now = deliver(&mut guard, &inbox, out);
                drop(guard);
                if wake_now {
                    wake();
                }
            }
        };
        let on_exit = {
            let shared = self.shared.clone();
            let inbox = self.inbox.clone();
            let wake = self.wake.clone();
            move |exit: Exit| {
                let mut guard = shared.lock().unwrap();
                if guard.generation != generation {
                    return;
                }
                let reason = exit_reason("Codex", &exit);
                let mut out = Outputs::default();
                if let Some(mut driver) = guard.driver.take() {
                    driver.on_exit(&reason, &mut out);
                }
                inbox.lock().unwrap().append(&mut out.events);
                // Dropping the handle kills anything the process left behind.
                let process = guard.process.take();
                drop(guard);
                drop(process);
                wake();
            }
        };
        let cwd = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let process =
            Process::spawn(&program, &["app-server"], &cwd, on_line, on_exit).map_err(|e| {
                shared.driver = None;
                format!("Could not start Codex at {}. {e}", program.display())
            })?;
        shared.process = Some(process);
        deliver(shared, &self.inbox, out);
        Ok(())
    }
}

impl Connection for CodexConnection {
    fn send(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        text: String,
    ) -> Result<(), &'static str> {
        CodexConnection::send(self, session, cwd, resume, text);
        Ok(())
    }

    fn command(&self, command: Command) {
        CodexConnection::command(self, command);
    }

    fn drain(&self) -> Vec<(u64, Event)> {
        CodexConnection::drain(self)
    }
}

impl Drop for CodexConnection {
    /// The reader threads keep the shared state alive, so the process must be ended
    /// here rather than when the last reference goes.
    fn drop(&mut self) {
        self.shared.lock().unwrap().closed = true;
        // Let a command already running finish, so no process starts after this.
        drop(self.worker.take());
        let process = {
            let mut shared = self.shared.lock().unwrap();
            shared.generation += 1;
            shared.driver = None;
            shared.process.take()
        };
        drop(process);
    }
}

/// Sends the driver's writes and queues its events. Returns whether the inbox went from
/// empty to non-empty, which is when the UI needs waking.
fn deliver(shared: &mut Shared, inbox: &Mutex<Vec<(u64, Event)>>, mut out: Outputs) -> bool {
    if let Some(process) = &shared.process {
        for line in out.writes.drain(..) {
            process.send(line);
        }
    }
    for (session, event) in &out.events {
        if let Event::ThreadReady { thread_id } = event {
            shared.threads.insert(*session, thread_id.clone());
        }
    }
    let mut inbox = inbox.lock().unwrap();
    let was_empty = inbox.is_empty();
    inbox.append(&mut out.events);
    was_empty && !inbox.is_empty()
}

/// One `claude` process per Claude session, as its protocol requires. A session's
/// process starts on its first message, in the session's directory, and starts again,
/// resuming the conversation, after it exits.
///
/// Like [`CodexConnection`], commands run on a worker thread in the order they were
/// given, and a failure to start arrives as [`Event::Failed`].
pub struct ClaudeConnection {
    wake: Wake,
    shared: Arc<Mutex<ClaudeShared>>,
    /// Kept apart from `shared`, so draining never waits for a process to start.
    inbox: Inbox,
    worker: Option<Worker>,
}

#[derive(Default)]
struct ClaudeShared {
    /// The `claude` program, found on first use when not given.
    program: Option<PathBuf>,
    live: HashMap<u64, Live>,
    /// The latest conversation per session, kept across process restarts.
    threads: HashMap<u64, String>,
    /// Messages per session that `send` accepted and the worker has not sent yet.
    queued: HashMap<u64, usize>,
    /// Incremented per process, so a late exit from an old one is ignored.
    generation: u64,
    /// Set when the connection is dropped, so queued commands start nothing.
    closed: bool,
}

struct Live {
    driver: ClaudeDriver,
    process: Process,
    generation: u64,
}

impl ClaudeShared {
    /// Why a message sent now could not steer, if it could not. A message still queued
    /// will start a turn, so a second one would steer it.
    fn steer_blocked(&self, session: u64) -> Option<&'static str> {
        let live = self.live.get(&session).map(|live| &live.driver);
        if self.queued.get(&session).is_some_and(|n| *n > 0) {
            return live.map_or(Some(claude::STEER_NOT_YET), ClaudeDriver::steer_unsupported);
        }
        live.and_then(ClaudeDriver::steer_blocked)
    }
}

impl ClaudeConnection {
    /// `program` is the `claude` binary to run, or `None` to look for it on first use.
    pub fn new(program: Option<PathBuf>, wake: Wake) -> Self {
        Self {
            wake,
            shared: Arc::new(Mutex::new(ClaudeShared {
                program,
                ..ClaudeShared::default()
            })),
            inbox: Arc::default(),
            worker: Some(Worker::new("claude-commands")),
        }
    }

    /// The session's process ID while it runs.
    pub fn pid(&self, session: u64) -> Option<u32> {
        let shared = self.shared.lock().unwrap();
        shared.live.get(&session).map(|live| live.process.pid())
    }

    pub fn is_running(&self, session: u64) -> bool {
        let shared = self.shared.lock().unwrap();
        shared
            .live
            .get(&session)
            .is_some_and(|live| live.driver.is_running())
    }

    /// Starts the session's process if it is not running, then sends `text`. Refuses,
    /// without sending, a steer Claude cannot take now, so the caller keeps the text and
    /// shows why.
    pub fn send(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        text: String,
    ) -> Result<(), &'static str> {
        {
            let mut shared = self.shared.lock().unwrap();
            if let Some(reason) = shared.steer_blocked(session) {
                return Err(reason);
            }
            *shared.queued.entry(session).or_default() += 1;
        }
        let start = (PathBuf::from(cwd), resume.map(str::to_owned));
        self.run(session, Some(start), move |driver, out| {
            driver.command(Command::Send { session, text }, out)
        });
        Ok(())
    }

    /// Stops or answers within a running session. With no process there is nothing
    /// to stop or answer.
    pub fn command(&self, command: Command) {
        let session = command_session(&command);
        self.run(session, None, move |driver, out| {
            driver.command(command, out)
        });
    }

    /// Takes every event that arrived since the last call.
    pub fn drain(&self) -> Vec<(u64, Event)> {
        std::mem::take(&mut self.inbox.lock().unwrap())
    }

    /// Queues `act` for the session's driver. `start`, given for a message, is the
    /// directory and saved conversation to start a process with if none runs.
    fn run(
        &self,
        session: u64,
        start: Option<(PathBuf, Option<String>)>,
        act: impl FnOnce(&mut ClaudeDriver, &mut Outputs) + Send + 'static,
    ) {
        let starter = self.starter();
        let Some(worker) = &self.worker else { return };
        worker.run(move || {
            let mut shared = starter.shared.lock().unwrap();
            if start.is_some()
                && let Some(n) = shared.queued.get_mut(&session)
            {
                *n -= 1;
                if *n == 0 {
                    shared.queued.remove(&session);
                }
            }
            if shared.closed {
                return;
            }
            if !shared.live.contains_key(&session) {
                let Some((cwd, resume)) = start else { return };
                let resume = shared.threads.get(&session).cloned().or(resume);
                if let Err(message) = starter.start(&mut shared, session, &cwd, resume) {
                    drop(shared);
                    fail(&starter.inbox, &starter.wake, session, message);
                    return;
                }
            }
            let mut out = Outputs::default();
            let live = shared.live.get_mut(&session).expect("started above");
            act(&mut live.driver, &mut out);
            let wake = claude_deliver(&mut shared, &starter.inbox, session, out);
            drop(shared);
            if wake {
                (starter.wake)();
            }
        });
    }

    fn starter(&self) -> ClaudeStarter {
        ClaudeStarter {
            wake: self.wake.clone(),
            shared: self.shared.clone(),
            inbox: self.inbox.clone(),
        }
    }
}

/// What the worker needs to start a session's `claude`.
struct ClaudeStarter {
    wake: Wake,
    shared: Arc<Mutex<ClaudeShared>>,
    inbox: Inbox,
}

impl ClaudeStarter {
    fn start(
        &self,
        shared: &mut ClaudeShared,
        session: u64,
        cwd: &Path,
        resume: Option<String>,
    ) -> Result<(), String> {
        if shared.program.is_none() {
            shared.program = crate::find::program("claude");
        }
        let program = shared.program.clone().ok_or(
            "Claude Code is not installed. Install the claude CLI and run `claude` once to sign in.",
        )?;
        shared.generation += 1;
        let generation = shared.generation;
        let mut out = Outputs::default();
        let driver = ClaudeDriver::new(session, resume.is_some(), random_seed(), &mut out);
        let mut args: Vec<&str> = claude::ARGS.to_vec();
        if let Some(resume) = &resume {
            args.extend(["--resume", resume]);
        }
        let on_line = {
            let shared = self.shared.clone();
            let inbox = self.inbox.clone();
            let wake = self.wake.clone();
            move |line: &[u8]| {
                let mut guard = shared.lock().unwrap();
                let mut out = Outputs::default();
                match guard.live.get_mut(&session) {
                    Some(live) if live.generation == generation => {
                        live.driver.on_line(line, &mut out)
                    }
                    // Output from a replaced process is not for the new driver.
                    _ => return,
                }
                let wake_now = claude_deliver(&mut guard, &inbox, session, out);
                drop(guard);
                if wake_now {
                    wake();
                }
            }
        };
        let on_exit = {
            let shared = self.shared.clone();
            let inbox = self.inbox.clone();
            let wake = self.wake.clone();
            move |exit: Exit| {
                let mut guard = shared.lock().unwrap();
                if guard
                    .live
                    .get(&session)
                    .is_none_or(|live| live.generation != generation)
                {
                    return;
                }
                let mut live = guard.live.remove(&session).expect("checked above");
                let mut out = Outputs::default();
                live.driver
                    .on_exit(&exit_reason("Claude Code", &exit), &mut out);
                inbox.lock().unwrap().append(&mut out.events);
                drop(guard);
                // Dropping the handle kills anything the process left behind.
                drop(live);
                wake();
            }
        };
        let process = Process::spawn(&program, &args, cwd, on_line, on_exit)
            .map_err(|e| format!("Could not start Claude Code at {}. {e}", program.display()))?;
        for line in out.writes.drain(..) {
            process.send(line);
        }
        shared.live.insert(
            session,
            Live {
                driver,
                process,
                generation,
            },
        );
        Ok(())
    }
}

impl Connection for ClaudeConnection {
    fn send(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        text: String,
    ) -> Result<(), &'static str> {
        ClaudeConnection::send(self, session, cwd, resume, text)
    }

    fn command(&self, command: Command) {
        ClaudeConnection::command(self, command);
    }

    fn drain(&self) -> Vec<(u64, Event)> {
        ClaudeConnection::drain(self)
    }
}

impl Drop for ClaudeConnection {
    /// The reader threads keep the shared state alive, so every process must be ended
    /// here rather than when the last reference goes.
    fn drop(&mut self) {
        self.shared.lock().unwrap().closed = true;
        // Let a command already running finish, so no process starts after this.
        drop(self.worker.take());
        let live: Vec<Live> = {
            let mut shared = self.shared.lock().unwrap();
            shared.generation += 1;
            shared.live.drain().map(|(_, live)| live).collect()
        };
        drop(live);
    }
}

/// Sends the driver's writes and queues its events. Returns whether the inbox went from
/// empty to non-empty, which is when the UI needs waking.
fn claude_deliver(
    shared: &mut ClaudeShared,
    inbox: &Inbox,
    session: u64,
    mut out: Outputs,
) -> bool {
    if let Some(live) = shared.live.get(&session) {
        for line in out.writes.drain(..) {
            live.process.send(line);
        }
    }
    for (session, event) in &out.events {
        if let Event::ThreadReady { thread_id } = event {
            shared.threads.insert(*session, thread_id.clone());
        }
    }
    let mut inbox = inbox.lock().unwrap();
    let was_empty = inbox.is_empty();
    inbox.append(&mut out.events);
    was_empty && !inbox.is_empty()
}

/// A seed that differs per process, so message UUIDs never repeat across restarts.
fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    hasher.write_u128(nanos);
    hasher.finish()
}
