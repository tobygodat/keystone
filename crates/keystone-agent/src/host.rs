//! Child processes for providers: spawn, line framing, stdin writes, a stderr tail,
//! and process-tree teardown. Drivers never touch any of this.
//!
//! Teardown never signals a bare PID that may have been reused. The leader and its
//! process group are signalled only while the leader is unreaped, which reserves its
//! PID. Descendants are signalled through pidfds, each checked against the process
//! it was found as.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Bytes of stderr kept for error messages.
const STDERR_TAIL: usize = 4 * 1024;
/// Time between SIGTERM and SIGKILL during teardown.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How long exit reporting waits for the last output after the process exits. A
/// descendant holding the pipe open must not hold exit reporting forever.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

pub struct Exit {
    pub code: Option<i32>,
    /// The end of the process's stderr, for showing why it stopped.
    pub stderr_tail: String,
}

/// A running child process. Dropping it tears down the whole process tree.
pub struct Process {
    stdin: Sender<String>,
    life: Arc<Life>,
}

struct Life {
    pid: u32,
    /// Set, under `lock`, just before the leader is reaped. Until then its PID and
    /// process group ID cannot be reused.
    reaped: AtomicBool,
    lock: Mutex<()>,
}

impl Process {
    /// Spawns `program`. `on_line` runs on a reader thread for every stdout line,
    /// without the trailing newline. `on_exit` runs once after the process exits and
    /// its remaining output has been read or abandoned.
    pub fn spawn(
        program: &Path,
        args: &[&str],
        cwd: &Path,
        mut on_line: impl FnMut(&[u8]) + Send + 'static,
        on_exit: impl FnOnce(Exit) + Send + 'static,
    ) -> io::Result<Self> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A process group of its own, so teardown reaches the shells it starts.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn()?;
        let life = Arc::new(Life {
            pid: child.id(),
            reaped: AtomicBool::new(false),
            lock: Mutex::new(()),
        });
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx, rx) = mpsc::channel::<String>();
        thread::Builder::new()
            .name("provider-stdin".into())
            .spawn(move || write_lines(stdin, rx))?;

        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
        {
            let tail = tail.clone();
            thread::Builder::new()
                .name("provider-stderr".into())
                .spawn(move || keep_tail(stderr, &tail))?;
        }

        let (done_tx, done_rx) = mpsc::channel::<()>();
        thread::Builder::new()
            .name("provider-stdout".into())
            .spawn(move || {
                read_lines(stdout, &mut on_line);
                let _ = done_tx.send(());
            })?;

        let waiter = life.clone();
        thread::Builder::new()
            .name("provider-wait".into())
            .spawn(move || {
                wait_without_reaping(waiter.pid);
                let code = {
                    let _guard = waiter.lock.lock().unwrap();
                    waiter.reaped.store(true, Ordering::SeqCst);
                    child.wait().ok().and_then(|status| status.code())
                };
                if done_rx.recv_timeout(DRAIN_GRACE).is_err() {
                    // Something the process started still holds its output open.
                    terminate(&waiter);
                    let _ = done_rx.recv_timeout(DRAIN_GRACE);
                }
                let bytes: Vec<u8> = tail.lock().unwrap().iter().copied().collect();
                on_exit(Exit {
                    code,
                    stderr_tail: String::from_utf8_lossy(&bytes).trim().to_owned(),
                });
            })?;
        Ok(Self { stdin: tx, life })
    }

    pub fn pid(&self) -> u32 {
        self.life.pid
    }

    /// Queues one line for stdin. A newline is added.
    pub fn send(&self, line: String) {
        let _ = self.stdin.send(line);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        terminate(&self.life);
    }
}

fn write_lines(mut stdin: ChildStdin, rx: mpsc::Receiver<String>) {
    while let Ok(mut line) = rx.recv() {
        line.push('\n');
        if stdin.write_all(line.as_bytes()).is_err() || stdin.flush().is_err() {
            return;
        }
    }
}

/// Reads newline-delimited output into one reused buffer, so a line costs no allocation.
fn read_lines(stdout: impl Read, on_line: &mut impl FnMut(&[u8])) {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut line = Vec::with_capacity(8 * 1024);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                if !line.is_empty() {
                    on_line(&line);
                }
            }
        }
    }
}

fn keep_tail(stderr: impl Read, tail: &Mutex<VecDeque<u8>>) {
    let mut reader = BufReader::new(stderr);
    let mut chunk = [0; 4096];
    while let Ok(n) = reader.read(&mut chunk) {
        if n == 0 {
            return;
        }
        let mut tail = tail.lock().unwrap();
        tail.extend(&chunk[..n]);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
    }
}

/// Blocks until the process exits, leaving it unreaped so its PID stays reserved.
#[cfg(unix)]
fn wait_without_reaping(pid: u32) {
    loop {
        // SAFETY: waitid writes only into the zeroed siginfo it is given.
        let result = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

#[cfg(not(unix))]
fn wait_without_reaping(_pid: u32) {}

/// SIGTERM to the process tree now and SIGKILL after a grace period.
fn terminate(life: &Arc<Life>) {
    let targets = Targets::collect(life);
    targets.signal(life, Signal::Term);
    let life = life.clone();
    thread::spawn(move || {
        thread::sleep(KILL_GRACE);
        targets.signal(&life, Signal::Kill);
    });
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Descendants found when teardown began, each pinned by a pidfd.
struct Targets {
    #[cfg(target_os = "linux")]
    descendants: Vec<pidfd::Pidfd>,
}

impl Targets {
    fn collect(life: &Life) -> Self {
        #[cfg(target_os = "linux")]
        {
            // Holding the lock keeps the leader from being reaped during the scan, so
            // its PID cannot be reused while it identifies the tree.
            let _guard = life.lock.lock().unwrap();
            Self {
                descendants: pidfd::descendants(life.pid, life.reaped.load(Ordering::SeqCst)),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = life;
            Self {}
        }
    }

    fn signal(&self, life: &Life, signal: Signal) {
        #[cfg(unix)]
        {
            let number = match signal {
                Signal::Term => libc::SIGTERM,
                Signal::Kill => libc::SIGKILL,
            };
            let _guard = life.lock.lock().unwrap();
            if !life.reaped.load(Ordering::SeqCst) {
                // SAFETY: the leader is unreaped, so its PID and group ID are still its own.
                unsafe {
                    libc::kill(-(life.pid as libc::pid_t), number);
                    libc::kill(life.pid as libc::pid_t, number);
                }
            }
            #[cfg(target_os = "linux")]
            for descendant in &self.descendants {
                descendant.signal(number);
            }
        }
        #[cfg(not(unix))]
        let _ = (life, signal);
    }
}

#[cfg(target_os = "linux")]
mod pidfd {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// A handle to one specific process. Signals sent through it never reach a later
    /// process that reuses the same PID.
    pub struct Pidfd(OwnedFd);

    impl Pidfd {
        fn open(pid: u32) -> Option<Self> {
            // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
            // SAFETY: a non-negative result is a descriptor this call now owns.
            (fd >= 0).then(|| Self(unsafe { OwnedFd::from_raw_fd(fd as i32) }))
        }

        pub fn signal(&self, signal: libc::c_int) {
            // SAFETY: the descriptor is valid for the lifetime of `self`.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                );
            }
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    struct Stat {
        pid: u32,
        ppid: u32,
        group: u32,
        start: u64,
    }

    fn stat(pid: u32) -> Option<Stat> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name can contain spaces, so parse after its closing paren.
        let fields: Vec<&str> = text.rsplit_once(')')?.1.split_whitespace().collect();
        Some(Stat {
            pid,
            ppid: fields.get(1)?.parse().ok()?,
            group: fields.get(2)?.parse().ok()?,
            start: fields.get(19)?.parse().ok()?,
        })
    }

    /// Descendants of `root` by parent link while it is unreaped, plus members of its
    /// process group. Each is pinned with a pidfd and re-checked, so a PID reused after
    /// the scan is never included.
    pub fn descendants(root: u32, root_reaped: bool) -> Vec<Pidfd> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let all: Vec<Stat> = entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .filter_map(stat)
            .collect();
        // After reaping, a process holding the root's PID is a stranger, and so is a
        // group led by it.
        if root_reaped && all.iter().any(|s| s.pid == root) {
            return Vec::new();
        }
        let mut found: Vec<Stat> = all
            .iter()
            .copied()
            .filter(|s| s.group == root && s.pid != root)
            .collect();
        // Walk children from the root and from every group member, so a shell in a
        // group of its own below a group member is found too. Visits are tracked apart
        // from targets, so a group member's children are still walked.
        let mut frontier: Vec<u32> = found.iter().map(|s| s.pid).collect();
        if !root_reaped {
            frontier.push(root);
        }
        let mut visited = std::collections::HashSet::new();
        while let Some(parent) = frontier.pop() {
            if !visited.insert(parent) {
                continue;
            }
            for s in &all {
                if s.ppid == parent && s.pid != root {
                    if !found.iter().any(|f| f.pid == s.pid) {
                        found.push(*s);
                    }
                    frontier.push(s.pid);
                }
            }
        }
        found
            .into_iter()
            .filter_map(|seen| {
                let fd = Pidfd::open(seen.pid)?;
                // The pidfd now pins whatever holds this PID. Keep it only if that is
                // still the process the scan found.
                (stat(seen.pid)?.start == seen.start).then_some(fd)
            })
            .collect()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    fn alive(pid: u32) -> bool {
        // Zombies count as gone.
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
            s.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        })
    }

    fn children_of(pid: u32) -> Vec<u32> {
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter(|p| {
                std::fs::read_to_string(format!("/proc/{p}/stat")).is_ok_and(|s| {
                    s.rsplit_once(')')
                        .and_then(|(_, r)| r.split_whitespace().nth(1)?.parse::<u32>().ok())
                        == Some(pid)
                })
            })
            .collect()
    }

    #[test]
    fn lines_round_trip_and_exit_reports_code_and_stderr_tail() {
        let (lines_tx, lines_rx) = channel();
        let (exit_tx, exit_rx) = channel();
        let process = Process::spawn(
            Path::new("/bin/sh"),
            &[
                "-c",
                "read a; echo \"got $a\"; read b; echo \"got $b\"; echo bye >&2; exit 3",
            ],
            Path::new("/"),
            move |line| {
                lines_tx
                    .send(String::from_utf8_lossy(line).into_owned())
                    .unwrap()
            },
            move |exit| exit_tx.send((exit.code, exit.stderr_tail)).unwrap(),
        )
        .unwrap();
        process.send("one".into());
        process.send("two".into());
        let wait = Duration::from_secs(5);
        assert_eq!(lines_rx.recv_timeout(wait).unwrap(), "got one");
        assert_eq!(lines_rx.recv_timeout(wait).unwrap(), "got two");
        assert_eq!(
            exit_rx.recv_timeout(wait).unwrap(),
            (Some(3), "bye".to_owned())
        );
    }

    #[test]
    fn dropping_a_process_kills_its_children() {
        let (exit_tx, exit_rx) = channel();
        let process = Process::spawn(
            Path::new("/bin/sh"),
            &["-c", "sleep 30 & sleep 30; wait"],
            Path::new("/"),
            |_| {},
            move |exit| exit_tx.send(exit.code).unwrap(),
        )
        .unwrap();
        let pid = process.pid();
        thread::sleep(Duration::from_millis(200));
        let children = children_of(pid);
        assert!(!children.is_empty());
        drop(process);
        exit_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread::sleep(Duration::from_millis(200));
        for child in children {
            assert!(!alive(child), "child {child} survived");
        }
    }

    #[test]
    fn teardown_reaches_a_separate_group_below_a_group_member() {
        let (line_tx, line_rx) = channel();
        let process = Process::spawn(
            Path::new("/bin/sh"),
            // The inner shell stays in the group; setsid moves the sleep into a session
            // and group of its own, as agents do with the shells they start.
            &["-c", "sh -c 'setsid sleep 30 & echo $!; wait' & wait"],
            Path::new("/"),
            move |line| {
                let _ = line_tx.send(String::from_utf8_lossy(line).trim().parse::<u32>().unwrap());
            },
            |_| {},
        )
        .unwrap();
        let sleeper = line_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(alive(sleeper));
        drop(process);
        thread::sleep(Duration::from_millis(500));
        assert!(
            !alive(sleeper),
            "the separately grouped sleep {sleeper} survived"
        );
    }

    #[test]
    fn exit_is_reported_while_a_leftover_child_holds_the_output_open() {
        let (exit_tx, exit_rx) = channel();
        let (child_tx, child_rx) = channel();
        let started = Instant::now();
        let _process = Process::spawn(
            Path::new("/bin/sh"),
            // The background sleep inherits stdout and stderr and outlives the shell.
            &["-c", "sleep 30 & echo $!; exit 4"],
            Path::new("/"),
            move |line| {
                let _ = child_tx.send(String::from_utf8_lossy(line).trim().parse::<u32>().unwrap());
            },
            move |exit| exit_tx.send(exit.code).unwrap(),
        )
        .unwrap();
        let leftover = child_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            exit_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some(4)
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        // The leftover was in the process group, so exit cleanup ended it.
        thread::sleep(Duration::from_millis(300));
        assert!(!alive(leftover), "leftover {leftover} survived");
    }
}
