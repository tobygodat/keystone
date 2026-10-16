//! The full stack against the scripted Claude: process host, connection, and driver.

use keystone_agent::ApprovalState;
use keystone_agent::{
    Approval, ClaudeConnection, Command, Event, ItemKind, ItemStatus, TurnStatus,
};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

mod gate;
use gate::Gate;
const NO_MODE: ApprovalState = ApprovalState {
    mode: keystone_core::ApprovalMode::Provider,
    overridden: false,
};

fn connect() -> (ClaudeConnection, Receiver<()>) {
    // SAFETY: tests in this file set the same values and run no other code that reads them.
    unsafe {
        std::env::set_var("KEYSTONE_FAKE_CLAUDE_DELAY_MS", "20");
        std::env::set_var("KEYSTONE_FAKE_CLAUDE_LOG_DIR", log_dir());
    }
    let (tx, rx) = channel();
    let wake = Arc::new(move || {
        let _ = tx.send(());
    });
    let program = PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-claude"));
    (ClaudeConnection::new(Some(program), wake), rx)
}

fn log_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fake-claude-answers");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The answers a fake Claude process received, as `applied <id>` or `ignored <id>`.
fn answers(pid: u32) -> Vec<String> {
    std::fs::read_to_string(log_dir().join(format!("{pid}.answers")))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Collects events until `done` matches one, waiting on wake-ups.
fn until(
    conn: &ClaudeConnection,
    wake: &Receiver<()>,
    done: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    loop {
        let batch = conn.drain();
        let finished = batch.iter().any(|(_, e)| done(e));
        events.extend(batch.into_iter().map(|(_, e)| e));
        if finished {
            return events;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(wake.recv_timeout(left).is_ok(), "timed out; got {events:?}");
    }
}

fn completed(event: &Event) -> bool {
    matches!(event, Event::TurnCompleted { .. })
}

fn reply(events: &[Event]) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage => {
            Some(item.text.clone())
        }
        _ => None,
    })
}

#[test]
fn turns_stream_steer_stop_and_ask_for_approval() {
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, NO_MODE, "hello".into()).unwrap();
    let events = until(&conn, &wake, completed);
    assert_eq!(events[0], Event::TurnStarted);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ThreadReady { .. }))
    );
    assert!(events.iter().any(|e| matches!(e, Event::ItemDelta { .. })));
    assert!(reply(&events).unwrap().contains("You wrote: hello"));

    // A message during the turn steers it: the reply is cut short and the steer is
    // answered in the same turn.
    conn.send(1, "/tmp", None, NO_MODE, "long".into()).unwrap();
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    conn.send(1, "/tmp", None, NO_MODE, "change course".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    assert!(reply(&events).unwrap().contains("You wrote: change course"));
    assert_eq!(
        events.iter().filter(|e| completed(e)).count(),
        1,
        "the steer joined the running turn"
    );

    // Stop ends the turn as interrupted.
    conn.send(1, "/tmp", None, NO_MODE, "another long one".into())
        .unwrap();
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    assert!(conn.is_running(1));
    conn.command(Command::Interrupt { session: 1 });
    let events = until(&conn, &wake, completed);
    assert_eq!(
        events.last(),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None,
            stopped: true,
        })
    );

    // A command needs approval; allowing it runs it.
    conn.send(1, "/tmp", None, NO_MODE, "please run it".into())
        .unwrap();
    let events = until(&conn, &wake, |e| matches!(e, Event::ApprovalRequested(_)));
    let Some(Event::ApprovalRequested(approval)) = events.last() else {
        unreachable!()
    };
    assert_eq!(approval.action, "echo scripted");
    conn.command(Command::Answer {
        session: 1,
        request: approval.request.clone(),
        allow: true,
    });
    let events = until(&conn, &wake, completed);
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.text == "scripted"
            && item.kind == ItemKind::Command { command: "echo scripted".into(), exit_code: Some(0) })));

    // Denying it declines the command.
    conn.send(1, "/tmp", None, NO_MODE, "run again".into())
        .unwrap();
    let events = until(&conn, &wake, |e| matches!(e, Event::ApprovalRequested(_)));
    let Some(Event::ApprovalRequested(approval)) = events.last() else {
        unreachable!()
    };
    conn.command(Command::Answer {
        session: 1,
        request: approval.request.clone(),
        allow: false,
    });
    let events = until(&conn, &wake, completed);
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.status == ItemStatus::Declined)));
}

#[test]
fn each_session_has_its_own_process_and_dropping_ends_them() {
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, NO_MODE, "one".into()).unwrap();
    conn.send(2, "/tmp", None, NO_MODE, "two".into()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut done = Vec::new();
    while done.len() < 2 {
        for (session, event) in conn.drain() {
            if completed(&event) {
                done.push(session);
            }
        }
        if done.len() < 2 {
            assert!(wake.recv_timeout(deadline - Instant::now()).is_ok());
        }
    }
    let (a, b) = (conn.pid(1).unwrap(), conn.pid(2).unwrap());
    assert_ne!(a, b);
    drop(conn);
    for pid in [a, b] {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let gone = state.is_empty()
                || state
                    .rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
            if gone {
                break;
            }
            assert!(Instant::now() < deadline, "claude {pid} still running");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn after_an_exit_the_conversation_is_resumed() {
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, NO_MODE, "hello".into()).unwrap();
    let events = until(&conn, &wake, completed);
    let first = events
        .iter()
        .find_map(|e| match e {
            Event::ThreadReady { thread_id } => Some(thread_id.clone()),
            _ => None,
        })
        .unwrap();
    let pid = conn.pid(1).unwrap();
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while conn.pid(1).is_some() {
        assert!(Instant::now() < deadline, "exit not noticed");
        let _ = wake.recv_timeout(Duration::from_millis(100));
        conn.drain();
    }
    // The caller passes no saved ID, as if it never reached its storage.
    conn.send(1, "/tmp", None, NO_MODE, "again".into()).unwrap();
    let events = until(&conn, &wake, completed);
    assert!(
        events.contains(&Event::ThreadReady {
            thread_id: first.clone()
        }),
        "expected a resume of {first}, got {events:?}"
    );
}

#[test]
fn a_missing_conversation_fails_with_a_clear_error() {
    let (conn, wake) = connect();
    conn.send(1, "/tmp", Some("missing"), NO_MODE, "hello".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    let Some(Event::TurnCompleted { status, error, .. }) = events.last() else {
        unreachable!()
    };
    assert_eq!(*status, TurnStatus::Failed);
    assert!(
        error
            .as_deref()
            .unwrap()
            .starts_with("Could not resume this Claude Code conversation"),
        "{error:?}"
    );
    assert!(reply(&events).is_none());
}

fn approval(events: &[Event]) -> Approval {
    events
        .iter()
        .find_map(|e| match e {
            Event::ApprovalRequested(approval) => Some(approval.clone()),
            _ => None,
        })
        .unwrap()
}

fn kill(conn: &ClaudeConnection, wake: &Receiver<()>, session: u64) {
    let pid = conn.pid(session).unwrap();
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while conn.pid(session).is_some() {
        assert!(Instant::now() < deadline, "exit not noticed");
        let _ = wake.recv_timeout(Duration::from_millis(100));
        conn.drain();
    }
}

/// Answers to an approval Stop withdrew, or to one from a process that has since
/// exited, never reach Claude, even when the new process reuses the request's ID.
#[test]
fn stale_answers_after_stop_and_restart_are_not_applied() {
    let (conn, wake) = connect();
    let deny = |request: &keystone_agent::RequestId| {
        conn.command(Command::Answer {
            session: 1,
            request: request.clone(),
            allow: false,
        })
    };

    conn.send(1, "/tmp", None, NO_MODE, "run it".into())
        .unwrap();
    let stopped = approval(&until(&conn, &wake, |e| {
        matches!(e, Event::ApprovalRequested(_))
    }));
    let first = conn.pid(1).unwrap();
    conn.command(Command::Interrupt { session: 1 });
    let events = until(&conn, &wake, completed);
    assert!(events.contains(&Event::TurnCompleted {
        status: TurnStatus::Interrupted,
        error: None,
        stopped: true,
    }));
    deny(&stopped.request);

    conn.send(1, "/tmp", None, NO_MODE, "run again".into())
        .unwrap();
    let orphaned = approval(&until(&conn, &wake, |e| {
        matches!(e, Event::ApprovalRequested(_))
    }));
    kill(&conn, &wake, 1);

    // The new process's first request has the same Claude ID as `stopped` had.
    conn.send(1, "/tmp", None, NO_MODE, "run once more".into())
        .unwrap();
    let current = approval(&until(&conn, &wake, |e| {
        matches!(e, Event::ApprovalRequested(_))
    }));
    let second = conn.pid(1).unwrap();
    deny(&stopped.request);
    deny(&orphaned.request);
    conn.command(Command::Answer {
        session: 1,
        request: current.request,
        allow: true,
    });
    let events = until(&conn, &wake, completed);
    assert!(
        events.iter().any(|e| matches!(e,
            Event::ItemCompleted(item) if item.text == "scripted" && item.status == ItemStatus::Completed)),
        "the stale denials were not applied: {events:?}"
    );
    assert_eq!(answers(first), Vec::<String>::new());
    assert_eq!(answers(second), ["applied fake-request-1"]);
}

/// The fake itself applies an answer only to the request it names.
#[test]
fn the_fake_applies_an_answer_only_to_the_request_it_names() {
    log_dir();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_keystone-fake-claude"))
        .env("KEYSTONE_FAKE_CLAUDE_DELAY_MS", "1")
        .env("KEYSTONE_FAKE_CLAUDE_LOG_DIR", log_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut write = |message: Value| writeln!(stdin, "{message}").unwrap();
    write(json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": "run it"}}));
    let mut next = || serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap();
    let request = loop {
        let message = next();
        if message["type"] == "control_request" {
            break message["request_id"].clone();
        }
    };
    let answer = |id: &Value, behavior: &str| {
        json!({"type": "control_response", "response": {"subtype": "success", "request_id": id,
               "response": {"behavior": behavior, "updatedInput": {}, "message": "no"}}})
    };
    write(answer(&json!("fake-request-0"), "deny"));
    write(answer(&request, "allow"));
    let result = loop {
        let message = next();
        if message["type"] == "user" && message["message"]["content"].is_array() {
            break message["message"]["content"][0].clone();
        }
    };
    drop(stdin);
    let pid = child.id();
    child.wait().unwrap();
    assert_eq!(result["content"], "scripted", "{result}");
    assert_eq!(
        answers(pid),
        ["ignored fake-request-0", "applied fake-request-1"]
    );
}

#[test]
fn a_claude_that_cannot_start_fails_the_session_that_needed_it() {
    let (tx, wake) = channel();
    let conn = ClaudeConnection::new(
        Some(PathBuf::from("/nonexistent/claude")),
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    // Accepted at once: starting happens on the worker.
    conn.send(7, "/tmp", None, NO_MODE, "hello".into()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = conn.drain();
        if !events.is_empty() {
            break events;
        }
        assert!(wake.recv_timeout(deadline - Instant::now()).is_ok());
    };
    assert!(
        matches!(&events[..], [(7, Event::Failed { message })]
            if message.starts_with("Could not start Claude Code at /nonexistent/claude.")),
        "got {events:?}"
    );
    assert!(conn.pid(7).is_none());
    // Nothing is left queued, so the next message is a new turn, not a refused steer.
    conn.send(7, "/tmp", None, NO_MODE, "again".into()).unwrap();
}

/// A message sent while the first one is still starting Claude would steer a turn
/// before Claude says whether it can be steered, so it is refused, at once, and not
/// sent.
#[test]
fn a_steer_before_claude_reports_anything_is_refused() {
    let (conn, wake) = connect();
    let gate = Gate::new(Some(&[1]));
    conn.hold_starts(gate.hook());
    conn.send(1, "/tmp", None, NO_MODE, "hello".into()).unwrap();
    gate.reached(1);
    let started = Instant::now();
    assert_eq!(
        conn.send(1, "/tmp", None, NO_MODE, "change course".into()),
        Err(keystone_agent::claude::STEER_NOT_YET)
    );
    assert!(started.elapsed() < PROMPT, "took {:?}", started.elapsed());
    gate.release(1);
    let events = until(&conn, &wake, completed);
    assert!(
        !events.iter().any(|e| matches!(e,
            Event::ItemCompleted(item) if item.text.contains("change course"))),
        "the refused message was sent: {events:?}"
    );
}

/// How long a call from the UI thread may take while another session starts.
const PROMPT: Duration = Duration::from_millis(100);

/// Collects events until `done` matches one of `session`'s, failing after `within`.
fn session_until(
    conn: &ClaudeConnection,
    wake: &Receiver<()>,
    session: u64,
    within: Duration,
    done: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let deadline = Instant::now() + within;
    let mut events = Vec::new();
    loop {
        let batch: Vec<Event> = conn
            .drain()
            .into_iter()
            .filter(|(s, _)| *s == session)
            .map(|(_, e)| e)
            .collect();
        let finished = batch.iter().any(&done);
        events.extend(batch);
        if finished {
            return events;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            wake.recv_timeout(left).is_ok(),
            "session {session} timed out; got {events:?}"
        );
    }
}

/// Stop and an approval's answer reach a running session while another session's
/// Claude is stuck starting.
#[test]
fn stop_and_answers_reach_a_session_while_another_is_starting() {
    let (conn, wake) = connect();
    let gate = Gate::new(Some(&[2]));
    conn.hold_starts(gate.hook());
    conn.send(1, "/tmp", None, NO_MODE, "long".into()).unwrap();
    session_until(&conn, &wake, 1, Duration::from_secs(5), |e| {
        matches!(e, Event::ItemDelta { .. })
    });
    conn.send(2, "/tmp", None, NO_MODE, "two".into()).unwrap();
    gate.reached(2);

    conn.command(Command::Interrupt { session: 1 });
    let events = session_until(&conn, &wake, 1, Duration::from_secs(2), completed);
    assert_eq!(
        events.last(),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None,
            stopped: true,
        })
    );

    conn.send(1, "/tmp", None, NO_MODE, "please run it".into())
        .unwrap();
    let events = session_until(&conn, &wake, 1, Duration::from_secs(2), |e| {
        matches!(e, Event::ApprovalRequested(_))
    });
    conn.command(Command::Answer {
        session: 1,
        request: approval(&events).request,
        allow: true,
    });
    let events = session_until(&conn, &wake, 1, Duration::from_secs(2), completed);
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.text == "scripted")));

    gate.release(2);
    let events = session_until(&conn, &wake, 2, Duration::from_secs(5), completed);
    assert!(reply(&events).unwrap().contains("You wrote: two"));
}

/// Sending to other sessions, a new turn or a steer, returns at once while another
/// session's Claude is stuck starting, and their turns run.
#[test]
fn sending_returns_at_once_while_another_session_is_starting() {
    let (conn, wake) = connect();
    let gate = Gate::new(Some(&[2]));
    conn.hold_starts(gate.hook());
    conn.send(1, "/tmp", None, NO_MODE, "long".into()).unwrap();
    session_until(&conn, &wake, 1, Duration::from_secs(5), |e| {
        matches!(e, Event::ItemDelta { .. })
    });
    conn.send(2, "/tmp", None, NO_MODE, "two".into()).unwrap();
    gate.reached(2);

    let started = Instant::now();
    conn.send(3, "/tmp", None, NO_MODE, "three".into()).unwrap();
    conn.send(1, "/tmp", None, NO_MODE, "change course".into())
        .unwrap();
    assert!(started.elapsed() < PROMPT, "took {:?}", started.elapsed());

    let events = session_until(&conn, &wake, 3, Duration::from_secs(3), completed);
    assert!(reply(&events).unwrap().contains("You wrote: three"));
    gate.release(2);
}

/// The IDs of fake Claude processes started in `dir`, which has a pid file.
fn started_in(dir: &std::path::Path) -> Vec<u32> {
    std::fs::read_to_string(dir.join(".fake-claude-pids"))
        .unwrap_or_default()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect()
}

/// A directory, with a pid file, that only one test starts Claude in.
fn pid_dir(name: &str) -> PathBuf {
    let dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".fake-claude-pids"), "").unwrap();
    dir
}

/// Dropping the connection gives up on a stuck start after a bounded wait, and the
/// process it finally starts is ended.
#[test]
fn dropping_gives_up_on_a_stuck_start_and_ends_what_it_starts() {
    let dir = pid_dir("held-start-after-drop");
    let (conn, wake) = connect();
    let gate = Gate::new(Some(&[2]));
    conn.hold_starts(gate.hook());
    conn.send(1, "/tmp", None, NO_MODE, "hello".into()).unwrap();
    session_until(&conn, &wake, 1, Duration::from_secs(5), completed);
    let running = conn.pid(1).unwrap();
    conn.send(2, dir.to_str().unwrap(), None, NO_MODE, "two".into())
        .unwrap();
    gate.reached(2);

    let started = Instant::now();
    drop(conn);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "drop took {took:?}");
    gate::gone(running);
    assert_eq!(started_in(&dir), Vec::<u32>::new());

    gate.release(2);
    let deadline = Instant::now() + Duration::from_secs(5);
    let late = loop {
        if let [pid] = started_in(&dir)[..] {
            break pid;
        }
        assert!(Instant::now() < deadline, "the held start never ran");
        std::thread::sleep(Duration::from_millis(20));
    };
    gate::gone(late);
}

/// A start that finishes while the connection is being dropped is waited for, and its
/// process ended before the drop returns.
#[test]
fn dropping_waits_briefly_for_a_start_and_ends_its_process() {
    let dir = pid_dir("held-start-during-drop");
    let (conn, _wake) = connect();
    let gate = Gate::new(Some(&[2]));
    conn.hold_starts(gate.hook());
    conn.send(2, dir.to_str().unwrap(), None, NO_MODE, "two".into())
        .unwrap();
    gate.reached(2);
    let releaser = {
        let gate = gate.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            gate.release(2);
        })
    };
    let started = Instant::now();
    drop(conn);
    let took = started.elapsed();
    releaser.join().unwrap();
    assert!(took < Duration::from_secs(3), "drop took {took:?}");
    let [late] = started_in(&dir)[..] else {
        panic!("expected one process, got {:?}", started_in(&dir));
    };
    gate::gone(late);
}

fn mode(mode: keystone_core::ApprovalMode) -> ApprovalState {
    ApprovalState {
        mode,
        overridden: true,
    }
}

/// The permission mode the scripted Claude reports in its reply.
fn mode_of(events: &[Event]) -> String {
    let reply = reply(events).expect("a reply");
    let start = reply.find("**Mode:** `").expect("a mode") + "**Mode:** `".len();
    reply[start..].split('`').next().unwrap().to_owned()
}

/// Each mode reaches Claude. A process changes mode in place where Claude allows it, and
/// is replaced, resuming its conversation, where Claude does not: entering or leaving
/// Full access, and returning to the user's own settings.
#[test]
fn approval_modes_reach_claude_and_restart_it_only_when_needed() {
    use keystone_core::ApprovalMode::*;
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, mode(Auto), "first".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    assert_eq!(mode_of(&events), "acceptEdits");
    let pid = conn.pid(1).unwrap();
    let thread = events.iter().find_map(|e| match e {
        Event::ThreadReady { thread_id } => Some(thread_id.clone()),
        _ => None,
    });

    conn.set_approval(1, mode(ReadOnly));
    conn.send(1, "/tmp", None, mode(ReadOnly), "second".into())
        .unwrap();
    assert_eq!(mode_of(&until(&conn, &wake, completed)), "plan");
    assert_eq!(conn.pid(1), Some(pid), "changed in place");

    // Full access runs the command without asking, in a new process.
    conn.send(1, "/tmp", None, mode(FullAccess), "run it".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ApprovalRequested(_)))
    );
    assert_eq!(mode_of(&events), "bypassPermissions");
    let full = conn.pid(1).unwrap();
    assert_ne!(full, pid);
    assert!(events.contains(&Event::ThreadReady {
        thread_id: thread.clone().unwrap()
    }));

    // Back to the user's own settings: a process launched with no mode.
    conn.send(1, "/tmp", None, mode(Provider), "run again".into())
        .unwrap();
    let events = until(&conn, &wake, |e| matches!(e, Event::ApprovalRequested(_)));
    assert_ne!(conn.pid(1), Some(full));
    conn.command(keystone_agent::Command::Interrupt { session: 1 });
    until(&conn, &wake, completed);
    assert!(events.contains(&Event::ThreadReady {
        thread_id: thread.unwrap()
    }));
    conn.send(1, "/tmp", None, mode(Provider), "last".into())
        .unwrap();
    assert_eq!(mode_of(&until(&conn, &wake, completed)), "default");
}

/// A change while a turn runs applies from the next turn, and a message sent before the
/// change but still waiting runs under the new mode. The turn is held at its approval,
/// and Claude's slow mode answer keeps the next message waiting.
#[test]
fn a_mode_change_during_a_turn_applies_to_the_next_one() {
    use keystone_core::ApprovalMode::*;
    let dir = project("mode-change", &[("hold-mode-ms", "300")]);
    let (conn, wake) = connect();
    conn.send(1, &dir, None, mode(Provider), "run it".into())
        .unwrap();
    let events = until(&conn, &wake, |e| matches!(e, Event::ApprovalRequested(_)));
    assert!(conn.is_running(1));
    conn.set_approval(1, mode(Ask));
    assert!(conn.is_running(1), "the change did not end the turn");
    conn.command(Command::Answer {
        session: 1,
        request: approval(&events).request,
        allow: true,
    });
    let events = until(&conn, &wake, completed);
    assert!(reply(&events).unwrap().contains("You wrote: run it"));
    assert_eq!(mode_of(&events), "default");
    conn.set_approval(1, mode(Auto));
    conn.send(1, &dir, None, mode(Auto), "next".into()).unwrap();
    conn.set_approval(1, mode(ReadOnly));
    let events = until(&conn, &wake, completed);
    assert!(reply(&events).unwrap().contains("You wrote: next"));
    assert_eq!(mode_of(&events), "plan");
}

/// While a message waits to start a turn in a new process, a second one cannot steer
/// yet: the new process has not said whether it reports lifecycle frames.
#[test]
fn a_message_behind_a_restart_is_refused_until_claude_reports() {
    use keystone_core::ApprovalMode::*;
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, mode(Provider), "hello".into())
        .unwrap();
    until(&conn, &wake, completed);
    conn.send(1, "/tmp", None, mode(FullAccess), "restarted".into())
        .unwrap();
    assert_eq!(
        conn.send(1, "/tmp", None, mode(FullAccess), "steer".into()),
        Err(keystone_agent::claude::STEER_NOT_YET)
    );
    let events = until(&conn, &wake, completed);
    assert!(reply(&events).unwrap().contains("You wrote: restarted"));
    assert_eq!(mode_of(&events), "bypassPermissions");
    assert!(
        !events.iter().any(|e| matches!(e,
            Event::ItemCompleted(item) if item.text.contains("steer"))),
        "the refused message was sent: {events:?}"
    );
}

/// A directory only one test starts Claude in, with the scripted Claude's settings
/// for it, such as `("user-mode", "acceptEdits")`.
fn project(name: &str, settings: &[(&str, &str)]) -> String {
    let dir = pid_dir(name);
    for (setting, value) in settings {
        std::fs::write(dir.join(format!(".fake-claude-{setting}")), value).unwrap();
    }
    dir.to_str().unwrap().to_owned()
}

/// Review round 2, finding 1. A message sent in Full access, whose process is still
/// starting when Read only is chosen, runs in Read only: nothing is written before
/// Claude confirms its mode, and the Full access process is replaced before that.
#[test]
fn a_mode_chosen_while_claude_starts_is_the_one_the_message_runs_in() {
    use keystone_core::ApprovalMode::*;
    let dir = project("spawn-race", &[]);
    let (conn, wake) = connect();
    let gate = Gate::new(Some(&[1]));
    conn.hold_starts(gate.hook());
    conn.send(1, &dir, None, mode(FullAccess), "spawn race".into())
        .unwrap();
    gate.reached(1);
    conn.set_approval(1, mode(ReadOnly));
    gate.release(1);
    let events = until(&conn, &wake, completed);
    assert_eq!(mode_of(&events), "plan");
    assert_eq!(
        events.iter().filter(|e| **e == Event::TurnStarted).count(),
        1,
        "one turn, carried to the new process"
    );
    assert_eq!(started_in(std::path::Path::new(&dir)).len(), 2);
}

/// Claude keeps plan mode across a `--resume` with no mode given. A mode Keystone
/// selected is passed on every resume, and Provider settings after an override pass
/// the user's own mode, read from a fresh process.
#[test]
fn a_resumed_claude_runs_in_the_selected_mode_not_the_one_it_left() {
    use keystone_core::ApprovalMode::*;
    let dir = project("resume-modes", &[("user-mode", "acceptEdits")]);
    let (conn, wake) = connect();
    conn.send(1, &dir, None, mode(ReadOnly), "first".into())
        .unwrap();
    assert_eq!(mode_of(&until(&conn, &wake, completed)), "plan");
    kill(&conn, &wake, 1);
    conn.send(1, &dir, None, mode(ReadOnly), "second".into())
        .unwrap();
    assert_eq!(mode_of(&until(&conn, &wake, completed)), "plan");
    conn.send(1, &dir, None, mode(Provider), "third".into())
        .unwrap();
    assert_eq!(mode_of(&until(&conn, &wake, completed)), "acceptEdits");
}

/// Review round 2, finding 3. Approving `ExitPlanMode` returns Claude to its mode
/// before plan mode, which may write without asking, so Read only refuses it and
/// says so; the user is not asked.
#[test]
fn read_only_refuses_to_leave_plan_mode() {
    use keystone_core::ApprovalMode::*;
    let dir = project("exit-plan", &[]);
    let (conn, wake) = connect();
    conn.send(
        1,
        &dir,
        None,
        mode(ReadOnly),
        "exit plan, then write".into(),
    )
    .unwrap();
    let events = until(&conn, &wake, completed);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ApprovalRequested(_)))
    );
    assert!(events.iter().any(
        |e| matches!(e, Event::Warning { message } if message.contains("Read only keeps it"))
    ));
    assert!(reply(&events).unwrap().contains("Stayed in plan mode"));
    assert_eq!(mode_of(&events), "plan");
}

/// Review round 2, finding 5. Stop while Claude has not answered a mode change leaves
/// nothing behind: the next message waits for that answer, sets its own mode, and runs
/// in it.
#[test]
fn stop_before_claude_confirms_a_mode_leaves_the_next_message_free() {
    use keystone_core::ApprovalMode::*;
    let dir = project("stop-mode", &[("hold-mode-ms", "600")]);
    let (conn, wake) = connect();
    conn.send(1, &dir, None, mode(Auto), "hello".into())
        .unwrap();
    assert_eq!(mode_of(&until(&conn, &wake, completed)), "acceptEdits");
    conn.send(1, &dir, None, mode(ReadOnly), "cancel this".into())
        .unwrap();
    conn.command(Command::Interrupt { session: 1 });
    let events = until(&conn, &wake, completed);
    assert_eq!(
        events.last(),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None,
            stopped: true,
        })
    );
    conn.send(1, &dir, None, mode(Auto), "after Stop".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    assert!(reply(&events).unwrap().contains("after Stop"));
    assert_eq!(mode_of(&events), "acceptEdits");
}

/// A mode change Claude never confirms fails the turn with nothing sent, and the next
/// message runs in a new process.
#[test]
fn an_unconfirmed_mode_fails_the_turn_and_the_next_message_replaces_claude() {
    use keystone_core::ApprovalMode::*;
    let dir = project("mode-timeout", &[("hold-mode-ms", "4000")]);
    let (conn, wake) = connect();
    conn.set_mode_timeout(Duration::from_millis(300));
    conn.send(1, &dir, None, NO_MODE, "hello".into()).unwrap();
    assert_eq!(mode_of(&until(&conn, &wake, completed)), "default");
    let pid = conn.pid(1).unwrap();
    conn.send(1, &dir, None, mode(Auto), "never sent".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    assert!(
        matches!(events.last(), Some(Event::TurnCompleted { status: TurnStatus::Failed, error: Some(error), stopped: false })
        if error.contains("did not confirm the approval mode"))
    );
    assert_eq!(reply(&events), None);
    std::fs::remove_file(PathBuf::from(&dir).join(".fake-claude-hold-mode-ms")).unwrap();
    conn.send(1, &dir, None, mode(Auto), "again".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    assert_eq!(mode_of(&events), "acceptEdits");
    assert!(reply(&events).unwrap().contains("again"));
    assert_ne!(conn.pid(1), Some(pid));
}

/// Review round 3, finding 1. A Claude that never answers `initialize` fails the turn
/// at the deadline, and the next message starts a new process.
#[test]
fn an_unanswered_initialize_fails_the_turn_and_the_next_message_replaces_claude() {
    let dir = project("init-timeout", &[("ignore-initialize", "all")]);
    let (conn, wake) = connect();
    conn.set_mode_timeout(Duration::from_millis(300));
    conn.send(1, &dir, None, NO_MODE, "never sent".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    assert!(
        matches!(events.last(), Some(Event::TurnCompleted { status: TurnStatus::Failed, error: Some(error), stopped: false })
        if error.contains("did not start")),
        "{events:?}"
    );
    let pid = conn.pid(1).unwrap();
    std::fs::remove_file(PathBuf::from(&dir).join(".fake-claude-ignore-initialize")).unwrap();
    conn.send(1, &dir, None, NO_MODE, "again".into()).unwrap();
    let events = until(&conn, &wake, completed);
    assert!(reply(&events).unwrap().contains("again"));
    assert_ne!(conn.pid(1), Some(pid));
    gate::gone(pid);
}

/// Review round 3, finding 2. The process that reads the user's own mode belongs to
/// the connection: dropping the connection ends it, even while it hangs.
#[test]
fn dropping_ends_a_hung_settings_probe() {
    use keystone_core::ApprovalMode::*;
    let dir = project("probe-hang", &[("ignore-initialize", "probe")]);
    let (conn, wake) = connect();
    conn.send(1, &dir, None, mode(ReadOnly), "first".into())
        .unwrap();
    until(&conn, &wake, completed);
    kill(&conn, &wake, 1);
    conn.send(1, &dir, None, mode(Provider), "second".into())
        .unwrap();
    let path = std::path::Path::new(&dir);
    let deadline = Instant::now() + Duration::from_secs(5);
    let probe = loop {
        if let [_, probe] = started_in(path)[..] {
            break probe;
        }
        assert!(Instant::now() < deadline, "the probe never started");
        std::thread::sleep(Duration::from_millis(20));
    };
    // Let the probe receive its request.
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    drop(conn);
    assert!(started.elapsed() < Duration::from_secs(3));
    gate::gone(probe);
    assert_eq!(started_in(path).len(), 2, "nothing started after the drop");
}
