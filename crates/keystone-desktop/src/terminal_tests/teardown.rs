use super::*;

#[gpui::test]
fn quitting_ends_every_shell(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let shells = {
        let cx = visual(handle, cx);
        cx.simulate_keystrokes("secondary-j");
        cx.simulate_keystrokes("secondary-shift-d");
        cx.run_until_parked();
        pids(cx)
    };
    assert_eq!(shells.len(), 2);
    assert!(shells.iter().all(|pid| alive(*pid)));
    cx.update(request_quit);
    cx.run_until_parked();
    for pid in shells {
        assert!(!alive(pid), "shell {pid} is gone after Quit");
    }
}

fn signalled(pid: u32) -> bool {
    pane::SIGNALLED.lock().unwrap().contains(&pid)
}

/// Starts a job in the pane that ignores SIGHUP, in the background unless
/// `foreground`, and returns its pid. It ends by itself after 30 s, so a failed test
/// does not leave it running.
fn start_stubborn_job(cx: &mut VisualTestContext, dir: &Path, foreground: bool) -> u32 {
    let file = dir.join(if foreground {
        "foreground.pid"
    } else {
        "background.pid"
    });
    run(
        cx,
        &format!(
            "sh -c 'trap \"\" HUP; echo $$ > {}; exec sleep 30'{}",
            file.display(),
            if foreground { "" } else { " &" }
        ),
    );
    let pid: u32 = wait_for_file(&file).trim().parse().unwrap();
    assert!(alive(pid));
    pid
}

#[gpui::test]
fn closing_a_pane_ends_a_job_that_ignores_hangup(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    let background = start_stubborn_job(cx, dir.path(), false);
    let foreground = start_stubborn_job(cx, dir.path(), true);
    let shell = pids(cx)[0];
    click(cx, "terminal-close");
    assert!(gone_within_3s(shell), "the shell is gone");
    assert!(
        gone_within_3s(background),
        "its background job that ignores SIGHUP"
    );
    assert!(gone_within_3s(foreground), "and its foreground one");
}

/// Nothing reads a closed pane's output, and on macOS a process exiting with output
/// still queued waits for it to drain.
#[gpui::test]
fn closing_a_pane_ends_a_job_that_writes_as_it_hangs_up(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    let file = dir.path().join("writer.pid");
    run(
        cx,
        &format!(
            "sh -c 'echo $$ > {}; trap \"seq 1 200000\" HUP; while [ -d {} ]; do sleep 0.1; done'",
            file.display(),
            dir.path().display()
        ),
    );
    let writer: u32 = wait_for_file(&file).trim().parse().unwrap();
    let shell = pids(cx)[0];
    click(cx, "terminal-close");
    assert!(
        gone_within_3s(writer),
        "the job writing as it hangs up is gone"
    );
    assert!(gone_within_3s(shell), "and so is the shell");
}

#[gpui::test]
fn quitting_ends_a_job_that_ignores_hangup(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let job = {
        let cx = visual(handle, cx);
        cx.simulate_keystrokes("secondary-j");
        start_stubborn_job(cx, dir.path(), false)
    };
    cx.update(request_quit);
    cx.run_until_parked();
    assert!(!alive(job), "Quit waited for the job to be killed");
}

#[gpui::test]
fn quitting_right_after_closing_a_pane_still_ends_its_jobs(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let (job, shell) = {
        let cx = visual(handle, cx);
        cx.simulate_keystrokes("secondary-j");
        let job = start_stubborn_job(cx, dir.path(), false);
        let shell = pids(cx)[0];
        click(cx, "terminal-close");
        (job, shell)
    };
    cx.update(request_quit);
    cx.run_until_parked();
    assert!(!alive(shell), "the closed pane's shell is gone after Quit");
    assert!(!alive(job), "and its job, inside the closed pane's grace");
}

#[gpui::test]
fn a_shell_that_exits_by_itself_gets_no_signal(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    run(cx, "echo ready");
    wait_until(cx, "the shell runs", |cx| has_line(&output(cx), 0, "ready"));
    let shell = pids(cx)[0];
    run(cx, "exit");
    wait_until(cx, "the pane closes", |cx| terminal(cx).is_none());
    std::thread::sleep(Duration::from_millis(100));
    assert!(!signalled(shell), "Keystone sent its PID and group nothing");

    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let closed = pids(cx)[0];
    click(cx, "terminal-close");
    assert!(signalled(closed), "a closed pane's shell is signalled");
}

/// Without a pin, as on a Linux kernel with no `pidfd_open`, closing a pane still
/// ends its shell, here one that ignores SIGHUP, through its PID and group while it
/// is unreaped.
#[gpui::test]
fn closing_a_pane_whose_shell_could_not_be_pinned_still_ends_it(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let ready = dir.path().join("ready");
    let script = format!("trap '' HUP; echo > {}; sleep 30", ready.display());
    cx.update(|cx| use_shell("/bin/sh", &["-c", &script], cx));
    pane::PIN_FAILS.set(true);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    pane::PIN_FAILS.set(false);
    wait_for_file(&ready);
    let shell = pids(cx)[0];
    click(cx, "terminal-close");
    assert!(signalled(shell));
    assert!(
        gone_within_3s(shell),
        "the shell that ignores SIGHUP is gone"
    );
}
