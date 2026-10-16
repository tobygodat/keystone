//! A running provider connection: the driver, its process, and an inbox of events
//! for the UI.

use crate::codex::{CodexDriver, Command, Outputs};
use crate::host::{Exit, Process};
use crate::item::Event;
use crate::worker::Worker;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Called from a reader thread when the inbox goes from empty to non-empty. The UI
/// drains everything in one pass, so a burst of streamed text costs one update.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

type Inbox = Arc<Mutex<Vec<(u64, Event)>>>;

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
        let session = match &command {
            Command::Open { session, .. }
            | Command::Send { session, .. }
            | Command::Interrupt { session }
            | Command::Answer { session, .. } => *session,
        };
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
                starter
                    .inbox
                    .lock()
                    .unwrap()
                    .push((session, Event::Failed { message }));
                (starter.wake)();
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
                let reason = match (exit.code, exit.stderr_tail.is_empty()) {
                    (_, false) => format!("Codex stopped: {}", exit.stderr_tail),
                    (Some(code), true) => format!("Codex stopped with exit code {code}"),
                    (None, true) => "Codex stopped".to_owned(),
                };
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
