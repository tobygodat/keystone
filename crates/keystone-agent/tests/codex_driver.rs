//! The Codex driver against a recording of real codex-cli 0.162.1 traffic for one
//! thread: a plain reply, a command approval, and an interrupted turn.

use keystone_agent::codex::{CodexDriver, Command, Outputs};
use keystone_agent::{Event, ItemKind, ItemStatus, RequestId, TurnStatus};
use serde_json::Value;

const THREAD: &str = "01a1418d-549d-7553-bd08-4183fac44aa7";
const SESSION: u64 = 7;

fn fixture() -> Vec<String> {
    include_str!("fixtures/codex-0.162.1-thread.jsonl")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A driver with its handshake done and the session's thread open.
fn opened() -> CodexDriver {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    let init: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(init["method"], "initialize");
    assert!(
        init["params"]["capabilities"]["optOutNotificationMethods"]
            .as_array()
            .unwrap()
            .contains(&"account/rateLimits/updated".into())
    );

    // Commands before the handshake completes wait for it.
    let mut out = Outputs::default();
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    assert!(out.writes.is_empty());
    driver.on_line(br#"{"id":1,"result":{"userAgent":"test"}}"#, &mut out);
    assert_eq!(out.writes[0], r#"{"method":"initialized"}"#);
    let start: Value = serde_json::from_str(&out.writes[1]).unwrap();
    assert_eq!(
        (start["id"].as_u64(), start["method"].as_str()),
        (Some(2), Some("thread/start"))
    );

    let mut out = Outputs::default();
    let response = format!(r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}"}}}}}}"#);
    driver.on_line(response.as_bytes(), &mut out);
    assert_eq!(
        out.events,
        vec![(
            SESSION,
            Event::ThreadReady {
                thread_id: THREAD.into()
            }
        )]
    );
    driver
}

fn feed(driver: &mut CodexDriver, lines: &[String]) -> Outputs {
    let mut out = Outputs::default();
    for line in lines {
        driver.on_line(line.as_bytes(), &mut out);
    }
    out
}

#[test]
fn recorded_thread_produces_turns_items_and_an_approval() {
    let mut driver = opened();
    let lines = fixture();
    let approval_at = lines
        .iter()
        .position(|l| l.contains("requestApproval"))
        .unwrap();

    let out = feed(&mut driver, &lines[..=approval_at]);
    let events: Vec<Event> = out
        .events
        .into_iter()
        .map(|(s, e)| {
            assert_eq!(s, SESSION);
            e
        })
        .collect();

    // The first turn: the streamed text matches the completed message.
    let streamed: String = events
        .iter()
        .take_while(|e| !matches!(e, Event::TurnCompleted { .. }))
        .filter_map(|e| match e {
            Event::ItemDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, "ALPHA");
    assert!(
        events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage && item.text == "ALPHA"))
    );
    assert!(events.contains(&Event::TurnCompleted {
        status: TurnStatus::Completed,
        error: None
    }));

    // The second turn asks to run a command. Codex offered no plain "decline", so a
    // denial would cancel the turn; allowing echoes the request ID.
    let Some(Event::ApprovalRequested(approval)) = events.last() else {
        panic!("expected an approval request, got {:?}", events.last());
    };
    assert_eq!(approval.action, "/usr/bin/bash -lc 'touch spike.txt'");
    assert_eq!(approval.detail.as_deref(), Some("/tmp/repo"));
    assert_eq!(approval.request, RequestId("0".into()));
    let mut out = Outputs::default();
    driver.command(
        Command::Answer {
            session: SESSION,
            request: approval.request.clone(),
            allow: true,
        },
        &mut out,
    );
    assert_eq!(
        out.writes,
        vec![r#"{"id":0,"result":{"decision":"accept"}}"#]
    );

    // The rest: the command completes, then the third turn is interrupted.
    let out = feed(&mut driver, &lines[approval_at + 1..]);
    let events: Vec<Event> = out.events.into_iter().map(|(_, e)| e).collect();
    assert!(events.contains(&Event::ApprovalResolved {
        request: RequestId("0".into())
    }));
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if matches!(&item.kind, ItemKind::Command { exit_code: Some(0), .. })
            && item.status == ItemStatus::Completed)));
    assert_eq!(
        events.last(),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None
        })
    );
    assert!(!driver.is_running(SESSION));
    assert_eq!(
        driver.unknown, 0,
        "every recorded message type is understood"
    );
}

#[test]
fn denying_uses_cancel_when_codex_offers_no_decline() {
    let mut driver = opened();
    let lines = fixture();
    let approval_at = lines
        .iter()
        .position(|l| l.contains("requestApproval"))
        .unwrap();
    feed(&mut driver, &lines[..=approval_at]);
    let mut out = Outputs::default();
    driver.command(
        Command::Answer {
            session: SESSION,
            request: RequestId("0".into()),
            allow: false,
        },
        &mut out,
    );
    assert_eq!(
        out.writes,
        vec![r#"{"id":0,"result":{"decision":"cancel"}}"#]
    );
    // A second answer to the same request writes nothing.
    let mut out = Outputs::default();
    driver.command(
        Command::Answer {
            session: SESSION,
            request: RequestId("0".into()),
            allow: true,
        },
        &mut out,
    );
    assert!(out.writes.is_empty());
}

#[test]
fn a_message_during_a_turn_steers_and_falls_back_to_a_new_turn() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "first".into(),
        },
        &mut out,
    );
    let start: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(start["method"], "turn/start");

    let mut out = Outputs::default();
    let started = format!(
        r#"{{"method":"turn/started","params":{{"threadId":"{THREAD}","turn":{{"id":"T1"}}}}}}"#
    );
    driver.on_line(started.as_bytes(), &mut out);
    driver.command(
        Command::Send {
            session: SESSION,
            text: "second".into(),
        },
        &mut out,
    );
    let steer: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(steer["method"], "turn/steer");
    assert_eq!(steer["params"]["expectedTurnId"], "T1");

    // The turn ended before the steer arrived, so the message starts the next turn.
    let mut out = Outputs::default();
    let rejected = format!(
        r#"{{"id":{},"error":{{"code":-32600,"message":"no active turn"}}}}"#,
        steer["id"]
    );
    driver.on_line(rejected.as_bytes(), &mut out);
    let next: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(next["method"], "turn/start");
    assert_eq!(next["params"]["input"][0]["text"], "second");
}

#[test]
fn unknown_server_requests_get_an_error_and_exit_fails_the_running_turn() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.on_line(
        br#"{"id":"r1","method":"mcpServer/elicitation/request","params":{}}"#,
        &mut out,
    );
    assert_eq!(
        out.writes,
        vec![
            r#"{"id":"r1","error":{"code":-32601,"message":"Keystone does not support mcpServer/elicitation/request yet"}}"#
        ]
    );

    driver.command(
        Command::Send {
            session: SESSION,
            text: "hi".into(),
        },
        &mut out,
    );
    let mut out = Outputs::default();
    driver.on_exit("Codex stopped", &mut out);
    assert_eq!(
        out.events,
        vec![(
            SESSION,
            Event::TurnCompleted {
                status: TurnStatus::Failed,
                error: Some("Codex stopped".into())
            }
        )]
    );
    assert!(!driver.is_open(SESSION));
}

#[test]
fn a_thread_that_cannot_resume_starts_fresh() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
    let mut out = Outputs::default();
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: Some("gone".into()),
        },
        &mut out,
    );
    let resume: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(resume["method"], "thread/resume");
    let mut out = Outputs::default();
    driver.on_line(
        br#"{"id":2,"error":{"code":-32600,"message":"thread not found"}}"#,
        &mut out,
    );
    let start: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(start["method"], "thread/start");
}
