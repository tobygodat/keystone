use super::*;
use crate::mcp::{Credentials, Server};
use std::sync::Arc;

const SESSION: u64 = 7;
const PARENT: u64 = 99;

fn caller() -> (Server, Issuer, Credentials) {
    let server = Server::start(Arc::new(|_, _, _, _| Ok(json!({})))).unwrap();
    let issuer = server.issuer();
    let credentials = issuer.issue(PARENT).unwrap();
    issuer.set_mode(PARENT, ApprovalMode::FullAccess);
    issuer.set_running(PARENT, true);
    (server, issuer, credentials)
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

fn open(driver: &mut CodexDriver, resume: bool, out: &mut Outputs) -> u64 {
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/synthetic".into(),
            resume: resume.then(|| "thread".into()),
        },
        out,
    );
    driver.next_id
}

fn initialize(driver: &mut CodexDriver, out: &mut Outputs) {
    driver.on_line(br#"{"id":1,"result":{}}"#, out);
}

fn thread_ready(driver: &mut CodexDriver, id: u64, out: &mut Outputs) {
    driver.on_line(
        json!({"id": id, "result": {"thread": {"id": "thread"}}})
            .to_string()
            .as_bytes(),
        out,
    );
}

fn human(driver: &mut CodexDriver, out: &mut Outputs) {
    driver.command(
        Command::Send {
            session: SESSION,
            text: "human".into(),
            images: Vec::new(),
        },
        out,
    );
}

fn assert_start(out: &Outputs, text: &str) {
    assert_eq!(out.writes.len(), 1, "{:?}", out.writes);
    let request: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(request["method"], "turn/start");
    assert_eq!(request["params"]["input"][0]["text"], text);
}

fn denied(out: &Outputs) -> bool {
    out.events.iter().any(|(session, event)| {
        *session == SESSION
            && matches!(event, Event::Failed { message } if message.contains("launching agent"))
    })
}

#[test]
fn stopped_pending_launch_does_not_poison_the_next_human_send() {
    let (_server, issuer, credentials) = caller();
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    let id = open(&mut driver, false, &mut out);
    driver.send_from(
        SESSION,
        "launch".into(),
        issuer,
        credentials.caller(),
        &mut out,
    );
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    drop(credentials);
    out = Outputs::default();
    human(&mut driver, &mut out);
    assert!(!denied(&out));
    initialize(&mut driver, &mut out);
    out = Outputs::default();
    thread_ready(&mut driver, id, &mut out);
    assert_start(&out, "human");
    assert!(!denied(&out));
}

#[test]
fn interrupt_now_cancels_pending_launches_before_or_after_initialize() {
    for ready in [false, true] {
        let (_server, issuer, credentials) = caller();
        let mut out = Outputs::default();
        let mut driver = CodexDriver::new("test", &mut out);
        let id = open(&mut driver, false, &mut out);
        if ready {
            initialize(&mut driver, &mut out);
        }
        driver.send_from(
            SESSION,
            "launch".into(),
            issuer,
            credentials.caller(),
            &mut out,
        );
        out = Outputs::default();
        driver.interrupt_now(SESSION, &mut out).unwrap();
        assert!(out.writes.is_empty());
        assert_eq!(
            out.events,
            vec![(
                SESSION,
                Event::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    error: None,
                    stopped: true,
                }
            )]
        );
        drop(credentials);
        out = Outputs::default();
        human(&mut driver, &mut out);
        assert!(!denied(&out));
        if !ready {
            initialize(&mut driver, &mut out);
        }
        out = Outputs::default();
        thread_ready(&mut driver, id, &mut out);
        assert_start(&out, "human");
    }
}

#[test]
fn failed_thread_open_does_not_poison_a_human_retry() {
    let (_server, issuer, credentials) = caller();
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    initialize(&mut driver, &mut out);
    let id = open(&mut driver, false, &mut out);
    driver.send_from(
        SESSION,
        "launch".into(),
        issuer,
        credentials.caller(),
        &mut out,
    );
    out = Outputs::default();
    driver.on_line(
        json!({"id": id, "error": {"code": -32000, "message": "open failed"}})
            .to_string()
            .as_bytes(),
        &mut out,
    );
    assert!(!driver.is_open(SESSION));
    assert!(out.events.iter().any(|(_, event)| {
        matches!(event, Event::Failed { message } if message.contains("open failed"))
    }));
    drop(credentials);
    let id = open(&mut driver, false, &mut out);
    out = Outputs::default();
    human(&mut driver, &mut out);
    assert!(!denied(&out));
    thread_ready(&mut driver, id, &mut out);
    assert_start(&out, "human");
}

#[test]
fn a_launch_rejected_before_open_does_not_poison_a_human_send() {
    let (_server, issuer, credentials) = caller();
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    initialize(&mut driver, &mut out);
    out = Outputs::default();
    driver.send_from(
        SESSION,
        "launch".into(),
        issuer,
        credentials.caller(),
        &mut out,
    );
    assert!(out.writes.is_empty());
    assert!(out.events.iter().any(|(_, event)| {
        matches!(event, Event::Failed { message } if message.contains("not open"))
    }));
    drop(credentials);
    let id = open(&mut driver, false, &mut out);
    thread_ready(&mut driver, id, &mut out);
    out = Outputs::default();
    human(&mut driver, &mut out);
    assert_start(&out, "human");
    assert!(!denied(&out));
}

#[test]
fn denying_a_guarded_message_keeps_the_following_human_message() {
    for revoke in [false, true] {
        let (_server, issuer, credentials) = caller();
        let mut out = Outputs::default();
        let mut driver = CodexDriver::new("test", &mut out);
        configure(&mut driver, ApprovalMode::Auto, false);
        let id = open(&mut driver, false, &mut out);
        driver.send_from(
            SESSION,
            "launch".into(),
            issuer.clone(),
            credentials.caller(),
            &mut out,
        );
        if revoke {
            drop(credentials);
        } else {
            issuer.set_mode(PARENT, ApprovalMode::ReadOnly);
        }
        out = Outputs::default();
        human(&mut driver, &mut out);
        assert!(!denied(&out));
        initialize(&mut driver, &mut out);
        out = Outputs::default();
        thread_ready(&mut driver, id, &mut out);
        assert_start(&out, "human");
        assert!(denied(&out));
    }
}

#[test]
fn denying_a_guarded_message_waiting_for_settings_keeps_human_input() {
    let (_server, issuer, credentials) = caller();
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    configure(&mut driver, ApprovalMode::Provider, true);
    let id = open(&mut driver, true, &mut out);
    let baseline = id - 1;
    initialize(&mut driver, &mut out);
    thread_ready(&mut driver, id, &mut out);
    out = Outputs::default();
    driver.send_from(
        SESSION,
        "launch".into(),
        issuer,
        credentials.caller(),
        &mut out,
    );
    assert!(out.writes.is_empty());
    drop(credentials);
    human(&mut driver, &mut out);
    assert!(!denied(&out));
    driver.on_line(
        json!({"id": baseline, "result": {
            "thread": {"id": "baseline"},
            "approvalPolicy": "on-request",
            "sandbox": {"type": "readOnly"}
        }})
        .to_string()
        .as_bytes(),
        &mut out,
    );
    assert_start(&out, "human");
    assert!(denied(&out));
}

#[test]
fn accepted_launch_survives_the_parent_turn_ending_during_initialize() {
    let (_server, issuer, credentials) = caller();
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    let id = open(&mut driver, false, &mut out);
    driver.send_from(
        SESSION,
        "launch".into(),
        issuer.clone(),
        credentials.caller(),
        &mut out,
    );
    issuer.set_running(PARENT, false);
    initialize(&mut driver, &mut out);
    out = Outputs::default();
    thread_ready(&mut driver, id, &mut out);
    assert_start(&out, "launch");
    assert!(!denied(&out));
}

#[test]
fn revoked_or_narrowed_callers_cannot_write_the_first_turn() {
    for change in ["revoked", "narrowed", "target widened"] {
        let (_server, issuer, credentials) = caller();
        issuer.set_mode(PARENT, ApprovalMode::Ask);
        let mut out = Outputs::default();
        let mut driver = CodexDriver::new("test", &mut out);
        configure(&mut driver, ApprovalMode::Ask, false);
        let id = open(&mut driver, false, &mut out);
        driver.send_from(
            SESSION,
            "launch".into(),
            issuer.clone(),
            credentials.caller(),
            &mut out,
        );
        match change {
            "revoked" => drop(credentials),
            "narrowed" => issuer.set_mode(PARENT, ApprovalMode::ReadOnly),
            "target widened" => configure(&mut driver, ApprovalMode::FullAccess, false),
            _ => unreachable!(),
        }
        initialize(&mut driver, &mut out);
        out = Outputs::default();
        thread_ready(&mut driver, id, &mut out);
        assert!(out.writes.is_empty(), "{change}");
        assert!(denied(&out), "{change}");
        assert!(!driver.is_running(SESSION));
    }
}

#[test]
fn launch_permission_is_only_checked_when_the_first_turn_can_write() {
    let (_server, issuer, credentials) = caller();
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    configure(&mut driver, ApprovalMode::Auto, false);
    let id = open(&mut driver, false, &mut out);
    issuer.set_mode(PARENT, ApprovalMode::ReadOnly);
    out = Outputs::default();
    driver.send_from(
        SESSION,
        "launch".into(),
        issuer.clone(),
        credentials.caller(),
        &mut out,
    );
    assert!(out.writes.is_empty());
    assert!(!denied(&out));
    issuer.set_mode(PARENT, ApprovalMode::FullAccess);
    initialize(&mut driver, &mut out);
    out = Outputs::default();
    thread_ready(&mut driver, id, &mut out);
    assert_start(&out, "launch");
}

#[test]
fn interrupt_now_remembers_stop_while_the_turn_has_no_id() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    let id = open(&mut driver, false, &mut out);
    initialize(&mut driver, &mut out);
    thread_ready(&mut driver, id, &mut out);
    out = Outputs::default();
    human(&mut driver, &mut out);
    assert_start(&out, "human");
    out = Outputs::default();
    driver.interrupt_now(SESSION, &mut out).unwrap();
    assert!(out.writes.is_empty());
    driver.on_line(
        br#"{"method":"turn/started","params":{"threadId":"thread","turn":{"id":"turn"}}}"#,
        &mut out,
    );
    assert_eq!(out.writes.len(), 1);
    let request: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(request["method"], "turn/interrupt");
    assert_eq!(request["params"]["turnId"], "turn");
}

#[test]
fn interrupt_now_is_a_noop_for_idle_sessions() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    out = Outputs::default();
    driver.interrupt_now(SESSION, &mut out).unwrap();
    assert!(out.writes.is_empty());
    assert!(out.events.is_empty());
    assert!(!driver.sessions.contains_key(&SESSION));
    let id = open(&mut driver, false, &mut out);
    initialize(&mut driver, &mut out);
    thread_ready(&mut driver, id, &mut out);
    out = Outputs::default();
    driver.interrupt_now(SESSION, &mut out).unwrap();
    assert!(out.writes.is_empty());
    assert!(out.events.is_empty());
}

#[test]
fn failed_initialize_reports_the_same_error_for_guarded_and_human_sends() {
    let (_server, issuer, credentials) = caller();
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    driver.on_line(
        br#"{"id":1,"error":{"code":-32000,"message":"initialize failed"}}"#,
        &mut out,
    );
    let mut guarded = Outputs::default();
    driver.send_from(
        SESSION,
        "launch".into(),
        issuer,
        credentials.caller(),
        &mut guarded,
    );
    let mut unguarded = Outputs::default();
    human(&mut driver, &mut unguarded);
    assert!(guarded.writes.is_empty());
    assert_eq!(guarded.events, unguarded.events);
    assert!(guarded.events.iter().any(|(_, event)| {
        matches!(event, Event::Failed { message } if message.contains("initialize failed"))
    }));
}
