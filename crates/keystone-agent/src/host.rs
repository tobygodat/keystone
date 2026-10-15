//! Child processes for providers: spawn, line framing, stdin writes, a stderr tail,
//! and process-tree teardown. Drivers never touch any of this.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Bytes of stderr kept for error messages.
const STDERR_TAIL: usize = 4 * 1024;
/// Time between SIGTERM and SIGKILL during teardown.
const KILL_GRACE: Duration = Duration::from_secs(2);

pub struct Exit {
    pub code: Option<i32>,
    /// The end of the process's stderr, for showing why it stopped.
    pub stderr_tail: String,
}

/// A running child process. Dropping it tears down the whole process tree.
pub struct Process {
    pid: u32,
    stdin: Sender<String>,
}

impl Process {
    /// Spawns `program`. `on_line` runs on a reader thread for every stdout line,
    /// without the trailing newline. `on_exit` runs once after stdout closes and the
    /// process has been reaped.
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
        let pid = child.id();
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx, rx) = mpsc::channel::<String>();
        thread::Builder::new()
            .name("provider-stdin".into())
            .spawn(move || write_lines(stdin, rx))?;

        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
        let stderr_thread = {
            let tail = tail.clone();
            thread::Builder::new()
                .name("provider-stderr".into())
                .spawn(move || keep_tail(stderr, &tail))?
        };

        thread::Builder::new()
            .name("provider-stdout".into())
            .spawn(move || {
                read_lines(stdout, &mut on_line);
                let code = wait(&mut child);
                let _ = stderr_thread.join();
                let bytes: Vec<u8> = tail.lock().unwrap().iter().copied().collect();
                on_exit(Exit {
                    code,
                    stderr_tail: String::from_utf8_lossy(&bytes).trim().to_owned(),
                });
            })?;
        Ok(Self { pid, stdin: tx })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Queues one line for stdin. A newline is added.
    pub fn send(&self, line: String) {
        let _ = self.stdin.send(line);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        terminate_tree(self.pid);
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

fn wait(child: &mut Child) -> Option<i32> {
    child.wait().ok().and_then(|status| status.code())
}

/// Sends SIGTERM to the process, its group, and every descendant, then SIGKILL to
/// whatever remains after a grace period. Agents start shells in groups of their own,
/// so the group alone does not reach them.
#[cfg(unix)]
pub fn terminate_tree(pid: u32) {
    let mut targets = descendants(pid);
    targets.push(pid);
    signal_all(pid, &targets, libc::SIGTERM);
    thread::spawn(move || {
        thread::sleep(KILL_GRACE);
        signal_all(pid, &targets, libc::SIGKILL);
    });
}

#[cfg(unix)]
fn signal_all(group: u32, pids: &[u32], signal: libc::c_int) {
    // SAFETY: kill only sends a signal; invalid or exited PIDs return an error.
    unsafe {
        libc::kill(-(group as libc::pid_t), signal);
        for &pid in pids {
            libc::kill(pid as libc::pid_t, signal);
        }
    }
}

#[cfg(not(unix))]
pub fn terminate_tree(_pid: u32) {
    // Windows needs a job object; see the platform section of the design.
}

/// Descendants of `root` from `/proc`, deepest first. Empty where `/proc` is absent.
#[cfg(unix)]
fn descendants(root: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let parents: Vec<(u32, u32)> = entries
        .filter_map(|entry| {
            let pid: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // The command name can contain spaces, so parse after its closing paren.
            let ppid = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            Some((pid, ppid))
        })
        .collect();
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for &(pid, ppid) in &parents {
            if ppid == parent && !found.contains(&pid) {
                found.push(pid);
                frontier.push(pid);
            }
        }
    }
    found.reverse();
    found
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

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
        let children = descendants(pid);
        assert!(!children.is_empty());
        drop(process);
        exit_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread::sleep(Duration::from_millis(200));
        for child in children {
            assert!(
                !Path::new(&format!("/proc/{child}")).exists(),
                "child {child} survived"
            );
        }
    }
}
