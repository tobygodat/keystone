//! The full stack against the scripted Claude: process host, connection, and driver.

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
    conn.send(1, "/tmp", None, "hello".into()).unwrap();
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
    conn.send(1, "/tmp", None, "long".into()).unwrap();
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    conn.send(1, "/tmp", None, "change course".into()).unwrap();
    let events = until(&conn, &wake, completed);
    assert!(reply(&events).unwrap().contains("You wrote: change course"));
    assert_eq!(
        events.iter().filter(|e| completed(e)).count(),
        1,
        "the steer joined the running turn"
    );

    // Stop ends the turn as interrupted.
    conn.send(1, "/tmp", None, "another long one".into())
        .unwrap();
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    assert!(conn.is_running(1));
    conn.command(Command::Interrupt { session: 1 });
    let events = until(&conn, &wake, completed);
    assert_eq!(
        events.last(),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None
        })
    );

    // A command needs approval; allowing it runs it.
    conn.send(1, "/tmp", None, "please run it".into()).unwrap();
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
    conn.send(1, "/tmp", None, "run again".into()).unwrap();
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
    conn.send(1, "/tmp", None, "one".into()).unwrap();
    conn.send(2, "/tmp", None, "two".into()).unwrap();
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
    conn.send(1, "/tmp", None, "hello".into()).unwrap();
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
    conn.send(1, "/tmp", None, "again".into()).unwrap();
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
    conn.send(1, "/tmp", Some("missing"), "hello".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    let Some(Event::TurnCompleted { status, error }) = events.last() else {
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

    conn.send(1, "/tmp", None, "run it".into()).unwrap();
    let stopped = approval(&until(&conn, &wake, |e| {
        matches!(e, Event::ApprovalRequested(_))
    }));
    let first = conn.pid(1).unwrap();
    conn.command(Command::Interrupt { session: 1 });
    let events = until(&conn, &wake, completed);
    assert!(events.contains(&Event::TurnCompleted {
        status: TurnStatus::Interrupted,
        error: None
    }));
    deny(&stopped.request);

    conn.send(1, "/tmp", None, "run again".into()).unwrap();
    let orphaned = approval(&until(&conn, &wake, |e| {
        matches!(e, Event::ApprovalRequested(_))
    }));
    kill(&conn, &wake, 1);

    // The new process's first request has the same Claude ID as `stopped` had.
    conn.send(1, "/tmp", None, "run once more".into()).unwrap();
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
    conn.send(7, "/tmp", None, "hello".into()).unwrap();
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
    conn.send(7, "/tmp", None, "again".into()).unwrap();
}

/// A message sent while the first one is still starting Claude would steer a turn
/// before Claude says whether it can be steered, so it is refused and not sent.
#[test]
fn a_steer_before_claude_reports_anything_is_refused() {
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, "long".into()).unwrap();
    assert_eq!(
        conn.send(1, "/tmp", None, "change course".into()),
        Err(keystone_agent::claude::STEER_NOT_YET)
    );
    let events = until(&conn, &wake, completed);
    assert!(
        !events.iter().any(|e| matches!(e,
            Event::ItemCompleted(item) if item.text.contains("change course"))),
        "the refused message was sent: {events:?}"
    );
}
