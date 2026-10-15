//! A running provider connection: the driver, its process, and an inbox of events
//! for the UI.

use crate::codex::{CodexDriver, Command, Outputs};
use crate::host::{Exit, Process};
use crate::item::Event;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Called from a reader thread when the inbox goes from empty to non-empty. The UI
/// drains everything in one pass, so a burst of streamed text costs one update.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// One shared `codex app-server` for every Codex session. The process starts on the
/// first command and starts again after it exits.
pub struct CodexConnection {
    program: PathBuf,
    client_version: String,
    wake: Wake,
    shared: Arc<Mutex<Shared>>,
}

#[derive(Default)]
struct Shared {
    driver: Option<CodexDriver>,
    process: Option<Process>,
    inbox: Vec<(u64, Event)>,
    /// Incremented per process, so a late exit from an old one is ignored.
    generation: u64,
}

impl CodexConnection {
    pub fn new(program: PathBuf, client_version: &str, wake: Wake) -> Self {
        Self {
            program,
            client_version: client_version.to_owned(),
            wake,
            shared: Arc::default(),
        }
    }

    pub fn program(&self) -> &Path {
        &self.program
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

    /// Sends a command, starting the app-server first if it is not running.
    pub fn command(&self, command: Command) -> io::Result<()> {
        let mut shared = self.shared.lock().unwrap();
        if shared.process.is_none() {
            self.start(&mut shared)?;
        }
        let mut out = Outputs::default();
        shared
            .driver
            .as_mut()
            .expect("driver exists while the process runs")
            .command(command, &mut out);
        deliver(&mut shared, out);
        let wake = !shared.inbox.is_empty();
        drop(shared);
        if wake {
            (self.wake)();
        }
        Ok(())
    }

    /// Takes every event that arrived since the last call.
    pub fn drain(&self) -> Vec<(u64, Event)> {
        std::mem::take(&mut self.shared.lock().unwrap().inbox)
    }

    fn start(&self, shared: &mut Shared) -> io::Result<()> {
        shared.generation += 1;
        let generation = shared.generation;
        let mut out = Outputs::default();
        shared.driver = Some(CodexDriver::new(&self.client_version, &mut out));
        let on_line = {
            let shared = self.shared.clone();
            let wake = self.wake.clone();
            move |line: &[u8]| {
                let mut guard = shared.lock().unwrap();
                let was_empty = guard.inbox.is_empty();
                let mut out = Outputs::default();
                if let Some(driver) = guard.driver.as_mut() {
                    driver.on_line(line, &mut out);
                }
                deliver(&mut guard, out);
                let wake_now = was_empty && !guard.inbox.is_empty();
                drop(guard);
                if wake_now {
                    wake();
                }
            }
        };
        let on_exit = {
            let shared = self.shared.clone();
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
                guard.inbox.append(&mut out.events);
                // Dropping the handle kills anything the process left behind.
                guard.process = None;
                drop(guard);
                wake();
            }
        };
        let cwd = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let process = Process::spawn(&self.program, &["app-server"], &cwd, on_line, on_exit)?;
        shared.process = Some(process);
        deliver(shared, out);
        Ok(())
    }
}

fn deliver(shared: &mut Shared, mut out: Outputs) {
    if let Some(process) = &shared.process {
        for line in out.writes.drain(..) {
            process.send(line);
        }
    }
    shared.inbox.append(&mut out.events);
}
