use super::*;
use crate::mcp::{Credentials, Server};
use crate::surface;
use keystone_core::integrations::Health;
use serde_json::{Value, json};

fn server() -> Server {
    Server::start(Arc::new(|_, _, _, _| Ok(json!({})))).unwrap()
}

fn caller(issuer: &Issuer) -> Credentials {
    let credentials = issuer.issue(99).unwrap();
    issuer.set_mode(99, ApprovalMode::FullAccess);
    issuer.set_running(99, true);
    credentials
}

fn wire(line: &str) -> Value {
    serde_json::from_str(line).unwrap()
}

/// Whether a line is the `get_usage` a chat process writes once Claude answers
/// `initialize`: after any `mcp_set_servers` and before any message.
fn usage_request(line: &str) -> bool {
    wire(line)
        == json!({"type": "control_request", "request_id": "keystone-usage",
            "request": {"subtype": "get_usage", "skip_behaviors": true}})
}

fn claude_init(driver: &mut ClaudeDriver, out: &mut Outputs) {
    driver.on_line(br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{"current_permission_mode":"default"}}}"#, out);
}

fn claude_mcp_ready(driver: &mut ClaudeDriver, out: &mut Outputs) {
    driver.on_line(br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{"added":["keystone"],"removed":[],"errors":{}}}}"#, out);
}

fn claude_lifecycle(driver: &mut ClaudeDriver, out: &mut Outputs) {
    driver.on_line(br#"{"type":"system","subtype":"init","session_id":"conversation","model":"model","capabilities":["msg_lifecycle_v1"],"permissionMode":"default"}"#, out);
}

fn codex_open(driver: &mut CodexDriver, resume: Option<&str>, out: &mut Outputs) {
    driver.command(
        Command::Open {
            session: 7,
            cwd: "/synthetic".into(),
            resume: resume.map(str::to_owned),
            model: None,
        },
        out,
    );
}

fn codex_initialized(driver: &mut CodexDriver, out: &mut Outputs) {
    driver.on_line(br#"{"id":1,"result":{}}"#, out);
}

fn codex_thread_ready(driver: &mut CodexDriver, out: &mut Outputs) {
    driver.on_line(br#"{"id":2,"result":{"thread":{"id":"thread"}}}"#, out);
}

fn codex_turn(driver: &mut CodexDriver, completed: bool, out: &mut Outputs) {
    let method = if completed {
        "turn/completed"
    } else {
        "turn/started"
    };
    driver.on_line(json!({"method":method,"params":{"threadId":"thread","turn":{"id":"turn","status":"completed"}}}).to_string().as_bytes(), out);
}

#[test]
fn claude_injects_after_initialize_and_holds_messages_until_server_success() {
    for resume in [false, true] {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(7, resume, 1, ApprovalMode::Provider, &mut out);
        driver.set_mcp("http://127.0.0.1:1234/mcp".into(), "synthetic-token".into());
        driver.command(
            Command::Send {
                session: 7,
                text: "hello".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        assert_eq!(out.writes.len(), 1);
        assert_eq!(wire(&out.writes[0])["request"]["subtype"], "initialize");
        out.writes.clear();
        claude_init(&mut driver, &mut out);
        assert_eq!(out.writes.len(), 2);
        assert!(usage_request(&out.writes[1]));
        let set = wire(&out.writes[0]);
        assert_eq!(set["request"]["subtype"], "mcp_set_servers");
        assert_eq!(set["request"]["servers"]["keystone"]["type"], "http");
        assert_eq!(
            set["request"]["servers"]["keystone"]["headers"]["Authorization"],
            "Bearer synthetic-token"
        );
        out.writes.clear();
        assert!(
            driver
                .steer("too early".into(), &mut out)
                .unwrap_err()
                .starts_with("busy:")
        );
        assert!(out.writes.is_empty());
        claude_mcp_ready(&mut driver, &mut out);
        assert_eq!(out.writes.len(), 1);
        assert_eq!(wire(&out.writes[0])["message"]["content"], "hello");
        assert!(!out.writes[0].contains("synthetic-token"));
    }
}

#[test]
fn claude_failed_mcp_configuration_never_writes_a_user_message_or_echoes_errors() {
    for response in [
        json!({"subtype":"error","error":"synthetic-token"}),
        json!({"subtype":"success","response":{"added":[],"errors":{"keystone":"synthetic-token"}}}),
    ] {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
        driver.set_mcp("http://127.0.0.1/mcp".into(), "synthetic-token".into());
        driver.command(
            Command::Send {
                session: 7,
                text: "hello".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        claude_init(&mut driver, &mut out);
        out = Outputs::default();
        let mut response = response;
        response["request_id"] = json!("keystone-2");
        driver.on_line(
            json!({"type":"control_response","response":response})
                .to_string()
                .as_bytes(),
            &mut out,
        );
        assert!(out.writes.is_empty());
        assert!(driver.credentials_failed());
        assert!(!driver.is_running());
        assert!(
            out.events
                .iter()
                .any(|(_, event)| matches!(event, Event::Failed { .. }))
        );
        assert!(!format!("{:?}", out.events).contains("synthetic-token"));
    }
}

#[test]
fn claude_mcp_tools_are_allowed_without_restricting_native_tools() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    driver.set_mcp("http://127.0.0.1/mcp".into(), "synthetic-token".into());
    claude_init(&mut driver, &mut out);
    claude_mcp_ready(&mut driver, &mut out);
    out = Outputs::default();
    driver.on_line(br#"{"type":"control_request","request_id":"tool","request":{"subtype":"can_use_tool","tool_name":"mcp__keystone__session_read","input":{"session":8}}}"#, &mut out);
    assert_eq!(
        wire(&out.writes[0])["response"]["response"]["behavior"],
        "allow"
    );
    assert!(out.events.is_empty());
    out = Outputs::default();
    driver.on_line(br#"{"type":"control_request","request_id":"native","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"echo native"}}}"#, &mut out);
    assert!(
        out.events
            .iter()
            .any(|(_, event)| matches!(event, Event::ApprovalRequested(_)))
    );
    assert!(!claude::args(ApprovalMode::Provider).contains(&"--tools"));
}

#[test]
fn claude_mcp_autoallow_is_limited_to_the_six_published_tools() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    driver.set_mcp("http://127.0.0.1/mcp".into(), "synthetic-token".into());
    claude_init(&mut driver, &mut out);
    claude_mcp_ready(&mut driver, &mut out);
    for tool in [
        "capabilities",
        "session_launch",
        "session_send",
        "session_read",
        "session_wait",
        "session_interrupt",
        "unknown",
        "x__native",
    ] {
        out = Outputs::default();
        driver.on_line(json!({"type":"control_request","request_id":tool,"request":{"subtype":"can_use_tool","tool_name":format!("mcp__keystone__{tool}"),"input":{}}}).to_string().as_bytes(), &mut out);
        if matches!(tool, "unknown" | "x__native") {
            assert!(out.writes.is_empty());
            assert!(
                out.events
                    .iter()
                    .any(|(_, event)| matches!(event, Event::ApprovalRequested(_)))
            );
        } else {
            assert_eq!(
                wire(&out.writes[0])["response"]["response"]["behavior"],
                "allow"
            );
            assert!(out.events.is_empty());
        }
    }
}

#[test]
fn codex_interrupt_does_not_acknowledge_a_queued_launch_as_idle() {
    let server = server();
    let issuer = server.issuer();
    let credentials = caller(&issuer);
    let connection = CodexConnection::new(None, "test", Arc::new(|| {}));
    connection.set_mcp(issuer);
    connection.shared.lock().unwrap().queued.insert(7, 1);
    assert!(
        connection
            .interrupt_from(7, credentials.caller())
            .unwrap_err()
            .starts_with("busy:")
    );
}

#[test]
fn a_failed_codex_start_does_not_forget_later_queued_messages() {
    let connection = CodexConnection::new(
        Some(PathBuf::from("/nonexistent-keystone-test-provider")),
        "test",
        Arc::new(|| {}),
    );
    connection.shared.lock().unwrap().queued.insert(7, 2);
    let pending = connection.shared.clone();
    connection.starter().run(
        7,
        move |message| {
            pending.lock().unwrap().unqueue(7);
            start_failed(message)
        },
        |_, _, _| panic!("missing provider started"),
    );
    assert_eq!(connection.shared.lock().unwrap().queued.get(&7), Some(&1));
}

#[test]
fn codex_connection_interrupt_cancels_an_opening_child_before_turn_start() {
    let server = server();
    let issuer = server.issuer();
    let credentials = caller(&issuer);
    let connection = CodexConnection::new(None, "test", Arc::new(|| {}));
    connection.set_mcp(issuer.clone());
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    codex_initialized(&mut driver, &mut out);
    codex_open(&mut driver, None, &mut out);
    driver.send_from(
        7,
        "cancelled".into(),
        issuer,
        credentials.caller(),
        &mut out,
    );
    connection.shared.lock().unwrap().driver = Some(driver);
    connection.interrupt_from(7, credentials.caller()).unwrap();
    let mut shared = connection.shared.lock().unwrap();
    out = Outputs::default();
    codex_thread_ready(shared.driver.as_mut().unwrap(), &mut out);
    assert!(out.writes.is_empty());
    drop(shared);
    assert!(
        connection
            .drain()
            .iter()
            .any(|(_, event)| matches!(event, Event::TurnCompleted { stopped: true, .. }))
    );
}

#[test]
fn claude_strict_steer_is_immediate_or_busy_and_never_held() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    out = Outputs::default();
    assert!(
        driver
            .steer("idle".into(), &mut out)
            .unwrap_err()
            .starts_with("busy:")
    );
    driver.command(
        Command::Send {
            session: 7,
            text: "first".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    assert!(driver.steer("starting".into(), &mut out).is_err());
    claude_init(&mut driver, &mut out);
    assert!(driver.steer("unknown lifecycle".into(), &mut out).is_err());
    claude_lifecycle(&mut driver, &mut out);
    out = Outputs::default();
    driver.steer("immediate".into(), &mut out).unwrap();
    assert_eq!(out.writes.len(), 1);
    assert_eq!(wire(&out.writes[0])["message"]["content"], "immediate");
    driver.command(
        Command::Configure {
            session: 7,
            approval: ApprovalState {
                mode: ApprovalMode::FullAccess,
                overridden: true,
            },
        },
        &mut out,
    );
    out = Outputs::default();
    assert!(driver.steer("wrong mode".into(), &mut out).is_err());
    assert!(out.writes.is_empty());
    driver.command(Command::Interrupt { session: 7 }, &mut out);
    out = Outputs::default();
    assert!(driver.steer("stopping".into(), &mut out).is_err());
    assert!(out.writes.is_empty());
}

#[test]
fn codex_injects_start_and_resume_only_after_initialization() {
    for resume in [None, Some("saved-thread")] {
        let mut out = Outputs::default();
        let mut driver = CodexDriver::new("test", &mut out);
        driver.set_mcp(7, "http://127.0.0.1/mcp".into(), "synthetic-token".into());
        out = Outputs::default();
        codex_open(&mut driver, resume, &mut out);
        driver.command(
            Command::Send {
                session: 7,
                text: "hello".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        assert!(out.writes.is_empty());
        codex_initialized(&mut driver, &mut out);
        assert_eq!(out.writes.len(), 2);
        assert_eq!(wire(&out.writes[0])["method"], "initialized");
        let open = wire(&out.writes[1]);
        assert_eq!(
            open["method"],
            if resume.is_some() {
                "thread/resume"
            } else {
                "thread/start"
            }
        );
        let config = &open["params"]["config"]["mcp_servers"]["keystone"];
        assert_eq!(config["url"], "http://127.0.0.1/mcp");
        assert_eq!(
            config["http_headers"]["Authorization"],
            "Bearer synthetic-token"
        );
        assert_eq!(config["tool_timeout_sec"], 60);
        out = Outputs::default();
        codex_thread_ready(&mut driver, &mut out);
        assert_eq!(wire(&out.writes[0])["method"], "turn/start");
        assert!(!out.writes[0].contains("synthetic-token"));
    }
}

#[test]
fn codex_strict_steer_never_falls_back_after_turn_completion() {
    for strict in [true, false] {
        let mut out = Outputs::default();
        let mut driver = CodexDriver::new("test", &mut out);
        codex_initialized(&mut driver, &mut out);
        codex_open(&mut driver, None, &mut out);
        codex_thread_ready(&mut driver, &mut out);
        driver.command(
            Command::Send {
                session: 7,
                text: "first".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        assert!(
            driver
                .steer(7, "starting".into(), &mut Outputs::default())
                .unwrap_err()
                .starts_with("busy:")
        );
        codex_turn(&mut driver, false, &mut out);
        out = Outputs::default();
        if strict {
            driver.steer(7, "steer".into(), &mut out).unwrap();
        } else {
            driver.command(
                Command::Send {
                    session: 7,
                    text: "steer".into(),
                    images: Vec::new(),
                },
                &mut out,
            );
        }
        let request = wire(&out.writes[0]);
        assert_eq!(request["method"], "turn/steer");
        let id = request["id"].clone();
        codex_turn(&mut driver, true, &mut out);
        out = Outputs::default();
        driver.on_line(
            json!({"id":id,"error":{"code":-32000,"message":"turn no longer running"}})
                .to_string()
                .as_bytes(),
            &mut out,
        );
        if strict {
            assert!(out.writes.is_empty());
            assert!(
                out.events
                    .iter()
                    .any(|(_, event)| matches!(event, Event::Warning { .. }))
            );
            assert!(
                driver
                    .steer(7, "idle".into(), &mut out)
                    .unwrap_err()
                    .starts_with("busy:")
            );
        } else {
            assert_eq!(wire(&out.writes[0])["method"], "turn/start");
        }
    }
}

#[test]
fn launch_authorization_is_rechecked_at_actual_claude_and_codex_writes() {
    for revoke in [false, true] {
        let server = server();
        let issuer = server.issuer();
        let credentials = caller(&issuer);
        let identity = credentials.caller();
        let mut out = Outputs::default();
        let mut claude = ClaudeDriver::new(7, false, 1, ApprovalMode::Ask, &mut out);
        claude.send_from("launch".into(), issuer.clone(), identity, &mut out);
        let mut codex = CodexDriver::new("test", &mut out);
        codex.command(
            Command::Configure {
                session: 7,
                approval: ApprovalState {
                    mode: ApprovalMode::Ask,
                    overridden: true,
                },
            },
            &mut out,
        );
        codex_open(&mut codex, None, &mut out);
        codex.send_from(7, "launch".into(), issuer.clone(), identity, &mut out);
        if revoke {
            drop(credentials);
        } else {
            issuer.set_mode(99, ApprovalMode::ReadOnly);
        }
        out = Outputs::default();
        claude_init(&mut claude, &mut out);
        assert!(
            matches!(out.writes.as_slice(), [line] if usage_request(line)),
            "{:?}",
            out.writes
        );
        assert!(!claude.is_running());
        codex_initialized(&mut codex, &mut out);
        out = Outputs::default();
        codex_thread_ready(&mut codex, &mut out);
        assert!(out.writes.is_empty());
        assert!(
            out.events
                .iter()
                .any(|(_, event)| matches!(event, Event::Failed { .. }))
        );
        claude.command(
            Command::Send {
                session: 7,
                text: "human retry".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        codex.command(
            Command::Send {
                session: 7,
                text: "human retry".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        assert_eq!(out.writes.len(), 2);
    }
}

#[test]
fn claude_refused_launch_keeps_human_input_held_behind_it() {
    let server = server();
    let issuer = server.issuer();
    let credentials = caller(&issuer);
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    driver.send_from(
        "launch".into(),
        issuer.clone(),
        credentials.caller(),
        &mut out,
    );
    driver.command(
        Command::Send {
            session: 7,
            text: "human".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    drop(credentials);
    out = Outputs::default();
    claude_init(&mut driver, &mut out);
    assert!(out.events.iter().any(|(_, event)| {
        matches!(event, Event::TurnCompleted { status: crate::item::TurnStatus::Failed, error: Some(message), .. } if message.contains("launching agent"))
    }));
    assert_eq!(out.writes.len(), 2, "{:?}", out.writes);
    assert!(usage_request(&out.writes[0]));
    assert_eq!(wire(&out.writes[1])["message"]["content"], "human");
    assert!(driver.is_running());
}

#[test]
fn admitted_launch_survives_parent_turn_completion() {
    let server = server();
    let issuer = server.issuer();
    let credentials = caller(&issuer);
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    driver.send_from(
        "admitted".into(),
        issuer.clone(),
        credentials.caller(),
        &mut out,
    );
    issuer.set_running(99, false);
    out = Outputs::default();
    claude_init(&mut driver, &mut out);
    assert_eq!(out.writes.len(), 2);
    assert!(usage_request(&out.writes[0]));
    assert_eq!(wire(&out.writes[1])["message"]["content"], "admitted");
}

#[test]
fn claude_interrupt_does_not_acknowledge_a_queued_launch_as_idle() {
    let server = server();
    let issuer = server.issuer();
    let credentials = caller(&issuer);
    let connection = ClaudeConnection::new(None, Arc::new(|| {}));
    connection.set_mcp(issuer);
    connection
        .starter
        .shared
        .lock()
        .unwrap()
        .queued
        .insert(7, 1);
    assert!(
        connection
            .interrupt_from(7, credentials.caller())
            .unwrap_err()
            .starts_with("busy:")
    );
}

#[test]
fn replacement_keeps_running_authorization_without_duplicate_turn_start() {
    let server = server();
    let issuer = server.issuer();
    let before = issuer.issue(7).unwrap().caller().generation;
    let connection = ClaudeConnection::new(
        Some(PathBuf::from("/nonexistent-keystone-test-provider")),
        Arc::new(|| {}),
    );
    connection.set_mcp(issuer.clone());
    let mut out = Outputs::default();
    let mut old = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    old.command(
        Command::Send {
            session: 7,
            text: "carried".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    let messages = old.take_unsent();
    connection
        .starter
        .shared
        .lock()
        .unwrap()
        .queued
        .insert(7, 1);
    let (reached, observed) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    let held = Mutex::new(held);
    connection.hold_starts(Arc::new(move |_| {
        reached.send(()).unwrap();
        held.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }));
    let starter = connection.starter.clone();
    let worker = std::thread::spawn(move || {
        let shared = starter.shared.lock().unwrap();
        starter.start(
            shared,
            7,
            Path::new("/"),
            None,
            None,
            true,
            move |driver, pending| {
                driver.carry(messages, pending);
                assert!(driver.is_running());
                assert!(
                    !pending
                        .events
                        .iter()
                        .any(|(_, e)| matches!(e, Event::TurnStarted))
                );
            },
        );
    });
    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    let after = issuer.issue(8).unwrap().caller().generation;
    let identity = (before + 1..after)
        .map(|generation| Caller {
            session: 7,
            generation,
        })
        .find(|&caller| issuer.is_live(caller))
        .unwrap();
    let authorized = issuer.authorize(identity, ApprovalMode::ReadOnly);
    release.send(()).unwrap();
    worker.join().unwrap();
    assert!(authorized);
    assert!(
        !issuer.is_live(identity),
        "failed start must retire its lease"
    );
}

#[test]
fn connection_scopes_follow_configure_and_turn_events() {
    let server = server();
    let issuer = server.issuer();
    let credentials = caller(&issuer);
    let identity = credentials.caller();
    let mut shared = Shared {
        mcp: Some(issuer.clone()),
        ..Shared::default()
    };
    shared.set_approval(
        99,
        ApprovalState {
            mode: ApprovalMode::ReadOnly,
            overridden: false,
        },
    );
    assert!(!issuer.authorize(identity, ApprovalMode::Auto));
    shared.set_approval(
        99,
        ApprovalState {
            mode: ApprovalMode::FullAccess,
            overridden: false,
        },
    );
    assert!(issuer.authorize(identity, ApprovalMode::Auto));
    let inbox = Mutex::default();
    deliver(
        &mut shared,
        &inbox,
        Outputs {
            events: vec![(
                99,
                Event::Failed {
                    message: "failed".into(),
                },
            )],
            writes: vec![],
        },
    );
    assert!(!issuer.authorize(identity, ApprovalMode::ReadOnly));
    deliver(
        &mut shared,
        &inbox,
        Outputs {
            events: vec![(99, Event::TurnStarted)],
            writes: vec![],
        },
    );
    assert!(issuer.authorize(identity, ApprovalMode::ReadOnly));
    deliver(
        &mut shared,
        &inbox,
        Outputs {
            events: vec![(
                99,
                Event::TurnCompleted {
                    status: crate::item::TurnStatus::Completed,
                    error: None,
                    stopped: false,
                },
            )],
            writes: vec![],
        },
    );
    assert!(!issuer.authorize(identity, ApprovalMode::ReadOnly));
}

#[test]
fn codex_connection_rotates_and_revokes_leases_but_keeps_open_thread_retry_valid() {
    let server = server();
    let issuer = server.issuer();
    let connection = CodexConnection::new(None, "test", Arc::new(|| {}));
    connection.set_mcp(issuer.clone());
    let first = issuer.issue(7).unwrap();
    let old = first.caller();
    let (_, _, lease) = first.into_parts();
    connection.shared.lock().unwrap().leases.insert(7, lease);
    let second = issuer.issue(7).unwrap();
    let current = second.caller();
    assert_ne!(old.generation, current.generation);
    let (_, _, lease) = second.into_parts();
    let mut shared = connection.shared.lock().unwrap();
    shared.leases.insert(7, lease);
    assert!(!issuer.is_live(old));
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    codex_initialized(&mut driver, &mut out);
    codex_open(&mut driver, None, &mut out);
    codex_thread_ready(&mut driver, &mut out);
    shared.driver = Some(driver);
    deliver(
        &mut shared,
        &connection.inbox,
        Outputs {
            events: vec![(7, start_failed("launch denied".into()))],
            writes: vec![],
        },
    );
    assert!(issuer.is_live(current));
    drop(shared);
    drop(connection);
    assert!(!issuer.is_live(current));
}

#[test]
fn connection_strict_calls_never_start_or_queue_a_provider() {
    let server = server();
    let issuer = server.issuer();
    let credentials = caller(&issuer);
    let identity = credentials.caller();
    let connections: Vec<Box<dyn Connection>> = vec![
        Box::new(ClaudeConnection::new(None, Arc::new(|| {}))),
        Box::new(CodexConnection::new(None, "test", Arc::new(|| {}))),
    ];
    for connection in connections {
        connection.set_mcp(issuer.clone());
        assert!(
            connection
                .steer_from(7, "idle".into(), identity)
                .unwrap_err()
                .starts_with("busy:")
        );
        connection.interrupt_from(7, identity).unwrap();
        assert!(!connection.is_running(7));
        assert!(connection.drain().is_empty());
    }
}

fn live(driver: ClaudeDriver, lease: Lease) -> Live {
    Live {
        _lease: Some(lease),
        driver,
        process: None,
        probe: None,
        pending: Outputs::default(),
        exited: None,
        generation: 1,
        cwd: PathBuf::from("/synthetic"),
        resume: None,
        model: None,
        replacing: false,
        seen: 0,
        epoch: 0,
        idle: Idle::Busy(None),
    }
}

/// A message the launching agent may no longer send leaves the queue, and with it the
/// reason the running process was not idle.
#[test]
fn a_refused_queued_message_no_longer_keeps_the_process_busy() {
    let server = server();
    let (_, _, lease) = server.issuer().issue(7).unwrap().into_parts();
    let connection = ClaudeConnection::new(None, Arc::new(|| {}));
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    claude_init(&mut driver, &mut out);
    claude_lifecycle(&mut driver, &mut out);
    let mut current = live(driver, lease);
    current.process = Some(
        Process::spawn(
            std::path::Path::new("/bin/cat"),
            &[],
            std::path::Path::new("/"),
            &LaunchProfile::default(),
            |_| {},
            |_| {},
        )
        .unwrap(),
    );
    let waiting = Idle::Busy(Some("a message is waiting to be sent"));
    current.idle = waiting.clone();
    {
        let mut shared = connection.starter.shared.lock().unwrap();
        shared.idle = Some(IdlePolicy::after(Duration::from_secs(600)));
        shared.live.insert(7, current);
        shared.queued.insert(7, 1);
    }
    let caller = Caller {
        session: 99,
        generation: 0,
    };
    let job = Job::Message(PathBuf::from("/synthetic"), None, None, Some(caller));
    connection
        .starter
        .run(7, job, |_, _| panic!("the refused message was dispatched"));
    let shared = connection.starter.shared.lock().unwrap();
    assert!(!shared.queued.contains_key(&7));
    assert_ne!(shared.live[&7].idle, waiting);
}

/// The first check's timeout cannot end the check at the interval's end, which may
/// come sooner than that timeout when the interval is short.
#[test]
fn the_first_checks_timeout_does_not_end_the_confirming_check() {
    let server = server();
    let (_, _, lease) = server.issuer().issue(7).unwrap().into_parts();
    let connection = ClaudeConnection::new(None, Arc::new(|| {}));
    let mut out = Outputs::default();
    let driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    let mut current = live(driver, lease);
    let epoch = current.epoch();
    let timer = |confirm| Timer {
        session: 7,
        generation: current.generation,
        epoch,
        kind: TimerKind::CheckTimedOut { confirm },
    };
    let (first, confirming) = (timer(false), timer(true));
    current.idle = Idle::Checking {
        epoch,
        confirm: true,
    };
    connection
        .starter
        .shared
        .lock()
        .unwrap()
        .live
        .insert(7, current);
    connection.starter.fire(first);
    let phase = connection.starter.shared.lock().unwrap().live[&7]
        .idle
        .clone();
    assert!(
        matches!(phase, Idle::Checking { confirm: true, .. }),
        "{phase:?}"
    );
    connection.starter.fire(confirming);
    let phase = connection.starter.shared.lock().unwrap().live[&7]
        .idle
        .clone();
    assert!(matches!(phase, Idle::Resident { .. }), "{phase:?}");
}

#[test]
fn claude_connection_revokes_failed_handshakes_and_drop_but_not_denied_launch_retry() {
    for failure in ["initialize", "mcp", "drop", "denied"] {
        let server = server();
        let issuer = server.issuer();
        let connection = ClaudeConnection::new(None, Arc::new(|| {}));
        connection.set_mcp(issuer.clone());
        let credentials = issuer.issue(7).unwrap();
        let identity = credentials.caller();
        let (url, token, lease) = credentials.into_parts();
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
        driver.set_mcp(url, token);
        driver.command(
            Command::Send {
                session: 7,
                text: "initial".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        if failure != "initialize" {
            claude_init(&mut driver, &mut out);
        }
        if failure == "denied" {
            claude_mcp_ready(&mut driver, &mut out);
            driver.on_line(
                br#"{"type":"result","subtype":"success","result":"done"}"#,
                &mut out,
            );
            let credentials = caller(&issuer);
            let identity = credentials.caller();
            drop(credentials);
            driver.send_from("denied".into(), issuer.clone(), identity, &mut out);
            assert!(!driver.credentials_failed());
            driver.command(
                Command::Send {
                    session: 7,
                    text: "human retry".into(),
                    images: Vec::new(),
                },
                &mut out,
            );
            assert_eq!(
                wire(out.writes.last().unwrap())["message"]["content"],
                "human retry"
            );
        }
        let mut shared = connection.starter.shared.lock().unwrap();
        shared.live.insert(7, live(driver, lease));
        if failure == "initialize" || failure == "mcp" {
            let id = if failure == "initialize" {
                "keystone-1"
            } else {
                "keystone-2"
            };
            let mut out = Outputs::default();
            shared
                .live
                .get_mut(&7)
                .unwrap()
                .driver
                .request_timed_out(id, &mut out);
            claude_deliver(&mut shared, &connection.starter.inbox, 7, out);
            assert!(!issuer.is_live(identity));
        } else if failure == "denied" {
            claude_deliver(
                &mut shared,
                &connection.starter.inbox,
                7,
                Outputs {
                    events: vec![(7, start_failed("launch denied".into()))],
                    writes: vec![],
                },
            );
            assert!(issuer.is_live(identity));
        }
        drop(shared);
        drop(connection);
        assert!(!issuer.is_live(identity));
    }
}

#[test]
fn mcp_credentials_are_not_injected_into_helpers_or_codex_baseline_probes() {
    let server = server();
    let issuer = server.issuer();
    let helper = ClaudeConnection::for_layout_helper(None, Arc::new(|| {}));
    helper.set_mcp(issuer);
    assert!(helper.starter.shared.lock().unwrap().mcp.is_none());
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    driver.command(
        Command::Configure {
            session: 7,
            approval: ApprovalState {
                mode: ApprovalMode::Provider,
                overridden: true,
            },
        },
        &mut out,
    );
    driver.set_mcp(7, "http://127.0.0.1/mcp".into(), "synthetic-token".into());
    codex_open(&mut driver, Some("resumed-thread"), &mut out);
    out = Outputs::default();
    codex_initialized(&mut driver, &mut out);
    let probe = wire(&out.writes[1]);
    assert_eq!(probe["method"], "thread/start");
    assert_eq!(probe["params"]["ephemeral"], true);
    assert!(probe["params"]["config"].is_null());
    assert!(!out.writes[1].contains("synthetic-token"));
    assert_eq!(wire(&out.writes[2])["method"], "thread/resume");
    assert!(out.writes[2].contains("synthetic-token"));
}

#[test]
fn codex_mcp_thread_failures_do_not_echo_config_credentials() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    driver.set_mcp(7, "http://127.0.0.1/mcp".into(), "synthetic-token".into());
    codex_open(&mut driver, None, &mut out);
    codex_initialized(&mut driver, &mut out);
    out = Outputs::default();
    driver.on_line(
        br#"{"id":2,"error":{"code":-32602,"message":"invalid config synthetic-token"}}"#,
        &mut out,
    );
    assert!(!driver.is_open(7));
    assert!(out.writes.is_empty());
    assert!(
        out.events
            .iter()
            .any(|(_, event)| matches!(event, Event::Failed { .. }))
    );
    assert!(!format!("{:?}", out.events).contains("synthetic-token"));
}

#[test]
fn mcp_process_exit_diagnostics_never_echo_authorization_headers() {
    let exit = Exit {
        code: Some(1),
        stderr_tail: "Authorization: Bearer synthetic-token".into(),
    };
    assert_eq!(
        exit_reason("Codex", &exit, true),
        "Codex stopped with exit code 1"
    );
    assert!(exit_reason("Codex", &exit, false).contains("synthetic-token"));
}

#[test]
fn claude_launch_rechecks_authorization_before_starting_any_process() {
    let server = server();
    let issuer = server.issuer();
    let credentials = caller(&issuer);
    let connection = ClaudeConnection::new(None, Arc::new(|| {}));
    connection.set_mcp(issuer.clone());
    connection
        .starter
        .shared
        .lock()
        .unwrap()
        .queued
        .insert(7, 1);
    let identity = credentials.caller();
    drop(credentials);
    connection.starter.run(
        7,
        Job::Message(PathBuf::from("/synthetic"), None, None, Some(identity)),
        |_, _| panic!("unauthorized launch dispatched"),
    );
    let shared = connection.starter.shared.lock().unwrap();
    assert!(shared.live.is_empty());
    assert!(shared.queued.is_empty());
    assert!(shared.program.is_none());
    assert_eq!(shared.starting, 0);
    drop(shared);
    assert!(
        connection
            .drain()
            .iter()
            .any(|(_, event)| matches!(event, Event::Failed { .. }))
    );
}

#[test]
fn current_target_and_caller_modes_are_checked_under_connection_locks() {
    let server = server();
    let issuer = server.issuer();
    let credentials = caller(&issuer);
    let identity = credentials.caller();
    issuer.set_mode(99, ApprovalMode::Ask);
    let connections: Vec<Box<dyn Connection>> = vec![
        Box::new(ClaudeConnection::new(None, Arc::new(|| {}))),
        Box::new(CodexConnection::new(None, "test", Arc::new(|| {}))),
    ];
    for connection in connections {
        connection.set_mcp(issuer.clone());
        connection.set_approval(
            7,
            ApprovalState {
                mode: ApprovalMode::FullAccess,
                overridden: true,
            },
        );
        assert!(
            !connection
                .steer_from(7, "denied".into(), identity)
                .unwrap_err()
                .contains("busy")
        );
        assert!(connection.interrupt_from(7, identity).is_err());
        connection.set_approval(
            7,
            ApprovalState {
                mode: ApprovalMode::Provider,
                overridden: false,
            },
        );
        assert!(
            connection
                .steer_from(7, "inherits".into(), identity)
                .unwrap_err()
                .starts_with("busy:")
        );
        connection.interrupt_from(7, identity).unwrap();
    }
}

/// A Claude driver with both Keystone servers asked for, and the message held for them.
fn claude_with_both_servers() -> (ClaudeDriver, Value) {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    driver.set_mcp("http://127.0.0.1/mcp".into(), "synthetic-token".into());
    driver.set_widgets(true);
    driver.command(
        Command::Send {
            session: 7,
            text: "hello".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    out = Outputs::default();
    claude_init(&mut driver, &mut out);
    let set = wire(&out.writes[0]);
    assert_eq!(set["request"]["subtype"], "mcp_set_servers");
    assert!(set["request"]["servers"]["keystone"].is_object());
    assert!(set["request"]["servers"][surface::SERVER].is_object());
    (driver, set)
}

fn sent_hello(out: &Outputs) -> bool {
    out.writes
        .iter()
        .any(|line| wire(line)["message"]["content"] == "hello")
}

/// A refused widget server leaves the orchestrator's in place: the message goes with
/// no second request and no failure.
#[test]
fn claude_refusing_only_the_widget_server_keeps_the_orchestrator_and_sends() {
    let (mut driver, set) = claude_with_both_servers();
    let mut out = Outputs::default();
    driver.on_line(
        json!({"type":"control_response","response":{"subtype":"success",
            "request_id": set["request_id"],
            "response":{"added":["keystone"],"removed":[],"errors":{"keystone_ui":"refused"}}}})
        .to_string()
        .as_bytes(),
        &mut out,
    );
    assert!(sent_hello(&out), "{:?}", out.writes);
    assert!(out.events.is_empty(), "{:?}", out.events);
    assert!(!driver.credentials_failed());
}

/// A request with both servers that goes unanswered is retried with the
/// orchestrator's alone. If the first request added it after all, Claude lists it as
/// unchanged, and that is still ready.
#[test]
fn claude_widget_timeout_with_the_orchestrator_retries_it_alone_and_sends() {
    let (mut driver, _) = claude_with_both_servers();
    let timed = driver.take_timed_requests();
    let mut out = Outputs::default();
    driver.request_timed_out(timed.last().unwrap(), &mut out);
    let retry = wire(&out.writes[0]);
    assert_eq!(retry["request"]["subtype"], "mcp_set_servers");
    assert!(retry["request"]["servers"]["keystone"].is_object());
    assert!(retry["request"]["servers"][surface::SERVER].is_null());
    assert!(out.events.is_empty(), "{:?}", out.events);
    let mut out = Outputs::default();
    driver.on_line(
        json!({"type":"control_response","response":{"subtype":"success",
            "request_id": retry["request_id"],
            "response":{"added":[],"removed":[],"errors":{}}}})
        .to_string()
        .as_bytes(),
        &mut out,
    );
    assert!(sent_hello(&out), "{:?}", out.writes);
    assert!(out.events.is_empty(), "{:?}", out.events);
    assert!(!driver.credentials_failed());
}

fn mcp_status(out: &Outputs) -> Vec<Vec<(String, Health)>> {
    out.events
        .iter()
        .filter_map(|(_, event)| match event {
            Event::McpStatus(servers) => {
                Some(servers.iter().map(|s| (s.name.clone(), s.health)).collect())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn claude_reports_init_mcp_servers_without_keystones_own() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    driver.set_mcp("http://127.0.0.1/mcp".into(), "synthetic-token".into());
    driver.command(
        Command::Send {
            session: 7,
            text: "hello".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    claude_init(&mut driver, &mut out);
    claude_mcp_ready(&mut driver, &mut out);
    out = Outputs::default();
    driver.on_line(br#"{"type":"system","subtype":"init","session_id":"c","mcp_servers":[{"name":"keystone","status":"connected"},{"name":"fixture-ok","status":"connected"},{"name":"fixture-bad","status":"failed"},{"name":"remote","status":"needs-auth"},{"name":"odd","status":"novel"}]}"#, &mut out);
    assert_eq!(
        mcp_status(&out),
        [vec![
            ("fixture-ok".into(), Health::Connected),
            ("fixture-bad".into(), Health::Failed),
            ("remote".into(), Health::NeedsAuth),
        ]]
    );
}

#[test]
fn claude_init_with_odd_mcp_servers_still_starts_the_session() {
    for servers in [
        r#"[{"name":3,"status":"connected"},{"name":"fixture-ok","status":null},{"name":"good","status":"connected"},"x"]"#,
        r#""not a list""#,
    ] {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
        driver.on_line(
            format!(r#"{{"type":"system","subtype":"init","session_id":"c","model":"m","mcp_servers":{servers}}}"#)
                .as_bytes(),
            &mut out,
        );
        assert!(
            out.events
                .iter()
                .any(|(_, e)| matches!(e, Event::ThreadReady { thread_id } if thread_id == "c")),
            "{servers}: {:?}",
            out.events
        );
        let expected: Vec<Vec<(String, Health)>> = if servers.starts_with('[') {
            vec![vec![("good".into(), Health::Connected)]]
        } else {
            vec![]
        };
        assert_eq!(mcp_status(&out), expected, "{servers}");
    }
}

#[test]
fn claude_without_keystones_server_reports_a_user_server_named_keystone() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(7, false, 1, ApprovalMode::Provider, &mut out);
    driver.on_line(br#"{"type":"system","subtype":"init","session_id":"c","mcp_servers":[{"name":"keystone","status":"pending"}]}"#, &mut out);
    assert_eq!(
        mcp_status(&out),
        [vec![("keystone".into(), Health::Pending)]]
    );
}

#[test]
fn codex_reports_each_threads_mcp_startup_as_one_list() {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("test", &mut out);
    let opt_out =
        wire(&out.writes[0])["params"]["capabilities"]["optOutNotificationMethods"].clone();
    assert!(
        !opt_out
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "mcpServer/startupStatus/updated")
    );
    driver.set_mcp(7, "http://127.0.0.1/mcp".into(), "synthetic-token".into());
    codex_open(&mut driver, None, &mut out);
    codex_initialized(&mut driver, &mut out);
    codex_thread_ready(&mut driver, &mut out);
    out = Outputs::default();
    for (name, status, error, reason) in [
        ("keystone", "ready", None, None),
        ("fixture-ok", "starting", None, None),
        (
            "fixture-bad",
            "failed",
            Some(
                "error sending request for url (https://u:fixture-secret@host/mcp?token=fixture-secret)",
            ),
            None,
        ),
        ("fixture-ok", "ready", None, None),
        (
            "remote",
            "failed",
            Some("sign in"),
            Some("reauthenticationRequired"),
        ),
    ] {
        driver.on_line(
            json!({"method": "mcpServer/startupStatus/updated", "params": {"threadId": "thread", "name": name, "status": status, "error": error, "failureReason": reason}})
                .to_string()
                .as_bytes(),
            &mut out,
        );
    }
    driver.on_line(br#"{"method":"mcpServer/startupStatus/updated","params":{"threadId":"other","name":"x","status":"ready"}}"#, &mut out);
    let reports = mcp_status(&out);
    assert_eq!(reports.len(), 4, "{reports:?}");
    assert_eq!(
        reports[3],
        [
            ("fixture-bad".into(), Health::Failed),
            ("fixture-ok".into(), Health::Connected),
            ("remote".into(), Health::NeedsAuth),
        ]
    );
    let events = format!("{:?}", out.events);
    assert!(!events.contains("fixture-secret"), "{events}");
}
