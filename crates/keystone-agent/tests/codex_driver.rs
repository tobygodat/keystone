//! The Codex driver against a recording of real codex-cli 0.162.1 traffic for one
//! thread: a plain reply, a command approval, and an interrupted turn.

use keystone_agent::codex::CodexDriver;
use keystone_agent::{ApprovalState, Command, Outputs};
use keystone_agent::{Catalog, Event, ItemKind, ItemStatus, ModelInfo, RequestId, TurnStatus};
use keystone_core::ApprovalMode;
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
            model: None,
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
            model: None,
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
            model: None,
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
            model: None,
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
    driver.command(
        Command::Open {
            session: 8,
            cwd: "/tmp/repo".into(),
            resume: None,
            model: Some("gpt-6-astra".into()),
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
            model: None,
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
            model: None,
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
            model: None,
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
            model: None,
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
            model: None,
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
        driver.command(
            Command::Open {
                session,
                cwd: "/tmp/repo".into(),
                resume: resume.map(str::to_owned),
                model: model.map(str::to_owned),
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
        r#"{"id":"entry-7","model":"gpt-x","displayName":"GPT X","description":"","hidden":false,"isDefault":false}"#,
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
                    },
                    ModelInfo {
                        model: "gpt-x".into(),
                        label: "GPT X".into(),
                        resolved: None,
                    },
                ],
                // Codex's `isDefault` is not the user's configured default.
                default_model: None,
            })
        )]
    );
    let mut out = Outputs::default();
    driver.command(
        Command::Open {
            session: 8,
            cwd: "/tmp/repo".into(),
            resume: None,
            model: Some("gpt-x".into()),
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
