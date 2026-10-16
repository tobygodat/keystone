//! The Codex driver against a recording of real codex-cli 0.162.1 traffic for one
//! thread: a plain reply, a command approval, and an interrupted turn.

use keystone_agent::codex::CodexDriver;
use keystone_agent::{Command, Outputs};
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
    // Agent messages carry no status on the wire; a started one is in progress.
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemStarted(item) if item.kind == ItemKind::AgentMessage && item.status == ItemStatus::InProgress)));
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

fn started(turn: &str) -> String {
    format!(
        r#"{{"method":"turn/started","params":{{"threadId":"{THREAD}","turn":{{"id":"{turn}"}}}}}}"#
    )
}

fn completed(turn: &str) -> String {
    format!(
        r#"{{"method":"turn/completed","params":{{"threadId":"{THREAD}","turn":{{"id":"{turn}","status":"completed"}}}}}}"#
    )
}

fn methods(out: &Outputs) -> Vec<String> {
    out.writes
        .iter()
        .map(|w| {
            serde_json::from_str::<Value>(w).unwrap()["method"]
                .as_str()
                .unwrap_or("response")
                .to_owned()
        })
        .collect()
}

#[test]
fn a_message_during_a_turn_steers_and_starts_a_new_turn_if_that_turn_ended() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "first".into(),
        },
        &mut out,
    );
    assert_eq!(methods(&out), ["turn/start"]);

    let mut out = Outputs::default();
    driver.on_line(started("T1").as_bytes(), &mut out);
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

    // Codex reports the turn's end before rejecting a steer for it.
    let mut out = Outputs::default();
    driver.on_line(completed("T1").as_bytes(), &mut out);
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
fn a_steer_rejected_while_the_turn_runs_is_a_warning_and_the_turn_continues() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "first".into(),
        },
        &mut out,
    );
    driver.on_line(started("T1").as_bytes(), &mut out);
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "second".into(),
        },
        &mut out,
    );
    let steer: Value = serde_json::from_str(&out.writes[0]).unwrap();
    let mut out = Outputs::default();
    let rejected = format!(
        r#"{{"id":{},"error":{{"code":-32000,"message":"busy"}}}}"#,
        steer["id"]
    );
    driver.on_line(rejected.as_bytes(), &mut out);
    assert!(out.writes.is_empty());
    assert!(
        matches!(&out.events[..], [(_, Event::Warning { message })] if message.contains("busy"))
    );
    assert!(driver.is_running(SESSION));
}

#[test]
fn a_steer_rejected_after_stop_does_not_start_a_new_turn() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "first".into(),
        },
        &mut out,
    );
    driver.on_line(started("T1").as_bytes(), &mut out);
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "steer".into(),
        },
        &mut out,
    );
    let steer: Value = serde_json::from_str(&out.writes[0]).unwrap();
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    let mut out = Outputs::default();
    let interrupted = format!(
        r#"{{"method":"turn/completed","params":{{"threadId":"{THREAD}","turn":{{"id":"T1","status":"interrupted"}}}}}}"#
    );
    driver.on_line(interrupted.as_bytes(), &mut out);
    let rejected = format!(
        r#"{{"id":{},"error":{{"code":-32600,"message":"no active turn"}}}}"#,
        steer["id"]
    );
    driver.on_line(rejected.as_bytes(), &mut out);
    assert!(
        methods(&out).is_empty(),
        "Stop must not be undone: {:?}",
        out.writes
    );
    assert!(!driver.is_running(SESSION));
}

#[test]
fn messages_and_a_stop_sent_during_startup_are_kept_in_order() {
    // Two messages before the thread exists: the first starts the turn, the second
    // waits for the turn's ID and then steers.
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
    let mut out = Outputs::default();
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    driver.command(
        Command::Send {
            session: SESSION,
            text: "one".into(),
        },
        &mut out,
    );
    driver.command(
        Command::Send {
            session: SESSION,
            text: "two".into(),
        },
        &mut out,
    );
    assert_eq!(methods(&out), ["thread/start"]);
    let mut out = Outputs::default();
    driver.on_line(
        format!(r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}"}}}}}}"#).as_bytes(),
        &mut out,
    );
    assert_eq!(methods(&out), ["turn/start"]);
    // A stop before the turn has an ID is remembered, and a third message is dropped.
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    driver.command(
        Command::Send {
            session: SESSION,
            text: "three".into(),
        },
        &mut out,
    );
    assert_eq!(methods(&out), ["turn/start"]);
    let mut out = Outputs::default();
    driver.on_line(started("T1").as_bytes(), &mut out);
    assert_eq!(
        methods(&out),
        ["turn/interrupt"],
        "the stop goes out once the turn has an ID"
    );
}

#[test]
fn a_stop_while_opening_ends_the_turn_and_drops_the_message() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
    let mut out = Outputs::default();
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    driver.command(
        Command::Send {
            session: SESSION,
            text: "one".into(),
        },
        &mut out,
    );
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    assert_eq!(
        out.events,
        vec![(
            SESSION,
            Event::TurnCompleted {
                status: TurnStatus::Interrupted,
                error: None
            }
        )]
    );
    let mut out = Outputs::default();
    driver.on_line(
        format!(r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}"}}}}}}"#).as_bytes(),
        &mut out,
    );
    assert!(methods(&out).is_empty(), "nothing starts after the stop");
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
fn a_thread_that_cannot_resume_reports_it_instead_of_starting_fresh() {
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
    driver.command(
        Command::Send {
            session: SESSION,
            text: "hi".into(),
        },
        &mut out,
    );
    assert_eq!(methods(&out), ["thread/resume"]);
    let mut out = Outputs::default();
    driver.on_line(
        br#"{"id":2,"error":{"code":-32600,"message":"thread not found"}}"#,
        &mut out,
    );
    assert!(out.writes.is_empty(), "no fresh thread and no turn");
    assert!(matches!(&out.events[..], [(_, Event::Failed { message })]
        if message.contains("Could not resume") && message.contains("thread not found")));
}
