//! `keystone-host attach`: joins its stdin and stdout to the daemon's socket, starting
//! the daemon when none answers. SSH runs it on the remote machine; tests run it
//! directly.

use super::frame::{self, Frame};
use super::paths::{Paths, private_dir};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long `attach` waits for a daemon it started.
const START_WAIT: Duration = Duration::from_secs(5);

/// Bridges until either side closes. A host that can never be reached safely, such as
/// one whose folders other users can open, answers with a `refused` frame, so the app
/// shows why and stops retrying. Any other failure to reach the daemon, such as one
/// still exiting, only ends `attach` with the reason, and the app tries again.
pub fn attach() -> Result<(), String> {
    let paths = Paths::from_env()
        .and_then(|paths| {
            private_dir(&paths.run)?;
            private_dir(&paths.state)?;
            Ok(paths)
        })
        .inspect_err(|reason| {
            if let Ok(line) = frame::encode(&Frame::Refused {
                reason: reason.clone(),
            }) {
                let _ = io::stdout().write_all(&line);
            }
        })?;
    let stream = connect(&paths)?;
    let mut up = stream.try_clone().map_err(|e| e.to_string())?;
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut up);
        let _ = up.shutdown(std::net::Shutdown::Write);
    });
    let mut down = stream;
    let mut stdout = io::stdout().lock();
    let _ = io::copy(&mut down, &mut stdout);
    let _ = stdout.flush();
    Ok(())
}

fn connect(paths: &Paths) -> Result<UnixStream, String> {
    let socket = paths.socket();
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok(stream);
    }
    start(paths)?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        match UnixStream::connect(&socket) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() >= deadline => {
                return Err(format!(
                    "keystone-host serve did not start within 5 s ({error}). Its log is {}",
                    paths.log().display()
                ));
            }
            Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// Starts `keystone-host serve` in a process group of its own, so a signal to this
/// `attach`'s group doesn't reach it, with its output in the log. A second daemon
/// started at the same moment finds the lock taken and exits.
fn start(paths: &Paths) -> Result<(), String> {
    let log = std::fs::File::options()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(paths.log())
        .map_err(|e| format!("Could not open {}: {e}", paths.log().display()))?;
    let program = std::env::current_exe().map_err(|e| e.to_string())?;
    Command::new(program)
        .arg("serve")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|e| format!("Could not start keystone-host serve: {e}"))?;
    Ok(())
}
