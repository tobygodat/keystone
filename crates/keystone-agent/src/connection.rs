//! Running provider connections: drivers, their processes, and an inbox of events
//! for the UI.

use crate::claude::{self, ClaudeDriver, Launch};
use crate::codex::CodexDriver;
use crate::driver::{ApprovalState, Command, Outputs};
use crate::host::{Exit, Process};
use crate::item::{Catalog, Event};
use crate::mcp::{Caller, Issuer, Lease};
use crate::version::{self, Provider};
use crate::worker::Worker;
use keystone_core::ApprovalMode;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Called from a reader thread when the inbox goes from empty to non-empty. The UI
/// drains everything in one pass, so a burst of streamed text costs one update.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

type Inbox = Arc<Mutex<Vec<(u64, Event)>>>;

#[derive(serde::Deserialize)]
struct ClaudeCapabilityFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    parent_tool_use_id: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Runs just before a provider process is looked up and started, with the session
/// that needs it. Tests use it to hold a start.
#[doc(hidden)]
pub type StartHook = Arc<dyn Fn(u64) + Send + Sync>;

/// What the UI needs from any provider. Commands run on the connection's worker in the
/// order they were given, and a provider that cannot start reports [`Event::Failed`]
/// for the session that needed it.
pub trait Connection {
    fn set_mcp(&self, _issuer: Issuer) {}
    fn is_running(&self, _session: u64) -> bool {
        false
    }
    #[allow(clippy::too_many_arguments)]
    fn send_from(
        &self,
        _session: u64,
        _cwd: &str,
        _resume: Option<&str>,
        _model: Option<&str>,
        _approval: ApprovalState,
        _text: String,
        _caller: Caller,
    ) -> Result<(), &'static str> {
        Err("Agent launches are not supported by this connection")
    }
    fn steer_from(
        &self,
        _session: u64,
        _text: String,
        _caller: Caller,
    ) -> Result<(), &'static str> {
        Err("Agent steering is not supported by this connection")
    }
    fn interrupt_from(&self, _session: u64, _caller: Caller) -> Result<(), &'static str> {
        Err("Agent interrupts are not supported by this connection")
    }
    /// Opens the session's conversation if needed, then sends `text`: a new turn, or a
    /// steer for the running one. `resume` is the saved provider conversation ID, and
    /// `model` the provider-native model the session asked for, applied when the
    /// conversation opens. `approval` is the session's approval state, which a turn
    /// this message starts runs under unless [`Connection::set_approval`] changes it
    /// first. Returns the reason, without sending, when the provider cannot take a
    /// steer now, so the caller can keep the text.
    fn send(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        model: Option<&str>,
        approval: ApprovalState,
        text: String,
    ) -> Result<(), &'static str>;
    /// Asks the provider which models it offers, from `cwd`. The answer arrives as
    /// [`Event::Catalog`] for `session`, a failure to ask included.
    fn request_catalog(&self, session: u64, cwd: &str);
    /// Changes the session's approval state at once. It applies to the next turn the
    /// provider starts for the session, including one for a message already sent and
    /// still waiting, and survives a provider restart.
    fn set_approval(&self, session: u64, approval: ApprovalState);
    /// Stops or answers within a session.
    fn command(&self, command: Command);
    /// Takes every event that arrived since the last call.
    fn drain(&self) -> Vec<(u64, Event)>;
}

/// Describes why a provider process ended.
fn exit_reason(name: &str, exit: &Exit, mcp: bool) -> String {
    match (exit.code, exit.stderr_tail.is_empty() || mcp) {
        (_, false) => format!("{name} stopped: {}", exit.stderr_tail),
        (Some(code), true) => format!("{name} stopped with exit code {code}"),
        (None, true) => format!("{name} stopped"),
    }
}

/// Queues one event for the UI and wakes it.
fn push(inbox: &Inbox, wake: &Wake, session: u64, event: Event) {
    inbox.lock().unwrap().push((session, event));
    wake();
}

/// The event for a provider that could not start for a session's command.
fn start_failed(message: String) -> Event {
    Event::Failed { message }
}

/// The event for a provider that could not start to list its models.
fn catalog_failed(message: String) -> Event {
    Event::Catalog(Catalog::Failed(message))
}

const CLAUDE_NOT_INSTALLED: &str =
    "Claude Code is not installed. Install the claude CLI and run `claude` once to sign in.";

fn command_session(command: &Command) -> u64 {
    match command {
        Command::Open { session, .. }
        | Command::Send { session, .. }
        | Command::Interrupt { session }
        | Command::Answer { session, .. }
        | Command::Configure { session, .. } => *session,
    }
}

/// How long dropping a connection waits for a process that is still starting, so it
/// can be ended. A start stuck longer is given up on; its process is ended when it
/// starts, if Keystone is still running.
const START_WAIT: Duration = Duration::from_secs(2);

/// One shared `codex app-server` for every Codex session. The process starts on the
/// first command and starts again after it exits.
///
/// Commands run on a worker thread in the order they were given, so finding and
/// starting Codex never blocks the caller. A failure to start arrives as
/// [`Event::Failed`] for the session that needed it. Finding and starting Codex hold
/// no lock, so a stuck start delays only the commands that need the process.
pub struct CodexConnection {
    client_version: String,
    wake: Wake,
    shared: Arc<Mutex<Shared>>,
    /// Signalled when a start finishes.
    started: Arc<Condvar>,
    /// Kept apart from `shared`, so draining never waits for a process to start.
    inbox: Inbox,
    worker: Option<Worker>,
}

#[derive(Default)]
struct Shared {
    mcp: Option<Issuer>,
    leases: HashMap<u64, Lease>,
    queued: HashMap<u64, usize>,
    notice: Option<String>,
    notices: version::Notices,
    /// The `codex` program, found on first use when not given.
    program: Option<PathBuf>,
    /// Present from when a process begins starting until it exits.
    driver: Option<CodexDriver>,
    process: Option<Process>,
    /// Whether a process is being started, outside the lock.
    starting: bool,
    /// Lines written before the starting process could take them.
    pending: Vec<String>,
    /// Why the starting process exited before it could be used.
    exited: Option<String>,
    /// The latest thread per session, kept across process restarts. A thread opened
    /// just before a crash may not have reached the caller's saved state yet.
    threads: HashMap<u64, String>,
    /// Each session's approval state, kept across process restarts and given to every
    /// driver before it handles the session's messages.
    approvals: HashMap<u64, ApprovalState>,
    /// Incremented per process, so a late exit from an old one is ignored.
    generation: u64,
    /// Set when the connection is dropped, so queued commands start nothing.
    closed: bool,
    before_start: Option<StartHook>,
}

impl Shared {
    fn unqueue(&mut self, session: u64) {
        if let Some(count) = self.queued.get_mut(&session) {
            *count -= 1;
            if *count == 0 {
                self.queued.remove(&session);
            }
        }
    }

    /// Records the session's approval state and gives it to the driver, under the lock
    /// its turns start under, so no turn can start with an older one.
    fn set_approval(&mut self, session: u64, approval: ApprovalState) {
        if let Some(issuer) = &self.mcp {
            issuer.set_mode(session, approval.mode);
        }
        self.approvals.insert(session, approval);
        if let Some(driver) = &mut self.driver {
            driver.command(
                Command::Configure { session, approval },
                &mut Outputs::default(),
            );
        }
    }
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
            started: Arc::default(),
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
    /// all under one lock so a process restart cannot fall between the two. The driver
    /// gets the session's latest approval state under the same lock, so a replacement
    /// process runs the turn under it too.
    pub fn send(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        model: Option<&str>,
        approval: ApprovalState,
        text: String,
    ) {
        let _ = self.send_inner(session, cwd, resume, model, approval, text, None);
    }

    #[allow(clippy::too_many_arguments)]
    fn send_inner(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        model: Option<&str>,
        approval: ApprovalState,
        text: String,
        caller: Option<Caller>,
    ) -> Result<(), &'static str> {
        {
            let mut shared = self.shared.lock().unwrap();
            if let Some(caller) = caller {
                authorize(shared.mcp.as_ref(), caller, approval.mode)?;
                if shared.queued.contains_key(&session)
                    || shared
                        .driver
                        .as_ref()
                        .is_some_and(|driver| driver.is_running(session))
                {
                    return Err("busy: The target already has a running turn");
                }
            }
            shared.set_approval(session, approval);
            *shared.queued.entry(session).or_default() += 1;
        }
        let cwd = cwd.to_owned();
        let resume = resume.map(str::to_owned);
        let model = model.map(str::to_owned);
        let pending = self.shared.clone();
        self.with_driver(
            session,
            move |message| {
                pending.lock().unwrap().unqueue(session);
                start_failed(message)
            },
            move |driver, shared, out| {
                shared.unqueue(session);
                let approval = shared.approvals.get(&session).copied().unwrap_or_default();
                if let Some(caller) = caller
                    && (!shared
                        .mcp
                        .as_ref()
                        .is_some_and(|issuer| issuer.permits_launch(caller, approval.mode))
                        || driver.is_running(session))
                {
                    out.event(
                        session,
                        start_failed(
                            "The launching agent no longer has permission to send this message"
                                .into(),
                        ),
                    );
                    return;
                }
                driver.command(Command::Configure { session, approval }, out);
                if !driver.is_open(session) {
                    if let Some(issuer) = &shared.mcp {
                        match issuer.issue(session) {
                            Ok(credentials) => {
                                let (url, token, lease) = credentials.into_parts();
                                shared.leases.insert(session, lease);
                                issuer.set_mode(session, approval.mode);
                                driver.set_mcp(session, url, token);
                            }
                            Err(_) => {
                                out.event(
                                    session,
                                    start_failed(
                                        "Could not issue Keystone tool credentials".into(),
                                    ),
                                );
                                return;
                            }
                        }
                    }
                    let resume = shared.threads.get(&session).cloned().or(resume);
                    driver.command(
                        Command::Open {
                            session,
                            cwd,
                            resume,
                            model,
                        },
                        out,
                    );
                }
                if let Some(caller) = caller {
                    driver.send_from(
                        session,
                        text,
                        shared.mcp.as_ref().unwrap().clone(),
                        caller,
                        out,
                    );
                } else {
                    driver.command(Command::Send { session, text }, out);
                }
            },
        );
        Ok(())
    }

    /// Sends a command, starting the app-server first if it is not running.
    pub fn command(&self, command: Command) {
        let session = command_session(&command);
        self.with_driver(session, start_failed, move |driver, _, out| {
            driver.command(command, out)
        });
    }

    /// Asks Codex for its models, starting the app-server first if it is not running;
    /// see [`Connection::request_catalog`].
    pub fn request_catalog(&self, session: u64) {
        self.with_driver(session, catalog_failed, move |driver, _, out| {
            driver.request_catalog(session, out)
        });
    }

    /// Changes the session's approval state at once; see [`Connection::set_approval`].
    pub fn set_approval(&self, session: u64, approval: ApprovalState) {
        self.shared.lock().unwrap().set_approval(session, approval);
    }

    /// The approval state the connection holds for the session, if it was given one.
    #[doc(hidden)]
    pub fn approval(&self, session: u64) -> Option<ApprovalState> {
        self.shared.lock().unwrap().approvals.get(&session).copied()
    }

    /// Runs `act` on the driver on the worker. `on_fail` is the event `session` gets
    /// when the app-server cannot start.
    fn with_driver(
        &self,
        session: u64,
        on_fail: impl FnOnce(String) -> Event + Send + 'static,
        act: impl FnOnce(&mut CodexDriver, &mut Shared, &mut Outputs) + Send + 'static,
    ) {
        let starter = self.starter();
        let Some(worker) = &self.worker else { return };
        worker.run(move || starter.run(session, on_fail, act));
    }

    /// Takes every event that arrived since the last call.
    pub fn drain(&self) -> Vec<(u64, Event)> {
        std::mem::take(&mut self.inbox.lock().unwrap())
    }

    #[doc(hidden)]
    pub fn hold_starts(&self, hook: StartHook) {
        self.shared.lock().unwrap().before_start = Some(hook);
    }

    fn starter(&self) -> Starter {
        Starter {
            client_version: self.client_version.clone(),
            wake: self.wake.clone(),
            shared: self.shared.clone(),
            started: self.started.clone(),
            inbox: self.inbox.clone(),
        }
    }
}

/// What the worker needs to start the app-server.
struct Starter {
    client_version: String,
    wake: Wake,
    shared: Arc<Mutex<Shared>>,
    started: Arc<Condvar>,
    inbox: Inbox,
}

impl Starter {
    /// Starts the app-server if it is not running, then runs `act` on its driver.
    fn run(
        &self,
        session: u64,
        on_fail: impl FnOnce(String) -> Event,
        act: impl FnOnce(&mut CodexDriver, &mut Shared, &mut Outputs),
    ) {
        let mut shared = self.shared.lock().unwrap();
        if shared.closed {
            return;
        }
        if shared.process.is_none() {
            shared.generation += 1;
            let generation = shared.generation;
            shared.driver = None;
            shared.leases.clear();
            shared.starting = true;
            shared.exited = None;
            let program = shared.program.clone();
            let hook = shared.before_start.clone();
            drop(shared);
            let spawned = self.spawn(session, generation, program, hook);
            shared = self.shared.lock().unwrap();
            if shared.closed || shared.generation != generation {
                drop(shared);
                drop(spawned);
                self.finish_start();
                return;
            }
            let failure = match spawned {
                Ok(process) => match shared.exited.take() {
                    None => {
                        for line in shared.pending.drain(..) {
                            process.send(line);
                        }
                        shared.process = Some(process);
                        None
                    }
                    Some(reason) => Some((reason, Some(process))),
                },
                Err(message) => Some((message, None)),
            };
            if let Some((message, process)) = failure {
                shared.driver = None;
                shared.pending.clear();
                drop(shared);
                drop(process);
                self.finish_start();
                push(&self.inbox, &self.wake, session, on_fail(message));
                return;
            }
            shared.starting = false;
            self.started.notify_all();
        }
        let mut out = Outputs::default();
        let notice = shared.notice.clone();
        if let Some(message) = shared.notices.update(session, notice) {
            out.event(session, Event::ProviderNotice { message });
        }
        let mut driver = shared
            .driver
            .take()
            .expect("driver exists while the process runs");
        act(&mut driver, &mut shared, &mut out);
        shared.driver = Some(driver);
        let wake = deliver(&mut shared, &self.inbox, out);
        drop(shared);
        if wake {
            (self.wake)();
        }
    }

    fn finish_start(&self) {
        self.shared.lock().unwrap().starting = false;
        self.started.notify_all();
    }

    /// Finds and starts `codex`, holding no lock.
    fn spawn(
        &self,
        session: u64,
        generation: u64,
        program: Option<PathBuf>,
        hook: Option<StartHook>,
    ) -> Result<Process, String> {
        if let Some(hook) = hook {
            hook(session);
        }
        let program = match program {
            Some(program) => program,
            None => {
                let found = crate::find::program("codex").ok_or(
                    "Codex is not installed. Install the codex CLI and run `codex login`.",
                )?;
                self.shared.lock().unwrap().program = Some(found.clone());
                found
            }
        };
        let cwd = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let compatibility = version::detect(Provider::Codex, &program, &cwd)?;
        {
            let mut shared = self.shared.lock().unwrap();
            if !shared.closed && shared.generation == generation {
                let mut out = Outputs::default();
                shared.driver = Some(CodexDriver::new(&self.client_version, &mut out));
                shared.notice = compatibility.notice;
                deliver(&mut shared, &self.inbox, out);
            }
        }
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
                let reason = exit_reason("Codex", &exit, guard.mcp.is_some());
                // Exited before the worker took it: the worker reports the failure.
                if guard.process.is_none() {
                    guard.exited = Some(reason);
                    return;
                }
                let mut out = Outputs::default();
                if let Some(mut driver) = guard.driver.take() {
                    driver.on_exit(&reason, &mut out);
                }
                guard.leases.clear();
                mcp_events(guard.mcp.as_ref(), &out.events);
                inbox.lock().unwrap().append(&mut out.events);
                // Dropping the handle kills anything the process left behind.
                let process = guard.process.take();
                drop(guard);
                drop(process);
                wake();
            }
        };
        Process::spawn(&program, &["app-server"], &cwd, on_line, on_exit)
            .map_err(|e| format!("Could not start Codex at {}. {e}", program.display()))
    }
}

impl Connection for CodexConnection {
    fn set_mcp(&self, issuer: Issuer) {
        self.shared.lock().unwrap().mcp = Some(issuer);
    }

    fn is_running(&self, session: u64) -> bool {
        CodexConnection::is_running(self, session)
    }

    fn send_from(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        model: Option<&str>,
        approval: ApprovalState,
        text: String,
        caller: Caller,
    ) -> Result<(), &'static str> {
        self.send_inner(session, cwd, resume, model, approval, text, Some(caller))
    }

    fn steer_from(&self, session: u64, text: String, caller: Caller) -> Result<(), &'static str> {
        let mut shared = self.shared.lock().unwrap();
        authorize(
            shared.mcp.as_ref(),
            caller,
            shared
                .approvals
                .get(&session)
                .copied()
                .unwrap_or_default()
                .mode,
        )?;
        if shared.closed || shared.starting || shared.process.is_none() {
            return Err("busy: The target is not running");
        }
        let mut out = Outputs::default();
        shared
            .driver
            .as_mut()
            .ok_or("busy: The target is not running")?
            .steer(session, text, &mut out)?;
        let wake = deliver(&mut shared, &self.inbox, out);
        drop(shared);
        if wake {
            (self.wake)();
        }
        Ok(())
    }

    fn interrupt_from(&self, session: u64, caller: Caller) -> Result<(), &'static str> {
        let mut shared = self.shared.lock().unwrap();
        authorize(
            shared.mcp.as_ref(),
            caller,
            shared
                .approvals
                .get(&session)
                .copied()
                .unwrap_or_default()
                .mode,
        )?;
        if shared.closed || shared.queued.contains_key(&session) {
            return Err("busy: The target is still starting");
        }
        if shared.driver.is_none() {
            return Ok(());
        }
        let mut out = Outputs::default();
        shared
            .driver
            .as_mut()
            .ok_or("busy: The target is not running")?
            .interrupt_now(session, &mut out)?;
        let wake = deliver(&mut shared, &self.inbox, out);
        drop(shared);
        if wake {
            (self.wake)();
        }
        Ok(())
    }
    fn send(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        model: Option<&str>,
        approval: ApprovalState,
        text: String,
    ) -> Result<(), &'static str> {
        CodexConnection::send(self, session, cwd, resume, model, approval, text);
        Ok(())
    }

    /// One app-server serves every directory, so `cwd` plays no part.
    fn request_catalog(&self, session: u64, _: &str) {
        CodexConnection::request_catalog(self, session);
    }

    fn set_approval(&self, session: u64, approval: ApprovalState) {
        CodexConnection::set_approval(self, session, approval);
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
    /// here rather than when the last reference goes. Queued commands are not waited
    /// for, and a start only up to [`START_WAIT`].
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.detach();
        }
        let process = {
            let mut shared = self.shared.lock().unwrap();
            shared.closed = true;
            let (mut shared, _) = self
                .started
                .wait_timeout_while(shared, START_WAIT, |s| s.starting)
                .unwrap();
            shared.generation += 1;
            shared.driver = None;
            shared.leases.clear();
            shared.process.take()
        };
        drop(process);
    }
}

/// Sends the driver's writes, or keeps them for a process still starting, and queues
/// its events. Returns whether the inbox went from empty to non-empty, which is when
/// the UI needs waking.
fn deliver(shared: &mut Shared, inbox: &Mutex<Vec<(u64, Event)>>, mut out: Outputs) -> bool {
    mcp_events(shared.mcp.as_ref(), &out.events);
    for (session, event) in &out.events {
        if matches!(event, Event::Failed { .. })
            && !shared
                .driver
                .as_ref()
                .is_some_and(|driver| driver.is_open(*session))
        {
            shared.leases.remove(session);
        }
    }
    match &shared.process {
        Some(process) => {
            for line in out.writes.drain(..) {
                process.send(line);
            }
        }
        None => shared.pending.append(&mut out.writes),
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
/// Each session's commands run on a worker thread of its own, in the order they were
/// given, and a failure to start arrives as [`Event::Failed`]. Finding and starting
/// Claude hold no lock, so a session that is stuck starting delays only its own
/// commands: other sessions' messages, Stop, and answers go through, and [`send`]
/// never waits for a start.
///
/// The driver writes a message only once Claude has confirmed the session's latest
/// approval mode, under the lock every mode change takes. When the mode needs a new
/// process and nothing of the turn was written, the session's worker replaces the
/// process and carries the turn's messages over.
///
/// [`send`]: ClaudeConnection::send
pub struct ClaudeConnection {
    starter: ClaudeStarter,
}

type Workers = Arc<Mutex<HashMap<u64, Worker>>>;

/// How long Claude may take to answer `initialize` or `set_permission_mode` before
/// the messages waiting for it fail and its process is replaced.
const MODE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long Claude may take to report the user's own permission mode.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Default)]
struct ClaudeShared {
    mcp: Option<Issuer>,
    notices: version::Notices,
    /// The `claude` program, found on first use when not given.
    program: Option<PathBuf>,
    live: HashMap<u64, Live>,
    /// The latest conversation per session, kept across process restarts.
    threads: HashMap<u64, String>,
    /// Messages per session that `send` accepted and the worker has not sent yet.
    queued: HashMap<u64, usize>,
    /// Each session's approval state. A process is launched in its mode, and replaced
    /// before a turn that its mode cannot run.
    approvals: HashMap<u64, ApprovalState>,
    /// Processes being started, outside the lock.
    starting: usize,
    /// Incremented per process, so a late exit from an old one is ignored.
    generation: u64,
    /// Set when the connection is dropped, so queued commands start nothing.
    closed: bool,
    before_start: Option<StartHook>,
    /// [`MODE_TIMEOUT`], shorter in tests.
    mode_timeout: Option<Duration>,
    launch: Launch,
    /// [`PROBE_TIMEOUT`] for the catalog probe, shorter in tests.
    probe_timeout: Option<Duration>,
    /// Processes reading the models Claude offers, here so dropping the connection
    /// ends them too. Keyed by `next_probe`.
    catalog_probes: HashMap<u64, Process>,
    next_probe: u64,
}

/// A session's process, from when it begins starting until it exits.
struct Live {
    _lease: Option<Lease>,
    driver: ClaudeDriver,
    /// `None` while the process starts.
    process: Option<Process>,
    /// The process reading the user's own permission mode while it runs, here so
    /// dropping the connection ends it too.
    probe: Option<Process>,
    /// Lines written, and events, from before the process could take them. Events
    /// wait too, so a start that fails reports only the failure.
    pending: Outputs,
    /// Why the process exited before the worker could use it.
    exited: Option<String>,
    generation: u64,
    /// What the process was started with, for a replacement.
    cwd: PathBuf,
    resume: Option<String>,
    model: Option<String>,
    /// Whether the worker has a job queued to replace the process.
    replacing: bool,
}

impl ClaudeShared {
    /// Why a message sent now could not steer, if it could not. A message still queued
    /// will start a turn, so a second one would steer it.
    fn steer_blocked(&self, session: u64) -> Option<&'static str> {
        let live = self.live.get(&session).map(|live| &live.driver);
        if self.queued.get(&session).is_some_and(|n| *n > 0) {
            // The queued message will start a turn, in a new process when this one
            // cannot run its approval mode.
            return match live {
                Some(driver) if !(driver.needs_restart() && !driver.is_running()) => {
                    driver.steer_unsupported()
                }
                _ => Some(claude::STEER_NOT_YET),
            };
        }
        live.and_then(ClaudeDriver::steer_blocked)
    }

    /// Counts one of the session's queued messages as handled.
    fn unqueue(&mut self, session: u64) {
        if let Some(n) = self.queued.get_mut(&session) {
            *n -= 1;
            if *n == 0 {
                self.queued.remove(&session);
            }
        }
    }
}

/// Records the session's approval state and gives it to its driver, which may now
/// write what waited for it, or need a new process. Returns what the driver wrote.
fn configure(shared: &mut ClaudeShared, session: u64, approval: ApprovalState) -> Outputs {
    let approval = shared.launch.approval(approval);
    if let Some(issuer) = &shared.mcp {
        issuer.set_mode(session, approval.mode);
    }
    shared.approvals.insert(session, approval);
    let mut out = Outputs::default();
    if let Some(live) = shared.live.get_mut(&session) {
        live.driver
            .command(Command::Configure { session, approval }, &mut out);
    }
    out
}

/// What a job on a session's worker is for.
enum Job {
    /// A message, with the directory, saved conversation, and model to start a process
    /// with.
    Message(PathBuf, Option<String>, Option<String>, Option<Caller>),
    /// Stop or an answer: nothing to start.
    Command,
    /// Replaces a process that cannot run the waiting turn's approval mode.
    Replace,
}

impl ClaudeConnection {
    /// `program` is the `claude` binary to run, or `None` to look for it on first use.
    pub fn new(program: Option<PathBuf>, wake: Wake) -> Self {
        Self::launching(program, wake, Launch::Chat)
    }

    /// A connection for the sidebar layout helper, whose reply is text Keystone parses.
    /// Every process starts in plan mode with flags to disable tools, MCP servers,
    /// slash commands, skills, hooks, and conversation persistence. It never resumes.
    /// Caller approval modes are ignored and approval answers cannot grant access.
    /// The caller still handles approval requests and must answer them to continue.
    /// The turn fails if reported capabilities contradict these restrictions or cannot
    /// be confirmed. These checks cannot undo startup actions; managed settings may apply.
    pub fn for_layout_helper(program: Option<PathBuf>, wake: Wake) -> Self {
        Self::launching(program, wake, Launch::LayoutHelper)
    }

    fn launching(program: Option<PathBuf>, wake: Wake, launch: Launch) -> Self {
        Self {
            starter: ClaudeStarter {
                wake,
                shared: Arc::new(Mutex::new(ClaudeShared {
                    program,
                    launch,
                    ..ClaudeShared::default()
                })),
                started: Arc::default(),
                inbox: Arc::default(),
                workers: Arc::default(),
            },
        }
    }

    /// The session's process ID while it runs.
    pub fn pid(&self, session: u64) -> Option<u32> {
        let shared = self.starter.shared.lock().unwrap();
        let live = shared.live.get(&session)?;
        live.process.as_ref().map(Process::pid)
    }

    pub fn is_running(&self, session: u64) -> bool {
        let shared = self.starter.shared.lock().unwrap();
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
        model: Option<&str>,
        approval: ApprovalState,
        text: String,
    ) -> Result<(), &'static str> {
        self.send_inner(session, cwd, resume, model, approval, text, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn send_inner(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        model: Option<&str>,
        approval: ApprovalState,
        text: String,
        caller: Option<Caller>,
    ) -> Result<(), &'static str> {
        let issuer;
        {
            let mut shared = self.starter.shared.lock().unwrap();
            if let Some(caller) = caller {
                authorize(shared.mcp.as_ref(), caller, approval.mode)?;
                if shared
                    .live
                    .get(&session)
                    .is_some_and(|live| live.driver.is_running())
                    || shared.queued.contains_key(&session)
                {
                    return Err("busy: The target already has a running turn");
                }
            }
            issuer = shared.mcp.clone();
            let out = configure(&mut shared, session, approval);
            let refused = shared.steer_blocked(session);
            if refused.is_none() {
                *shared.queued.entry(session).or_default() += 1;
            }
            self.starter.deliver(shared, session, out);
            if let Some(reason) = refused {
                return Err(reason);
            }
        }
        let start = Job::Message(
            PathBuf::from(cwd),
            resume.map(str::to_owned),
            model.map(str::to_owned),
            caller,
        );
        self.starter.queue(session, start, move |driver, out| {
            if let Some(caller) = caller {
                if driver.is_running() {
                    out.event(
                        session,
                        start_failed("busy: The target already has a running turn".into()),
                    );
                    return;
                }
                driver.send_from(text, issuer.unwrap(), caller, out);
            } else {
                driver.command(Command::Send { session, text }, out);
            }
        });
        Ok(())
    }

    /// Asks a fresh `claude` in `cwd` which models it offers, on a thread of its own;
    /// see [`Connection::request_catalog`]. A layout helper connection refuses, starting
    /// nothing, since its every process runs restricted.
    pub fn request_catalog(&self, session: u64, cwd: &str) {
        if self.starter.shared.lock().unwrap().launch == Launch::LayoutHelper {
            let refused = catalog_failed("The layout helper does not list models.".into());
            return push(&self.starter.inbox, &self.starter.wake, session, refused);
        }
        let starter = self.starter.clone();
        let cwd = PathBuf::from(cwd);
        std::thread::spawn(move || starter.probe_catalog(session, &cwd));
    }

    /// Stops or answers within a running session. With no process there is nothing
    /// to stop or answer.
    pub fn command(&self, command: Command) {
        let session = command_session(&command);
        self.starter
            .queue(session, Job::Command, move |driver, out| {
                driver.command(command, out)
            });
    }

    /// Changes the session's approval state at once; see [`Connection::set_approval`].
    pub fn set_approval(&self, session: u64, approval: ApprovalState) {
        let mut shared = self.starter.shared.lock().unwrap();
        let out = configure(&mut shared, session, approval);
        self.starter.deliver(shared, session, out);
    }

    /// Takes every event that arrived since the last call.
    pub fn drain(&self) -> Vec<(u64, Event)> {
        std::mem::take(&mut self.starter.inbox.lock().unwrap())
    }

    #[doc(hidden)]
    pub fn hold_starts(&self, hook: StartHook) {
        self.starter.shared.lock().unwrap().before_start = Some(hook);
    }

    /// Shortens how long Claude may take to start or confirm a permission mode.
    #[doc(hidden)]
    pub fn set_mode_timeout(&self, timeout: Duration) {
        self.starter.shared.lock().unwrap().mode_timeout = Some(timeout);
    }

    /// Shortens how long a catalog probe may take to answer, for tests.
    pub fn set_probe_timeout(&self, timeout: Duration) {
        self.starter.shared.lock().unwrap().probe_timeout = Some(timeout);
    }
}

/// What a session's worker, its process's reader, and its timers share.
#[derive(Clone)]
struct ClaudeStarter {
    wake: Wake,
    shared: Arc<Mutex<ClaudeShared>>,
    /// Signalled when a start finishes.
    started: Arc<Condvar>,
    /// Kept apart from `shared`, so draining never waits for a process to start.
    inbox: Inbox,
    workers: Workers,
}

impl ClaudeStarter {
    /// Queues `act` for the session's driver on the session's worker.
    fn queue(
        &self,
        session: u64,
        job: Job,
        act: impl FnOnce(&mut ClaudeDriver, &mut Outputs) + Send + 'static,
    ) {
        let starter = self.clone();
        let mut workers = self.workers.lock().unwrap();
        let worker = workers
            .entry(session)
            .or_insert_with(|| Worker::new("claude-session"));
        worker.run(move || starter.run(session, job, act));
    }

    /// Delivers the driver's outputs ([`claude_deliver`]), then, without the lock,
    /// wakes the UI, times new `initialize` and `set_permission_mode` requests, and
    /// queues the process's replacement when a waiting turn needs one.
    fn deliver(&self, mut shared: MutexGuard<'_, ClaudeShared>, session: u64, out: Outputs) {
        let wake = claude_deliver(&mut shared, &self.inbox, session, out);
        let timeout = shared.mode_timeout.unwrap_or(MODE_TIMEOUT);
        let mut timed = Vec::new();
        let mut replace = false;
        let mut generation = 0;
        if let Some(live) = shared.live.get_mut(&session)
            && live.process.is_some()
        {
            timed = live.driver.take_timed_requests();
            generation = live.generation;
            if live.driver.wants_restart() && !live.replacing {
                live.replacing = true;
                replace = true;
            }
        }
        drop(shared);
        if wake {
            (self.wake)();
        }
        if replace {
            self.queue(session, Job::Replace, |_, _| {});
        }
        for id in timed {
            let starter = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(timeout);
                let mut shared = starter.shared.lock().unwrap();
                let mut out = Outputs::default();
                match shared.live.get_mut(&session) {
                    Some(live) if live.generation == generation => {
                        live.driver.request_timed_out(&id, &mut out)
                    }
                    _ => return,
                }
                starter.deliver(shared, session, out);
            });
        }
    }

    /// Runs `act` on the session's driver. A message, or a [`Job::Replace`], first
    /// replaces a process that cannot run the session's approval mode, if no turn of
    /// its own was written yet, carrying any waiting turn over; a message starts a
    /// process when none runs.
    fn run(&self, session: u64, job: Job, act: impl FnOnce(&mut ClaudeDriver, &mut Outputs)) {
        let mut shared = self.shared.lock().unwrap();
        if shared.closed {
            return;
        }
        if let Job::Message(_, _, _, Some(caller)) = &job
            && !shared.mcp.as_ref().is_some_and(|issuer| {
                issuer.permits_launch(
                    *caller,
                    shared
                        .approvals
                        .get(&session)
                        .copied()
                        .unwrap_or_default()
                        .mode,
                )
            })
        {
            shared.unqueue(session);
            drop(shared);
            push(
                &self.inbox,
                &self.wake,
                session,
                start_failed(
                    "The launching agent no longer has permission to start this conversation"
                        .into(),
                ),
            );
            return;
        }
        let message = matches!(job, Job::Message(..));
        let command = matches!(job, Job::Command);
        let mut start = match job {
            Job::Message(cwd, resume, model, _) => Some((cwd, resume, model)),
            Job::Command | Job::Replace => None,
        };
        let mut carried = Vec::new();
        let mut lost = false;
        if let Some(live) = shared.live.get_mut(&session) {
            if !message && !command {
                live.replacing = false;
            }
            // This worker runs the session's commands in order, so no start of its own
            // is in progress, and the process is in place. A running turn moves only if
            // nothing of it was written.
            let replace = if live.driver.is_running() {
                !command && live.driver.wants_restart()
            } else {
                message && live.driver.needs_restart()
            };
            if replace {
                carried = live.driver.take_unsent();
                let live = shared.live.remove(&session).expect("checked above");
                start = start.or(Some((
                    live.cwd.clone(),
                    live.resume.clone(),
                    live.model.clone(),
                )));
                self.inbox.lock().unwrap().push((session, Event::ModelLost));
                lost = true;
                // Dropping the handle ends the process; its late exit is ignored.
                drop(live);
            }
        }
        if !shared.live.contains_key(&session) {
            if let Some((cwd, resume, model)) = start
                && (message || !carried.is_empty())
            {
                let act = move |driver: &mut ClaudeDriver, out: &mut Outputs| {
                    driver.carry(carried, out);
                    if message {
                        act(driver, out);
                    }
                };
                self.start(shared, session, &cwd, resume, model, message, act);
            } else {
                drop(shared);
            }
            if lost {
                (self.wake)();
            }
            return;
        }
        if message {
            shared.unqueue(session);
        }
        let mut out = Outputs::default();
        let live = shared.live.get_mut(&session).expect("checked above");
        act(&mut live.driver, &mut out);
        self.deliver(shared, session, out);
    }

    /// Starts the session's process for `act`: its first message, or a turn carried
    /// over. The driver takes it first, so it knows its turn when the first output
    /// arrives, which can come before Claude reads anything. Nothing is written to
    /// Claude until it answers `initialize`, under the lock, in the latest mode.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &self,
        mut shared: MutexGuard<'_, ClaudeShared>,
        session: u64,
        cwd: &Path,
        resume: Option<String>,
        model: Option<String>,
        message: bool,
        act: impl FnOnce(&mut ClaudeDriver, &mut Outputs),
    ) {
        let launch = shared.launch;
        let resume = match launch {
            Launch::Chat => shared.threads.get(&session).cloned().or(resume),
            Launch::LayoutHelper => None,
        };
        shared.generation += 1;
        let generation = shared.generation;
        let approval = shared.approvals.get(&session).copied().unwrap_or_default();
        let mode = approval.mode;
        // A conversation Keystone set a mode in may resume in it, such as plan mode,
        // so the user's own settings are read from a fresh process and passed.
        let probe = mode == ApprovalMode::Provider && resume.is_some() && approval.overridden;
        let mut pending = Outputs::default();
        let mut driver = ClaudeDriver::launched(
            session,
            resume.is_some(),
            random_seed(),
            mode,
            launch,
            &mut pending,
        );
        let mut lease = None;
        if launch == Launch::Chat
            && let Some(issuer) = &shared.mcp
        {
            match issuer.issue(session) {
                Ok(credentials) => {
                    let (url, token, issued) = credentials.into_parts();
                    lease = Some(issued);
                    issuer.set_mode(session, mode);
                    driver.set_mcp(url, token);
                }
                Err(_) => {
                    if message {
                        shared.unqueue(session);
                    }
                    drop(shared);
                    push(
                        &self.inbox,
                        &self.wake,
                        session,
                        start_failed("Could not issue Keystone tool credentials".into()),
                    );
                    return;
                }
            }
        }
        act(&mut driver, &mut pending);
        if let Some(issuer) = &shared.mcp {
            issuer.set_running(session, driver.is_running());
        }
        // Counted only now that the driver has the turn, so a message sent during the
        // start is refused like any steer too early to send.
        if message {
            shared.unqueue(session);
        }
        shared.live.insert(
            session,
            Live {
                _lease: lease,
                driver,
                process: None,
                probe: None,
                pending,
                exited: None,
                generation,
                cwd: cwd.to_owned(),
                resume: resume.clone(),
                model: model.clone(),
                replacing: false,
            },
        );
        shared.starting += 1;
        let program = shared.program.clone();
        let hook = shared.before_start.clone();
        drop(shared);
        let spawned = self.spawn(
            session, generation, program, hook, cwd, resume, model, mode, probe, launch,
        );

        let mut shared = self.shared.lock().unwrap();
        let current = shared
            .live
            .get(&session)
            .is_some_and(|live| live.generation == generation);
        if shared.closed || !current {
            drop(shared);
            drop(spawned);
            self.finish_start();
            return;
        }
        let mut live = shared.live.remove(&session).expect("checked above");
        let mut out = std::mem::take(&mut live.pending);
        let ended = match spawned {
            Err(message) => {
                live._lease = None;
                drop(shared);
                self.finish_start();
                push(&self.inbox, &self.wake, session, start_failed(message));
                return;
            }
            Ok(process) => {
                for line in out.writes.drain(..) {
                    process.send(line);
                }
                live.process = Some(process);
                match live.exited.take() {
                    Some(reason) => {
                        live._lease = None;
                        live.driver.on_exit(&reason, &mut out);
                        Some(live)
                    }
                    None => {
                        shared.live.insert(session, live);
                        None
                    }
                }
            }
        };
        shared.starting -= 1;
        self.started.notify_all();
        // The mode may have changed during the start to one this process cannot run.
        self.deliver(shared, session, out);
        // Dropping the handle of a process that already exited ends what it left.
        drop(ended);
    }

    fn finish_start(&self) {
        self.shared.lock().unwrap().starting -= 1;
        self.started.notify_all();
    }

    /// Finds and starts the session's `claude`, holding no lock. A mode Keystone
    /// selected is always passed, on `--resume` too; with Provider settings, `probe`
    /// asks for the user's own mode to pass. A model the session asked for is passed
    /// on every launch too, since whether a resume keeps it is not known.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &self,
        session: u64,
        generation: u64,
        program: Option<PathBuf>,
        hook: Option<StartHook>,
        cwd: &Path,
        resume: Option<String>,
        model: Option<String>,
        mode: ApprovalMode,
        probe: bool,
        launch: Launch,
    ) -> Result<Process, String> {
        if let Some(hook) = hook {
            hook(session);
        }
        let program = self.find_program(program)?;
        let compatibility = version::detect(Provider::Claude, &program, cwd)?;
        {
            let mut shared = self.shared.lock().unwrap();
            match shared.live.get_mut(&session) {
                Some(live) if live.generation == generation => {
                    live.driver.set_cancel_queued(compatibility.cancel_queued)
                }
                _ => {}
            }
        }
        self.report_compatibility(session, compatibility.notice);
        let baseline = if probe {
            Some(self.probe_permission_mode(session, generation, &program, cwd, launch)?)
        } else {
            None
        };
        let args = launch.args(
            mode,
            baseline.as_deref(),
            model.as_deref(),
            resume.as_deref(),
        );
        let on_line = {
            let starter = self.clone();
            move |line: &[u8]| {
                let mut guard = starter.shared.lock().unwrap();
                let mut out = Outputs::default();
                match guard.live.get_mut(&session) {
                    Some(live) if live.generation == generation => {
                        if let Ok(init) = serde_json::from_slice::<ClaudeCapabilityFrame>(line)
                            && init.kind == "system"
                            && init.subtype.as_deref() == Some("init")
                            && init.parent_tool_use_id.is_none()
                        {
                            live.driver.set_cancel_queued(
                                init.capabilities
                                    .iter()
                                    .any(|capability| capability == "interrupt_cancel_queued_v1"),
                            );
                        }
                        live.driver.on_line(line, &mut out)
                    }
                    // Output from a replaced process is not for the new driver.
                    _ => return,
                }
                starter.deliver(guard, session, out);
            }
        };
        let on_exit = {
            let shared = self.shared.clone();
            let inbox = self.inbox.clone();
            let wake = self.wake.clone();
            move |exit: Exit| {
                let mut guard = shared.lock().unwrap();
                let reason = exit_reason("Claude Code", &exit, guard.mcp.is_some());
                match guard.live.get_mut(&session) {
                    Some(live) if live.generation == generation => {
                        // Exited before the worker took it: the worker reports it.
                        if live.process.is_none() {
                            live.exited = Some(reason);
                            return;
                        }
                    }
                    _ => return,
                }
                let mut live = guard.live.remove(&session).expect("checked above");
                live._lease = None;
                let mut out = Outputs::default();
                live.driver.on_exit(&reason, &mut out);
                mcp_events(guard.mcp.as_ref(), &out.events);
                inbox.lock().unwrap().append(&mut out.events);
                drop(guard);
                // Dropping the handle kills anything the process left behind.
                drop(live);
                wake();
            }
        };
        Process::spawn(&program, &args, cwd, on_line, on_exit)
            .map_err(|e| format!("Could not start Claude Code at {}. {e}", program.display()))
    }

    /// The `claude` to run: `program` when given, else the one found on first use.
    fn find_program(&self, program: Option<PathBuf>) -> Result<PathBuf, String> {
        match program {
            Some(program) => Ok(program),
            None => {
                let found = crate::find::program("claude").ok_or(CLAUDE_NOT_INSTALLED)?;
                self.shared.lock().unwrap().program = Some(found.clone());
                Ok(found)
            }
        }
    }

    /// Asks a fresh `claude` in `cwd` which models it offers, as its `initialize`
    /// answer lists them, and delivers the answer as [`Event::Catalog`] for `session`.
    /// The process is closed as soon as it answers, with no message sent, or when
    /// [`PROBE_TIMEOUT`] passes; one that exits first fails the catalog with why it
    /// stopped. It belongs to the connection, so dropping the connection ends it, and
    /// then nothing is delivered.
    fn probe_catalog(&self, session: u64, cwd: &Path) {
        let program = self.shared.lock().unwrap().program.clone();
        let catalog = match self.find_program(program) {
            Err(message) => Some(Catalog::Failed(message)),
            Ok(program) => self.read_catalog(session, &program, cwd),
        };
        if let Some(catalog) = catalog {
            push(&self.inbox, &self.wake, session, Event::Catalog(catalog));
        }
    }

    fn report_compatibility(&self, session: u64, notice: Option<String>) {
        let mut shared = self.shared.lock().unwrap();
        if shared.closed {
            return;
        }
        let changed = shared.notices.update(session, notice);
        drop(shared);
        if let Some(message) = changed {
            push(
                &self.inbox,
                &self.wake,
                session,
                Event::ProviderNotice { message },
            );
        }
    }

    /// Runs the catalog probe; `None` when the connection was dropped meanwhile.
    fn read_catalog(&self, session: u64, program: &Path, cwd: &Path) -> Option<Catalog> {
        let compatibility = match version::detect(Provider::Claude, program, cwd) {
            Err(message) => return Some(Catalog::Failed(message)),
            Ok(compatibility) => compatibility,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let exited = tx.clone();
        let on_line = move |line: &[u8]| {
            let Ok(message) = serde_json::from_slice::<serde_json::Value>(line) else {
                return;
            };
            if message["type"] == "control_response"
                && message["response"]["request_id"] == "keystone-catalog"
            {
                let _ = tx.send(claude::initialize_catalog(line));
            }
        };
        let on_exit = move |exit: Exit| {
            let _ = exited.send(Some(Catalog::Failed(exit_reason(
                "Claude Code",
                &exit,
                false,
            ))));
        };
        let process = match Process::spawn(program, &claude::ARGS, cwd, on_line, on_exit) {
            Ok(process) => process,
            Err(e) => {
                return Some(Catalog::Failed(format!(
                    "Could not start Claude Code at {}. {e}",
                    program.display()
                )));
            }
        };
        process.send(
            serde_json::json!({"type": "control_request", "request_id": "keystone-catalog",
                "request": {"subtype": "initialize"}})
            .to_string(),
        );
        let (id, timeout) = {
            let mut shared = self.shared.lock().unwrap();
            if shared.closed {
                drop(shared);
                drop(process);
                return None;
            }
            shared.next_probe += 1;
            let id = shared.next_probe;
            shared.catalog_probes.insert(id, process);
            (id, shared.probe_timeout.unwrap_or(PROBE_TIMEOUT))
        };
        let answer = rx.recv_timeout(timeout);
        let process = self.shared.lock().unwrap().catalog_probes.remove(&id);
        let closed = process.is_none();
        drop(process);
        if closed {
            return None;
        }
        let catalog = match answer {
            Ok(Some(catalog)) => catalog,
            Ok(None) => Catalog::Failed("Claude Code did not report its models.".into()),
            Err(RecvTimeoutError::Timeout) => {
                Catalog::Failed("Claude Code did not answer in time.".into())
            }
            Err(RecvTimeoutError::Disconnected) => {
                Catalog::Failed("Claude Code stopped before answering.".into())
            }
        };
        if matches!(catalog, Catalog::Ready { .. }) {
            self.report_compatibility(session, compatibility.notice);
        }
        Some(catalog)
    }

    /// The permission mode the user's own settings give a new conversation in `cwd`,
    /// as a fresh `claude` reports it in its `initialize` answer. It is closed at once,
    /// with no message sent. The probe is kept with the session's start, so a replaced
    /// start or a dropped connection ends it, and then this fails at once.
    fn probe_permission_mode(
        &self,
        session: u64,
        generation: u64,
        program: &Path,
        cwd: &Path,
        launch: Launch,
    ) -> Result<String, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let on_line = move |line: &[u8]| {
            let Ok(message) = serde_json::from_slice::<serde_json::Value>(line) else {
                return;
            };
            let response = &message["response"];
            if message["type"] == "control_response" && response["request_id"] == "keystone-probe" {
                let _ = tx.send(
                    response["response"]["current_permission_mode"]
                        .as_str()
                        .map(str::to_owned),
                );
            }
        };
        let failed = |reason: &str| {
            format!(
                "Could not read your Claude Code approval settings to restore them ({reason}). Choose an approval mode to continue."
            )
        };
        if launch == Launch::LayoutHelper {
            return Err(failed("the layout helper never reads them"));
        }
        let process = Process::spawn(program, &claude::ARGS, cwd, on_line, |_| {})
            .map_err(|e| failed(&e.to_string()))?;
        process.send(
            serde_json::json!({"type": "control_request", "request_id": "keystone-probe",
                "request": {"subtype": "initialize"}})
            .to_string(),
        );
        {
            let mut shared = self.shared.lock().unwrap();
            let closed = shared.closed;
            match shared.live.get_mut(&session) {
                Some(live) if live.generation == generation && !closed => {
                    live.probe = Some(process);
                }
                _ => {
                    drop(shared);
                    drop(process);
                    return Err(failed("Keystone closed the session"));
                }
            }
        }
        let answer = rx.recv_timeout(PROBE_TIMEOUT);
        let process = {
            let mut shared = self.shared.lock().unwrap();
            match shared.live.get_mut(&session) {
                Some(live) if live.generation == generation => live.probe.take(),
                _ => None,
            }
        };
        let ended = process.is_none();
        drop(process);
        match answer {
            _ if ended => Err(failed("Keystone closed the session")),
            Ok(Some(mode)) => Ok(mode),
            Ok(None) => Err(failed("Claude Code did not report it")),
            Err(_) => Err(failed("Claude Code did not answer")),
        }
    }
}

impl Connection for ClaudeConnection {
    fn set_mcp(&self, issuer: Issuer) {
        let mut shared = self.starter.shared.lock().unwrap();
        if shared.launch == Launch::Chat {
            shared.mcp = Some(issuer);
        }
    }

    fn is_running(&self, session: u64) -> bool {
        ClaudeConnection::is_running(self, session)
    }

    fn send_from(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        model: Option<&str>,
        approval: ApprovalState,
        text: String,
        caller: Caller,
    ) -> Result<(), &'static str> {
        self.send_inner(session, cwd, resume, model, approval, text, Some(caller))
    }

    fn steer_from(&self, session: u64, text: String, caller: Caller) -> Result<(), &'static str> {
        let mut shared = self.starter.shared.lock().unwrap();
        authorize(
            shared.mcp.as_ref(),
            caller,
            shared
                .approvals
                .get(&session)
                .copied()
                .unwrap_or_default()
                .mode,
        )?;
        if shared.closed || shared.queued.contains_key(&session) {
            return Err("busy: The target cannot steer immediately");
        }
        let live = shared
            .live
            .get_mut(&session)
            .ok_or("busy: The target is not running")?;
        if live.process.is_none() || live.replacing {
            return Err("busy: The target is not running");
        }
        let mut out = Outputs::default();
        live.driver.steer(text, &mut out)?;
        self.starter.deliver(shared, session, out);
        Ok(())
    }

    fn interrupt_from(&self, session: u64, caller: Caller) -> Result<(), &'static str> {
        let mut shared = self.starter.shared.lock().unwrap();
        authorize(
            shared.mcp.as_ref(),
            caller,
            shared
                .approvals
                .get(&session)
                .copied()
                .unwrap_or_default()
                .mode,
        )?;
        if shared.queued.contains_key(&session) {
            return Err("busy: The target is still starting");
        }
        if !shared
            .live
            .get(&session)
            .is_some_and(|live| live.driver.is_running())
        {
            return Ok(());
        }
        if shared.closed {
            return Err("busy: The target is not running");
        }
        let live = shared
            .live
            .get_mut(&session)
            .ok_or("busy: The target is not running")?;
        if live.process.is_none() || live.replacing || !live.driver.is_running() {
            return Err("busy: The target is not running");
        }
        let mut out = Outputs::default();
        live.driver
            .command(Command::Interrupt { session }, &mut out);
        self.starter.deliver(shared, session, out);
        Ok(())
    }
    fn send(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        model: Option<&str>,
        approval: ApprovalState,
        text: String,
    ) -> Result<(), &'static str> {
        ClaudeConnection::send(self, session, cwd, resume, model, approval, text)
    }

    fn request_catalog(&self, session: u64, cwd: &str) {
        ClaudeConnection::request_catalog(self, session, cwd);
    }

    fn set_approval(&self, session: u64, approval: ApprovalState) {
        ClaudeConnection::set_approval(self, session, approval);
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
    /// here rather than when the last reference goes. Queued commands are not waited
    /// for, and starts only up to [`START_WAIT`]; a start that finishes after this
    /// ends its own process.
    fn drop(&mut self) {
        let workers: Vec<Worker> = {
            let mut workers = self.starter.workers.lock().unwrap();
            workers.drain().map(|(_, worker)| worker).collect()
        };
        for worker in workers {
            worker.detach();
        }
        let (live, probes): (Vec<Live>, Vec<Process>) = {
            let mut shared = self.starter.shared.lock().unwrap();
            shared.closed = true;
            shared.generation += 1;
            (
                shared
                    .live
                    .drain()
                    .map(|(_, mut live)| {
                        live._lease = None;
                        live
                    })
                    .collect(),
                shared.catalog_probes.drain().map(|(_, p)| p).collect(),
            )
        };
        drop(live);
        drop(probes);
        let shared = self.starter.shared.lock().unwrap();
        let _ = self
            .starter
            .started
            .wait_timeout_while(shared, START_WAIT, |s| s.starting > 0)
            .unwrap();
    }
}

/// Sends the driver's writes, or keeps them for a process still starting, and queues
/// its events. Returns whether the inbox went from empty to non-empty, which is when
/// the UI needs waking.
fn claude_deliver(
    shared: &mut ClaudeShared,
    inbox: &Inbox,
    session: u64,
    mut out: Outputs,
) -> bool {
    mcp_events(shared.mcp.as_ref(), &out.events);
    if shared
        .live
        .get(&session)
        .is_some_and(|live| live.driver.credentials_failed())
        && let Some(live) = shared.live.get_mut(&session)
    {
        live._lease = None;
    }
    if let Some(live) = shared.live.get_mut(&session) {
        match &live.process {
            Some(process) => {
                for line in out.writes.drain(..) {
                    process.send(line);
                }
            }
            None => {
                live.pending.writes.append(&mut out.writes);
                live.pending.events.append(&mut out.events);
            }
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

fn authorize(
    issuer: Option<&Issuer>,
    caller: Caller,
    mode: ApprovalMode,
) -> Result<(), &'static str> {
    if issuer.is_some_and(|issuer| issuer.authorize(caller, mode)) {
        Ok(())
    } else {
        Err(
            "The calling agent is no longer running or its approval mode does not permit this action",
        )
    }
}

fn mcp_events(issuer: Option<&Issuer>, events: &[(u64, Event)]) {
    let Some(issuer) = issuer else { return };
    for (session, event) in events {
        match event {
            Event::TurnStarted => issuer.set_running(*session, true),
            Event::TurnCompleted { .. } | Event::Failed { .. } => {
                issuer.set_running(*session, false)
            }
            _ => {}
        }
    }
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

#[cfg(test)]
#[path = "provider_mcp_tests.rs"]
mod provider_mcp_tests;
