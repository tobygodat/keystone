use super::*;
use std::sync::mpsc::Receiver;

const SH: &str = "/bin/sh";

fn run(script: &str, worktree: &Path, root: &Path) -> (SetupRun, Receiver<()>) {
    let (woke, wakes) = mpsc::channel();
    let woke = Mutex::new(woke);
    let run = start(SH.as_ref(), script, worktree, root, move || {
        let _ = woke.lock().unwrap().send(());
    })
    .unwrap();
    (run, wakes)
}

/// Every event until the run exits, or what arrived by `within`.
fn events(run: &SetupRun, wakes: &Receiver<()>, within: Duration) -> Vec<SetupEvent> {
    let deadline = Instant::now() + within;
    let mut all = Vec::new();
    loop {
        all.extend(run.drain());
        if matches!(all.last(), Some(SetupEvent::Exited(_))) {
            return all;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || wakes.recv_timeout(left).is_err() {
            all.extend(run.drain());
            return all;
        }
    }
}

fn lines(events: &[SetupEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            SetupEvent::Line(line) => Some(line.as_str()),
            SetupEvent::Exited(_) => None,
        })
        .collect()
}

fn group_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the group exists.
    unsafe { libc::killpg(pid, 0) == 0 }
}

#[test]
fn a_passing_script_reports_its_lines_then_exit_zero() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run("echo installing; echo done", folder.path(), folder.path());
    let events = events(&run, &wakes, Duration::from_secs(10));
    assert_eq!(lines(&events), ["installing", "done"]);
    assert_eq!(events.last(), Some(&SetupEvent::Exited(SetupExit::Code(0))));
}

#[test]
fn a_failing_script_reports_its_stderr_and_exit_code() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run(
        "echo starting; echo 'missing .env' >&2; exit 3",
        folder.path(),
        folder.path(),
    );
    let events = events(&run, &wakes, Duration::from_secs(10));
    assert_eq!(lines(&events), ["starting", "missing .env"]);
    assert_eq!(events.last(), Some(&SetupEvent::Exited(SetupExit::Code(3))));
}

#[test]
fn the_script_runs_in_the_worktree_with_both_paths_set() {
    let root = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let (run, wakes) = run(
        r#"pwd -P; echo "root=$KEYSTONE_PROJECT_ROOT"; echo "tree=$KEYSTONE_WORKTREE_PATH""#,
        worktree.path(),
        root.path(),
    );
    let events = events(&run, &wakes, Duration::from_secs(10));
    let canonical = worktree.path().canonicalize().unwrap();
    assert_eq!(
        lines(&events),
        [
            canonical.to_str().unwrap().to_owned(),
            format!("root={}", root.path().display()),
            format!("tree={}", worktree.path().display()),
        ]
    );
}

#[test]
fn escapes_are_stripped_and_a_progress_redraw_keeps_its_last_frame() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run(
        r"printf '\033[1;32mgreen\033[0m \033]0;title\007ok\r\n'; printf 'fetch 10%%\rfetch 50%%\rfetch 100%%\n'; printf 'tab\there'",
        folder.path(),
        folder.path(),
    );
    let events = events(&run, &wakes, Duration::from_secs(10));
    assert_eq!(lines(&events), ["green ok", "fetch 100%", "tab here"]);
}

#[test]
fn a_long_line_is_cut() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run(
        "i=0; while [ $i -lt 600 ]; do printf x; i=$((i+1)); done; echo",
        folder.path(),
        folder.path(),
    );
    let events = events(&run, &wakes, Duration::from_secs(10));
    let lines = lines(&events);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].chars().count(), LINE_MAX_CHARS + 1);
    assert!(lines[0].ends_with('…'));
}

#[test]
fn stop_ends_the_script_and_the_processes_it_started() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run(
        "echo started; sleep 30 & sleep 30; wait",
        folder.path(),
        folder.path(),
    );
    assert_eq!(
        lines(&events(&run, &wakes, Duration::from_secs(5))),
        ["started"]
    );
    let pid = run.pid();
    assert!(group_alive(pid));
    let stopped = Instant::now();
    run.stop();
    run.stop();
    let events = events(&run, &wakes, Duration::from_secs(10));
    assert!(
        matches!(
            events.last(),
            Some(SetupEvent::Exited(SetupExit::Signal(_)))
        ),
        "{events:?}"
    );
    assert!(
        stopped.elapsed() < Duration::from_secs(4),
        "{:?}",
        stopped.elapsed()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while group_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!group_alive(pid), "the background sleep was stopped too");
}

#[test]
fn a_script_that_ignores_term_is_killed() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run(
        "trap '' TERM; echo started; while :; do sleep 1; done",
        folder.path(),
        folder.path(),
    );
    assert_eq!(
        lines(&events(&run, &wakes, Duration::from_secs(5))),
        ["started"]
    );
    let pid = run.pid();
    let stopped = Instant::now();
    assert!(stop_and_wait([&run], Duration::from_millis(1500)));
    assert!(
        !group_alive(pid),
        "the group is gone once stop_and_wait returns"
    );
    assert!(
        stopped.elapsed() < Duration::from_secs(3),
        "{:?}",
        stopped.elapsed()
    );
    assert_eq!(
        events(&run, &wakes, Duration::from_secs(1)).last(),
        Some(&SetupEvent::Exited(SetupExit::Signal(libc::SIGKILL)))
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while group_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!group_alive(pid));
}

#[test]
fn dropping_a_run_stops_it() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run("echo started; sleep 30", folder.path(), folder.path());
    events(&run, &wakes, Duration::from_secs(5));
    let pid = run.pid();
    drop(run);
    let deadline = Instant::now() + Duration::from_secs(4);
    while group_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!group_alive(pid));
}

#[test]
fn a_shell_that_cannot_start_in_a_missing_folder_is_an_error() {
    let folder = tempfile::tempdir().unwrap();
    let missing = folder.path().join("gone");
    assert!(start(SH.as_ref(), "true", &missing, folder.path(), || {}).is_err());
}

#[test]
fn the_script_leads_a_session_of_its_own() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run("echo started; sleep 30", folder.path(), folder.path());
    events(&run, &wakes, Duration::from_secs(5));
    // SAFETY: getsid only reads.
    assert_eq!(unsafe { libc::getsid(run.pid()) }, run.pid());
}

#[test]
fn a_child_that_ignores_term_is_killed_once_the_shell_has_exited() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run(
        "(trap '' TERM; sleep 30) & echo started; wait",
        folder.path(),
        folder.path(),
    );
    assert_eq!(
        lines(&events(&run, &wakes, Duration::from_secs(5))),
        ["started"]
    );
    let pid = run.pid();
    let stopped = Instant::now();
    run.stop();
    let events = events(&run, &wakes, Duration::from_secs(6));
    assert!(
        matches!(events.last(), Some(SetupEvent::Exited(_))),
        "{events:?}"
    );
    assert!(!group_alive(pid), "the child that ignored SIGTERM is gone");
    assert!(
        stopped.elapsed() < Duration::from_secs(5),
        "{:?}",
        stopped.elapsed()
    );
}

#[test]
fn dropping_a_run_that_succeeded_leaves_what_it_started() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run(
        "sleep 30 >/dev/null 2>&1 & echo $!",
        folder.path(),
        folder.path(),
    );
    let events = events(&run, &wakes, Duration::from_secs(5));
    assert_eq!(events.last(), Some(&SetupEvent::Exited(SetupExit::Code(0))));
    let child: i32 = lines(&events)[0].parse().unwrap();
    drop(run);
    thread::sleep(Duration::from_millis(300));
    // SAFETY: signal 0 only checks whether the process exists; SIGKILL then ends the
    // test's own background sleep.
    unsafe {
        assert_eq!(libc::kill(child, 0), 0, "the background sleep still runs");
        libc::kill(child, libc::SIGKILL);
    }
}

#[test]
fn a_flood_of_output_keeps_the_latest_lines_and_wakes_once() {
    let folder = tempfile::tempdir().unwrap();
    let (run, wakes) = run("seq 1 100000", folder.path(), folder.path());
    assert!(run.wait_until(Instant::now() + Duration::from_secs(20)));
    assert_eq!(
        wakes.try_iter().count(),
        1,
        "one wake until the first drain"
    );
    let events = run.drain();
    let lines = lines(&events);
    assert!(lines.len() <= KEPT_LINES, "{}", lines.len());
    assert_eq!(lines.last(), Some(&"100000"));
    assert_eq!(events.last(), Some(&SetupEvent::Exited(SetupExit::Code(0))));
}

#[test]
fn the_shell_is_the_users_unless_it_is_unset() {
    assert_eq!(shell_for(Some("/bin/zsh".into())), "/bin/zsh");
    assert_eq!(shell_for(Some("".into())), "/bin/sh");
    assert_eq!(shell_for(None), "/bin/sh");
}
