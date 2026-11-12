//! Child processes for providers: spawn, line framing, stdin writes, a stderr tail,
//! and process-tree teardown. Drivers never touch any of this.
//!
//! Teardown never signals a bare PID that may have been reused. The leader is
//! signalled only while unreaped. Its process group is signalled only while its ID is
//! reserved: a sentinel child of Keystone sits in the group, unreaped, until teardown
//! ends, so the kernel cannot hand that ID to anyone else. Descendants in other groups
//! are signalled through pidfds on Linux, each re-checked as it is found. macOS has no
//! pidfd: there each one is re-checked by start time just before every signal.

use crate::profile::LaunchProfile;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

/// A process Keystone started some other way than [`Process`], such as a terminal's
/// shell, pinned so that ending its tree later never signals a reused PID.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct Root(os::Pin);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Root {
    /// Pins `pid`. Call it while `pid` is the caller's own unreaped child.
    pub fn pin(pid: u32) -> Option<Self> {
        os::stat(pid).and_then(|seen| os::pin(&seen)).map(Self)
    }

    /// The live processes to end with this one: itself, the members of `groups`, and
    /// everything descended from them, each pinned. Nothing is found once it exited,
    /// because its descendants may then have been adopted by strangers' PIDs.
    pub fn tree(&self, groups: &[u32]) -> Tree {
        let seeds = self
            .0
            .duplicate()
            .filter(os::Pin::is_alive)
            .map(|pin| tree::Node {
                pid: self.0.pid,
                pin: Some(pin),
            });
        Tree::found(seeds.into_iter().collect(), groups, self.0.pid)
    }
}

/// Processes found in a tree, each pinned, so signals reach only the processes found
/// even after their PIDs are reused.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct Tree {
    pins: Vec<os::Pin>,
    root: u32,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Tree {
    fn found(mut seeds: Vec<tree::Node<os::Pin>>, groups: &[u32], root: u32) -> Self {
        let all = os::all();
        if !seeds.is_empty() {
            let members: Vec<_> = all
                .iter()
                .filter(|s| groups.contains(&s.group) && seeds.iter().all(|n| n.pid != s.pid))
                .filter_map(|s| {
                    Some(tree::Node {
                        pid: s.pid,
                        pin: Some(os::pin(s)?),
                    })
                })
                .collect();
            seeds.extend(members);
        }
        tree::adopt(&mut seeds, root, &all, os::pin, os::Pin::is_alive);
        let pins = seeds.into_iter().filter_map(|n| n.pin).collect();
        Self { pins, root }
    }

    pub fn signal(&self, signal: libc::c_int) {
        for pin in &self.pins {
            pin.signal(signal);
        }
    }

    /// Whether any process found has not exited yet.
    pub fn is_alive(&self) -> bool {
        self.pins.iter().any(|pin| !pin.is(Targets::exited))
    }

    /// Adds the processes the ones still alive have started since they were found.
    pub fn refresh(&mut self) {
        let seeds = self
            .pins
            .iter()
            .filter(|pin| pin.is_alive())
            .filter_map(|pin| {
                Some(tree::Node {
                    pid: pin.pid,
                    pin: Some(pin.duplicate()?),
                })
            })
            .collect();
        for pin in Self::found(seeds, &[], self.root).pins {
            if self.pins.iter().all(|known| known.pid != pin.pid) {
                self.pins.push(pin);
            }
        }
    }
}

/// Forks a child of Keystone into `group` that waits for a signal and holds nothing
/// open, and that dies with Keystone.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_sentinel(group: u32) -> Option<i32> {
    #[cfg(target_os = "macos")]
    // SAFETY: getpid and sysconf only read.
    let (keystone, open_max) = unsafe {
        (
            libc::getpid(),
            libc::sysconf(libc::_SC_OPEN_MAX).clamp(0, 1 << 16) as libc::c_int,
        )
    };
    // SAFETY: after fork the child calls only async-signal-safe functions: prctl,
    // close_range and pause on Linux; close, getppid, sleep and _exit on macOS.
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            // Keep none of Keystone's descriptors, such as the provider's stdin or the
            // workspace lock.
            #[cfg(target_os = "linux")]
            {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
                loop {
                    libc::pause();
                }
            }
            // macOS has no parent-death signal, so the sentinel looks each second.
            #[cfg(target_os = "macos")]
            {
                for fd in 3..open_max {
                    libc::close(fd);
                }
                while libc::getppid() == keystone {
                    libc::sleep(1);
                }
                libc::_exit(0);
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
/// macOS's waitid also returns at once for a stopped process, however often it is
/// asked, so there kqueue reports the exit instead.
#[cfg(target_os = "macos")]
pub(crate) fn wait_without_reaping(pid: u32) {
    // SAFETY: kevent reads one change and writes at most one event into zeroed
    // structs that outlive the call; the queue is closed once.
    let reported = unsafe {
        let queue = libc::kqueue();
        queue >= 0 && {
            let mut change: libc::kevent = std::mem::zeroed();
            change.ident = pid as libc::uintptr_t;
            change.filter = libc::EVFILT_PROC;
            change.flags = libc::EV_ADD | libc::EV_ONESHOT;
            change.fflags = libc::NOTE_EXIT;
            let mut event: libc::kevent = std::mem::zeroed();
            let reported = loop {
                match libc::kevent(queue, &change, 1, &mut event, 1, std::ptr::null()) {
                    // An exited, unreaped process gives an ESRCH error event.
                    1 => {
                        break event.flags & libc::EV_ERROR == 0
                            || event.data == libc::ESRCH as libc::intptr_t;
                    }
                    -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                    _ => break false,
                }
            };
            libc::close(queue);
            reported
        }
    };
    // Without kqueue, ask waitid until it reports an exit rather than a stop.
    while !reported && !exited_unreaped(pid) {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether child `pid` has exited, leaving it unreaped. Also true once it is no child.
#[cfg(target_os = "macos")]
fn exited_unreaped(pid: u32) -> bool {
    // SAFETY: waitid writes only into the zeroed siginfo it is given.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let flags = libc::WEXITED | libc::WNOWAIT | libc::WNOHANG;
        if libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) < 0 {
            return io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
        }
        info.si_pid != 0
            && matches!(
                info.si_code,
                libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
            )
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn wait_without_reaping(pid: u32) {
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
pub(crate) fn wait_without_reaping(_pid: u32) {}

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

/// Descendants found when teardown began, each pinned so a reused PID is never signalled.
struct Targets {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    descendants: Vec<os::Pin>,
}

impl Targets {
    fn collect(life: &Life) -> Self {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // Holding the lock keeps the leader and the sentinel from being reaped
            // during the scan, so the PID and group ID it relies on stay reserved.
            let _guard = life.lock.lock().unwrap();
            Self {
                descendants: os::descendants(
                    life.pid,
                    !life.reaped.load(Ordering::SeqCst),
                    life.group_reserved(),
                ),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            for descendant in &self.descendants {
                descendant.signal(number);
            }
        }
        #[cfg(not(unix))]
        let _ = (life, signal);
    }

    fn has_foreign(&self) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return self.descendants.iter().any(os::Pin::is_foreign);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        false
    }

    /// Adds the processes `found` has that these lack. Returns whether there were any.
    fn absorb(&mut self, found: Targets) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let before = self.descendants.len();
            for pin in found.descendants {
                if !self.descendants.iter().any(|known| known.pid == pin.pid) {
                    self.descendants.push(pin);
                }
            }
            self.descendants.len() > before
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = found;
            false
        }
    }

    /// Waits until the leader and every process found satisfy `done`, reading each
    /// one's state as the kernel reports it.
    fn wait_until(&self, life: &Life, done: fn(Option<char>) -> bool) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        loop {
            let leader = {
                // While unreaped, the leader's PID is still its own.
                let _guard = life.lock.lock().unwrap();
                life.reaped.load(Ordering::SeqCst) || done(os::state(life.pid))
            };
            if leader && self.descendants.iter().all(|pin| pin.is(done)) {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

#[cfg(target_os = "macos")]
use libproc as os;
#[cfg(target_os = "linux")]
use pidfd as os;

/// The state letter of process `pid` as Linux's `/proc` writes it (`R`, `S`, `T`,
/// `Z` and so on), or `None` once nothing holds that PID.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn process_state(pid: u32) -> Option<char> {
    os::state(pid)
}

/// Whether a process other than `group`'s leader is alive in that group. The caller
/// keeps the group ID reserved, such as by leaving the leader unreaped.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn group_has_others(group: u32) -> bool {
    os::all()
        .iter()
        .any(|s| s.group == group && s.pid != group && !Targets::exited(process_state(s.pid)))
}

/// The parent of process `pid`, or `None` once nothing holds that PID.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn process_parent(pid: u32) -> Option<u32> {
    os::stat(pid).map(|stat| stat.ppid)
}

/// When process `pid` started, in the platform's own units, or `None` once nothing
/// holds that PID. A PID that holds a process with the same start time is the same
/// process.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn process_start(pid: u32) -> Option<u64> {
    os::stat(pid).map(|stat| stat.start)
}

/// A provider some other process started, with every process seen below it while it
/// was watched, each pinned so a reused PID is never signalled. `keystone-host reap`
/// keeps one per provider, so it can end the whole tree if the daemon that started
/// the provider dies.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct Watched {
    root: os::Pin,
    found: Vec<os::Pin>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Watched {
    /// Pins `pid` while it still holds the process that started at `start`.
    pub fn new(pid: u32, start: u64) -> Option<Self> {
        let seen = os::stat(pid).filter(|stat| stat.start == start)?;
        Some(Self {
            root: os::pin(&seen)?,
            found: Vec::new(),
        })
    }

    /// Forgets what exited and adds what is new: every child of a process it holds, in
    /// any group or session, and while the provider runs, every member of its group.
    /// Returns whether it added any. A process that left the tree before a scan saw
    /// it, such as one whose parent had already exited, is out of reach.
    pub fn scan(&mut self) -> bool {
        // The provider sits in the walk through a borrow, never moved, so it stays
        // pinned however the scan ends.
        enum Held<'a> {
            Root(&'a os::Pin),
            Found(os::Pin),
        }
        impl Held<'_> {
            fn pin(&self) -> &os::Pin {
                match self {
                    Held::Root(pin) => pin,
                    Held::Found(pin) => pin,
                }
            }
        }
        let all = os::all();
        let root = self.root.pid;
        let mut nodes = vec![tree::Node {
            pid: root,
            pin: Some(Held::Root(&self.root)),
        }];
        nodes.extend(
            self.found
                .drain(..)
                .filter(os::Pin::exists)
                .map(|pin| tree::Node {
                    pid: pin.pid,
                    pin: Some(Held::Found(pin)),
                }),
        );
        let before = nodes.len();
        let members: Vec<os::Pin> = all
            .iter()
            .filter(|s| s.group == root && !nodes.iter().any(|n| n.pid == s.pid))
            .filter_map(os::pin)
            .collect();
        // Pinned while the provider, the group's leader, still lives: the group's ID
        // was its own the whole time.
        if self.root.is_alive() {
            nodes.extend(members.into_iter().map(|pin| tree::Node {
                pid: pin.pid,
                pin: Some(Held::Found(pin)),
            }));
        }
        tree::adopt(
            &mut nodes,
            root,
            &all,
            |seen| os::pin(seen).map(Held::Found),
            |held| held.pin().exists(),
        );
        let added = nodes.len() > before;
        self.found = nodes
            .into_iter()
            .filter_map(|node| match node.pin {
                Some(Held::Found(pin)) => Some(pin),
                _ => None,
            })
            .collect();
        added
    }

    /// Ends everything it holds, as [`Process::end`] does: freezes it with SIGSTOP,
    /// rescanning once all of it has stopped until a scan finds nothing new, then sends
    /// SIGKILL. Each signal goes only to a process still the one pinned. Returns
    /// whether all of it exited by `deadline`. A process Keystone may not signal isn't
    /// waited for to stop, and it hasn't exited until it does so on its own.
    pub fn end(&mut self, deadline: Instant) -> bool {
        self.scan();
        loop {
            self.signal(libc::SIGSTOP);
            if !self.wait(|pin| pin.is(Targets::stopped), deadline) || !self.scan() {
                break;
            }
        }
        self.signal(libc::SIGKILL);
        self.wait(|pin| !pin.exists(), deadline)
    }

    /// The processes it holds that haven't exited, including any Keystone may not
    /// signal.
    pub fn left(&self) -> Vec<u32> {
        std::iter::once(&self.root)
            .chain(&self.found)
            .filter(|pin| pin.exists())
            .map(|pin| pin.pid)
            .collect()
    }

    fn signal(&self, signal: libc::c_int) {
        std::iter::once(&self.root)
            .chain(&self.found)
            .for_each(|pin| pin.signal(signal));
    }

    fn wait(&self, done: impl Fn(&os::Pin) -> bool, deadline: Instant) -> bool {
        loop {
            if std::iter::once(&self.root).chain(&self.found).all(&done) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Choosing which processes belong to a provider's tree, from a scan of every process.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod tree {
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Stat {
        pub pid: u32,
        pub ppid: u32,
        pub group: u32,
        /// When the process started, in the platform's own units.
        pub start: u64,
    }

    /// Chooses targets from a non-atomic process scan without trusting a PID that may
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
    pub fn select<F>(
        root: u32,
        root_unreaped: bool,
        group_reserved: bool,
        all: &[Stat],
        open: impl Fn(&Stat) -> Option<F>,
        alive: impl Fn(&F) -> bool,
    ) -> Vec<F> {
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
        adopt(&mut nodes, root, all, open, alive);
        nodes.into_iter().filter_map(|n| n.pin).collect()
    }

    pub struct Node<F> {
        pub pid: u32,
        /// `None` only for an unreaped child of this process, which cannot be replaced.
        pub pin: Option<F>,
    }

    /// Adds every process `all` shows below `nodes`, other than `root`, by the rule
    /// [`select`] gives for a child.
    pub fn adopt<F>(
        nodes: &mut Vec<Node<F>>,
        root: u32,
        all: &[Stat],
        open: impl Fn(&Stat) -> Option<F>,
        alive: impl Fn(&F) -> bool,
    ) {
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

#[cfg(target_os = "linux")]
mod pidfd {
    use super::tree::{Stat, select};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    pub use Pidfd as Pin;

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

        /// Whether the pinned process hasn't exited, though Keystone may not signal it,
        /// such as a command run through `sudo`: only `ESRCH` means it is gone. A
        /// zombie has exited. The state is read first, so it is the pinned process's
        /// whenever the pin still holds it after.
        pub fn exists(&self) -> bool {
            let state = state(self.pid);
            // SAFETY: the descriptor is valid for the lifetime of `self`.
            let sent = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    0,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                ) == 0
            };
            let held = sent || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
            held && !matches!(state, Some('Z' | 'X'))
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

        pub fn duplicate(&self) -> Option<Self> {
            Some(Self {
                fd: self.fd.try_clone().ok()?,
                pid: self.pid,
            })
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

    pub fn stat(pid: u32) -> Option<Stat> {
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

    /// Every process `/proc` lists.
    pub fn all() -> Vec<Stat> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .filter_map(stat)
            .collect()
    }

    /// Pins the process a scan saw, if its PID still holds it.
    pub fn pin(seen: &Stat) -> Option<Pidfd> {
        let fd = Pidfd::open(seen.pid)?;
        // The pidfd now pins whatever holds this PID. Keep it only if that is still
        // the process the scan found.
        (stat(seen.pid)? == *seen).then_some(fd)
    }

    /// The live processes to signal for `root`'s tree, each pinned by a pidfd.
    pub fn descendants(root: u32, root_unreaped: bool, group_reserved: bool) -> Vec<Pidfd> {
        let all = all();
        select(
            root,
            root_unreaped,
            group_reserved,
            &all,
            pin,
            Pidfd::is_alive,
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// init runs as root, so a user's process may not signal it, and it still runs.
        #[test]
        fn a_process_keystone_may_not_signal_still_exists() {
            assert!(pin(&stat(1).unwrap()).unwrap().exists());
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pinned = pin(&stat(child.id()).unwrap()).unwrap();
            child.wait().unwrap();
            assert!(!pinned.exists(), "one that exited is gone");
        }
    }
}

#[cfg(target_os = "macos")]
mod libproc {
    use super::tree::{Stat, select};

    /// A process as one scan saw it. macOS has no pidfd, so each use re-reads the
    /// process and acts only if the PID still holds a process with the same start
    /// time. A PID reused between that check and the signal could still be reached;
    /// the window is one system call.
    pub struct Pin {
        pub pid: u32,
        seen: Stat,
    }

    fn info(pid: u32) -> Option<libc::proc_bsdinfo> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        (written == size).then_some(info)
    }

    pub fn stat(pid: u32) -> Option<Stat> {
        let info = info(pid)?;
        Some(Stat {
            pid,
            ppid: info.pbi_ppid,
            group: info.pbi_pgid,
            start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        })
    }

    /// `pid`'s state in `/proc`'s letters, `None` once nothing holds that PID.
    pub fn state(pid: u32) -> Option<char> {
        Some(match info(pid)?.pbi_status {
            libc::SSTOP => 'T',
            libc::SZOMB => 'Z',
            libc::SSLEEP => 'S',
            _ => 'R',
        })
    }

    impl Pin {
        fn open(seen: &Stat) -> Option<Self> {
            (stat(seen.pid)? == *seen).then_some(Self {
                pid: seen.pid,
                seen: *seen,
            })
        }

        /// Whether the PID still holds the process the scan saw, not yet exited.
        pub fn is_alive(&self) -> bool {
            stat(self.seen.pid).is_some_and(|now| now.start == self.seen.start)
                && state(self.seen.pid) != Some('Z')
        }

        pub fn is(&self, check: fn(Option<char>) -> bool) -> bool {
            let state = state(self.seen.pid);
            if self.is_alive() {
                check(state)
            } else {
                check(None)
            }
        }

        /// Whether the process hasn't exited, though Keystone may not signal it, such
        /// as a command run through `sudo`. macOS refuses the full read of another
        /// user's process, so then whatever holds the PID counts until it exits: one
        /// that only took the PID over holds its session rather than free it unproven.
        pub fn exists(&self) -> bool {
            if stat(self.seen.pid).is_some() {
                return self.is_alive();
            }
            let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
            // SAFETY: proc_pidinfo writes at most `size` bytes into `info`. This
            // flavor is the one macOS gives for any user's process.
            let written = unsafe {
                libc::proc_pidinfo(
                    self.seen.pid as libc::c_int,
                    libc::PROC_PIDT_SHORTBSDINFO,
                    0,
                    (&raw mut info).cast(),
                    size,
                )
            };
            written == size && info.pbsi_status != libc::SZOMB
        }

        /// Whether Keystone may not signal the process, such as one running as
        /// another user.
        pub fn is_foreign(&self) -> bool {
            // SAFETY: signal 0 only checks permission.
            let refused = unsafe { libc::kill(self.seen.pid as libc::pid_t, 0) } != 0;
            refused
                && self.is_alive()
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }

        pub fn duplicate(&self) -> Option<Self> {
            Some(Self {
                pid: self.pid,
                seen: self.seen,
            })
        }

        pub fn signal(&self, signal: libc::c_int) {
            if self.is_alive() {
                // SAFETY: the PID held this process at the check just made.
                unsafe { libc::kill(self.seen.pid as libc::pid_t, signal) };
            }
        }
    }

    /// Every process the kernel lists.
    pub fn all() -> Vec<Stat> {
        // SAFETY: with no buffer, proc_listallpids returns how many PIDs there are.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let mut pids = vec![0 as libc::pid_t; count.max(0) as usize + 256];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer holds `bytes` bytes, and the call writes at most that.
        let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        pids.truncate(found.max(0) as usize);
        pids.into_iter()
            .filter(|pid| *pid > 0)
            .filter_map(|pid| stat(pid as u32))
            .collect()
    }

    /// Pins the process a scan saw, if its PID still holds it.
    pub fn pin(seen: &Stat) -> Option<Pin> {
        Pin::open(seen)
    }

    /// The live processes to signal for `root`'s tree, each pinned.
    pub fn descendants(root: u32, root_unreaped: bool, group_reserved: bool) -> Vec<Pin> {
        let all = all();
        select(
            root,
            root_unreaped,
            group_reserved,
            &all,
            pin,
            Pin::is_alive,
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// launchd runs as root, so a user's process may neither read it in full nor
        /// signal it, and it still runs.
        #[test]
        fn a_process_keystone_may_not_signal_still_exists() {
            // SAFETY: geteuid only reads.
            if unsafe { libc::geteuid() } == 0 {
                eprintln!("skipped: root may read and signal launchd");
                return;
            }
            let seen = Stat {
                pid: 1,
                ppid: 0,
                group: 1,
                start: 0,
            };
            assert!(stat(1).is_none(), "macOS refuses the full read");
            assert!(Pin { pid: 1, seen }.exists());
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let seen = stat(child.id()).unwrap();
            child.wait().unwrap();
            assert!(
                !Pin {
                    pid: seen.pid,
                    seen
                }
                .exists(),
                "one that exited is gone"
            );
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    fn alive(pid: u32) -> bool {
        // Exited processes count as gone.
        !matches!(process_state(pid), None | Some('Z' | 'X'))
    }

    fn children_of(pid: u32) -> Vec<u32> {
        os::all()
            .into_iter()
            .filter(|stat| stat.ppid == pid)
            .map(|stat| stat.pid)
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
            // and group of its own, as agents do with the shells they start. macOS has
            // no setsid command, so Python calls it.
            &[
                "-c",
                "sh -c '/usr/bin/python3 -c \"import os, time; os.setsid(); time.sleep(30)\" & echo $!; wait' & wait",
            ],
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
