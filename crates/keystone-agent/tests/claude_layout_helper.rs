use keystone_agent::{
    ApprovalState, ClaudeConnection, Command, Event, ItemKind, ItemStatus, TurnStatus, Wake, claude,
};
use keystone_core::ApprovalMode;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const HELPER_FLAGS: [&str; 13] = [
    "--permission-mode",
    "plan",
    "--tools",
    "",
    "--strict-mcp-config",
    "--mcp-config",
    r#"{"mcpServers":{}}"#,
    "--disable-slash-commands",
    "--safe-mode",
    "--settings",
    r#"{"disableAllHooks":true}"#,
    "--no-session-persistence",
    "--include-hook-events",
];

fn connect(
    build: fn(Option<PathBuf>, Wake) -> ClaudeConnection,
) -> (ClaudeConnection, Receiver<()>) {
    let (tx, rx) = channel();
    let wake: Wake = Arc::new(move || {
        let _ = tx.send(());
    });
    let program = PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-claude"));
    (build(Some(program), wake), rx)
}

fn project(settings: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for log in ["argv", "messages", "pids"] {
        std::fs::write(dir.path().join(format!(".fake-claude-{log}")), "").unwrap();
    }
    for (setting, value) in settings {
        std::fs::write(dir.path().join(format!(".fake-claude-{setting}")), value).unwrap();
    }
    dir
}

fn lines(dir: &Path, log: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(format!(".fake-claude-{log}")))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn argv(dir: &Path) -> Vec<Vec<String>> {
    let parse = |line: String| serde_json::from_str(&line).unwrap();
    lines(dir, "argv").into_iter().map(parse).collect()
}

fn messages(dir: &Path) -> Vec<String> {
    let parse = |line: String| serde_json::from_str(&line).unwrap();
    lines(dir, "messages").into_iter().map(parse).collect()
}

fn helper_argv() -> Vec<String> {
    claude::ARGS
        .iter()
        .chain(&HELPER_FLAGS)
        .map(|arg| arg.to_string())
        .collect()
}

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

fn failure(events: &[Event]) -> String {
    match events.last() {
        Some(Event::TurnCompleted {
            status: TurnStatus::Failed,
            error: Some(error),
            ..
        }) => error.clone(),
        _ => panic!("expected a failed turn, got {events:?}"),
    }
}

fn mode(mode: ApprovalMode) -> ApprovalState {
    ApprovalState {
        mode,
        overridden: true,
    }
}

fn kill(conn: &ClaudeConnection, wake: &Receiver<()>) {
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
}

#[test]
fn every_helper_process_is_restricted_in_plan_mode_and_never_resumes_or_probes() {
    let dir = project(&[("ignore-initialize", "all")]);
    let cwd = dir.path().to_str().unwrap();
    let (conn, wake) = connect(ClaudeConnection::for_layout_helper);
    conn.set_mode_timeout(Duration::from_millis(300));
    let provider = mode(ApprovalMode::Provider);
    conn.send(1, cwd, Some("saved-conversation"), provider, "first".into())
        .unwrap();
    assert!(failure(&until(&conn, &wake, completed)).contains("did not start"));

    std::fs::remove_file(dir.path().join(".fake-claude-ignore-initialize")).unwrap();
    conn.command(Command::Configure {
        session: 1,
        approval: mode(ApprovalMode::FullAccess),
    });
    conn.set_approval(1, mode(ApprovalMode::Ask));
    let full = mode(ApprovalMode::FullAccess);
    conn.send(
        1,
        cwd,
        Some("saved-conversation"),
        full,
        "run second".into(),
    )
    .unwrap();
    let events = until(&conn, &wake, |e| matches!(e, Event::ApprovalRequested(_)));
    let Some(Event::ApprovalRequested(approval)) = events.last() else {
        unreachable!()
    };
    conn.command(Command::Answer {
        session: 1,
        request: approval.request.clone(),
        allow: true,
    });
    let events = until(&conn, &wake, completed);
    let command = events.iter().find_map(|e| match e {
        Event::ItemCompleted(item) if matches!(item.kind, ItemKind::Command { .. }) => Some(item),
        _ => None,
    });
    assert!(
        command.is_some_and(|c| c.status == ItemStatus::Declined && c.text.contains("denied")),
        "an allow from the caller reaches Claude as a denial: {events:?}"
    );
    assert!(reply(&events).unwrap().contains("**Mode:** `plan`"));

    kill(&conn, &wake);
    conn.send(1, cwd, Some("saved-conversation"), full, "third".into())
        .unwrap();
    assert!(
        reply(&until(&conn, &wake, completed))
            .unwrap()
            .contains("**Mode:** `plan`")
    );

    assert_eq!(argv(dir.path()), vec![helper_argv(); 3]);
    assert_eq!(messages(dir.path()), ["run second", "third"]);
}

#[test]
fn the_helper_fails_when_claude_reports_or_leaves_out_what_its_flags_turn_off() {
    let cases: [(&str, &str, &str, bool); 6] = [
        (
            "init-commands",
            r#"[{"name":"x"}]"#,
            "Claude Code 2.1.300 listed 1 slash commands or skills",
            false,
        ),
        (
            "init-commands",
            "missing",
            "Claude Code 2.1.300 did not list its slash commands at startup, so Keystone could not confirm",
            false,
        ),
        (
            "hook",
            "1",
            "Claude Code ran a hook although Keystone turned hooks off",
            false,
        ),
        (
            "init-tools",
            r#"["Bash","Read"]"#,
            "Claude Code 2.1.300 listed 2 tools although",
            true,
        ),
        (
            "init-tools",
            "missing",
            "Claude Code 2.1.300 did not list its tools, so Keystone could not confirm",
            true,
        ),
        ("init-tools", r#""Bash""#, "did not list its tools", true),
    ];
    for (setting, value, expected, written) in cases {
        let dir = project(&[(setting, value)]);
        let (conn, wake) = connect(ClaudeConnection::for_layout_helper);
        let cwd = dir.path().to_str().unwrap();
        conn.send(
            1,
            cwd,
            None,
            mode(ApprovalMode::ReadOnly),
            "a layout".into(),
        )
        .unwrap();
        let events = until(&conn, &wake, completed);
        let error = failure(&events);
        assert!(error.contains(expected), "{setting}={value}: {error}");
        let consequence = if written {
            "The request was stopped and its reply not used."
        } else {
            "The request was not sent."
        };
        assert!(error.ends_with(consequence), "{setting}={value}: {error}");
        assert_eq!(
            messages(dir.path()).len(),
            usize::from(written),
            "{setting}={value}"
        );
        std::thread::sleep(Duration::from_millis(400));
        let later: Vec<_> = conn.drain().into_iter().map(|(_, e)| e).collect();
        assert!(
            reply(&events).is_none() && later.is_empty(),
            "{setting}={value}: {later:?}"
        );
    }
}

#[test]
fn a_claude_that_rejects_a_helper_flag_fails_with_its_reason() {
    let dir = project(&[("reject-flag", "--safe-mode")]);
    let (conn, wake) = connect(ClaudeConnection::for_layout_helper);
    let cwd = dir.path().to_str().unwrap();
    conn.send(
        1,
        cwd,
        None,
        mode(ApprovalMode::ReadOnly),
        "a layout".into(),
    )
    .unwrap();
    let error = failure(&until(&conn, &wake, completed));
    assert!(error.contains("unknown option '--safe-mode'"), "{error}");
    assert_eq!(argv(dir.path()), [helper_argv()]);
    assert!(messages(dir.path()).is_empty());
}

#[test]
fn chat_is_launched_and_answered_as_before() {
    let dir = project(&[("init-commands", "missing")]);
    let (conn, wake) = connect(ClaudeConnection::new);
    let cwd = dir.path().to_str().unwrap();
    conn.send(1, cwd, None, mode(ApprovalMode::ReadOnly), "hello".into())
        .unwrap();
    let events = until(&conn, &wake, completed);
    assert!(reply(&events).unwrap().contains("**Mode:** `plan`"));
    let expected: Vec<String> = claude::args(ApprovalMode::ReadOnly)
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(argv(dir.path()), [expected]);
}

#[test]
fn a_success_without_system_init_cannot_confirm_helper_capabilities() {
    for helper in [true, false] {
        let dir = project(&[("skip-init", "1")]);
        let build = if helper {
            ClaudeConnection::for_layout_helper
        } else {
            ClaudeConnection::new
        };
        let (conn, wake) = connect(build);
        conn.send(
            1,
            dir.path().to_str().unwrap(),
            None,
            mode(ApprovalMode::ReadOnly),
            "a layout".into(),
        )
        .unwrap();
        let events = until(&conn, &wake, completed);
        if helper {
            assert!(failure(&events).contains("could not confirm"));
            assert!(!events.iter().any(|event| matches!(
                event,
                Event::TurnCompleted {
                    status: TurnStatus::Completed,
                    ..
                }
            )));
        } else {
            assert!(events.iter().any(|event| matches!(
                event,
                Event::TurnCompleted {
                    status: TurnStatus::Completed,
                    ..
                }
            )));
        }
    }
}

fn scripted_init(permission: &str) -> serde_json::Value {
    serde_json::json!({"type":"system", "subtype":"init", "session_id":"scripted",
        "tools":[], "mcp_servers":[], "slash_commands":[], "skills":[],
        "permissionMode":permission, "claude_code_version":"test-version"})
}

fn scripted_reply(failed: bool) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"type":"assistant", "message":{"id":"layout-reply","content":[{"type":"text",
            "text":"{\"side\":\"right\",\"width\":360}"}]}}),
        serde_json::json!({"type":"result", "subtype":if failed { "error_during_execution" } else { "success" },
            "is_error":failed, "errors":if failed { vec!["provider diagnostic"] } else { vec![] }}),
    ]
}

fn scripted_turn(
    helper: bool,
    setting: &str,
    frames: Vec<serde_json::Value>,
) -> (Vec<Event>, Vec<String>) {
    let frames = serde_json::to_string(&frames).unwrap();
    let dir = project(&[(setting, &frames)]);
    let build = if helper {
        ClaudeConnection::for_layout_helper
    } else {
        ClaudeConnection::new
    };
    let (conn, wake) = connect(build);
    conn.send(
        1,
        dir.path().to_str().unwrap(),
        None,
        mode(ApprovalMode::ReadOnly),
        "a layout".into(),
    )
    .unwrap();
    let events = until(&conn, &wake, completed);
    std::thread::sleep(Duration::from_millis(150));
    let later = conn.drain();
    assert!(
        later.is_empty(),
        "late frames escaped refusal or completion: {later:?}"
    );
    (events, messages(dir.path()))
}

#[test]
fn unsolicited_success_requires_initialize_and_a_written_helper_turn() {
    for helper in [true, false] {
        let mut frames = vec![scripted_init("plan")];
        frames.extend(scripted_reply(false));
        if helper {
            frames.push(serde_json::json!({"type":"control_response", "response":{
                "subtype":"success", "request_id":"keystone-1", "response":{
                    "commands":[], "current_permission_mode":"plan"}}}));
            frames.push(scripted_init("plan"));
            frames.extend(scripted_reply(false));
        }
        let (events, messages) = scripted_turn(helper, "initialize-frames", frames);
        assert!(
            messages.is_empty(),
            "the unanswered initialize must hold the prompt"
        );
        if helper {
            assert!(failure(&events).contains("could not confirm"));
        } else {
            assert!(reply(&events).unwrap().contains("right"));
            assert!(events.iter().any(|e| matches!(
                e,
                Event::TurnCompleted {
                    status: TurnStatus::Completed,
                    ..
                }
            )));
        }
    }
}

fn non_plan_report(subtype: &str) {
    for helper in [true, false] {
        let contradiction = if subtype == "init" {
            scripted_init("bypassPermissions")
        } else {
            serde_json::json!({"type":"system", "subtype":"status", "permissionMode":"default"})
        };
        let mut frames = vec![contradiction, scripted_init("plan")];
        frames.extend(scripted_reply(false));
        let (events, messages) = scripted_turn(helper, "turn-frames", frames);
        assert_eq!(messages, ["a layout"]);
        if helper {
            assert!(failure(&events).contains("permission mode"), "{events:?}");
            assert!(
                reply(&events).is_none(),
                "late assistant text escaped refusal: {events:?}"
            );
        } else {
            assert!(reply(&events).unwrap().contains("right"));
            assert!(events.iter().any(|e| matches!(
                e,
                Event::TurnCompleted {
                    status: TurnStatus::Completed,
                    ..
                }
            )));
        }
    }
}

#[test]
fn non_plan_init_is_terminal_for_helper_and_compatible_with_chat() {
    non_plan_report("init");
}

#[test]
fn non_plan_status_is_terminal_for_helper_and_compatible_with_chat() {
    non_plan_report("status");
}

#[test]
fn unanswered_initialize_preserves_provider_failure_diagnostics() {
    for helper in [true, false] {
        let mut frames = vec![scripted_init("plan")];
        frames.extend(scripted_reply(true));
        let (events, messages) = scripted_turn(helper, "initialize-frames", frames);
        assert!(messages.is_empty());
        assert!(failure(&events).contains("provider diagnostic"));
    }
}
