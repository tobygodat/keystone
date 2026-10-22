//! Both drivers host `show_ui`: Claude Code through an SDK-hosted MCP server added
//! with `mcp_set_servers`, Codex through a `dynamicTools` entry on `thread/start`. Wire
//! shapes follow the no-inference probe in `tests/fixtures/show-ui/`.

use keystone_agent::claude::ClaudeDriver;
use keystone_agent::codex::CodexDriver;
use keystone_agent::surface::{self, Status};
use keystone_agent::{Command, Event, Outputs};
use keystone_core::ApprovalMode;
use serde_json::{Value, json};

const SESSION: u64 = 3;

fn parse(line: &str) -> Value {
    serde_json::from_str(line).unwrap()
}

fn line(value: Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

/// A Claude driver past `initialize`, with the widget server asked for and, when
/// `accept`, accepted. Returns the driver and what the last step wrote.
fn claude(accept: bool) -> (ClaudeDriver, Outputs) {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, 1, ApprovalMode::Provider, &mut out);
    driver.set_widgets(true);
    driver.command(
        Command::Send {
            session: SESSION,
            text: "show a checklist".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    // Nothing is written before Claude answers `initialize`.
    assert_eq!(out.writes.len(), 1);
    let mut out = Outputs::default();
    driver.on_line(
        &line(json!({"type": "control_response", "response":
            {"subtype": "success", "request_id": "keystone-1", "response": {}}})),
        &mut out,
    );
    let set = parse(&out.writes[0]);
    assert_eq!(set["request"]["subtype"], "mcp_set_servers");
    assert_eq!(
        set["request"]["servers"],
        json!({"keystone_ui": {"type": "sdk", "name": "keystone_ui", "timeout": 86_400_000,
            "disableAutoBackground": true}})
    );
    // The message waits for the answer, so the tool is there for its turn.
    assert_eq!(out.writes.len(), 1);
    let answer = if accept {
        json!({"added": ["keystone_ui"], "removed": [], "errors": {}})
    } else {
        json!({"added": [], "removed": [], "errors": {"keystone_ui": "refused"}})
    };
    let mut out = Outputs::default();
    driver.on_line(
        &line(json!({"type": "control_response", "response":
            {"subtype": "success", "request_id": set["request_id"], "response": answer}})),
        &mut out,
    );
    let sent = parse(out.writes.last().unwrap());
    assert_eq!(sent["type"], "user");
    (driver, out)
}

fn mcp(driver: &mut ClaudeDriver, request: &str, message: Value) -> Outputs {
    let mut out = Outputs::default();
    driver.on_line(
        &line(
            json!({"type": "control_request", "request_id": request, "request":
            {"subtype": "mcp_message", "server_name": "keystone_ui", "message": message}}),
        ),
        &mut out,
    );
    out
}

fn rpc(out: &Outputs) -> Value {
    let reply = parse(&out.writes[0]);
    assert_eq!(reply["type"], "control_response");
    assert_eq!(reply["response"]["subtype"], "success");
    reply["response"]["response"]["mcp_response"].clone()
}

fn call(driver: &mut ClaudeDriver, request: &str, tool_use: &str, arguments: Value) -> Outputs {
    mcp(
        driver,
        request,
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
            "name": "show_ui", "arguments": arguments,
            "_meta": {"claudecode/toolUseId": tool_use}}}),
    )
}

#[test]
fn claude_hosts_the_tool_and_answers_the_mcp_handshake() {
    let (mut driver, _) = claude(true);
    let init = rpc(&mcp(
        &mut driver,
        "r1",
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {}}}),
    ));
    assert_eq!(init["id"], 0);
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(init["result"]["serverInfo"]["name"], "keystone_ui");
    let note = rpc(&mcp(
        &mut driver,
        "r2",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    ));
    assert_eq!(note["result"], json!({}));
    let list = rpc(&mcp(
        &mut driver,
        "r3",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    ));
    let tool = &list["result"]["tools"][0];
    assert_eq!(tool["name"], "show_ui");
    assert!(
        tool["description"]
            .as_str()
            .unwrap()
            .contains(surface::CATALOG)
    );
    assert_eq!(tool["inputSchema"]["required"], json!(["messages"]));
    let unknown = rpc(&mcp(
        &mut driver,
        "r4",
        json!({"jsonrpc": "2.0", "id": 2, "method": "resources/list"}),
    ));
    assert_eq!(unknown["error"]["code"], -32601);

    // The tool runs without asking: it changes nothing but Keystone's transcript.
    let mut out = Outputs::default();
    driver.on_line(
        &line(
            json!({"type": "control_request", "request_id": "perm", "request": {
            "subtype": "can_use_tool", "tool_name": "mcp__keystone_ui__show_ui",
            "input": {"messages": []}, "tool_use_id": "toolu_1"}}),
        ),
        &mut out,
    );
    let allow = parse(&out.writes[0]);
    assert_eq!(allow["response"]["response"]["behavior"], "allow");
    assert!(
        out.events.is_empty(),
        "no approval is shown: {:?}",
        out.events
    );
}

#[test]
fn claude_returns_a_press_as_the_tool_result_and_refuses_invalid_calls() {
    let (mut driver, _) = claude(true);
    let bad = call(&mut driver, "c1", "toolu_bad", surface::examples::invalid());
    let result = &rpc(&bad)["result"];
    assert_eq!(result["isError"], true);
    let error = parse(result["content"][0]["text"].as_str().unwrap());
    assert_eq!(error["error"]["code"], "VALIDATION_FAILED");
    assert_eq!(
        error["error"]["path"],
        "/messages/1/updateComponents/components/11/child"
    );
    assert!(bad.events.is_empty());

    let shown = call(
        &mut driver,
        "c2",
        "toolu_ok",
        surface::examples::checklist(),
    );
    assert!(shown.writes.is_empty(), "the call waits for a press");
    let [
        (
            SESSION,
            Event::SurfaceShown {
                surface,
                waiting: true,
            },
        ),
    ] = shown.events.as_slice()
    else {
        panic!("{:?}", shown.events)
    };
    assert_eq!(surface.call, "toolu_ok");
    assert_eq!(surface.status, Status::Waiting);

    let mut out = Outputs::default();
    let action = surface.action("send", 0).unwrap();
    driver.command(
        Command::Act {
            session: SESSION,
            call: "toolu_ok".into(),
            text: action.clone(),
        },
        &mut out,
    );
    let reply = parse(&out.writes[0]);
    assert_eq!(reply["response"]["request_id"], "c2");
    let rpc = &reply["response"]["response"]["mcp_response"];
    assert_eq!(rpc["id"], 7);
    assert_eq!(rpc["result"]["content"][0]["text"], action);

    // A second press finds no waiting call, so it goes back to the app to send.
    let mut out = Outputs::default();
    driver.command(
        Command::Act {
            session: SESSION,
            call: "toolu_ok".into(),
            text: action.clone(),
        },
        &mut out,
    );
    assert!(out.writes.is_empty());
    assert_eq!(
        out.events,
        [(
            SESSION,
            Event::ActionUndelivered {
                call: "toolu_ok".into(),
                text: action
            }
        )]
    );
}

#[test]
fn claude_drops_a_waiting_call_when_claude_cancels_it_or_exits() {
    let (mut driver, _) = claude(true);
    call(&mut driver, "c1", "toolu_1", surface::examples::checklist());
    let mut out = Outputs::default();
    driver.on_line(
        &line(json!({"type": "control_cancel_request", "request_id": "c1"})),
        &mut out,
    );
    driver.command(
        Command::Act {
            session: SESSION,
            call: "toolu_1".into(),
            text: "{}".into(),
        },
        &mut out,
    );
    assert!(matches!(
        out.events.as_slice(),
        [(_, Event::ActionUndelivered { .. })]
    ));

    call(&mut driver, "c2", "toolu_2", surface::examples::checklist());
    let mut out = Outputs::default();
    driver.on_exit("Claude Code stopped", &mut out);
    driver.command(
        Command::Act {
            session: SESSION,
            call: "toolu_2".into(),
            text: "{}".into(),
        },
        &mut out,
    );
    assert!(
        out.events
            .iter()
            .any(|(_, e)| matches!(e, Event::ActionUndelivered { .. }))
    );
}

#[test]
fn a_surface_without_a_button_is_answered_at_once() {
    let (mut driver, _) = claude(true);
    let mut arguments = surface::examples::checklist();
    let components = arguments["messages"][1]["updateComponents"]["components"]
        .as_array_mut()
        .unwrap();
    components.truncate(4);
    components[1]["children"] = json!(["title", "hint"]);
    let out = call(&mut driver, "c1", "toolu_1", arguments);
    let text = rpc(&out)["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.contains("no Button"), "{text}");
    assert!(matches!(
        out.events.as_slice(),
        [(_, Event::SurfaceShown { waiting: false, .. })]
    ));
}

#[test]
fn claude_goes_on_without_the_tool_when_it_refuses_the_server() {
    let (mut driver, out) = claude(false);
    assert!(
        out.events.is_empty(),
        "a refusal is not an error: {:?}",
        out.events
    );
    let mut out = Outputs::default();
    driver.on_line(
        &line(
            json!({"type": "control_request", "request_id": "perm", "request": {
            "subtype": "can_use_tool", "tool_name": "mcp__keystone_ui__show_ui",
            "input": {}, "tool_use_id": "toolu_1"}}),
        ),
        &mut out,
    );
    // The tool is no longer Keystone's to allow, so it is asked like any other.
    assert!(matches!(
        out.events.as_slice(),
        [(_, Event::ApprovalRequested(_))]
    ));
}

#[test]
fn a_widget_server_that_never_answers_only_leaves_the_tool_out() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, 1, ApprovalMode::Provider, &mut out);
    driver.set_widgets(true);
    driver.command(
        Command::Send {
            session: SESSION,
            text: "hello".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    let mut out = Outputs::default();
    driver.on_line(
        &line(json!({"type": "control_response", "response":
            {"subtype": "success", "request_id": "keystone-1", "response": {}}})),
        &mut out,
    );
    let mut timed = driver.take_timed_requests();
    assert_eq!(timed.len(), 2);
    let mut out = Outputs::default();
    driver.request_timed_out(&timed.pop().unwrap(), &mut out);
    assert_eq!(parse(&out.writes[0])["type"], "user");
    assert!(out.events.is_empty(), "{:?}", out.events);
    assert!(!driver.needs_restart());
}

fn codex(widgets: bool) -> (CodexDriver, Value) {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::with_widgets("0.0.0-test", widgets, &mut out);
    let init = parse(&out.writes[0]);
    assert_eq!(
        init["params"]["capabilities"]["experimentalApi"].as_bool(),
        widgets.then_some(true)
    );
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/repo".into(),
            resume: None,
            model: None,
        },
        &mut out,
    );
    let mut out = Outputs::default();
    driver.on_line(br#"{"id":1,"result":{"userAgent":"test"}}"#, &mut out);
    (driver, parse(&out.writes[1]))
}

fn open(driver: &mut CodexDriver, id: u64) -> Outputs {
    let mut out = Outputs::default();
    driver.on_line(
        &line(json!({"id": id, "result": {"thread": {"id": "thread-1"}}})),
        &mut out,
    );
    out
}

fn turn(driver: &mut CodexDriver, method: &str, id: &str) {
    driver.on_line(
        &line(json!({"method": method, "params": {"threadId": "thread-1",
            "turn": {"id": id, "status": "completed"}}})),
        &mut Outputs::default(),
    );
}

fn tool_call(driver: &mut CodexDriver, id: u64, call: &str, arguments: Value) -> Outputs {
    let mut out = Outputs::default();
    driver.on_line(
        &line(
            json!({"id": id, "method": "item/tool/call", "params": {"threadId": "thread-1",
            "turnId": "turn-1", "callId": call, "tool": "show_ui", "arguments": arguments}}),
        ),
        &mut out,
    );
    out
}

#[test]
fn codex_offers_the_tool_on_new_threads_and_answers_calls() {
    let (mut driver, start) = codex(true);
    assert_eq!(start["method"], "thread/start");
    assert_eq!(start["params"]["dynamicTools"][0]["name"], "show_ui");
    assert_eq!(start["params"]["dynamicTools"][0]["type"], "function");
    open(&mut driver, 2);
    turn(&mut driver, "turn/started", "turn-1");

    let bad = tool_call(&mut driver, 50, "call-bad", surface::examples::invalid());
    let reply = parse(&bad.writes[0]);
    assert_eq!(reply["id"], 50);
    assert_eq!(reply["result"]["success"], false);
    let error = parse(reply["result"]["contentItems"][0]["text"].as_str().unwrap());
    assert_eq!(error["error"]["code"], "VALIDATION_FAILED");

    let shown = tool_call(&mut driver, 51, "call-ok", surface::examples::form());
    assert!(shown.writes.is_empty());
    let [
        (
            SESSION,
            Event::SurfaceShown {
                surface,
                waiting: true,
            },
        ),
    ] = shown.events.as_slice()
    else {
        panic!("{:?}", shown.events)
    };
    let action = surface.action("submit", 0).unwrap();
    let mut out = Outputs::default();
    driver.command(
        Command::Act {
            session: SESSION,
            call: "call-ok".into(),
            text: action.clone(),
        },
        &mut out,
    );
    assert_eq!(
        parse(&out.writes[0]),
        json!({"id": 51, "result": {"contentItems": [{"type": "inputText", "text": action}], "success": true}})
    );

    // Codex names the item after the call; it shows as the show_ui tool.
    let mut out = Outputs::default();
    driver.on_line(
        &line(
            json!({"method": "item/completed", "params": {"threadId": "thread-1", "turnId": "turn-1",
            "item": {"type": "dynamicToolCall", "id": "call-ok", "tool": "show_ui", "arguments": {},
                "status": "completed", "success": true,
                "contentItems": [{"type": "inputText", "text": "done"}]}}}),
        ),
        &mut out,
    );
    let [(_, Event::ItemCompleted(item))] = out.events.as_slice() else {
        panic!("{:?}", out.events)
    };
    assert_eq!(
        item.kind,
        keystone_agent::ItemKind::Tool {
            name: "show_ui".into()
        }
    );
    assert_eq!(item.text, "done");
}

#[test]
fn codex_starts_the_thread_without_the_tool_when_it_is_refused() {
    let (mut driver, start) = codex(true);
    let mut out = Outputs::default();
    driver.on_line(
        &line(json!({"id": start["id"], "error": {"code": -32600,
            "message": "thread/start.dynamicTools requires experimentalApi capability"}})),
        &mut out,
    );
    let retry = parse(&out.writes[0]);
    assert_eq!(retry["method"], "thread/start");
    assert!(retry["params"].get("dynamicTools").is_none());
    assert!(out.events.is_empty());
    let out = open(&mut driver, retry["id"].as_u64().unwrap());
    assert!(matches!(
        out.events.as_slice(),
        [(_, Event::ThreadReady { .. })]
    ));
    // Later threads are not offered it again.
    let mut out = Outputs::default();
    driver.command(
        Command::Open {
            session: SESSION + 1,
            cwd: "/tmp/repo".into(),
            resume: None,
            model: None,
        },
        &mut out,
    );
    assert!(
        parse(&out.writes[0])["params"]
            .get("dynamicTools")
            .is_none()
    );
}

/// A call from a turn that already ended, or a repeated call ID, is refused rather
/// than shown waiting with no turn to answer it.
#[test]
fn codex_refuses_calls_from_an_ended_turn_and_repeated_calls() {
    let (mut driver, _) = codex(true);
    open(&mut driver, 2);
    let idle = tool_call(&mut driver, 50, "call-0", surface::examples::checklist());
    assert_eq!(parse(&idle.writes[0])["result"]["success"], false);
    assert!(idle.events.is_empty());

    turn(&mut driver, "turn/started", "turn-1");
    turn(&mut driver, "turn/completed", "turn-1");
    turn(&mut driver, "turn/started", "turn-2");
    let stale = tool_call(&mut driver, 51, "call-1", surface::examples::checklist());
    assert_eq!(parse(&stale.writes[0])["result"]["success"], false);
    assert!(stale.events.is_empty());

    let mut out = Outputs::default();
    driver.on_line(
        &line(
            json!({"id": 52, "method": "item/tool/call", "params": {"threadId": "thread-1",
            "turnId": "turn-2", "callId": "call-2", "tool": "show_ui",
            "arguments": surface::examples::checklist()}}),
        ),
        &mut out,
    );
    assert!(out.writes.is_empty(), "the call waits for the user");
    let mut again = Outputs::default();
    driver.on_line(
        &line(
            json!({"id": 53, "method": "item/tool/call", "params": {"threadId": "thread-1",
            "turnId": "turn-2", "callId": "call-2", "tool": "show_ui",
            "arguments": surface::examples::checklist()}}),
        ),
        &mut again,
    );
    assert_eq!(parse(&again.writes[0])["id"], 53);
    assert_eq!(parse(&again.writes[0])["result"]["success"], false);
    assert!(again.events.is_empty());
}

/// While a turn starts, no call is shown: Codex sends `turn/started` before the
/// turn's items. A call ID answered once, or shown without waiting, is not shown again.
#[test]
fn codex_refuses_calls_while_a_turn_starts_and_replays_of_answered_calls() {
    let (mut driver, _) = codex(true);
    open(&mut driver, 2);
    let mut out = Outputs::default();
    driver.command(
        Command::Send {
            session: SESSION,
            text: "go".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    let starting = tool_call(&mut driver, 50, "call-0", surface::examples::checklist());
    assert_eq!(parse(&starting.writes[0])["result"]["success"], false);
    assert!(starting.events.is_empty());

    turn(&mut driver, "turn/started", "turn-1");
    let shown = tool_call(&mut driver, 51, "call-1", surface::examples::checklist());
    assert!(shown.writes.is_empty());
    let mut out = Outputs::default();
    driver.command(
        Command::Act {
            session: SESSION,
            call: "call-1".into(),
            text: "{}".into(),
        },
        &mut out,
    );
    assert_eq!(parse(&out.writes[0])["id"], 51);
    let replay = tool_call(&mut driver, 52, "call-1", surface::examples::checklist());
    assert_eq!(parse(&replay.writes[0])["result"]["success"], false);
    assert!(replay.events.is_empty());

    let mut buttonless = surface::examples::checklist();
    let components = buttonless["messages"][1]["updateComponents"]["components"]
        .as_array_mut()
        .unwrap();
    components.truncate(8);
    components[1]["children"] = json!(["title", "hint", "docs", "tests", "changelog", "divider"]);
    let first = tool_call(&mut driver, 53, "call-2", buttonless.clone());
    assert_eq!(parse(&first.writes[0])["result"]["success"], true);
    let again = tool_call(&mut driver, 54, "call-2", buttonless);
    assert_eq!(parse(&again.writes[0])["result"]["success"], false);
    assert!(again.events.is_empty());
}

/// Only Codex's refusal of the tool itself turns widgets off; a thread's own error
/// leaves them on for the next thread.
#[test]
fn codex_keeps_the_tool_after_an_unrelated_thread_start_error() {
    let (mut driver, start) = codex(true);
    let mut out = Outputs::default();
    driver.on_line(
        &line(json!({"id": start["id"], "error": {"code": -32600,
            "message": "cwd does not exist"}})),
        &mut out,
    );
    assert!(
        out.writes
            .iter()
            .all(|line| parse(line)["method"] != "thread/start"),
        "no retry without the tool"
    );
    let mut out = Outputs::default();
    driver.command(
        Command::Open {
            session: SESSION + 1,
            cwd: "/tmp/repo".into(),
            resume: None,
            model: None,
        },
        &mut out,
    );
    assert_eq!(
        parse(&out.writes[0])["params"]["dynamicTools"][0]["name"],
        "show_ui"
    );
}

#[test]
fn codex_without_widgets_sends_no_experimental_fields() {
    let (_, start) = codex(false);
    assert!(start["params"].get("dynamicTools").is_none());
}

#[test]
fn a_waiting_codex_call_ends_with_its_turn() {
    let (mut driver, _) = codex(true);
    open(&mut driver, 2);
    turn(&mut driver, "turn/started", "turn-1");
    tool_call(&mut driver, 51, "call-1", surface::examples::checklist());
    let mut out = Outputs::default();
    driver.on_line(
        br#"{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"interrupted"}}}"#,
        &mut out,
    );
    let mut out = Outputs::default();
    driver.command(
        Command::Act {
            session: SESSION,
            call: "call-1".into(),
            text: "{}".into(),
        },
        &mut out,
    );
    assert!(out.writes.is_empty());
    assert!(matches!(
        out.events.as_slice(),
        [(_, Event::ActionUndelivered { .. })]
    ));
}

/// The scripted providers show a widget, take the press as the tool's result, and say
/// what came back: the round trip the desktop app makes, without the app.
#[cfg(unix)]
mod scripted {
    use keystone_agent::{
        ApprovalState, ClaudeConnection, CodexConnection, Command, Connection, Event, TurnStatus,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    fn collect(
        conn: &dyn Connection,
        rx: &Receiver<()>,
        done: impl Fn(&Event) -> bool,
    ) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut events = Vec::new();
        loop {
            let batch: Vec<Event> = conn.drain().into_iter().map(|(_, e)| e).collect();
            let finished = batch.iter().any(&done);
            events.extend(batch);
            if finished {
                return events;
            }
            assert!(
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .is_ok(),
                "timed out: {events:?}"
            );
        }
    }

    fn round_trip(conn: &dyn Connection, rx: &Receiver<()>, dir: &std::path::Path, prompt: &str) {
        conn.send(
            1,
            dir.to_str().unwrap(),
            None,
            None,
            ApprovalState::default(),
            prompt.into(),
            Vec::new(),
        )
        .unwrap();
        let events = collect(conn, rx, |e| {
            matches!(
                e,
                Event::SurfaceShown { waiting: true, .. } | Event::TurnCompleted { .. }
            )
        });
        let Some(surface) = events.iter().find_map(|e| match e {
            Event::SurfaceShown {
                surface,
                waiting: true,
            } => Some(surface.clone()),
            _ => None,
        }) else {
            panic!("no waiting surface: {events:?}")
        };
        let button = if surface.surface_id == "bug-report" {
            "submit"
        } else {
            "send"
        };
        let action = surface.action(button, 1_760_000_000_000).unwrap();
        conn.command(Command::Act {
            session: 1,
            call: surface.call.clone(),
            text: action.clone(),
        });
        let events = collect(conn, rx, |e| matches!(e, Event::TurnCompleted { .. }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                Event::TurnCompleted {
                    status: TurnStatus::Completed,
                    ..
                }
            )),
            "{events:?}"
        );
        let reply = events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::ItemCompleted(item)
                    if item.kind == keystone_agent::ItemKind::AgentMessage =>
                {
                    Some(item.text.clone())
                }
                _ => None,
            })
            .unwrap();
        assert!(reply.contains(&action), "{reply}");
    }

    fn wake() -> (Arc<dyn Fn() + Send + Sync>, Receiver<()>) {
        let (tx, rx) = channel();
        (
            Arc::new(move || {
                let _ = tx.send(());
            }),
            rx,
        )
    }

    fn program(dir: &std::path::Path, binary: &str, env: &str) -> std::path::PathBuf {
        let path = dir.join("provider");
        std::fs::write(&path, format!("#!/bin/sh\n{env}\nexec '{binary}' \"$@\"\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn fake_claude_round_trips_a_checklist_and_a_form() {
        for prompt in ["show a widget checklist", "show a widget form"] {
            let dir = tempfile::tempdir().unwrap();
            let program = program(
                dir.path(),
                env!("CARGO_BIN_EXE_keystone-fake-claude"),
                "export KEYSTONE_FAKE_CLAUDE_DELAY_MS=1",
            );
            let (wake, rx) = wake();
            let conn = ClaudeConnection::new(Some(program), wake);
            round_trip(&conn, &rx, dir.path(), prompt);
        }
    }

    #[test]
    fn fake_codex_round_trips_a_checklist_and_a_form() {
        for prompt in ["show a widget checklist", "show a widget form"] {
            let dir = tempfile::tempdir().unwrap();
            let program = program(
                dir.path(),
                env!("CARGO_BIN_EXE_keystone-fake-codex"),
                "export KEYSTONE_FAKE_CODEX_DELAY_MS=1 KEYSTONE_FAKE_CODEX_VERSION=0.163.0",
            );
            let (wake, rx) = wake();
            let conn = CodexConnection::new(Some(program), "test", wake);
            round_trip(&conn, &rx, dir.path(), prompt);
        }
    }

    #[test]
    fn an_older_codex_gets_no_tool_and_replies_in_plain_text() {
        let dir = tempfile::tempdir().unwrap();
        let program = program(
            dir.path(),
            env!("CARGO_BIN_EXE_keystone-fake-codex"),
            "export KEYSTONE_FAKE_CODEX_DELAY_MS=1 KEYSTONE_FAKE_CODEX_VERSION=0.162.1",
        );
        let (wake, rx) = wake();
        let conn = CodexConnection::new(Some(program), "test", wake);
        Connection::send(
            &conn,
            1,
            dir.path().to_str().unwrap(),
            None,
            None,
            ApprovalState::default(),
            "show a widget checklist".into(),
            Vec::new(),
        )
        .unwrap();
        let events = collect(&conn, &rx, |e| matches!(e, Event::TurnCompleted { .. }));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::SurfaceShown { .. }))
        );
        assert!(events.iter().any(|e| matches!(e, Event::ItemCompleted(item)
            if item.text.contains("not available here, so this is plain text"))));
    }
}
