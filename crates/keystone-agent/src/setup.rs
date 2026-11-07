//! A project's worktree setup script: the user's own shell command, run in a new
//! worktree's folder before the session's first message. Its output arrives as lines,
//! cleaned of terminal control, and then how it exited. See `docs/design/storage.md`
//! for where the script is saved.
//!
//! The script runs in a session and process group of its own, led by the shell, with
//! no controlling terminal. Stopping it ends the whole group: SIGTERM, then SIGKILL to
//! whatever is still there once the rest have exited or the grace runs out, even after
//! the shell itself has gone. The shell is left unreaped until then, so its ID, which
//! is also the group's, cannot name anyone else's group while it is signalled. A
//! script that exits on its own is left alone: what it started in the background is
//! its choice, and keeps running after the run is dropped.

use crate::host;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// Longest line kept, in characters.
pub const LINE_MAX_CHARS: usize = 400;
/// Most output lines held between two drains. Older ones are dropped first.
pub const KEPT_LINES: usize = 200;
/// Most bytes of one unfinished line held while waiting for its end.
const PARTIAL_MAX_BYTES: usize = 4096;
/// How long a stopped script's group has to exit before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long [`stop_and_wait`] waits for the shells it killed to be reaped.
const REAP_WAIT: Duration = Duration::from_millis(500);
/// How long output is still read after the script exits, for a background process
/// that keeps its output open.
const OUTPUT_GRACE: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupExit {
    Code(i32),
    Signal(i32),
}

impl SetupExit {
    pub fn succeeded(self) -> bool {
        self == Self::Code(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupEvent {
    Line(String),
    /// The last event of a run.
    Exited(SetupExit),
}

struct Shared {
    child: Child,
    /// When the run was asked to stop.
    stopped: Option<Instant>,
    /// Set once the shell is reaped. Until then its ID still names the script's
    /// group, so the group may be signalled.
    reaped: bool,
    finished: bool,
    events: VecDeque<SetupEvent>,
}

pub struct SetupRun {
    pid: i32,
    shared: Arc<(Mutex<Shared>, Condvar)>,
}

/// The shell a script runs with: `env_shell`, the user's `$SHELL`, else `/bin/sh`.
pub fn shell_for(env_shell: Option<OsString>) -> OsString {
    env_shell
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".into())
}

/// Runs `script` with `shell -c` in `worktree`. `wake` runs on another thread when
/// events arrive after the last [`SetupRun::drain`].
pub fn start(
    shell: &OsStr,
    script: &str,
    worktree: &Path,
    project_root: &Path,
    wake: impl Fn() + Send + Sync + 'static,
) -> io::Result<SetupRun> {
    let (output, writer) = io::pipe()?;
    let mut command = Command::new(shell);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .arg("-c")
        .arg(script)
        .current_dir(worktree)
        .env("KEYSTONE_PROJECT_ROOT", project_root)
        .env("KEYSTONE_WORKTREE_PATH", worktree)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    drop(command);
    let pid = child.id() as libc::pid_t;
    let shared = Arc::new((
        Mutex::new(Shared {
            child,
            stopped: None,
            reaped: false,
            finished: false,
            events: VecDeque::new(),
        }),
        Condvar::new(),
    ));
    let abandon = |error: io::Error| {
        let mut state = shared.0.lock().unwrap();
        // SAFETY: the shell is unreaped, so `pid` still names its group.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        let _ = state.child.wait();
        state.reaped = true;
        Err(error)
    };
    let wake = Arc::new(wake);
    let (read_done, read_finished) = mpsc::channel::<()>();
    let reader = {
        let shared = shared.clone();
        let wake = wake.clone();
        thread::Builder::new()
            .name("setup-output".into())
            .spawn(move || {
                read_lines(output, |line| {
                    let mut state = shared.0.lock().unwrap();
                    if state.finished {
                        return;
                    }
                    if state.events.len() == KEPT_LINES {
                        state.events.pop_front();
                    }
                    let was_empty = state.events.is_empty();
                    state.events.push_back(SetupEvent::Line(line));
                    drop(state);
                    if was_empty {
                        wake();
                    }
                });
                let _ = read_done.send(());
            })
    };
    if let Err(error) = reader {
        return abandon(error);
    }
    let waiter = {
        let shared = shared.clone();
        thread::Builder::new()
            .name("setup-wait".into())
            .spawn(move || {
                let exit = wait(&shared, pid);
                let _ = read_finished.recv_timeout(OUTPUT_GRACE);
                let (lock, ended) = &*shared;
                let mut state = lock.lock().unwrap();
                let was_empty = state.events.is_empty();
                state.events.push_back(SetupEvent::Exited(exit));
                state.finished = true;
                ended.notify_all();
                drop(state);
                if was_empty {
                    wake();
                }
            })
    };
    if let Err(error) = waiter {
        return abandon(error);
    }
    Ok(SetupRun { pid, shared })
}

/// Waits for the shell to exit. If the run was stopped, the rest of its group gets
/// the remaining grace and then SIGKILL, before the shell is reaped.
fn wait(shared: &(Mutex<Shared>, Condvar), pid: libc::pid_t) -> SetupExit {
    host::wait_without_reaping(pid as u32);
    let (lock, ended) = shared;
    loop {
        let stopped = lock.lock().unwrap().stopped;
        match stopped {
            Some(at) if at.elapsed() < STOP_GRACE && others_alive(pid) => thread::sleep(POLL),
            _ => break,
        }
    }
    let mut state = lock.lock().unwrap();
    if state.stopped.is_some() {
        // SAFETY: the shell is unreaped, so `pid` still names its group.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
    let status = loop {
        match state.child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => break None,
            Ok(None) | Err(_) => {
                drop(state);
                thread::sleep(POLL);
                state = lock.lock().unwrap();
            }
        }
    };
    state.reaped = true;
    ended.notify_all();
    match status.map(|status| (status.code(), status.signal())) {
        Some((Some(code), _)) => SetupExit::Code(code),
        Some((None, Some(signal))) => SetupExit::Signal(signal),
        _ => SetupExit::Code(-1),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn others_alive(pid: libc::pid_t) -> bool {
    host::group_has_others(pid as u32)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn others_alive(_pid: libc::pid_t) -> bool {
    false
}

impl SetupRun {
    /// The script's process ID, which is also its process group's.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Takes the events that arrived since the last call: at most [`KEPT_LINES`]
    /// lines, the latest, then the exit once there is one.
    pub fn drain(&self) -> Vec<SetupEvent> {
        std::mem::take(&mut self.lock().events).into()
    }

    /// Asks the script's whole process group to end, and kills what is left of it
    /// after a grace period. Returns at once, and does nothing once the script has
    /// exited on its own.
    pub fn stop(&self) {
        if !self.terminate() {
            return;
        }
        let (shared, pid) = (self.shared.clone(), self.pid);
        let spawned = thread::Builder::new()
            .name("setup-stop".into())
            .spawn(move || {
                let (lock, ended) = &*shared;
                let state = lock.lock().unwrap();
                let (state, _) = ended
                    .wait_timeout_while(state, STOP_GRACE, |state| !state.reaped)
                    .unwrap();
                kill(&state, pid);
            });
        if spawned.is_err() {
            self.kill();
        }
    }

    /// Sends SIGTERM to the group unless the shell is reaped. Whether this is the
    /// run's first stop.
    fn terminate(&self) -> bool {
        let mut state = self.lock();
        if state.reaped {
            return false;
        }
        let first = state.stopped.is_none();
        state.stopped.get_or_insert_with(Instant::now);
        // SAFETY: the lock keeps the shell unreaped, so `pid` still names its group.
        unsafe { libc::killpg(self.pid, libc::SIGTERM) };
        first
    }

    fn kill(&self) {
        kill(&self.lock(), self.pid);
    }

    fn wait_reaped(&self, deadline: Instant) -> bool {
        let (lock, ended) = &*self.shared;
        let left = deadline.saturating_duration_since(Instant::now());
        let state = lock.lock().unwrap();
        ended
            .wait_timeout_while(state, left, |state| !state.reaped)
            .unwrap()
            .0
            .reaped
    }

    #[cfg(test)]
    fn wait_until(&self, deadline: Instant) -> bool {
        let (lock, ended) = &*self.shared;
        let left = deadline.saturating_duration_since(Instant::now());
        let state = lock.lock().unwrap();
        ended
            .wait_timeout_while(state, left, |state| !state.finished)
            .unwrap()
            .0
            .finished
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.0.lock().unwrap()
    }
}

/// SIGKILL to the group of the shell `state` holds, unless it is reaped.
fn kill(state: &Shared, pid: libc::pid_t) {
    if !state.reaped {
        // SAFETY: the caller holds the lock, so the shell stays unreaped and `pid`
        // still names its group.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
}

impl Drop for SetupRun {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Stops every run on the calling thread: SIGTERM to each group, up to `grace` for
/// them all to end, then SIGKILL to every group still there and a short wait for
/// those shells. When it returns, every group it stopped has been sent SIGKILL or
/// has ended. Returns whether every shell was also reaped.
pub fn stop_and_wait<'a>(runs: impl IntoIterator<Item = &'a SetupRun>, grace: Duration) -> bool {
    let runs: Vec<&SetupRun> = runs.into_iter().collect();
    for run in &runs {
        run.terminate();
    }
    let deadline = Instant::now() + grace;
    for run in &runs {
        run.wait_reaped(deadline);
    }
    for run in &runs {
        run.kill();
    }
    let deadline = Instant::now() + REAP_WAIT;
    runs.iter().all(|run| run.wait_reaped(deadline))
}

/// Splits `output` into lines at `\n`. A bare `\r` starts the line again, as a
/// terminal redraws a progress bar, so only the last redraw is kept.
fn read_lines(mut output: impl Read, mut emit: impl FnMut(String)) {
    let mut line = Vec::new();
    let mut carriage = false;
    let mut chunk = [0u8; 8192];
    let finish = |line: &mut Vec<u8>, emit: &mut dyn FnMut(String)| {
        let text = clean(&String::from_utf8_lossy(line));
        line.clear();
        if !text.trim().is_empty() {
            emit(text);
        }
    };
    loop {
        let read = match output.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for &byte in &chunk[..read] {
            if carriage && byte != b'\n' {
                line.clear();
            }
            carriage = false;
            match byte {
                b'\n' => finish(&mut line, &mut emit),
                b'\r' => carriage = true,
                _ if line.len() < PARTIAL_MAX_BYTES => line.push(byte),
                _ => {}
            }
        }
    }
    finish(&mut line, &mut emit);
}

/// Removes terminal escape sequences and control characters, and cuts the line to
/// [`LINE_MAX_CHARS`].
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                Some('(' | ')') => {
                    chars.next();
                }
                _ => {}
            },
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    match out.char_indices().nth(LINE_MAX_CHARS) {
        Some((cut, _)) => {
            out.truncate(cut);
            out.push('…');
            out
        }
        None => out,
    }
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
