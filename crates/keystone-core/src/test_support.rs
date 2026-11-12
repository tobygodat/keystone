//! Helpers that tests in every crate share. Built for this crate's tests and, through the
//! `test-support` feature, for the other crates' tests.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes `contents` to `path` and makes it executable, so a test can run it at once.
///
/// The file is written by a child process. A file this process wrote could not be run
/// reliably: while the write descriptor is open, another test thread's spawn can fork
/// and inherit it, and until that child execs, the kernel refuses to run the file with
/// "Text file busy". A child's descriptor closes when it exits, so once this returns no
/// process holds the file open for writing.
pub fn write_executable(path: &Path, contents: impl AsRef<[u8]>) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("couldn't start sh to write {}: {e}", path.display()));
    let written = child.stdin.take().unwrap().write_all(contents.as_ref());
    let status = child.wait().unwrap();
    written.unwrap_or_else(|e| panic!("couldn't write {}: {e}", path.display()));
    assert!(
        status.success(),
        "couldn't write {}: {status}",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::write_executable;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn an_executable_runs_at_once_while_other_threads_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let runs = std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = Command::new("/bin/true").status();
                    }
                });
            }
            let runs: Vec<_> = (0..300)
                .map(|i| {
                    let path = dir.path().join(format!("script-{i}"));
                    write_executable(&path, "#!/bin/sh\nexit 0\n");
                    Command::new(&path).status()
                })
                .collect();
            stop.store(true, Ordering::Relaxed);
            runs
        });
        let busy = runs
            .iter()
            .filter(|run| matches!(run, Err(e) if e.raw_os_error() == Some(libc::ETXTBSY)))
            .count();
        assert_eq!(busy, 0, "runs of 300 that failed with Text file busy");
        for run in runs {
            assert!(run.unwrap().success());
        }
    }
}
