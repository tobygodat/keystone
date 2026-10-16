//! The full stack against the scripted app-server: process host, connection, and driver.

use keystone_agent::codex::Command;
use keystone_agent::{CodexConnection, Event, ItemKind, TurnStatus};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

fn connect() -> (CodexConnection, Receiver<()>) {
    // SAFETY: tests in this file set the same value and run no other code that reads it.
    unsafe { std::env::set_var("KEYSTONE_FAKE_CODEX_DELAY_MS", "20") };
    let (tx, rx) = channel();
    let wake = Arc::new(move || {
        let _ = tx.send(());
    });
    let program = PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-codex"));
    (CodexConnection::new(Some(program), "0.0.0-test", wake), rx)
}

/// Collects events until `done` matches one, waiting on wake-ups.
fn until(conn: &CodexConnection, wake: &Receiver<()>, done: impl Fn(&Event) -> bool) -> Vec<Event> {
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

#[test]
fn turns_stream_steer_stop_and_ask_for_approval() {
    let (conn, wake) = connect();
    conn.command(Command::Open {
        session: 1,
        cwd: "/tmp".into(),
        resume: None,
    });
    conn.command(Command::Send {
        session: 1,
        text: "hello".into(),
    });
    let events = until(&conn, &wake, completed);
    assert!(matches!(events[0], Event::ThreadReady { .. }));
    let reply = events.iter().find_map(|e| match e {
        Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage => {
            Some(item.text.clone())
        }
        _ => None,
    });
    assert!(reply.unwrap().contains("You wrote: hello"));

    // A message during the turn steers it.
    conn.command(Command::Send {
        session: 1,
        text: "long".into(),
    });
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    conn.command(Command::Send {
        session: 1,
        text: "change course".into(),
    });
    let events = until(&conn, &wake, completed);
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.text == "Steered (test data): change course")));

    // Stop ends the turn as interrupted.
    conn.command(Command::Send {
        session: 1,
        text: "another".into(),
    });
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
    conn.command(Command::Send {
        session: 1,
        text: "please run it".into(),
    });
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
        Event::ItemCompleted(item) if item.text == "scripted\n")));
}

#[test]
fn two_sessions_share_one_process() {
    let (conn, wake) = connect();
    for session in [1, 2] {
        conn.command(Command::Open {
            session,
            cwd: "/tmp".into(),
            resume: None,
        });
        conn.command(Command::Send {
            session,
            text: format!("session {session}"),
        });
    }
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
    done.sort();
    assert_eq!(done, vec![1, 2]);
}

#[test]
fn dropping_the_connection_stops_codex() {
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, "hello".into());
    until(&conn, &wake, |e| matches!(e, Event::ThreadReady { .. }));
    let pid = conn.pid().expect("app-server running");
    drop(conn);
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        if state
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        {
            break;
        }
        assert!(Instant::now() < deadline, "app-server {pid} still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn after_a_crash_the_latest_thread_is_resumed_even_if_the_caller_has_none() {
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, "hello".into());
    let events = until(&conn, &wake, completed);
    let Some(Event::ThreadReady { thread_id: first }) = events.first() else {
        panic!("expected a thread first, got {events:?}");
    };
    let first = first.clone();
    let pid = conn.pid().unwrap();
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while conn.pid().is_some() {
        assert!(Instant::now() < deadline, "exit not noticed");
        let _ = wake.recv_timeout(Duration::from_millis(100));
        conn.drain();
    }
    // The caller passes no saved thread, as if the ID never reached its storage.
    conn.send(1, "/tmp", None, "again".into());
    let events = until(&conn, &wake, completed);
    assert!(
        events.contains(&Event::ThreadReady {
            thread_id: first.clone()
        }),
        "expected a resume of {first}, got {events:?}"
    );
}

#[test]
fn a_codex_that_cannot_start_fails_the_session_that_needed_it() {
    let (tx, wake) = channel();
    let conn = CodexConnection::new(
        Some(PathBuf::from("/nonexistent/codex")),
        "0.0.0-test",
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    conn.send(7, "/tmp", None, "hello".into());
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
            if message.starts_with("Could not start Codex at /nonexistent/codex.")),
        "got {events:?}"
    );
    assert!(conn.pid().is_none());
}
