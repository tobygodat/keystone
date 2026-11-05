//! The Codex driver against a recording of real codex-cli 0.162.1 traffic for one
//! thread: a plain reply, a command approval, and an interrupted turn.

use keystone_agent::codex::CodexDriver;
use keystone_agent::{ApprovalState, Command, Outputs};
use keystone_agent::{
    Catalog, CommandSource, Commands, Event, ItemKind, ItemStatus, ModelInfo, RequestId,
    SlashCommand, Speed, TurnStatus,
};
use keystone_core::{ApprovalMode, ModelChoice};
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
    let opted_out = init["params"]["capabilities"]["optOutNotificationMethods"]
        .as_array()
        .unwrap();
    assert!(opted_out.contains(&"account/updated".into()));
    // Usage displays read these two.
    assert!(!opted_out.contains(&"account/rateLimits/updated".into()));
    assert!(!opted_out.contains(&"thread/tokenUsage/updated".into()));

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
        error: None,
        stopped: false
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
            error: None,
            stopped: false
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
            images: Vec::new(),
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
            images: Vec::new(),
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

/// A steer that becomes the next turn waits until the app has seen the turn end, so
/// the files the turn left can be read first. A release that came first lets it go.
#[test]
fn a_steer_that_starts_the_next_turn_waits_until_the_turn_end_is_released() {
    for release_first in [false, true] {
        let mut driver = opened();
        driver.hold_follow_ups(SESSION);
        let mut out = Outputs::default();
        let send = |text: &str| Command::Send {
            session: SESSION,
            text: text.into(),
            images: Vec::new(),
        };
        driver.command(send("first"), &mut out);
        driver.on_line(started("T1").as_bytes(), &mut out);
        let mut out = Outputs::default();
        driver.command(send("second"), &mut out);
        let steer: Value = serde_json::from_str(&out.writes[0]).unwrap();
        let mut out = Outputs::default();
        driver.on_line(completed("T1").as_bytes(), &mut out);
        if release_first {
            driver.release_follow_ups(SESSION, &mut out);
        }
        let rejected = format!(
            r#"{{"id":{},"error":{{"code":-32600,"message":"no active turn"}}}}"#,
            steer["id"]
        );
        driver.on_line(rejected.as_bytes(), &mut out);
        if !release_first {
            assert!(out.writes.is_empty(), "held until released");
            assert!(
                driver.is_running(SESSION),
                "a held follow-up counts as running"
            );
            driver.release_follow_ups(SESSION, &mut out);
        }
        let next: Value = serde_json::from_str(&out.writes[0]).unwrap();
        assert_eq!(next["method"], "turn/start");
        assert_eq!(next["params"]["input"][0]["text"], "second");
    }
}

/// Images go as `localImage` items after the text, on a new turn and on a steer, as
/// the TOB-59 probe found Codex 0.163.0 takes them.
#[test]
fn images_are_sent_as_local_image_items_after_the_text() {
    let image = |path: &str| keystone_agent::Image {
        path: path.into(),
        media_type: "image/png".into(),
        base64: None,
    };
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "first".into(),
            images: vec![image("/data/attachments/a.png")],
        },
        &mut out,
    );
    let start: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(
        start["params"]["input"],
        serde_json::json!([
            {"type": "text", "text": "first"},
            {"type": "localImage", "path": "/data/attachments/a.png"},
        ])
    );

    let mut out = Outputs::default();
    driver.on_line(started("T1").as_bytes(), &mut out);
    driver.command(
        Command::Send {
            session: SESSION,
            text: String::new(),
            images: vec![image("/data/attachments/b.png")],
        },
        &mut out,
    );
    let steer: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(steer["method"], "turn/steer");
    assert_eq!(
        steer["params"]["input"],
        serde_json::json!([{"type": "localImage", "path": "/data/attachments/b.png"}])
    );
}

#[test]
fn a_steer_rejected_while_the_turn_runs_is_a_warning_and_the_turn_continues() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "first".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    driver.on_line(started("T1").as_bytes(), &mut out);
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "second".into(),
            images: Vec::new(),
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
            images: Vec::new(),
        },
        &mut out,
    );
    driver.on_line(started("T1").as_bytes(), &mut out);
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "steer".into(),
            images: Vec::new(),
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
            images: Vec::new(),
        },
        &mut out,
    );
    driver.command(
        Command::Send {
            session: SESSION,
            text: "two".into(),
            images: Vec::new(),
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
            images: Vec::new(),
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
            images: Vec::new(),
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
                error: None,
                stopped: true
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
            images: Vec::new(),
        },
        &mut out,
    );
    let mut out = Outputs::default();
    driver.on_exit("Codex stopped", &mut out);
    assert_eq!(
        out.events,
        vec![
            // The store may hold more than streamed before the exit.
            (SESSION, Event::RecoveryNeeded),
            (
                SESSION,
                Event::TurnCompleted {
                    status: TurnStatus::Failed,
                    error: Some("Codex stopped".into()),
                    stopped: false
                }
            ),
            (SESSION, Event::ModelLost),
        ]
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
            images: Vec::new(),
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
    assert!(
        matches!(&out.events[..], [(_, Event::Failed { message }), (_, Event::ModelLost)]
        if message.contains("Could not resume") && message.contains("thread not found"))
    );
}

/// The model a thread reported describes nothing once the app-server is gone, for
/// every session it hosted, idle ones included.
#[test]
fn an_exit_loses_the_model_of_every_session_the_process_hosted() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(choose_model(8, "gpt-6-astra"), &mut out);
    driver.command(
        Command::Open {
            session: 8,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    let mut out = Outputs::default();
    driver.on_exit("Codex stopped", &mut out);
    let mut lost: Vec<u64> = out
        .events
        .iter()
        .filter_map(|(session, e)| (*e == Event::ModelLost).then_some(*session))
        .collect();
    lost.sort_unstable();
    assert_eq!(lost, [SESSION, 8]);
}

fn send(driver: &mut CodexDriver, text: &str) -> Value {
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: text.into(),
            images: Vec::new(),
        },
        &mut out,
    );
    serde_json::from_str(&out.writes[0]).unwrap()
}

fn finish_turn(driver: &mut CodexDriver, turn: &str) {
    feed(driver, &[started(turn), completed(turn)]);
}

fn configure(driver: &mut CodexDriver, mode: ApprovalMode, overridden: bool) {
    driver.command(
        Command::Configure {
            session: SESSION,
            approval: ApprovalState { mode, overridden },
        },
        &mut Outputs::default(),
    );
}

const READ_ONLY: ApprovalMode = ApprovalMode::ReadOnly;

#[test]
fn the_default_sends_no_policy_and_a_mode_applies_from_the_next_turn() {
    let mut driver = opened();
    let first = send(&mut driver, "first");
    assert_eq!(first["method"], "turn/start");
    assert!(first["params"].get("approvalPolicy").is_none());
    assert!(first["params"].get("sandboxPolicy").is_none());

    // Changed while the turn runs: a message would join a turn under the old mode, so
    // it waits and starts the next turn, under the new one.
    feed(&mut driver, &[started("T1")]);
    configure(&mut driver, READ_ONLY, false);
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "steer".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    assert!(
        out.writes.is_empty(),
        "not steered into the old mode's turn"
    );
    let mut out = Outputs::default();
    driver.on_line(completed("T1").as_bytes(), &mut out);
    let next: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(next["method"], "turn/start");
    assert_eq!(next["params"]["input"][0]["text"], "steer");
    assert_eq!(next["params"]["approvalPolicy"], "on-request");
    assert_eq!(next["params"]["sandboxPolicy"]["type"], "readOnly");
    feed(&mut driver, &[started("T1b")]);
    // A steer into a turn under the selected mode goes through.
    assert_eq!(send(&mut driver, "steer 2")["method"], "turn/steer");
    feed(&mut driver, &[completed("T1b")]);

    configure(&mut driver, ApprovalMode::FullAccess, false);
    finish_turn(&mut driver, "T2");
    let full = send(&mut driver, "full");
    assert_eq!(full["params"]["approvalPolicy"], "never");
    assert_eq!(full["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
}

/// A new thread reports the user's settings. Codex keeps an override on the thread, so
/// going back to the default sends those settings with every later turn.
#[test]
fn returning_to_the_default_restores_the_settings_a_new_thread_reported() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    let response = format!(
        r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}"}},"approvalPolicy":"untrusted","sandbox":{{"type":"workspaceWrite","networkAccess":false}}}}}}"#
    );
    feed(&mut driver, &[response]);
    configure(&mut driver, READ_ONLY, false);
    assert_eq!(
        send(&mut driver, "a")["params"]["sandboxPolicy"]["type"],
        "readOnly"
    );
    finish_turn(&mut driver, "T1");

    configure(&mut driver, ApprovalMode::Provider, false);
    let restored = send(&mut driver, "b");
    assert_eq!(restored["params"]["approvalPolicy"], "untrusted");
    assert_eq!(
        restored["params"]["sandboxPolicy"],
        serde_json::json!({"type": "workspaceWrite", "networkAccess": false})
    );
    finish_turn(&mut driver, "T2");
    assert_eq!(
        send(&mut driver, "c")["params"]["approvalPolicy"],
        "untrusted"
    );
}

/// After a restart the resumed thread reports its old override, so the user's settings
/// come from a throwaway thread, and the turn waits for them.
#[test]
fn a_resumed_overridden_thread_waits_for_the_users_settings() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
    let mut out = Outputs::default();
    configure(&mut driver, ApprovalMode::Provider, true);
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: Some(THREAD.into()),
        },
        &mut out,
    );
    driver.command(
        Command::Send {
            session: SESSION,
            text: "hi".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    assert_eq!(methods(&out), ["thread/start", "thread/resume"]);
    let probe: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(probe["params"]["ephemeral"], true);
    assert_eq!(probe["params"]["cwd"], "/tmp/repo");

    // The resume answers first and reports the stale override; nothing is sent yet.
    let resumed = format!(
        r#"{{"id":3,"result":{{"thread":{{"id":"{THREAD}"}},"approvalPolicy":"never","sandbox":{{"type":"dangerFullAccess"}}}}}}"#
    );
    let out = feed(&mut driver, &[resumed]);
    assert!(out.writes.is_empty());
    let out = feed(
        &mut driver,
        &[r#"{"id":2,"result":{"thread":{"id":"probe"},"approvalPolicy":"on-request","sandbox":{"type":"readOnly"}}}"#.into()],
    );
    let turn: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(turn["method"], "turn/start");
    assert_eq!(turn["params"]["approvalPolicy"], "on-request");
    assert_eq!(turn["params"]["sandboxPolicy"]["type"], "readOnly");
    assert_eq!(turn["params"]["input"][0]["text"], "hi");
}

#[test]
fn unreadable_settings_fail_the_message_instead_of_keeping_the_override() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
    configure(&mut driver, ApprovalMode::Provider, true);
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: Some(THREAD.into()),
        },
        &mut out,
    );
    driver.command(
        Command::Send {
            session: SESSION,
            text: "hi".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    feed(
        &mut driver,
        &[format!(
            r#"{{"id":3,"result":{{"thread":{{"id":"{THREAD}"}}}}}}"#
        )],
    );
    let out = feed(
        &mut driver,
        &[r#"{"id":2,"error":{"code":-1,"message":"no"}}"#.into()],
    );
    assert!(out.writes.is_empty());
    assert!(
        matches!(&out.events[..], [(_, Event::Failed { message })] if message.contains("approval settings"))
    );
}

/// A steer Codex rejects because its turn just ended starts the next turn, under the
/// mode chosen since, never the one the ended turn ran under.
#[test]
fn a_steer_retried_as_a_new_turn_uses_the_latest_mode() {
    let mut driver = opened();
    configure(&mut driver, ApprovalMode::FullAccess, false);
    let first = send(&mut driver, "first");
    assert_eq!(first["params"]["approvalPolicy"], "never");
    feed(&mut driver, &[started("T1")]);
    let steer = send(&mut driver, "steer");
    assert_eq!(steer["method"], "turn/steer");
    configure(&mut driver, READ_ONLY, false);
    let id = steer["id"].as_u64().unwrap();
    let out = feed(
        &mut driver,
        &[
            completed("T1"),
            format!(r#"{{"id":{id},"error":{{"code":-32600,"message":"no active turn"}}}}"#),
        ],
    );
    let retried: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(retried["method"], "turn/start");
    assert_eq!(retried["params"]["input"][0]["text"], "steer");
    assert_eq!(retried["params"]["approvalPolicy"], "on-request");
    assert_eq!(retried["params"]["sandboxPolicy"]["type"], "readOnly");
}

#[test]
fn only_full_access_drops_the_sandbox_and_the_default_sends_nothing() {
    use keystone_agent::codex::{ApprovalPolicy, Sandbox, policy};
    assert_eq!(policy(ApprovalMode::Provider), None);
    for mode in ApprovalMode::ALL {
        let unsandboxed = policy(mode).is_some_and(|p| p.sandbox == Sandbox::FullAccess);
        let never_asks = policy(mode).is_some_and(|p| p.approval == ApprovalPolicy::Never);
        assert_eq!(unsandboxed, mode == ApprovalMode::FullAccess, "{mode:?}");
        assert_eq!(never_asks, mode == ApprovalMode::FullAccess, "{mode:?}");
    }
    assert_eq!(
        policy(ApprovalMode::ReadOnly).map(|p| p.sandbox),
        Some(Sandbox::ReadOnly)
    );
}

/// Review round 2, finding 4. A turn under the user's settings that Codex rejects is
/// retried with those settings again, as is every later one: the thread may still hold
/// the override, so nothing is ever sent without them.
#[test]
fn a_rejected_restore_is_retried_with_the_users_settings() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    feed(
        &mut driver,
        &[format!(
            r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}"}},"approvalPolicy":"on-request","sandbox":{{"type":"workspaceWrite"}}}}}}"#
        )],
    );
    configure(&mut driver, ApprovalMode::FullAccess, false);
    assert_eq!(
        send(&mut driver, "full")["params"]["approvalPolicy"],
        "never"
    );
    finish_turn(&mut driver, "T1");

    configure(&mut driver, ApprovalMode::Provider, false);
    let restore = send(&mut driver, "restore");
    assert_eq!(restore["params"]["sandboxPolicy"]["type"], "workspaceWrite");
    let id = restore["id"].as_u64().unwrap();
    feed(
        &mut driver,
        &[format!(
            r#"{{"id":{id},"error":{{"code":-32001,"message":"Server overloaded; retry later."}}}}"#
        )],
    );
    let retry = send(&mut driver, "retry");
    assert_eq!(retry["params"]["approvalPolicy"], "on-request");
    assert_eq!(retry["params"]["sandboxPolicy"]["type"], "workspaceWrite");
}

/// Review round 2, finding 4, after a restart: the user's settings read from a
/// throwaway thread are sent again when the restoring turn fails.
#[test]
fn a_rejected_restore_after_reading_the_settings_is_retried_with_them() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
    configure(&mut driver, ApprovalMode::Provider, true);
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: Some(THREAD.into()),
        },
        &mut out,
    );
    driver.command(
        Command::Send {
            session: SESSION,
            text: "hi".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    feed(
        &mut driver,
        &[format!(
            r#"{{"id":3,"result":{{"thread":{{"id":"{THREAD}"}},"approvalPolicy":"never","sandbox":{{"type":"dangerFullAccess"}}}}}}"#
        )],
    );
    let out = feed(
        &mut driver,
        &[r#"{"id":2,"result":{"thread":{"id":"probe"},"approvalPolicy":"on-request","sandbox":{"type":"readOnly"}}}"#.into()],
    );
    let turn: Value = serde_json::from_str(&out.writes[0]).unwrap();
    let id = turn["id"].as_u64().unwrap();
    feed(
        &mut driver,
        &[format!(
            r#"{{"id":{id},"error":{{"code":-1,"message":"no"}}}}"#
        )],
    );
    let retry = send(&mut driver, "again");
    assert_eq!(retry["params"]["approvalPolicy"], "on-request");
    assert_eq!(retry["params"]["sandboxPolicy"]["type"], "readOnly");
}

fn interrupted(turn: &str) -> String {
    format!(
        r#"{{"method":"turn/completed","params":{{"threadId":"{THREAD}","turn":{{"id":"{turn}","status":"interrupted"}}}}}}"#
    )
}

fn approval(request: u64, decisions: &str) -> String {
    format!(
        r#"{{"id":{request},"method":"item/commandExecution/requestApproval","params":{{"threadId":"{THREAD}","itemId":"exec","command":"touch x","availableDecisions":{decisions}}}}}"#
    )
}

fn answer(driver: &mut CodexDriver, request: u64, allow: bool) -> Vec<String> {
    let mut out = Outputs::default();
    driver.command(
        Command::Answer {
            session: SESSION,
            request: RequestId(request.to_string()),
            allow,
        },
        &mut out,
    );
    out.writes
}

/// Whether the user ended the turn, as the turn's end reports it.
fn stopped(out: &Outputs) -> Option<bool> {
    out.events.iter().find_map(|(_, e)| match e {
        Event::TurnCompleted { stopped, .. } => Some(*stopped),
        _ => None,
    })
}

#[test]
fn a_stop_is_reported_by_its_own_turn_only() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "one".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    feed(&mut driver, &[started("a")]);
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    assert_eq!(stopped(&feed(&mut driver, &[interrupted("a")])), Some(true));

    driver.command(
        Command::Send {
            session: SESSION,
            text: "two".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    feed(&mut driver, &[started("b")]);
    assert_eq!(
        stopped(&feed(&mut driver, &[interrupted("b")])),
        Some(false),
        "an interruption the user did not ask for is not theirs"
    );
}

#[test]
fn a_stop_after_the_turn_already_ended_leaves_that_end_alone() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    assert!(
        out.events.is_empty() && out.writes.is_empty(),
        "nothing to stop"
    );

    driver.command(
        Command::Send {
            session: SESSION,
            text: "one".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    feed(&mut driver, &[started("a")]);
    let failed = format!(
        r#"{{"method":"turn/completed","params":{{"threadId":"{THREAD}","turn":{{"id":"a","status":"failed","error":{{"message":"boom"}}}}}}}}"#
    );
    assert_eq!(stopped(&feed(&mut driver, &[failed])), Some(false));
    // The user pressed Stop before the failure reached them.
    let mut out = Outputs::default();
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    assert!(
        out.events.is_empty() && out.writes.is_empty(),
        "a second end would hide the failure"
    );

    driver.command(
        Command::Send {
            session: SESSION,
            text: "two".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    feed(&mut driver, &[started("b")]);
    assert_eq!(
        stopped(&feed(&mut driver, &[interrupted("b")])),
        Some(false)
    );
}

#[test]
fn only_a_denial_that_cancels_the_turn_ends_it_for_the_user() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "one".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    feed(&mut driver, &[started("a")]);

    // Codex offers "decline": the turn goes on, so a later interruption is not the user's.
    feed(
        &mut driver,
        &[approval(10, r#"["accept","decline","cancel"]"#)],
    );
    assert_eq!(
        answer(&mut driver, 10, false),
        [r#"{"id":10,"result":{"decision":"decline"}}"#]
    );
    feed(
        &mut driver,
        &[approval(11, r#"["accept","decline","cancel"]"#)],
    );
    answer(&mut driver, 11, true);
    assert_eq!(
        stopped(&feed(&mut driver, &[interrupted("a")])),
        Some(false)
    );

    // No "decline" offered: denying cancels the turn, and its interruption is the user's.
    driver.command(
        Command::Send {
            session: SESSION,
            text: "two".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    feed(&mut driver, &[started("b")]);
    feed(&mut driver, &[approval(12, r#"["accept","cancel"]"#)]);
    assert_eq!(
        answer(&mut driver, 12, false),
        [r#"{"id":12,"result":{"decision":"cancel"}}"#]
    );
    assert_eq!(stopped(&feed(&mut driver, &[interrupted("b")])), Some(true));

    // ...and that cancel does not carry over to the next turn.
    driver.command(
        Command::Send {
            session: SESSION,
            text: "three".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    feed(&mut driver, &[started("c")]);
    assert_eq!(
        stopped(&feed(&mut driver, &[interrupted("c")])),
        Some(false)
    );
}

#[test]
fn a_chosen_model_opens_the_thread_with_it_and_the_reply_reports_the_model() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
    let open = |driver: &mut CodexDriver, session, resume: Option<&str>, model: Option<&str>| {
        let mut out = Outputs::default();
        if let Some(model) = model {
            driver.command(choose_model(session, model), &mut out);
        }
        driver.command(
            Command::Open {
                session,
                cwd: "/tmp/repo".into(),
                resume: resume.map(str::to_owned),
            },
            &mut out,
        );
        serde_json::from_str::<Value>(&out.writes[0]).unwrap()
    };
    let start = open(&mut driver, SESSION, None, Some("gpt-6-astra"));
    assert_eq!(start["method"], "thread/start");
    assert_eq!(start["params"]["model"], "gpt-6-astra");
    let resume = open(&mut driver, 8, Some(THREAD), Some("gpt-6-sol"));
    assert_eq!(resume["method"], "thread/resume");
    assert_eq!(resume["params"]["model"], "gpt-6-sol");
    let plain = open(&mut driver, 9, None, None);
    assert!(plain["params"].get("model").is_none(), "{plain}");

    let mut out = Outputs::default();
    let id = start["id"].as_u64().unwrap();
    let reply =
        format!(r#"{{"id":{id},"result":{{"thread":{{"id":"{THREAD}"}},"model":"gpt-6-astra"}}}}"#);
    driver.on_line(reply.as_bytes(), &mut out);
    assert_eq!(
        out.events,
        vec![
            (
                SESSION,
                Event::ThreadReady {
                    thread_id: THREAD.into()
                }
            ),
            (
                SESSION,
                Event::ModelReported {
                    model: "gpt-6-astra".into()
                }
            ),
        ]
    );
}

/// The catalog is every page `model/list` offers, hidden entries left out, each named
/// by its `model` (what a thread is started with), not its `id`; an error on any page
/// fails it; and a cursor that never ends is followed only so far.
#[test]
fn the_model_list_becomes_a_catalog_across_its_pages_without_hidden_entries() {
    let mut driver = opened();
    let ask = |driver: &mut CodexDriver| {
        let mut out = Outputs::default();
        driver.request_catalog(SESSION, &mut out);
        let request: Value = serde_json::from_str(&out.writes[0]).unwrap();
        assert_eq!(request["method"], "model/list");
        assert!(request["params"].get("cursor").is_none(), "{request}");
        request["id"].as_u64().unwrap()
    };
    let page = |driver: &mut CodexDriver, id: u64, data: &str, cursor: &str| {
        let mut out = Outputs::default();
        let reply = format!(r#"{{"id":{id},"result":{{"data":[{data}],"nextCursor":{cursor}}}}}"#);
        driver.on_line(reply.as_bytes(), &mut out);
        out
    };
    /// The request a page's reply caused, when its cursor asked for another page.
    fn next_request(out: &Outputs, cursor: &str) -> u64 {
        let [line] = &out.writes[..] else {
            panic!("one request, got {:?}", out.writes);
        };
        let request: Value = serde_json::from_str(line).unwrap();
        assert_eq!(request["method"], "model/list");
        assert_eq!(request["params"]["cursor"], cursor);
        request["id"].as_u64().unwrap()
    }

    let id = ask(&mut driver);
    let out = page(
        &mut driver,
        id,
        r#"{"id":"gpt-6.1-sol","model":"gpt-6.1-sol","displayName":"GPT-6.1 Sol","description":"Balanced","hidden":false,"isDefault":true},
           {"id":"gpt-reserve","model":"gpt-reserve","displayName":"Reserve","description":"","hidden":true,"isDefault":false}"#,
        r#""page-2""#,
    );
    assert!(out.events.is_empty(), "nothing until the last page");
    let id = next_request(&out, "page-2");
    let out = page(
        &mut driver,
        id,
        r#"{"id":"entry-7","model":"gpt-x","displayName":"GPT X","description":"","hidden":false,"isDefault":false,
            "supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Quick"},{"reasoningEffort":"high","description":"Deep"}],
            "defaultReasoningEffort":"high","serviceTiers":[{"id":"priority","name":"Fast","description":"Faster"}],
            "defaultServiceTier":null,"additionalSpeedTiers":["fast"]}"#,
        "null",
    );
    assert!(out.writes.is_empty());
    assert_eq!(
        out.events,
        vec![(
            SESSION,
            Event::Catalog(Catalog::Ready {
                models: vec![
                    ModelInfo {
                        model: "gpt-6.1-sol".into(),
                        label: "GPT-6.1 Sol".into(),
                        resolved: None,
                        ..Default::default()
                    },
                    ModelInfo {
                        model: "gpt-x".into(),
                        label: "GPT X".into(),
                        resolved: None,
                        efforts: vec!["low".into(), "high".into()],
                        default_effort: Some("high".into()),
                        // `default` is Codex's standard tier, listed first.
                        speeds: vec![
                            Speed {
                                value: "default".into(),
                                label: "Standard".into(),
                            },
                            Speed {
                                value: "priority".into(),
                                label: "Fast".into(),
                            },
                        ],
                    },
                ],
                // Codex's `isDefault` is not the user's configured default.
                default_model: None,
            })
        )]
    );
    let mut out = Outputs::default();
    driver.command(choose_model(8, "gpt-x"), &mut out);
    driver.command(
        Command::Open {
            session: 8,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    let start: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(start["method"], "thread/start");
    assert_eq!(start["params"]["model"], "gpt-x");

    // An error on a later page fails the whole catalog.
    let id = ask(&mut driver);
    let out = page(&mut driver, id, "", r#""page-2""#);
    let id = next_request(&out, "page-2");
    let mut out = Outputs::default();
    let reply = format!(r#"{{"id":{id},"error":{{"code":-32600,"message":"invalid cursor"}}}}"#);
    driver.on_line(reply.as_bytes(), &mut out);
    assert_eq!(
        out.events,
        vec![(
            SESSION,
            Event::Catalog(Catalog::Failed("invalid cursor".into()))
        )]
    );

    let mut id = ask(&mut driver);
    for n in 1..20 {
        let entry = format!(
            r#"{{"id":"m{n}","model":"m{n}","displayName":"M{n}","hidden":false,"isDefault":false}}"#
        );
        let out = page(&mut driver, id, &entry, r#""more""#);
        assert!(out.events.is_empty(), "page {n}");
        id = next_request(&out, "more");
    }
    let out = page(
        &mut driver,
        id,
        r#"{"id":"m20","model":"m20","displayName":"M20","hidden":false,"isDefault":false}"#,
        r#""more""#,
    );
    assert!(out.writes.is_empty(), "page 21 is not asked for");
    assert_eq!(
        out.events,
        vec![(
            SESSION,
            Event::Catalog(Catalog::Failed(
                "Codex returned more than 20 pages of models".into()
            ))
        )]
    );

    ask(&mut driver);
    let mut out = Outputs::default();
    driver.on_exit("Codex stopped", &mut out);
    assert!(out.events.contains(&(
        SESSION,
        Event::Catalog(Catalog::Failed("Codex stopped".into()))
    )));
}

/// Skills are asked for one folder; the answer lists them, a refusal and an exit
/// before the answer each fail them.
#[test]
fn skills_for_a_folder_become_commands_and_a_refusal_or_exit_fails_them() {
    let mut driver = opened();
    let ask = |driver: &mut CodexDriver| {
        let mut out = Outputs::default();
        driver.request_commands(SESSION, "/tmp/repo", &mut out);
        let request: Value = serde_json::from_str(&out.writes[0]).unwrap();
        assert_eq!(request["method"], "skills/list");
        assert_eq!(
            request["params"],
            serde_json::json!({"cwds": ["/tmp/repo"]})
        );
        request["id"].as_u64().unwrap()
    };
    let answer = |driver: &mut CodexDriver, reply: String| {
        let mut out = Outputs::default();
        driver.on_line(reply.as_bytes(), &mut out);
        out.events
    };
    let id = ask(&mut driver);
    let reply = format!(
        r#"{{"id":{id},"result":{{"data":[{{"cwd":"/tmp/repo","skills":[{{"name":"review","description":"Review it","path":"/r/SKILL.md","scope":"repo","enabled":true,"pluginId":null}}]}}]}}}}"#
    );
    assert_eq!(
        answer(&mut driver, reply),
        vec![(
            SESSION,
            Event::Commands(Commands::Ready(vec![SlashCommand {
                invocation: "$review".into(),
                description: "Review it".into(),
                argument_hint: String::new(),
                source: CommandSource::Skill,
                terminal_only: false,
            }]))
        )]
    );
    let id = ask(&mut driver);
    let reply = format!(
        r#"{{"id":{id},"error":{{"code":-32600,"message":"unknown variant `skills/list`"}}}}"#
    );
    assert_eq!(
        answer(&mut driver, reply),
        vec![(
            SESSION,
            Event::Commands(Commands::Failed(
                "Codex could not list its skills: unknown variant `skills/list`".into()
            ))
        )]
    );
    ask(&mut driver);
    let mut out = Outputs::default();
    driver.on_exit("Codex exited", &mut out);
    assert!(out.events.contains(&(
        SESSION,
        Event::Commands(Commands::Failed("Codex exited".into()))
    )));
}

#[test]
fn rate_limit_and_token_usage_notifications_become_usage_events() {
    let mut driver = opened();
    let out = feed(
        &mut driver,
        &[
            r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex","limitName":null,"normalModelSlug":null,"primary":{"usedPercent":85,"windowDurationMins":300,"resetsAt":1800000000},"secondary":{"usedPercent":12,"windowDurationMins":10080,"resetsAt":1800500000},"credits":{"hasCredits":true,"unlimited":false,"balance":"5"},"individualLimit":null,"spendControlReached":null,"planType":"pro","rateLimitReachedType":null}}}"#.into(),
            r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex_other","limitName":"Spark","primary":{"usedPercent":3,"windowDurationMins":null,"resetsAt":null},"secondary":null}}}"#.into(),
            r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":null,"limitName":null,"primary":null,"secondary":null}}}"#.into(),
            r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex_bengalfox","limitName":null,"primary":{"usedPercent":9,"windowDurationMins":300,"resetsAt":null},"secondary":null}}}"#.into(),
            format!(r#"{{"method":"thread/tokenUsage/updated","params":{{"threadId":"{THREAD}","turnId":"t1","tokenUsage":{{"total":{{"totalTokens":90000,"inputTokens":1,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":1,"reasoningOutputTokens":0}},"last":{{"totalTokens":45000,"inputTokens":1,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":1,"reasoningOutputTokens":0}},"modelContextWindow":258400}}}}}}"#),
            r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"other-thread","turnId":"t1","tokenUsage":{"total":{"totalTokens":1},"last":{"totalTokens":1},"modelContextWindow":null}}}"#.into(),
        ],
    );
    let window =
        |id: &str, label: &str, used: f64, resets_at: Option<u64>| keystone_core::LimitWindow {
            id: id.into(),
            label: label.into(),
            used_percent: used,
            resets_at,
        };
    assert_eq!(
        out.events,
        vec![
            (
                0,
                Event::Usage(vec![
                    window("codex/primary", "5-hour", 85.0, Some(1_800_000_000)),
                    window("codex/secondary", "Weekly", 12.0, Some(1_800_500_000)),
                ])
            ),
            (
                0,
                Event::Usage(vec![window(
                    "codex_other/primary",
                    "Spark Primary",
                    3.0,
                    None
                )])
            ),
            (
                0,
                Event::Usage(vec![window(
                    "codex_bengalfox/primary",
                    "codex_bengalfox 5-hour",
                    9.0,
                    None
                )])
            ),
            (
                SESSION,
                Event::ContextUsage {
                    used: 45_000,
                    window: Some(258_400)
                }
            ),
        ]
    );
    assert_eq!(driver.unknown, 0);
}

#[test]
fn a_turn_failed_on_the_usage_limit_reports_an_unnamed_limit() {
    let mut driver = opened();
    let mut out = Outputs::default();
    // Codex 0.163.0's schema: `turn.error.codexErrorInfo` is a camelCase kind, or an
    // object for the kinds that carry an HTTP status.
    for (turn, info, limited) in [
        ("a", r#""usageLimitExceeded""#, true),
        ("b", r#""rateLimitExceeded""#, false),
        (
            "c",
            r#"{"httpConnectionFailed":{"httpStatusCode":429}}"#,
            false,
        ),
    ] {
        driver.command(
            Command::Send {
                session: SESSION,
                text: "one".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        feed(&mut driver, &[started(turn)]);
        let failed = format!(
            r#"{{"method":"turn/completed","params":{{"threadId":"{THREAD}","turn":{{"id":"{turn}","status":"failed","error":{{"message":"You've hit your usage limit.","codexErrorInfo":{info},"additionalDetails":null}}}}}}}}"#
        );
        let events: Vec<_> = feed(&mut driver, &[failed])
            .events
            .into_iter()
            .map(|(_, event)| event)
            .filter(|e| matches!(e, Event::UsageLimit(_) | Event::TurnCompleted { .. }))
            .collect();
        let expected_end = Event::TurnCompleted {
            status: TurnStatus::Failed,
            error: Some("You've hit your usage limit.".into()),
            stopped: false,
        };
        if limited {
            assert_eq!(
                events,
                vec![Event::UsageLimit(Default::default()), expected_end]
            );
        } else {
            assert_eq!(events, vec![expected_end], "{info}");
        }
    }
}

#[test]
fn a_fork_opens_codex_s_fork_of_the_source_through_its_turn_and_names_each_turn_s_point() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{"userAgent":"test"}}"#, &mut out);
    driver.fork_on_open(
        SESSION,
        keystone_core::ForkPoint {
            thread: "source".into(),
            at: "turn-2".into(),
        },
    );
    let mut out = Outputs::default();
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    let fork: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(fork["method"], "thread/fork");
    assert_eq!(fork["params"]["threadId"], "source");
    assert_eq!(fork["params"]["lastTurnId"], "turn-2");
    assert_eq!(fork["params"]["cwd"], "/tmp/repo");
    assert_eq!(fork["params"]["excludeTurns"], true);
    assert!(fork["params"].get("dynamicTools").is_none());
    assert_eq!(out.writes.len(), 1, "a fork reads no settings baseline");

    let response = format!(
        r#"{{"id":{},"result":{{"thread":{{"id":"{THREAD}","forkedFromId":"source"}}}}}}"#,
        fork["id"]
    );
    let out = feed(&mut driver, &[response]);
    assert_eq!(
        out.events,
        vec![(
            SESSION,
            Event::ThreadReady {
                thread_id: THREAD.into()
            }
        )]
    );

    // A turn of the fork ends at a point in the fork's own thread.
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "next".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    let out = feed(&mut driver, &[started("turn-4"), completed("turn-4")]);
    let events: Vec<_> = out.events.into_iter().map(|(_, e)| e).collect();
    let end = events
        .iter()
        .position(|e| matches!(e, Event::TurnCompleted { .. }))
        .unwrap();
    assert_eq!(
        events[end - 1],
        Event::ForkPoint(keystone_core::ForkPoint {
            thread: THREAD.into(),
            at: "turn-4".into(),
        })
    );
}

#[test]
fn a_fork_codex_refuses_fails_with_the_handoff_way_on_and_no_new_thread() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.on_line(br#"{"id":1,"result":{"userAgent":"test"}}"#, &mut out);
    driver.fork_on_open(
        SESSION,
        keystone_core::ForkPoint {
            thread: "source".into(),
            at: "not-a-turn".into(),
        },
    );
    let mut out = Outputs::default();
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    let fork: Value = serde_json::from_str(&out.writes[0]).unwrap();
    let response = format!(
        r#"{{"id":{},"error":{{"code":-32600,"message":"turn not found: not-a-turn"}}}}"#,
        fork["id"]
    );
    let out = feed(&mut driver, &[response]);
    assert!(out.writes.is_empty(), "{:?}", out.writes);
    assert!(out.events.iter().any(|(_, e)| matches!(e,
        Event::Failed { message } if *message == format!("Could not fork this Codex conversation: turn not found: not-a-turn. Hand off ({}) to continue it.", keystone_agent::HAND_OFF_KEYS))),
        "{:?}", out.events);
}

/// An `account/rateLimits/read` answer, as Codex 0.163.0 sends one.
fn rate_limits_answer(body: &str) -> String {
    format!(r#"{{"id":"keystone-rate-limits",{body}}}"#)
}

fn limit_window(
    id: &str,
    label: &str,
    used: f64,
    resets_at: Option<u64>,
) -> keystone_core::LimitWindow {
    keystone_core::LimitWindow {
        id: id.into(),
        label: label.into(),
        used_percent: used,
        resets_at,
    }
}

#[test]
fn an_initialized_app_server_is_asked_its_rate_limits_after_the_waiting_requests() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: None,
        },
        &mut out,
    );
    let mut out = Outputs::default();
    driver.on_line(br#"{"id":1,"result":{"userAgent":"test"}}"#, &mut out);
    let written: Vec<Value> = out
        .writes
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        written
            .iter()
            .map(|w| w["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["initialized", "thread/start", "account/rateLimits/read"]
    );
    assert_eq!(
        written[2],
        serde_json::json!({"id": "keystone-rate-limits", "method": "account/rateLimits/read",
            "params": {"excludeResetCreditDetails": true}})
    );
    assert_eq!(written[1]["id"], 2);
    assert!(out.events.is_empty());
}

#[test]
fn a_rate_limits_answer_reports_each_bucket_once_and_reads_nothing_else() {
    let mut driver = opened();
    let out = feed(
        &mut driver,
        &[rate_limits_answer(
            r#""result":{"accountId":"synthetic","ordinaryUsageAllowed":true,"rateLimitResetCredits":null,
            "rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":41,"windowDurationMins":300,"resetsAt":1800000000},"secondary":{"usedPercent":12,"windowDurationMins":10080,"resetsAt":1800500000},"credits":{"hasCredits":true,"unlimited":false,"balance":"5"},"planType":"pro"},
            "rateLimitsByLimitId":{
                "codex":{"limitId":"codex","primary":{"usedPercent":41,"windowDurationMins":300,"resetsAt":1800000000},"secondary":{"usedPercent":12,"windowDurationMins":10080,"resetsAt":1800500000}},
                "codex_other":{"limitId":"codex_other","limitName":"Spark","primary":{"usedPercent":3,"windowDurationMins":null,"resetsAt":null},"secondary":null},
                "codex_bengalfox":{"limitId":null,"primary":{"usedPercent":9,"windowDurationMins":300,"resetsAt":null}},
                "renamed":{"limitId":"codex_spark","limitName":"Spark","primary":{"usedPercent":5,"windowDurationMins":300,"resetsAt":null}},
                "broken":{"primary":{"usedPercent":"full"}},
                "empty":null}}"#,
        )],
    );
    assert_eq!(
        out.events,
        vec![(
            0,
            Event::Usage(vec![
                limit_window("codex/primary", "5-hour", 41.0, Some(1_800_000_000)),
                limit_window("codex/secondary", "Weekly", 12.0, Some(1_800_500_000)),
                limit_window("codex_other/primary", "Spark Primary", 3.0, None),
                limit_window("codex_spark/primary", "Spark 5-hour", 5.0, None),
            ])
        )]
    );
    assert!(out.writes.is_empty());
    assert_eq!(driver.unknown, 0);
}

#[test]
fn a_rate_limits_error_or_unreadable_answer_is_ignored_quietly() {
    let mut driver = opened();
    let out = feed(
        &mut driver,
        &[
            rate_limits_answer(
                r#""error":{"code":-32600,"message":"codex account authentication required to read rate limits"}"#,
            ),
            rate_limits_answer(
                r#""error":{"code":-32600,"message":"Invalid request: unknown variant `account/rateLimits/read`"}"#,
            ),
            rate_limits_answer(
                r#""error":{"code":-32603,"message":"no"},"result":{"rateLimits":{"primary":{"usedPercent":50}}}"#,
            ),
            rate_limits_answer(r#""result":null"#),
            rate_limits_answer(r#""result":"a string""#),
            rate_limits_answer(r#""result":{"rateLimits":"full","rateLimitsByLimitId":[1,2]}"#),
            rate_limits_answer(r#""result":{"rateLimits":{"primary":{"usedPercent":"12"}}}"#),
            rate_limits_answer(
                r#""result":{"rateLimits":{"limitId":null,"primary":null,"secondary":null},"rateLimitsByLimitId":null}"#,
            ),
            r#"{"id":"keystone-rate-limits"}"#.into(),
        ],
    );
    assert!(out.events.is_empty(), "{:?}", out.events);
    assert!(out.writes.is_empty(), "{:?}", out.writes);
    assert_eq!(driver.unknown, 0);
}

#[test]
fn a_late_rate_limits_answer_keeps_the_windows_a_notification_reported() {
    let mut driver = opened();
    let out = feed(
        &mut driver,
        &[
            r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":40,"windowDurationMins":300,"resetsAt":1800000000},"secondary":null}}}"#.into(),
            rate_limits_answer(
                r#""result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":30,"windowDurationMins":300,"resetsAt":1700000000},"secondary":{"usedPercent":12,"windowDurationMins":10080,"resetsAt":1800500000}}}"#,
            ),
        ],
    );
    assert_eq!(
        out.events,
        vec![
            (
                0,
                Event::Usage(vec![limit_window(
                    "codex/primary",
                    "5-hour",
                    40.0,
                    Some(1_800_000_000)
                )])
            ),
            (
                0,
                Event::Usage(vec![limit_window(
                    "codex/secondary",
                    "Weekly",
                    12.0,
                    Some(1_800_500_000)
                )])
            ),
        ]
    );
}

#[test]
fn a_rate_limits_answer_during_a_turn_leaves_the_turn_alone() {
    let mut driver = opened();
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "first".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    driver.on_line(started("T1").as_bytes(), &mut out);
    assert!(driver.is_running(SESSION));
    let out = feed(
        &mut driver,
        &[rate_limits_answer(
            r#""result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":85,"windowDurationMins":300,"resetsAt":1800000000}}}"#,
        )],
    );
    assert!(driver.is_running(SESSION));
    assert!(out.writes.is_empty());
    assert_eq!(
        out.events,
        vec![(
            0,
            Event::Usage(vec![limit_window(
                "codex/primary",
                "5-hour",
                85.0,
                Some(1_800_000_000)
            )])
        )]
    );
}

fn choose(
    driver: &mut CodexDriver,
    model: Option<&str>,
    effort: Option<&str>,
    speed: Option<&str>,
) {
    driver.command(
        Command::Choose {
            session: SESSION,
            choice: ModelChoice {
                model: model.map(str::to_owned),
                effort: effort.map(str::to_owned),
                speed: speed.map(str::to_owned),
            },
        },
        &mut Outputs::default(),
    );
}

fn settings_updated(model: &str) -> String {
    format!(
        r#"{{"method":"thread/settings/updated","params":{{"threadId":"{THREAD}","threadSettings":{{"model":"{model}","effort":"high","serviceTier":"priority"}}}}}}"#
    )
}

/// Codex keeps a `turn/start` override for the thread's later turns, and `null` resets
/// it, so each turn sends exactly the fields chosen and leaves the rest out.
#[test]
fn a_turn_start_carries_only_the_chosen_fields_and_never_null() {
    let mut driver = opened();
    let plain = send(&mut driver, "plain");
    for field in ["model", "effort", "serviceTier"] {
        assert!(plain["params"].get(field).is_none(), "{plain}");
    }
    finish_turn(&mut driver, "T1");

    choose(&mut driver, None, Some("high"), None);
    let effort = send(&mut driver, "effort");
    assert_eq!(effort["params"]["effort"], "high");
    assert!(effort["params"].get("model").is_none(), "{effort}");
    assert!(effort["params"].get("serviceTier").is_none(), "{effort}");
    finish_turn(&mut driver, "T2");

    choose(&mut driver, Some("gpt-x"), Some("low"), Some("priority"));
    let all = send(&mut driver, "all");
    assert_eq!(all["params"]["model"], "gpt-x");
    assert_eq!(all["params"]["effort"], "low");
    assert_eq!(all["params"]["serviceTier"], "priority");
}

/// A change during a turn never reaches it: a steer carries no settings, and the next
/// `turn/start` carries the change.
#[test]
fn a_choice_changed_during_a_turn_reaches_only_the_next_turn() {
    let mut driver = opened();
    send(&mut driver, "first");
    feed(&mut driver, &[started("T1")]);
    choose(&mut driver, Some("gpt-x"), Some("high"), None);
    let steer = send(&mut driver, "steer");
    assert_eq!(steer["method"], "turn/steer");
    for field in ["model", "effort", "serviceTier"] {
        assert!(steer["params"].get(field).is_none(), "{steer}");
    }
    feed(&mut driver, &[completed("T1")]);
    let next = send(&mut driver, "next");
    assert_eq!(next["method"], "turn/start");
    assert_eq!(next["params"]["model"], "gpt-x");
    assert_eq!(next["params"]["effort"], "high");
}

/// The thread's reported model is lost before a turn that asks for another one, and
/// `thread/settings/updated` reports the model the thread runs on now.
#[test]
fn a_model_change_loses_the_old_report_and_the_settings_update_reports_the_new_one() {
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
    let start: Value = serde_json::from_str(&out.writes[0]).unwrap();
    let reply = format!(
        r#"{{"id":{},"result":{{"thread":{{"id":"{THREAD}"}},"model":"gpt-6-luna"}}}}"#,
        start["id"]
    );
    let out = feed(&mut driver, &[reply]);
    assert!(out.events.contains(&(
        SESSION,
        Event::ModelReported {
            model: "gpt-6-luna".into()
        }
    )));

    // The same model again loses nothing.
    choose(&mut driver, Some("gpt-6-luna"), None, None);
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "same".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    assert!(out.events.is_empty(), "{:?}", out.events);
    finish_turn(&mut driver, "T1");

    choose(&mut driver, Some("gpt-x"), None, None);
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "switch".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    assert_eq!(out.events, [(SESSION, Event::ModelLost)]);
    assert_eq!(methods(&out), ["turn/start"]);

    let out = feed(&mut driver, &[settings_updated("gpt-x"), started("T2")]);
    assert_eq!(
        out.events.first(),
        Some(&(
            SESSION,
            Event::ModelReported {
                model: "gpt-x".into()
            }
        ))
    );
    // A repeat of the model it reported already says nothing new.
    let out = feed(&mut driver, &[settings_updated("gpt-x")]);
    assert!(out.events.is_empty(), "{:?}", out.events);
}

/// The [`Command::Choose`] that asks for `model` alone.
fn choose_model(session: u64, model: &str) -> Command {
    Command::Choose {
        session,
        choice: keystone_core::ModelChoice {
            model: Some(model.into()),
            ..Default::default()
        },
    }
}
