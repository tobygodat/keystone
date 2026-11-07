//! `keystone-host reap`: one watcher per provider process the daemon starts. It
//! follows the provider's process tree, and if the daemon dies, it ends that tree
//! before a later daemon may end the provider's turn and let the next one resume.
//!
//! Each watcher holds an exclusive lock on its own file in the run folder,
//! `reap-<pid>-<start>.lock`, from before the provider gets any input until nothing of
//! its tree is left, and removes the file before it lets go. The file's first line names
//! the session. A daemon that starts after a crash finds the files its predecessor left
//! and holds each session until its watcher lets go. A file still there with nobody
//! holding its lock lost its watcher before the tree was gone.

use super::Route;
use super::fault;
use crate::host::{Watched, process_start};
use keystone_core::Provider;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How often a watcher looks at the tree and the daemon.
const LOOK: Duration = Duration::from_millis(250);
/// How long one attempt to end the tree waits for it to exit.
const END_WAIT: Duration = Duration::from_secs(1);
/// How long the daemon waits for a new watcher to say it holds the lock and the
/// provider.
const READY_WAIT: Duration = Duration::from_secs(2);

macro_rules! log {
    ($($arg:tt)*) => { eprintln!("keystone-host reap: {}", format_args!($($arg)*)) };
}

/// Which session a watched provider runs for: the first line of its watcher's file.
#[derive(Serialize, Deserialize)]
pub(crate) struct Watching {
    pub client: String,
    pub session: u64,
    pub provider: Provider,
    pub account: Option<String>,
}

impl Watching {
    pub fn route(&self) -> Route {
        Route {
            provider: self.provider,
            account: self.account.clone(),
        }
    }
}

/// Starts a watcher from `program` for provider `pid`, which this daemon just started,
/// and returns once the watcher holds the provider. See [`start_watcher`].
pub(crate) fn watch(
    program: &Path,
    run: &Path,
    watching: &Watching,
    pid: u32,
) -> Result<(), String> {
    let start = process_start(pid).ok_or("the provider exited before it could be watched")?;
    start_watcher(program, run, watching, pid, start, Some(std::process::id()))
}

/// Locks the watcher file for provider `pid`, which started at `start`, names the
/// session in it, and starts `keystone-host reap` from `program` holding that lock, in
/// a session of its own so a signal to the daemon's group or session doesn't reach it.
/// Returns once the watcher says it holds the lock and has pinned the provider, and
/// fails, removing the file, when it doesn't. The daemon holds the lock until then, so
/// a daemon that dies at any point leaves either a held lock or a provider that never
/// got any input. `daemon` is the daemon whose death ends the tree, or `None` when it
/// has already died.
fn start_watcher(
    program: &Path,
    run: &Path,
    watching: &Watching,
    pid: u32,
    start: u64,
    daemon: Option<u32>,
) -> Result<(), String> {
    let path = run.join(format!("reap-{pid}-{start}.lock"));
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("Could not open {}: {e}", path.display()))?;
    if !try_lock(&file) {
        return Err(format!("{} is already locked", path.display()));
    }
    let started = spawn(
        program,
        &path,
        &file,
        watching,
        [pid, daemon.unwrap_or(0)],
        start,
    );
    if started.is_err() {
        forget(&path);
    }
    started
}

fn spawn(
    program: &Path,
    path: &Path,
    mut file: &File,
    watching: &Watching,
    [pid, daemon]: [u32; 2],
    start: u64,
) -> Result<(), String> {
    let mut line = serde_json::to_vec(watching).map_err(|e| e.to_string())?;
    line.push(b'\n');
    file.set_len(0)
        .and_then(|()| file.write_all(&line))
        .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
    let (mut ready, said) =
        io::pipe().map_err(|e| format!("Could not start keystone-host reap: {e}"))?;
    let (lock, said_fd) = (file.as_raw_fd(), said.as_raw_fd());
    let mut command = Command::new(program);
    command
        .arg0("keystone-host")
        .arg("reap")
        .arg(path)
        .arg(lock.to_string())
        .arg(said_fd.to_string())
        .arg(pid.to_string())
        .arg(start.to_string())
        .arg(daemon.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    // SAFETY: between fork and exec the child calls only setsid and fcntl, which are
    // async-signal-safe, and touches only its own copy of the descriptor table.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            for fd in [lock, said_fd] {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let spawned = command.spawn();
    drop(said);
    let mut child = spawned.map_err(|e| format!("Could not start keystone-host reap: {e}"))?;
    if !answered(&mut ready) {
        let _ = child.kill();
        let _ = child.wait();
        return Err("keystone-host reap couldn't take over watching it".into());
    }
    let _ = thread::Builder::new()
        .name("host-reap-wait".into())
        .spawn(move || child.wait());
    Ok(())
}

/// Whether the watcher wrote its byte within [`READY_WAIT`]. One that can't watch
/// exits without it, which closes the pipe.
fn answered(ready: &mut io::PipeReader) -> bool {
    let mut poll = libc::pollfd {
        fd: ready.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let deadline = Instant::now() + READY_WAIT;
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        // SAFETY: poll reads and writes only the one pollfd, which outlives the call.
        let found = unsafe { libc::poll(&mut poll, 1, wait.as_millis() as libc::c_int) };
        if found < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return found > 0 && matches!(ready.read(&mut [0]), Ok(1));
    }
}

fn try_lock(file: &File) -> bool {
    // SAFETY: flock only reads the descriptor, which `file` keeps open.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// The watcher: `keystone-host reap <file> <fd> <ready> <pid> <start> <daemon>`.
pub fn reap() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let [path, fd, ready, pid, start, daemon] = args.as_slice() else {
        return Err("usage: keystone-host reap <file> <fd> <ready> <pid> <start> <daemon>".into());
    };
    let number = |text: &str| -> Result<u64, String> {
        text.parse()
            .map_err(|_| format!("reap: {text:?} is not a whole number"))
    };
    let descriptor = |text: &str| -> Result<RawFd, String> {
        let fd = RawFd::try_from(number(text)?).map_err(|e| e.to_string())?;
        // SAFETY: fcntl only reads the descriptor's flags.
        if fd < 3 || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(format!("reap: descriptor {fd} isn't open"));
        }
        Ok(fd)
    };
    let (fd, ready) = (descriptor(fd)?, descriptor(ready)?);
    // SAFETY: the daemon passed these descriptors to this process alone, and nothing
    // else here owns them.
    let (lock, mut ready) = unsafe { (File::from_raw_fd(fd), File::from_raw_fd(ready)) };
    if !try_lock(&lock) {
        return Err(format!(
            "reap: descriptor {fd} doesn't hold the lock of {path}"
        ));
    }
    let path = PathBuf::from(path);
    let (pid, start) = (number(pid)? as u32, number(start)?);
    let daemon = number(daemon)? as libc::pid_t;
    let Some(mut watched) = Watched::new(pid, start) else {
        forget(&path);
        return Err(format!("reap: provider {pid} has already exited"));
    };
    if fault("reap-refuse") {
        forget(&path);
        return Err("reap: refusing to watch (test fault)".into());
    }
    // A daemon that stopped waiting kills this watcher, and one that died leaves it to
    // end the tree, so a write that fails changes nothing.
    let _ = ready.write_all(&[1]);
    drop(ready);
    if outlived(&mut watched, daemon) {
        log!("the daemon stopped, so ending what is left of provider {pid}");
        end(&mut watched, &lock, pid);
    }
    forget(&path);
    Ok(())
}

/// Follows the tree until nothing it holds is left, or the daemon dies. Returns
/// whether the daemon died. While the daemon lives this ends nothing: a provider that
/// exits on its own leaves the rest to the daemon's teardown, as for a local session,
/// and the lock stays held until that is done.
fn outlived(watched: &mut Watched, daemon: libc::pid_t) -> bool {
    loop {
        // SAFETY: getppid only reads. Once the daemon dies, this process has another
        // parent, whatever process later takes the daemon's PID. A daemon of 0 died
        // before this watcher started.
        if unsafe { libc::getppid() } != daemon {
            return true;
        }
        watched.scan();
        if watched.left().is_empty() {
            return false;
        }
        thread::sleep(LOOK);
    }
}

/// Ends the tree, however long it takes, and notes in the lock file each set of
/// processes it is still waiting for, so the next daemon can name them.
fn end(watched: &mut Watched, mut lock: &File, pid: u32) {
    let mut noted = Vec::new();
    loop {
        let ended = if fault("reap-hold") {
            watched.scan();
            false
        } else {
            watched.end(Instant::now() + END_WAIT)
        };
        if ended {
            return;
        }
        let left = watched.left();
        if left != noted {
            log!("provider {pid}'s processes {left:?} haven't exited yet");
            if let Ok(mut line) = serde_json::to_vec(&left) {
                line.push(b'\n');
                let _ = lock.write_all(&line);
            }
            noted = left;
        }
        thread::sleep(LOOK);
    }
}

/// Removes the watcher's file. The lock goes with this process.
fn forget(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// A watcher file a daemon found when it started.
pub(crate) struct Left {
    path: PathBuf,
    file: File,
    pub watching: Watching,
    /// The provider it watches, and when that provider started.
    pid: u32,
    start: u64,
}

/// What became of a [`Left`] file's watcher.
pub(crate) enum Watcher {
    /// It still holds the lock.
    Holds,
    /// It removed its file and let go, so nothing it held is left.
    Finished,
    /// It let go and left its file, so it stopped before what it held was gone.
    Lost,
}

impl Left {
    fn open(path: PathBuf) -> Option<Left> {
        let name = path.file_name()?.to_str()?;
        let (pid, start) = name
            .strip_prefix("reap-")?
            .strip_suffix(".lock")?
            .split_once('-')?;
        let (pid, start) = (pid.parse().ok()?, start.parse().ok()?);
        let file = File::options().read(true).write(true).open(&path).ok()?;
        let first = std::fs::read_to_string(&path).unwrap_or_default();
        match serde_json::from_str(first.lines().next().unwrap_or_default()) {
            Ok(watching) => Some(Left {
                path,
                file,
                watching,
                pid,
                start,
            }),
            // The daemon died before it named the session, so before it started the
            // watcher or gave the provider any input.
            Err(_) if try_lock(&file) => {
                forget(&path);
                None
            }
            Err(error) => {
                log!("{} names no session ({error})", path.display());
                None
            }
        }
    }

    pub fn watcher(&self) -> Watcher {
        if !try_lock(&self.file) {
            return Watcher::Holds;
        }
        let here = std::fs::metadata(&self.path);
        match (here, self.file.metadata()) {
            (Ok(here), Ok(mine)) if (here.dev(), here.ino()) == (mine.dev(), mine.ino()) => {
                Watcher::Lost
            }
            _ => Watcher::Finished,
        }
    }

    /// The processes its watcher last said it waits for.
    pub fn stuck(&self) -> Vec<u32> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line).ok())
            .unwrap_or_default()
    }

    /// Takes over for a [`Watcher::Lost`] watcher. While its provider still runs, a
    /// new watcher from `program` ends its tree at once, since the daemon that started
    /// it is gone. Otherwise what the provider started can't be found, so this removes
    /// the file and returns the provider's PID.
    pub fn rewatch(self, program: &Path, run: &Path) -> Result<Left, u32> {
        let Left {
            path,
            file,
            watching,
            pid,
            start,
        } = self;
        drop(file);
        if process_start(pid) == Some(start) {
            match start_watcher(program, run, &watching, pid, start, None) {
                Ok(()) => {
                    if let Some(left) = Left::open(path.clone()) {
                        return Ok(left);
                    }
                }
                Err(error) => log!("couldn't watch provider {pid} again: {error}"),
            }
        }
        forget(&path);
        Err(pid)
    }
}

/// The watcher files in `run`, but those whose watchers finished. Waits up to `wait`
/// for the held ones to finish.
pub(crate) fn left(run: &Path, wait: Duration) -> Vec<Left> {
    let Ok(entries) = std::fs::read_dir(run) else {
        return Vec::new();
    };
    let mut found: Vec<Left> = entries
        .flatten()
        .filter_map(|entry| Left::open(entry.path()))
        .collect();
    let deadline = Instant::now() + wait;
    loop {
        let mut holding = false;
        found.retain(|left| match left.watcher() {
            Watcher::Holds => {
                holding = true;
                true
            }
            Watcher::Finished => false,
            Watcher::Lost => true,
        });
        if !holding || Instant::now() >= deadline {
            return found;
        }
        thread::sleep(Duration::from_millis(25));
    }
}
