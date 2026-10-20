//! Child processes for providers: spawn, line framing, stdin writes, a stderr tail,
//! and process-tree teardown. Drivers never touch any of this.
//!
//! Teardown never signals a bare PID that may have been reused. The leader is
//! signalled only while unreaped. Its process group is signalled only while its ID is
//! reserved: a sentinel child of Keystone sits in the group, unreaped, until teardown
//! ends, so the kernel cannot hand that ID to anyone else. Descendants in other groups
//! are signalled through pidfds, each re-checked as it is found.

use crate::profile::LaunchProfile;
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
    /// Set, under `lock`, just before the leader is reaped. Until then its PID cannot
    /// be reused.
    reaped: AtomicBool,
    /// Keystone's own child in the provider's group. While it is unreaped, alive or a
    /// zombie, the group ID stays reserved. Reaped only when teardown ends.
    sentinel: Option<i32>,
    sentinel_reaped: AtomicBool,
    lock: Mutex<()>,
    /// Set once [`Process::end`] finished teardown, so dropping does not repeat it.
    ended: AtomicBool,
}

impl Life {
    /// Whether the process group ID still names this provider's group.
    fn group_reserved(&self) -> bool {
        !self.reaped.load(Ordering::SeqCst)
            || (self.sentinel.is_some() && !self.sentinel_reaped.load(Ordering::SeqCst))
    }
}

impl Process {
    /// Spawns `program` with `profile`'s environment. `on_line` runs on a reader thread for every stdout line,
    /// without the trailing newline. `on_exit` runs once after the process exits and
    /// its remaining output has been read or abandoned.
    pub fn spawn(
        program: &Path,
        args: &[&str],
        cwd: &Path,
        profile: &LaunchProfile,
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
        profile.apply(&mut command);
        // A process group of its own, so teardown reaches the shells it starts.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn()?;
        let life = Arc::new(Life {
            pid: child.id(),
            reaped: AtomicBool::new(false),
            sentinel: spawn_sentinel(child.id()),
            sentinel_reaped: AtomicBool::new(false),
            lock: Mutex::new(()),
            ended: AtomicBool::new(false),
        });
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx, rx) = mpsc::channel::<String>();
        thread::Builder::new()
            .name("provider-stdin".into())
            .spawn(move || write_lines(stdin, rx))?;

        // Each output reader reports when its stream closes.
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
        {
            let tail = tail.clone();
            let done_tx = done_tx.clone();
            thread::Builder::new()
                .name("provider-stderr".into())
                .spawn(move || {
                    keep_tail(stderr, &tail);
                    let _ = done_tx.send(());
                })?;
        }

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
                // Wait for both stdout and stderr to close, so the last lines and the
                // stderr tail are complete.
                let drained = |timeout| (0..2).all(|_| done_rx.recv_timeout(timeout).is_ok());
                if !drained(DRAIN_GRACE) {
                    // Something the process started still holds its output open.
                    terminate(&waiter, false);
                    drained(DRAIN_GRACE);
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

    /// Whether the tree holds a process Keystone may not signal, such as a command run
    /// through `sudo`. [`Process::end`] cannot end one, where dropping's SIGTERM may
    /// reach it through its parent.
    pub fn has_foreign(&self) -> bool {
        Targets::collect(&self.life).has_foreign()
    }

    /// Ends the process tree on the calling thread and returns only once the leader
    /// and every process found in its tree that Keystone may signal have exited,
    /// however long that takes. For a caller that must not start a successor beside the
    /// old tree, and that checked [`Process::has_foreign`] first.
    ///
    /// Nothing gets SIGTERM, whose handler could start a process outside the tree as it
    /// exits. The tree is frozen with SIGSTOP instead, rescanning once everything found
    /// has stopped, until a scan finds nothing new: a stopped process forks nothing,
    /// and a child forked before its parent stopped is found through that parent. Then
    /// the closed set gets SIGKILL. A process that had already left the tree before
    /// this began is out of reach, as it is for dropping.
    pub fn end(self) {
        let mut targets = Targets::collect(&self.life);
        loop {
            targets.signal(&self.life, Signal::Stop);
            targets.wait_until(&self.life, Targets::stopped);
            if !targets.absorb(Targets::collect(&self.life)) {
                break;
            }
        }
        targets.signal(&self.life, Signal::Kill);
        targets.wait_until(&self.life, Targets::exited);
        reap_sentinel(&self.life);
        self.life.ended.store(true, Ordering::SeqCst);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.life.ended.load(Ordering::SeqCst) {
            terminate(&self.life, true);
        }
    }
}

/// Forks a child of Keystone into `group` that waits for a signal and holds nothing
/// open. Linux only; elsewhere the group is signalled only while the leader lives.
#[cfg(target_os = "linux")]
fn spawn_sentinel(group: u32) -> Option<i32> {
    // SAFETY: after fork the child calls only async-signal-safe functions: prctl,
    // close_range, and pause.
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            // Die with Keystone, and keep none of its descriptors, such as the provider's
            // stdin or the workspace lock.
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
            loop {
                libc::pause();
            }
        }
        if pid < 0 {
            return None;
        }
        if libc::setpgid(pid, group as libc::pid_t) == 0 {
            Some(pid)
        } else {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn spawn_sentinel(_group: u32) -> Option<i32> {
    None
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

/// SIGTERM to the process tree now and SIGKILL after a grace period. The `last`
/// teardown then reaps the sentinel, releasing the group ID.
fn terminate(life: &Arc<Life>, last: bool) {
    let targets = Targets::collect(life);
    targets.signal(life, Signal::Term);
    let life = life.clone();
    thread::spawn(move || {
        thread::sleep(KILL_GRACE);
        targets.signal(&life, Signal::Kill);
        if last {
            reap_sentinel(&life);
        }
    });
}

/// Reaps the group's sentinel, releasing the group ID. Only teardown's end does this.
fn reap_sentinel(life: &Life) {
    #[cfg(unix)]
    if let Some(sentinel) = life.sentinel {
        let _guard = life.lock.lock().unwrap();
        if !life.sentinel_reaped.swap(true, Ordering::SeqCst) {
            // SAFETY: the sentinel is Keystone's own unreaped child.
            unsafe {
                libc::kill(sentinel, libc::SIGKILL);
                libc::waitpid(sentinel, std::ptr::null_mut(), 0);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = life;
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Stop,
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
            // Holding the lock keeps the leader and the sentinel from being reaped
            // during the scan, so the PID and group ID it relies on stay reserved.
            let _guard = life.lock.lock().unwrap();
            Self {
                descendants: pidfd::descendants(
                    life.pid,
                    !life.reaped.load(Ordering::SeqCst),
                    life.group_reserved(),
                ),
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
                Signal::Stop => libc::SIGSTOP,
                Signal::Kill => libc::SIGKILL,
            };
            let _guard = life.lock.lock().unwrap();
            if life.group_reserved() {
                // SAFETY: the group ID is reserved, so it names only this provider's group.
                unsafe { libc::kill(-(life.pid as libc::pid_t), number) };
            }
            if !life.reaped.load(Ordering::SeqCst) {
                // SAFETY: the leader is unreaped, so its PID is still its own.
                unsafe { libc::kill(life.pid as libc::pid_t, number) };
            }
            #[cfg(target_os = "linux")]
            for descendant in &self.descendants {
                descendant.signal(number);
            }
        }
        #[cfg(not(unix))]
        let _ = (life, signal);
    }

    fn has_foreign(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.descendants.iter().any(pidfd::Pidfd::is_foreign);
        #[cfg(not(target_os = "linux"))]
        false
    }

    /// Adds the processes `found` has that these lack. Returns whether there were any.
    fn absorb(&mut self, found: Targets) -> bool {
        #[cfg(target_os = "linux")]
        {
            let before = self.descendants.len();
            for pin in found.descendants {
                if !self.descendants.iter().any(|known| known.pid == pin.pid) {
                    self.descendants.push(pin);
                }
            }
            self.descendants.len() > before
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = found;
            false
        }
    }

    /// Waits until the leader and every process found satisfy `done`, reading each
    /// one's state as `/proc` reports it.
    fn wait_until(&self, life: &Life, done: fn(Option<char>) -> bool) {
        #[cfg(target_os = "linux")]
        loop {
            let leader = {
                // While unreaped, the leader's PID is still its own.
                let _guard = life.lock.lock().unwrap();
                life.reaped.load(Ordering::SeqCst) || done(pidfd::state(life.pid))
            };
            if leader && self.descendants.iter().all(|pin| pin.is(done)) {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (life, done);
    }

    /// Whether a process in `state`, sent SIGSTOP, forks nothing more: it has stopped,
    /// or waits in the kernel, where the pending stop makes any fork fail and stops
    /// it before it runs again.
    fn stopped(state: Option<char>) -> bool {
        matches!(state, Some('T' | 't' | 'D')) || Self::exited(state)
    }

    /// Whether a process in `state` has exited. A zombie has, though its parent has not
    /// reaped it yet.
    fn exited(state: Option<char>) -> bool {
        matches!(state, None | Some('Z' | 'X'))
    }
}

#[cfg(target_os = "linux")]
mod pidfd {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// A handle to one specific process. Signals sent through it never reach a later
    /// process that reuses the same PID.
    pub struct Pidfd {
        fd: OwnedFd,
        pub pid: u32,
    }

    /// The state letter `/proc` reports for `pid`, `None` once nothing holds that PID.
    pub fn state(pid: u32) -> Option<char> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        text.rsplit_once(')')?
            .1
            .split_whitespace()
            .next()?
            .chars()
            .next()
    }

    impl Pidfd {
        fn open(pid: u32) -> Option<Self> {
            // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
            // SAFETY: a non-negative result is a descriptor this call now owns.
            (fd >= 0).then(|| Self {
                fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
                pid,
            })
        }

        /// Whether the pinned process's state satisfies `check`. The state is read by
        /// PID, so it counts only if the pinned process still holds that PID after the
        /// read; once the process is reaped its state is `None`.
        pub fn is(&self, check: fn(Option<char>) -> bool) -> bool {
            let state = state(self.pid);
            if self.is_alive() {
                check(state)
            } else {
                check(None)
            }
        }

        /// Whether the pinned process is still running. Signal 0 checks without sending.
        pub fn is_alive(&self) -> bool {
            // SAFETY: the descriptor is valid for the lifetime of `self`.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    0,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                ) == 0
            }
        }

        /// Whether Keystone may not signal the pinned process, such as one running as
        /// another user.
        pub fn is_foreign(&self) -> bool {
            // SAFETY: the descriptor is valid for the lifetime of `self`.
            let refused = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    0,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                ) != 0
            };
            refused && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }

        pub fn signal(&self, signal: libc::c_int) {
            // SAFETY: the descriptor is valid for the lifetime of `self`.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                );
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(super) struct Stat {
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

    /// The live processes to signal for `root`'s tree, each pinned by a pidfd.
    pub fn descendants(root: u32, root_unreaped: bool, group_reserved: bool) -> Vec<Pidfd> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let all: Vec<Stat> = entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .filter_map(stat)
            .collect();
        select(
            root,
            root_unreaped,
            group_reserved,
            &all,
            |seen| {
                let fd = Pidfd::open(seen.pid)?;
                // The pidfd now pins whatever holds this PID. Keep it only if that is
                // still the process the scan found.
                (stat(seen.pid)? == *seen).then_some(fd)
            },
            Pidfd::is_alive,
        )
    }

    /// Chooses targets from a non-atomic `/proc` scan without trusting a PID that may
    /// have been reused during it.
    ///
    /// `open` pins a scanned process and returns `None` unless it is still exactly as
    /// scanned. `alive` reports whether a pinned process is still running.
    ///
    /// - Group members seed the walk while the group ID is reserved, whenever they
    ///   started. Otherwise a group with that ID may be a stranger's.
    /// - The root itself is walked only while unreaped, when its PID is still its own.
    /// - A child counts only if, after it is pinned and re-checked, its parent is still
    ///   alive through its own pin. Then the parent's PID meant that parent the whole
    ///   time, and the child is really its child.
    pub(super) fn select<F>(
        root: u32,
        root_unreaped: bool,
        group_reserved: bool,
        all: &[Stat],
        open: impl Fn(&Stat) -> Option<F>,
        alive: impl Fn(&F) -> bool,
    ) -> Vec<F> {
        struct Node<F> {
            pid: u32,
            /// `None` only for the unreaped root, which cannot be replaced.
            pin: Option<F>,
        }
        let mut nodes: Vec<Node<F>> = Vec::new();
        if root_unreaped {
            nodes.push(Node {
                pid: root,
                pin: None,
            });
        }
        for member in all
            .iter()
            .filter(|s| group_reserved && s.group == root && s.pid != root)
        {
            if let Some(pin) = open(member) {
                nodes.push(Node {
                    pid: member.pid,
                    pin: Some(pin),
                });
            }
        }
        let mut next = 0;
        while next < nodes.len() {
            let parent = nodes[next].pid;
            for child in all.iter().filter(|s| s.ppid == parent && s.pid != root) {
                if nodes.iter().any(|n| n.pid == child.pid) {
                    continue;
                }
                let Some(pin) = open(child) else { continue };
                if nodes[next].pin.as_ref().is_none_or(&alive) {
                    nodes.push(Node {
                        pid: child.pid,
                        pin: Some(pin),
                    });
                }
            }
            next += 1;
        }
        nodes.into_iter().filter_map(|n| n.pin).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::RefCell;

        fn proc(pid: u32, ppid: u32, group: u32, start: u64) -> Stat {
            Stat {
                pid,
                ppid,
                group,
                start,
            }
        }

        /// The reviewer's interleaving: the scan saw group member 20, which then died;
        /// its PID went to a stranger, whose child 30 the same scan also saw.
        #[test]
        fn a_child_of_a_replaced_parent_is_not_selected() {
            let all = [proc(20, 1, 10, 100), proc(30, 20, 30, 500)];
            let dead = RefCell::new(vec![20]);
            let chosen = select(
                10,
                false,
                true,
                &all,
                |s| Some(s.pid),
                |pid| !dead.borrow().contains(pid),
            );
            assert_eq!(
                chosen,
                vec![20],
                "only the scanned member itself, never the stranger's child"
            );
            dead.borrow_mut().clear();
            let chosen = select(10, false, true, &all, |s| Some(s.pid), |_| true);
            assert_eq!(
                chosen,
                vec![20, 30],
                "with the parent alive, its child is ours"
            );
        }

        #[test]
        fn group_members_count_only_while_the_group_id_is_reserved() {
            // Started long after the leader: still ours while the ID is reserved.
            let all = [proc(20, 1, 10, 900)];
            assert_eq!(
                select(10, false, true, &all, |s| Some(s.pid), |_| true),
                vec![20]
            );
            assert!(select(10, false, false, &all, |s| Some(s.pid), |_| true).is_empty());
        }

        #[test]
        fn a_process_that_changed_since_the_scan_is_skipped() {
            let all = [proc(20, 10, 20, 100), proc(21, 10, 21, 100)];
            let chosen = select(
                10,
                true,
                true,
                &all,
                |s| (s.pid != 21).then_some(s.pid),
                |_| true,
            );
            assert_eq!(chosen, vec![20]);
        }
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
            &LaunchProfile::default(),
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
            &LaunchProfile::default(),
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

    /// `end` returns only after the tree is gone, including a descendant that ignores
    /// SIGTERM, without waiting out a grace period for it.
    #[test]
    fn ending_a_process_returns_once_its_tree_is_gone() {
        let process = Process::spawn(
            Path::new("/bin/sh"),
            &["-c", "sh -c 'trap \"\" TERM; sleep 30' & sleep 30 & wait"],
            Path::new("/"),
            &LaunchProfile::default(),
            |_| {},
            |_| {},
        )
        .unwrap();
        let pid = process.pid();
        thread::sleep(Duration::from_millis(200));
        let mut tree = vec![pid];
        tree.extend(children_of(pid));
        let grandchildren: Vec<u32> = tree.iter().flat_map(|&p| children_of(p)).collect();
        tree.extend(grandchildren);
        assert!(tree.len() >= 3, "{tree:?}");
        let started = std::time::Instant::now();
        process.end();
        assert!(started.elapsed() < KILL_GRACE, "{:?}", started.elapsed());
        for member in tree {
            assert!(!alive(member), "{member} survived end()");
        }
    }

    /// Spawns `python3 -c code` in `dir` and waits for its first line.
    fn python(dir: &Path, code: &str) -> Process {
        let (tx, rx) = channel();
        let process = Process::spawn(
            Path::new("/usr/bin/python3"),
            &["-c", code],
            dir,
            &LaunchProfile::default(),
            move |_| {
                let _ = tx.send(());
            },
            |_| {},
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        process
    }

    /// The PIDs a test provider wrote to `file` in `dir`, one per line. Each is
    /// SIGKILLed here as well, so a failing test leaves nothing behind.
    fn escaped(dir: &Path, file: &str) -> Vec<(u32, bool)> {
        let text = std::fs::read_to_string(dir.join(file)).unwrap_or_default();
        text.lines()
            .filter_map(|line| line.trim().parse().ok())
            .map(|pid: u32| {
                let survived = alive(pid);
                // SAFETY: test cleanup of a process this test started.
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                (pid, survived)
            })
            .collect()
    }

    /// A reviewer's case: a SIGTERM handler that starts a process outside the tree as
    /// the leader exits. `end` sends no SIGTERM, so it never runs.
    #[test]
    fn ending_a_process_runs_no_exit_handler_that_escapes_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let process = python(
            dir.path(),
            r#"import os, signal, time
def term(sig, frame):
    if os.fork() == 0:
        os.setsid()
        open('escaped', 'a').write(f'{os.getpid()}\n')
        time.sleep(30)
        os._exit(0)
    time.sleep(0.2)
    os._exit(0)
signal.signal(signal.SIGTERM, term)
print('ready', flush=True)
while True: time.sleep(1)
"#,
        );
        process.end();
        thread::sleep(Duration::from_millis(300));
        let escaped = escaped(dir.path(), "escaped");
        assert!(escaped.is_empty(), "the handler ran: {escaped:?}");
    }

    /// A tree holding a process of another user is reported, so eviction can leave it
    /// running rather than leave that process behind. Needs passwordless `sudo`.
    #[test]
    fn a_tree_running_a_command_as_another_user_is_foreign() {
        let sudo = std::process::Command::new("sudo")
            .args(["-n", "true"])
            .status()
            .is_ok_and(|status| status.success());
        if !sudo {
            eprintln!("skipped: no passwordless sudo");
            return;
        }
        let process = Process::spawn(
            Path::new("/bin/sh"),
            &["-c", "sleep 30 & sudo -n sleep 30 & wait"],
            Path::new("/"),
            &LaunchProfile::default(),
            |_| {},
            |_| {},
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process.has_foreign() {
            assert!(
                Instant::now() < deadline,
                "sudo's tree never seen as foreign"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let mine = Process::spawn(
            Path::new("/bin/sh"),
            &["-c", "sleep 30 & wait"],
            Path::new("/"),
            &LaunchProfile::default(),
            |_| {},
            |_| {},
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(!mine.has_foreign());
        drop(process);
        mine.end();
    }

    /// A leader that keeps forking children into sessions of their own while teardown
    /// scans its tree: none of them outlives `end`, though the leader dies first.
    #[test]
    fn ending_a_forking_tree_leaves_none_of_it_running() {
        let dir = tempfile::tempdir().unwrap();
        let process = python(
            dir.path(),
            r#"import os, time
print('ready', flush=True)
while True:
    if os.fork() == 0:
        os.setsid()
        open('forked', 'a').write(f'{os.getpid()}\n')
        time.sleep(30)
        os._exit(0)
    time.sleep(0.001)
"#,
        );
        thread::sleep(Duration::from_millis(300));
        process.end();
        let pids = escaped(dir.path(), "forked");
        assert!(pids.len() > 10, "{}", pids.len());
        let survivors: Vec<u32> = pids.iter().filter(|(_, s)| *s).map(|(p, _)| *p).collect();
        assert!(survivors.is_empty(), "{survivors:?} of {}", pids.len());
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
            &LaunchProfile::default(),
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
    fn a_group_member_born_after_the_leader_exits_is_ended_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let (exit_tx, exit_rx) = channel();
        let script = format!(
            // The subshell outlives the leader without holding its output, then starts
            // a sleep in the same group and exits, orphaning it.
            "(sleep 0.3; sleep 30 & echo $! > {}) >/dev/null 2>&1 & exit 0",
            pid_file.display()
        );
        let process = Process::spawn(
            Path::new("/bin/sh"),
            &["-c", &script],
            Path::new("/"),
            &LaunchProfile::default(),
            |_| {},
            move |exit| exit_tx.send(exit.code).unwrap(),
        )
        .unwrap();
        assert_eq!(
            exit_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some(0)
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let orphan = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|t| t.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the orphan never started");
            thread::sleep(Duration::from_millis(50));
        };
        assert!(alive(orphan));
        drop(process);
        thread::sleep(Duration::from_millis(500));
        assert!(
            !alive(orphan),
            "orphan {orphan} born after the leader exited survived"
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
            &LaunchProfile::default(),
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
