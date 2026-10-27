//! Idle eviction and lazy resume against the scripted Claude, on a clock the tests
//! move: the process host, connection, deadline scheduler and driver together.

use keystone_agent::idle::{IdleCandidate, IdlePolicy, ManualClock};
use keystone_agent::{
    ApprovalState, ClaudeConnection, Command, Connection, Event, RequestId, TurnStatus,
};
use keystone_core::ApprovalMode;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const TEN_MINUTES: Duration = Duration::from_secs(600);
const ASK: ApprovalState = ApprovalState {
    mode: ApprovalMode::Ask,
    overridden: true,
};
const PROVIDER: ApprovalState = ApprovalState {
    mode: ApprovalMode::Provider,
    overridden: false,
};

struct Rig {
    conn: ClaudeConnection,
    wake: Receiver<()>,
    clock: Arc<ManualClock>,
    events: Vec<(u64, Event)>,
    dirs: Vec<tempfile::TempDir>,
}

/// A scripted Claude in idle mode per session directory, with `settings` written as
/// `.fake-claude-<name>` files.
fn rig(enabled: bool) -> Rig {
    let (tx, wake) = channel();
    let program = PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-claude"));
    let conn = ClaudeConnection::new(
        Some(program),
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    let clock = Arc::new(ManualClock::default());
    if enabled {
        conn.enable_idle_eviction(IdlePolicy {
            timeout: TEN_MINUTES,
            clock: clock.clone(),
        });
    }
    Rig {
        conn,
        wake,
        clock,
        events: Vec::new(),
        dirs: Vec::new(),
    }
}

impl Rig {
    /// A directory for one session, with the scripted Claude's settings.
    fn dir(&mut self, settings: &[(&str, &str)]) -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        for file in ["pids", "argv", "messages"] {
            std::fs::write(dir.path().join(format!(".fake-claude-{file}")), "").unwrap();
        }
        for (name, value) in [("idle", "1"), ("delay-ms", "1")].iter().chain(settings) {
            std::fs::write(dir.path().join(format!(".fake-claude-{name}")), value).unwrap();
        }
        let path = dir.path().to_owned();
        self.dirs.push(dir);
        path
    }

    fn send(
        &self,
        session: u64,
        dir: &Path,
        model: Option<&str>,
        approval: ApprovalState,
        text: &str,
    ) {
        self.conn
            .send(
                session,
                dir.to_str().unwrap(),
                None,
                model,
                approval,
                text.into(),
            )
            .unwrap();
    }

    /// Collects events until one for `session` matches, and returns its index.
    fn until(&mut self, session: u64, done: impl Fn(&Event) -> bool) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let start = self.events.len();
            self.events.extend(self.conn.drain());
            if let Some(found) = self.events[start..]
                .iter()
                .position(|(s, e)| *s == session && done(e))
            {
                return start + found;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                self.wake.recv_timeout(left).is_ok(),
                "timed out; got {:?}",
                self.events
            );
        }
    }

    fn completed(&mut self, session: u64) -> usize {
        self.until(session, |e| matches!(e, Event::TurnCompleted { .. }))
    }

    /// Waits for the session's idle phase to read `want`.
    fn residency(&self, session: u64, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let now = self.conn.residency(session);
            if now.as_deref().is_some_and(|now| now.contains(want)) {
                return;
            }
            assert!(Instant::now() < deadline, "{want}: still {now:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Lets queued jobs and timers run, then says whether anything offered `session`.
    fn offered(&mut self, session: u64) -> Option<IdleCandidate> {
        std::thread::sleep(Duration::from_millis(150));
        self.events.extend(self.conn.drain());
        self.events.iter().rev().find_map(|(s, e)| match e {
            Event::IdleCandidate(candidate) if *s == session => Some(candidate.clone()),
            _ => None,
        })
    }

    fn candidate(&mut self, session: u64) -> IdleCandidate {
        let index = self.until(session, |e| matches!(e, Event::IdleCandidate(_)));
        match &self.events[index].1 {
            Event::IdleCandidate(candidate) => candidate.clone(),
            _ => unreachable!(),
        }
    }

    /// Evicts `session` and waits until its process and everything it started are gone.
    fn evict(&mut self, session: u64, candidate: IdleCandidate, dir: &Path) {
        let pid = self.conn.pid(session).unwrap();
        // The folder lists the child of every scripted Claude run there, such as a
        // settings probe's; only this process's own count here.
        let own: Vec<String> = lines(dir, "children")
            .into_iter()
            .filter(|child| parent(child.parse().unwrap()) == Some(pid))
            .collect();
        self.conn.evict_idle(session, candidate);
        self.until(session, |e| *e == Event::ModelLost);
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.conn.pid(session).is_some() || alive(pid) {
            assert!(Instant::now() < deadline, "{pid} survived eviction");
            std::thread::sleep(Duration::from_millis(5));
        }
        for child in own {
            while alive(child.parse().unwrap()) {
                assert!(Instant::now() < deadline, "child {child} survived");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn parent(pid: u32) -> Option<u32> {
    keystone_agent::host::process_parent(pid)
}

impl Rig {
    /// Has the scripted Claude in `dir` report a usage reading after everything it wrote
    /// so far, and waits until the connection read it, and with it all of that.
    fn settle(&mut self, session: u64, dir: &Path) {
        inject(
            dir,
            &[
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","utilization":0.1,"resetsAt":1800000000},"session_id":"$SESSION"}"#,
            ],
        );
        self.until(session, |e| matches!(e, Event::Usage(_)));
    }

    /// How many times `session` was offered so far.
    fn candidates(&mut self, session: u64) -> usize {
        std::thread::sleep(Duration::from_millis(150));
        self.events.extend(self.conn.drain());
        self.events
            .iter()
            .filter(|(s, e)| *s == session && matches!(e, Event::IdleCandidate(_)))
            .count()
    }
}

/// Has the scripted Claude in `dir` write `lines` now, `$SESSION` naming its session,
/// and waits until it took them.
fn inject(dir: &Path, lines: &[&str]) {
    if lines.is_empty() {
        return;
    }
    let staged = dir.join(".fake-claude-emit.staged");
    std::fs::write(&staged, lines.join("\n")).unwrap();
    std::fs::rename(&staged, dir.join(".fake-claude-emit")).unwrap();
    wait_for(|| !dir.join(".fake-claude-emit").exists(), "the emit file");
}

fn wait_for(done: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn alive(pid: u32) -> bool {
    !matches!(keystone_agent::host::process_state(pid), None | Some('Z'))
}

fn lines(dir: &Path, file: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(format!(".fake-claude-{file}")))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn argv(dir: &Path) -> Vec<Vec<String>> {
    lines(dir, "argv")
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|args: &Vec<String>| !args.iter().any(|a| a == "--version"))
        .collect()
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1).map(String::as_str)
}

/// No idle eviction produces a turn end, a failure or a recovery request.
fn no_failure(events: &[(u64, Event)]) {
    assert!(
        !events.iter().any(|(_, e)| matches!(
            e,
            Event::Failed { .. }
                | Event::RecoveryNeeded
                | Event::TurnCompleted {
                    status: TurnStatus::Failed | TurnStatus::Interrupted,
                    ..
                }
        )),
        "{events:?}"
    );
}

#[test]
fn a_proven_idle_process_ends_after_the_full_interval_and_resumes_on_the_next_send() {
    let mut rig = rig(true);
    let dir = rig.dir(&[("child", "1")]);
    rig.send(1, &dir, Some("fake-haiku"), ASK, "hello");
    rig.completed(1);
    rig.residency(1, "armed");
    let first = rig.conn.pid(1).unwrap();
    assert!(
        lines(&dir, "children")
            .iter()
            .any(|child| parent(child.parse().unwrap()) == Some(first)),
        "the process has its own child"
    );
    let thread = rig
        .events
        .iter()
        .find_map(|(_, e)| match e {
            Event::ThreadReady { thread_id } => Some(thread_id.clone()),
            _ => None,
        })
        .unwrap();

    rig.clock.advance(TEN_MINUTES - Duration::from_secs(1));
    assert_eq!(rig.offered(1), None, "no eviction before ten minutes");
    assert!(alive(first));
    rig.clock.advance(Duration::from_secs(2));
    let candidate = rig.candidate(1);
    assert_eq!(candidate.thread, thread);
    let ended = rig.events.len();
    rig.evict(1, candidate, &dir);
    no_failure(&rig.events[ended..]);
    assert!(!rig.conn.is_running(1));
    assert_eq!(
        rig.conn.residency(1),
        None,
        "dormant: no process, no driver"
    );
    assert!(
        !rig.events[ended..]
            .iter()
            .any(|(_, e)| matches!(e, Event::ModelReported { .. }))
    );

    // The next message starts a new process on the same conversation, directory,
    // approval mode and requested model, and is written exactly once.
    let resumed = rig.events.len();
    rig.send(1, &dir, Some("fake-haiku"), ASK, "again");
    rig.completed(1);
    let second = rig.conn.pid(1).unwrap();
    assert_ne!(first, second);
    let launches = argv(&dir);
    assert_eq!(launches.len(), 2, "{launches:?}");
    let last = &launches[1];
    assert_eq!(flag(last, "--resume"), Some(thread.as_str()));
    assert_eq!(flag(last, "--model"), Some("fake-haiku"));
    assert_eq!(flag(last, "--permission-mode"), Some("default"));
    assert_eq!(lines(&dir, "messages"), [r#""hello""#, r#""again""#]);
    // The model is reported again only by the new process's `init`.
    let reported = rig.events[resumed..]
        .iter()
        .position(|(_, e)| matches!(e, Event::ModelReported { model } if model == "fake-haiku"))
        .unwrap();
    let started = rig.events[resumed..]
        .iter()
        .position(|(_, e)| *e == Event::TurnStarted)
        .unwrap();
    assert!(started < reported);
}

#[test]
fn activity_starts_the_interval_over() {
    let mut rig = rig(true);
    let dir = rig.dir(&[]);
    rig.send(1, &dir, None, PROVIDER, "one");
    rig.completed(1);
    rig.residency(1, "armed");
    rig.clock.advance(Duration::from_secs(500));
    rig.send(1, &dir, None, PROVIDER, "two");
    rig.completed(1);
    rig.residency(1, "armed");
    rig.clock.advance(Duration::from_secs(500));
    assert_eq!(rig.offered(1), None, "the first interval no longer counts");
    rig.clock.advance(Duration::from_secs(101));
    rig.candidate(1);
}

/// Work Claude reports outlives the turn: a background or ambient task, or a hook.
/// Each keeps the process running however long it takes, and once it ends the
/// process ends only after a new full interval.
#[test]
fn background_work_keeps_the_process_running_and_its_end_starts_a_full_interval() {
    for (prompt, release, reason) in [
        ("start a background task", "background", "background task"),
        ("start an ambient task", "ambient", "background task"),
        ("hook please", "hook", "hook"),
    ] {
        let mut rig = rig(true);
        let dir = rig.dir(&[]);
        rig.send(1, &dir, None, PROVIDER, prompt);
        rig.completed(1);
        rig.residency(1, reason);
        rig.clock.advance(Duration::from_secs(3600));
        assert_eq!(rig.offered(1), None, "{prompt}");
        let pid = rig.conn.pid(1).unwrap();
        assert!(alive(pid));
        std::fs::write(dir.join(format!(".fake-claude-release-{release}")), "").unwrap();
        rig.residency(1, "armed");
        rig.clock.advance(TEN_MINUTES - Duration::from_secs(1));
        assert_eq!(rig.offered(1), None, "{prompt}");
        rig.clock.advance(Duration::from_secs(2));
        let candidate = rig.candidate(1);
        rig.evict(1, candidate, &dir);
    }
}

/// An approval waiting for the user, a tool call still running, and a reply still
/// streaming all keep the process, in one session while another is evicted.
#[test]
fn one_idle_session_ends_while_others_run_or_wait_for_approval() {
    let mut rig = rig(true);
    let idle = rig.dir(&[]);
    let approval = rig.dir(&[]);
    let tool = rig.dir(&[]);
    let streaming = rig.dir(&[("delay-ms", "200")]);
    rig.send(1, &idle, None, PROVIDER, "hello");
    rig.completed(1);
    rig.send(2, &approval, None, ASK, "run it");
    rig.until(2, |e| matches!(e, Event::ApprovalRequested(_)));
    rig.send(3, &tool, None, PROVIDER, "hold tool");
    rig.until(3, |e| matches!(e, Event::ItemStarted(_)));
    rig.send(4, &streaming, None, PROVIDER, "long");
    rig.until(4, |e| matches!(e, Event::ItemDelta { .. }));
    rig.residency(1, "armed");
    for session in [2, 3, 4] {
        rig.residency(session, "a turn is running");
    }
    rig.clock.advance(Duration::from_secs(3600));
    let candidate = rig.candidate(1);
    rig.evict(1, candidate, &idle);
    for session in [2, 3, 4] {
        assert_eq!(rig.offered(session), None);
        assert!(alive(rig.conn.pid(session).unwrap()));
    }
    assert!(rig.conn.is_running(2) && rig.conn.is_running(3));
}

/// A check Claude cannot pass, or does not answer, leaves the process running with
/// its reason until something happens, with no retry on a timer.
#[test]
fn an_unproven_check_leaves_the_process_running_until_activity() {
    for (quiesce, reason) in [
        ("missing", "did not report its pending approvals"),
        ("running", "reports the session running"),
        ("no-snapshot", "did not answer the quiescence check in time"),
    ] {
        let mut rig = rig(true);
        let dir = rig.dir(&[("quiesce", quiesce)]);
        rig.send(1, &dir, None, PROVIDER, "hello");
        rig.completed(1);
        // The check's timer starts once its request is written, which the test cannot
        // see, so time moves until it runs out.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !rig
            .conn
            .residency(1)
            .is_some_and(|now| now.contains(reason))
        {
            assert!(
                Instant::now() < deadline,
                "{quiesce}: {:?}",
                rig.conn.residency(1)
            );
            rig.clock.advance(Duration::from_millis(500));
            std::thread::sleep(Duration::from_millis(20));
        }
        rig.clock.advance(Duration::from_secs(3600));
        assert_eq!(rig.offered(1), None, "{quiesce}");
        rig.residency(1, reason);
        assert!(alive(rig.conn.pid(1).unwrap()));
    }
}

#[test]
fn idle_eviction_is_off_by_default() {
    let mut rig = rig(false);
    let dir = rig.dir(&[]);
    rig.send(1, &dir, None, PROVIDER, "hello");
    rig.completed(1);
    rig.clock.advance(Duration::from_secs(36_000));
    assert_eq!(rig.offered(1), None);
    assert!(
        rig.conn
            .residency(1)
            .unwrap()
            .contains("no Claude profile has a proven quiescence contract")
    );
}

/// A candidate made stale by a message, or by a new approval mode, ends nothing.
#[test]
fn a_stale_candidate_ends_nothing() {
    for change in ["send", "mode"] {
        let mut rig = rig(true);
        let dir = rig.dir(&[]);
        rig.send(1, &dir, None, PROVIDER, "hello");
        rig.completed(1);
        rig.residency(1, "armed");
        rig.clock.advance(TEN_MINUTES);
        let candidate = rig.candidate(1);
        let pid = rig.conn.pid(1).unwrap();
        match change {
            "send" => rig.send(1, &dir, None, PROVIDER, "next"),
            _ => rig.conn.set_approval(1, ASK),
        }
        rig.conn.evict_idle(1, candidate);
        if change == "send" {
            rig.completed(1);
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(rig.conn.pid(1), Some(pid), "{change}");
        assert!(!rig.events.iter().any(|(_, e)| *e == Event::ModelLost));
    }
}

/// A message sent as the eviction runs reaches Claude exactly once, in the old
/// process if it came first, else in its successor.
#[test]
fn a_message_racing_the_eviction_is_sent_exactly_once() {
    for _ in 0..5 {
        let mut rig = rig(true);
        let dir = rig.dir(&[]);
        rig.send(1, &dir, None, PROVIDER, "hello");
        rig.completed(1);
        rig.residency(1, "armed");
        rig.clock.advance(TEN_MINUTES);
        let candidate = rig.candidate(1);
        rig.conn.evict_idle(1, candidate);
        rig.send(1, &dir, None, PROVIDER, "race");
        let done = rig.completed(1);
        assert!(matches!(
            rig.events[done].1,
            Event::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ));
        let sent = lines(&dir, "messages");
        assert_eq!(sent, [r#""hello""#, r#""race""#]);
        no_failure(&rig.events);
    }
}

/// Approval IDs are the process's own, so an answer to one from before the eviction
/// does not answer the successor's request.
#[test]
fn an_answer_to_an_approval_from_before_the_eviction_answers_nothing() {
    let mut rig = rig(true);
    let dir = rig.dir(&[]);
    rig.send(1, &dir, None, ASK, "run it");
    let at = rig.until(1, |e| matches!(e, Event::ApprovalRequested(_)));
    let Event::ApprovalRequested(old) = rig.events[at].1.clone() else {
        unreachable!()
    };
    rig.conn.command(Command::Answer {
        session: 1,
        request: old.request.clone(),
        allow: true,
    });
    rig.completed(1);
    rig.residency(1, "armed");
    rig.clock.advance(TEN_MINUTES);
    let candidate = rig.candidate(1);
    rig.evict(1, candidate, &dir);

    rig.send(1, &dir, None, ASK, "run it again");
    let at = rig.until(1, |e| matches!(e, Event::ApprovalRequested(_)));
    let Event::ApprovalRequested(new) = rig.events[at].1.clone() else {
        unreachable!()
    };
    assert_ne!(new.request, old.request);
    let stale: RequestId = old.request;
    rig.conn.command(Command::Answer {
        session: 1,
        request: stale,
        allow: true,
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(rig.conn.is_running(1), "the stale answer did not apply");
    rig.conn.command(Command::Answer {
        session: 1,
        request: new.request,
        allow: true,
    });
    rig.completed(1);
}

/// A session on the provider's default model resumes without `--model`, whatever the
/// evicted process reported running.
#[test]
fn the_provider_default_model_stays_unrequested_on_resume() {
    let mut rig = rig(true);
    let dir = rig.dir(&[]);
    rig.send(1, &dir, None, PROVIDER, "hello");
    rig.until(
        1,
        |e| matches!(e, Event::ModelReported { model } if model == "fake-opus"),
    );
    rig.completed(1);
    rig.residency(1, "armed");
    rig.clock.advance(TEN_MINUTES);
    let candidate = rig.candidate(1);
    rig.evict(1, candidate, &dir);
    rig.send(1, &dir, None, PROVIDER, "again");
    rig.completed(1);
    let launches = argv(&dir);
    assert_eq!(launches.len(), 2);
    assert!(launches.iter().all(|args| flag(args, "--model").is_none()));
    assert!(launches[1].iter().any(|a| a == "--resume"));
}

/// Dropping the connection ends its processes and its scheduler, which holds the
/// clock only while it runs.
#[test]
fn dropping_the_connection_ends_the_scheduler_and_every_process() {
    let mut rig = rig(true);
    // Startup request timers hold the connection's state until they run out.
    rig.conn.set_mode_timeout(Duration::from_millis(500));
    let dir = rig.dir(&[("child", "1")]);
    rig.send(1, &dir, None, PROVIDER, "hello");
    rig.completed(1);
    rig.residency(1, "armed");
    let pid = rig.conn.pid(1).unwrap();
    let Rig { conn, clock, .. } = rig;
    drop(conn);
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) || Arc::strong_count(&clock) > 1 {
        assert!(
            Instant::now() < deadline,
            "process alive {}, clock held {} times",
            alive(pid),
            Arc::strong_count(&clock)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    clock.advance(TEN_MINUTES);
    for child in lines(&dir, "children") {
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(child.parse().unwrap()) {
            assert!(Instant::now() < deadline, "child {child} survived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Where in an eviction something happens: between a quiescence check's answer and
/// its task snapshot, before the interval or at its end, or once the app was offered
/// the process and before it confirms.
#[derive(Clone, Copy, Debug)]
enum Boundary {
    Refresh,
    Confirm,
    Checkpoint,
}

/// What happens there: Claude's own work or output, or a user's command.
#[derive(Clone, Copy, Debug)]
enum Arrival {
    Task,
    Hook,
    LateOutput,
    Unreadable,
    Stop,
    Answer,
    Configure,
}

/// Claude's work, late output, a frame Keystone cannot read, or a command arriving at
/// any boundary of an eviction ends nothing on the proof made before it. The process
/// stays until whatever started has ended, is proven idle again, and has been for a
/// new full interval; only then is it offered and ended.
#[test]
fn anything_at_any_boundary_of_an_eviction_needs_a_new_proof_and_a_full_interval() {
    use Arrival::*;
    for boundary in [Boundary::Refresh, Boundary::Confirm, Boundary::Checkpoint] {
        for arrival in [Task, Hook, LateOutput, Unreadable, Stop, Answer, Configure] {
            let case = format!("{arrival:?} at {boundary:?}");
            let mut rig = rig(true);
            let dir = rig.dir(&[]);
            let hold = || std::fs::write(dir.join(".fake-claude-hold-snapshot"), "").unwrap();
            let holding = || dir.join(".fake-claude-holding-snapshot").exists();
            if let Boundary::Refresh = boundary {
                hold();
            }
            rig.send(1, &dir, None, PROVIDER, "hello");
            rig.completed(1);
            let mut stale = None;
            match boundary {
                Boundary::Refresh => wait_for(holding, &case),
                Boundary::Confirm => {
                    rig.settle(1, &dir);
                    rig.residency(1, "armed");
                    hold();
                    rig.clock.advance(TEN_MINUTES);
                    wait_for(holding, &case);
                }
                Boundary::Checkpoint => {
                    rig.settle(1, &dir);
                    rig.residency(1, "armed");
                    rig.clock.advance(TEN_MINUTES);
                    stale = Some(rig.candidate(1));
                }
            }
            let offers = rig.candidates(1);
            let pid = rig.conn.pid(1).unwrap();

            let (lines, release): (&[&str], &[&str]) = match arrival {
                Task => (
                    &[
                        r#"{"type":"system","subtype":"background_tasks_changed","session_id":"$SESSION","tasks":[{"task_id":"late","task_type":"local_bash","description":"late"}]}"#,
                    ],
                    &[
                        r#"{"type":"system","subtype":"background_tasks_changed","session_id":"$SESSION","tasks":[]}"#,
                    ],
                ),
                Hook => (
                    &[
                        r#"{"type":"system","subtype":"hook_started","hook_id":"late","hook_name":"Stop","hook_event":"Stop","session_id":"$SESSION"}"#,
                    ],
                    &[
                        r#"{"type":"system","subtype":"hook_response","hook_id":"late","outcome":"success","session_id":"$SESSION"}"#,
                    ],
                ),
                LateOutput => (
                    &[
                        r#"{"type":"system","subtype":"task_notification","task_id":"old","status":"completed","summary":"done","session_id":"$SESSION"}"#,
                    ],
                    &[],
                ),
                Unreadable => (
                    &[r#"{"type":"system","subtype":"hook_started","session_id":"$SESSION"}"#],
                    &[],
                ),
                _ => (&[], &[]),
            };
            inject(&dir, lines);
            rig.settle(1, &dir);
            let before = rig.conn.activity(1);
            match arrival {
                Stop => rig.conn.command(Command::Interrupt { session: 1 }),
                Answer => rig.conn.command(Command::Answer {
                    session: 1,
                    request: RequestId("\"gone\"".into()),
                    allow: true,
                }),
                Configure => rig.conn.command(Command::Configure {
                    session: 1,
                    approval: PROVIDER,
                }),
                _ => {}
            }
            if matches!(arrival, Stop | Answer | Configure) {
                wait_for(|| rig.conn.activity(1) > before, &case);
            }
            if let Some(stale) = stale.clone() {
                // The arrival reached the connection before the app confirms.
                wait_for(
                    || rig.conn.residency(1).is_some_and(|now| now != "offered"),
                    &case,
                );
                rig.conn.evict_idle(1, stale);
            } else {
                std::fs::write(dir.join(".fake-claude-release-snapshot"), "").unwrap();
                wait_for(|| !holding(), &case);
            }
            match arrival {
                Hook => {
                    rig.residency(1, "a hook is running");
                    rig.clock.advance(Duration::from_secs(3600));
                    assert_eq!(rig.candidates(1), offers, "{case}");
                }
                Task if stale.is_some() => {
                    rig.residency(1, "a background task is running");
                    rig.clock.advance(Duration::from_secs(3600));
                    assert_eq!(rig.candidates(1), offers, "{case}");
                }
                _ => {}
            }
            assert_eq!(rig.conn.pid(1), Some(pid), "{case}");
            assert!(
                !rig.events.iter().any(|(_, e)| *e == Event::ModelLost),
                "{case}"
            );
            inject(&dir, release);
            rig.settle(1, &dir);

            rig.residency(1, "armed");
            rig.clock.advance(TEN_MINUTES - Duration::from_secs(1));
            assert_eq!(rig.candidates(1), offers, "{case}: before a full interval");
            assert_eq!(rig.conn.pid(1), Some(pid), "{case}");
            rig.clock.advance(Duration::from_secs(2));
            let candidate = rig.candidate(1);
            rig.evict(1, candidate, &dir);
            no_failure(&rig.events);
        }
    }
}

/// A candidate from before an eviction ends nothing once the next message resumed the
/// conversation in a new process.
#[test]
fn a_candidate_from_an_ended_process_ends_nothing_after_resume() {
    let mut rig = rig(true);
    let dir = rig.dir(&[]);
    rig.send(1, &dir, None, PROVIDER, "hello");
    rig.completed(1);
    rig.residency(1, "armed");
    rig.clock.advance(TEN_MINUTES);
    let old = rig.candidate(1);
    rig.evict(1, old.clone(), &dir);
    rig.send(1, &dir, None, PROVIDER, "again");
    rig.completed(1);
    rig.residency(1, "armed");
    let pid = rig.conn.pid(1).unwrap();
    rig.conn.evict_idle(1, old);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(rig.conn.pid(1), Some(pid));
    assert!(alive(pid));
    rig.residency(1, "armed");
}

/// A candidate the app declines keeps its process, which reports the app's reason
/// until something happens; a stale decline changes nothing.
#[test]
fn a_declined_candidate_keeps_its_process_with_the_reason_until_activity() {
    let mut rig = rig(true);
    let dir = rig.dir(&[]);
    rig.send(1, &dir, None, PROVIDER, "hello");
    rig.completed(1);
    rig.residency(1, "armed");
    rig.clock.advance(TEN_MINUTES);
    let candidate = rig.candidate(1);
    let pid = rig.conn.pid(1).unwrap();
    rig.conn
        .decline_idle(1, candidate.clone(), "Keystone kept it: not saved".into());
    rig.residency(1, "Keystone kept it: not saved");
    rig.clock.advance(Duration::from_secs(3600));
    assert_eq!(rig.candidates(1), 1);
    rig.conn.evict_idle(1, candidate.clone());
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        rig.conn.pid(1),
        Some(pid),
        "a declined candidate ends nothing"
    );

    rig.send(1, &dir, None, PROVIDER, "again");
    rig.completed(1);
    rig.residency(1, "armed");
    rig.conn.decline_idle(1, candidate, "stale".into());
    rig.residency(1, "armed");
    rig.clock.advance(TEN_MINUTES);
    let candidate = rig.candidate(1);
    rig.evict(1, candidate, &dir);
}
