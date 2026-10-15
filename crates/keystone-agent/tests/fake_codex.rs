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
    (CodexConnection::new(program, "0.0.0-test", wake), rx)
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
    })
    .unwrap();
    conn.command(Command::Send {
        session: 1,
        text: "hello".into(),
    })
    .unwrap();
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
    })
    .unwrap();
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    conn.command(Command::Send {
        session: 1,
        text: "change course".into(),
    })
    .unwrap();
    let events = until(&conn, &wake, completed);
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.text == "Steered (test data): change course")));

    // Stop ends the turn as interrupted.
    conn.command(Command::Send {
        session: 1,
        text: "another".into(),
    })
    .unwrap();
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    assert!(conn.is_running(1));
    conn.command(Command::Interrupt { session: 1 }).unwrap();
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
    })
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
    })
    .unwrap();
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
        })
        .unwrap();
        conn.command(Command::Send {
            session,
            text: format!("session {session}"),
        })
        .unwrap();
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
    conn.send(1, "/tmp", None, "hello".into()).unwrap();
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
