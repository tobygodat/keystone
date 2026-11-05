//! The Claude driver against recordings of real Claude Code 2.1.300 traffic: a plain
//! reply, an allowed and a denied command, a steer that aborts the reply, a steer
//! sent during a tool call, a stopped turn, a message stopped before it started, and
//! a resumed conversation. The recordings were made with the same message UUIDs the
//! driver generates from `SEED`.

use keystone_agent::claude::ClaudeDriver;
use keystone_agent::{
    Catalog, Command, Event, ItemKind, ItemStatus, ModelInfo, Outputs, Speed, TurnStatus,
};
use keystone_core::{ApprovalMode, ModelChoice};
use serde_json::Value;
use std::collections::HashMap;

const CONVERSATION: &str = "39e3a669-3dfc-4bfd-9fd6-02464374d10a";
const SESSION: u64 = 4;
const SEED: u64 = 0x6b65_7973_746f_6e65;

/// The messages sent during the recording, in order.
const SENT: [&str; 9] = [
    "Reply with exactly: HELLO KEYSTONE",
    "Use the Bash tool to run `echo approved > a.txt`, then reply DONE.",
    "Use the Bash tool to run `rm a.txt`, then reply DONE.",
    "Count from 1 to 80, one number per line. Do not use tools.",
    "Stop counting now and reply only STEERED.",
    "Use the Bash tool to run `sleep 3; echo slept`, then tell me what it printed.",
    "Also, end your reply with the word BANANA.",
    "Write a 400 word essay about rivers. Do not use tools.",
    "Write a long poem about the sea.",
];

fn send(n: usize) -> Command {
    Command::Send {
        session: SESSION,
        text: SENT[n - 1].into(),
        images: Vec::new(),
    }
}

fn parse(line: &str) -> Value {
    serde_json::from_str(line).unwrap()
}

/// Replays the session, sending each message just before Claude reports it queued,
/// stopping where the recording stopped, and allowing then denying the two commands.
fn replay() -> (ClaudeDriver, Outputs) {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    let init = parse(&out.writes[0]);
    assert_eq!(
        (init["type"].as_str(), init["request"]["subtype"].as_str()),
        (Some("control_request"), Some("initialize"))
    );
    let mut answers = [true, false].into_iter();
    for line in include_str!("fixtures/claude-2.1.300-session.jsonl").lines() {
        let message = parse(line);
        if message["type"] == "command_lifecycle" && message["state"] == "queued" {
            let uuid = message["command_uuid"].as_str().unwrap();
            let n = usize::from_str_radix(&uuid[uuid.len() - 12..], 16).unwrap();
            driver.command(send(n), &mut out);
        }
        let stops_here = message["response"]["request_id"] == "keystone-2"
            || (message["state"] == "cancelled"
                && message["command_uuid"].as_str().unwrap().ends_with('9'));
        if stops_here {
            driver.command(Command::Interrupt { session: SESSION }, &mut out);
        }
        let before = out.events.len();
        driver.on_line(line.as_bytes(), &mut out);
        let asked = out.events[before..].iter().find_map(|(_, e)| match e {
            Event::ApprovalRequested(approval) => Some(approval.request.clone()),
            _ => None,
        });
        if let Some(request) = asked {
            let allow = answers.next().expect("only two approvals were recorded");
            driver.command(
                Command::Answer {
                    session: SESSION,
                    request,
                    allow,
                },
                &mut out,
            );
        }
    }
    (driver, out)
}

#[test]
fn recorded_session_produces_turns_items_and_approvals() {
    let (driver, out) = replay();
    assert_eq!(driver.unknown, 0, "every message type was recognized");
    assert!(!driver.is_running());
    let events: Vec<Event> = out
        .events
        .into_iter()
        .map(|(s, e)| {
            assert_eq!(s, SESSION);
            e
        })
        .collect();

    // The conversation ID is reported once, though `init` repeats every turn.
    let ready: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::ThreadReady { .. }))
        .collect();
    assert_eq!(
        ready,
        [&Event::ThreadReady {
            thread_id: CONVERSATION.into()
        }]
    );

    // Steers join the turn they steer: four messages make one turn at the end, and
    // Stop ends it as interrupted with no error, ended by the user.
    let ends: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TurnCompleted {
                status,
                error,
                stopped,
            } => Some((*status, error.clone(), *stopped)),
            _ => None,
        })
        .collect();
    use TurnStatus::*;
    assert_eq!(
        ends,
        [
            (Completed, None, false),
            (Completed, None, false),
            (Completed, None, false),
            (Completed, None, false),
            (Interrupted, None, true)
        ]
    );
    let starts = events.iter().filter(|e| **e == Event::TurnStarted).count();
    assert_eq!(starts, 5);

    let replies: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage => {
                Some(item.text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(replies[0], "HELLO KEYSTONE");
    assert_eq!(replies[1], "DONE");
    assert!(replies.contains(&"STEERED"));
    assert!(replies.iter().any(|r| r.ends_with("BANANA")));
    assert!(!replies.iter().any(|r| r.contains("[Request interrupted")));

    // The count the steer cut short streamed in deltas onto its own item, which then
    // completed with the partial count, not the full one.
    let mut streamed: Vec<(&str, String)> = Vec::new();
    for e in &events {
        if let Event::ItemDelta { item_id, text } = e {
            match streamed.iter_mut().find(|(id, _)| id == item_id) {
                Some((_, all)) => all.push_str(text),
                None => streamed.push((item_id, text.clone())),
            }
        }
    }
    let (counted, count) = streamed
        .iter()
        .find(|(_, text)| text.starts_with("1\n2\n3\n"))
        .expect("the count streamed");
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemStarted(item) if item.id == *counted && item.kind == ItemKind::AgentMessage)));
    let completed = events
        .iter()
        .find_map(|e| match e {
            Event::ItemCompleted(item) if item.id == *counted => Some(item),
            _ => None,
        })
        .expect("the cut-off count completed");
    assert_eq!(completed.status, ItemStatus::Completed);
    assert_eq!(completed.text, *count);
    assert!(
        completed.text.trim_end().ends_with("\n52"),
        "{:?}",
        completed.text
    );
    assert!(!completed.text.contains("\n80"), "the count was cut short");

    // Every sent message is echoed as a user item.
    let echoed: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::ItemCompleted(item) if item.kind == ItemKind::UserMessage => {
                Some(item.text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(echoed, SENT[..8]);

    // The allowed command ran; the denied one is shown as declined.
    let commands: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::ItemCompleted(item) => match &item.kind {
                ItemKind::Command { command, exit_code } => {
                    Some((command.as_str(), *exit_code, item.status))
                }
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        commands,
        [
            ("echo approved > a.txt", Some(0), ItemStatus::Completed),
            ("cat a.txt && rm a.txt", None, ItemStatus::Declined),
            ("sleep 3; echo slept", Some(0), ItemStatus::Completed),
        ]
    );
    let approval = events
        .iter()
        .find_map(|e| match e {
            Event::ApprovalRequested(approval) => Some(approval),
            _ => None,
        })
        .unwrap();
    assert_eq!(approval.action, "echo approved > a.txt");
    assert_eq!(
        approval.item_id.as_deref(),
        Some("toolu_01XARNb8347Ma1fkCnRv6K9j")
    );
    assert!(approval.detail.is_some());
}

#[test]
fn writes_steer_with_priority_and_answer_with_the_asked_input() {
    let (_, out) = replay();
    let writes: Vec<Value> = out.writes.iter().map(|w| parse(w)).collect();
    let users: Vec<&Value> = writes.iter().filter(|w| w["type"] == "user").collect();
    assert_eq!(users.len(), 9);
    assert_eq!(users[0]["message"]["content"], SENT[0]);
    assert_eq!(users[0]["uuid"], "6b657973-746f-46e6-8005-000000000001");
    assert!(
        users[0].get("priority").is_none(),
        "a new turn has no priority"
    );
    assert_eq!(users[4]["priority"], "now", "a steer interrupts the reply");

    let answers: Vec<&Value> = writes
        .iter()
        .filter(|w| w["type"] == "control_response")
        .map(|w| &w["response"]["response"])
        .collect();
    assert_eq!(answers[0]["behavior"], "allow");
    assert_eq!(
        answers[0]["updatedInput"]["command"],
        "echo approved > a.txt"
    );
    assert_eq!(answers[1]["behavior"], "deny");

    let interrupts: Vec<&Value> = writes
        .iter()
        .filter(|w| w["request"]["subtype"] == "interrupt")
        .collect();
    assert_eq!(interrupts.len(), 2);
    assert_eq!(interrupts[0]["request_id"], "keystone-2");
    assert_eq!(interrupts[0]["request"]["cancel_queued"], true);
}

#[test]
fn a_steer_during_a_tool_call_waits_for_the_tool() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(1), &mut out);
    for line in [
        r#"{"type":"command_lifecycle","command_uuid":"6b657973-746f-46e6-8005-000000000001","state":"queued"}"#,
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m1"}},"parent_tool_use_id":null}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Bash","input":{}}},"parent_tool_use_id":null}"#,
    ] {
        driver.on_line(line.as_bytes(), &mut out);
    }
    out.writes.clear();
    driver.command(send(2), &mut out);
    assert_eq!(parse(&out.writes[0])["priority"], "next");
}

/// Images go as base64 `image` blocks after the text, as the TOB-59 probe found Claude
/// Code 2.1.301 takes them, and a message of images alone has no empty text block.
#[test]
fn images_are_sent_as_base64_blocks_after_the_text() {
    let image = |media_type: &str, data: &[u8]| keystone_agent::Image {
        path: "/data/attachments/x".into(),
        media_type: media_type.into(),
        base64: Some(
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data).into(),
        ),
    };
    let content = |text: &str, images| {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
        ready(&mut driver, &mut out, ApprovalMode::Provider);
        out.writes.clear();
        let text = text.into();
        driver.command(
            Command::Send {
                session: SESSION,
                text,
                images,
            },
            &mut out,
        );
        parse(&out.writes[0])["message"]["content"].clone()
    };
    assert_eq!(
        content(
            "What changed?",
            vec![
                image("image/png", b"png bytes"),
                image("image/webp", b"webp")
            ]
        ),
        serde_json::json!([
            {"type": "text", "text": "What changed?"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "cG5nIGJ5dGVz"}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": "d2VicA=="}},
        ])
    );
    assert_eq!(
        content("", vec![image("image/gif", b"gif")]),
        serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/gif", "data": "Z2lm"}},
        ])
    );
    assert_eq!(
        content("plain", Vec::new()),
        "plain",
        "text alone stays a string"
    );
}

/// The resume recording's one message UUID was renumbered to the first a driver makes.
#[test]
fn a_resumed_conversation_keeps_its_id() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, true, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(
        Command::Send {
            session: SESSION,
            text: "What exact phrase did I ask you to reply with in my first message? Answer with only that phrase.".into(),
            images: Vec::new(),
        },
        &mut out,
    );
    for line in include_str!("fixtures/claude-2.1.300-resume.jsonl").lines() {
        driver.on_line(line.as_bytes(), &mut out);
    }
    let events: Vec<Event> = out.events.into_iter().map(|(_, e)| e).collect();
    assert!(events.contains(&Event::ThreadReady {
        thread_id: CONVERSATION.into()
    }));
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage && item.text == "HELLO KEYSTONE")));
    assert_eq!(
        events.last(),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
            stopped: false,
        })
    );
}

#[test]
fn a_conversation_claude_no_longer_has_fails_instead_of_starting_fresh() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, true, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(1), &mut out);
    // Claude 2.1.300's only output for an unknown `--resume` ID, before it exits 1.
    driver.on_line(
        br#"{"type":"result","subtype":"error_during_execution","duration_ms":0,"is_error":true,"num_turns":0,"session_id":"00000000-0000-4000-8000-000000000000","errors":["No conversation found with session ID: 00000000-0000-4000-8000-000000000000"]}"#,
        &mut out,
    );
    let Some((_, Event::TurnCompleted { status, error, .. })) = out.events.last() else {
        panic!("expected the turn to end, got {:?}", out.events);
    };
    assert_eq!(*status, TurnStatus::Failed);
    let error = error.as_deref().unwrap();
    assert!(
        error.starts_with("Could not resume this Claude Code conversation: No conversation found")
    );
    assert!(!driver.is_running());
}

#[test]
fn signed_out_claude_reports_an_error_and_no_reply() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(1), &mut out);
    let uuid = "6b657973-746f-46e6-8005-000000000001";
    // Claude 2.1.300 with no login, trimmed to the fields Keystone reads.
    for line in [
        format!(r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"queued"}}"#),
        format!(r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"started"}}"#),
        r#"{"type":"system","subtype":"init","session_id":"4adbc14b-a833-445f-8c45-dd16939b8e9b","capabilities":["msg_lifecycle_v1"]}"#.into(),
        r#"{"type":"assistant","message":{"id":"f64cb87e","model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"Not logged in · Please run /login"}]},"parent_tool_use_id":null,"error":"authentication_failed","is_api_error_message":true}"#.into(),
        r#"{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error","result":"Not logged in · Please run /login"}"#.into(),
        format!(r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"cancelled"}}"#),
    ] {
        driver.on_line(line.as_bytes(), &mut out);
    }
    assert!(!out.events.iter().any(|(_, e)| matches!(e,
        Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage)));
    let Some((_, Event::TurnCompleted { status, error, .. })) = out.events.last() else {
        panic!("expected the turn to end, got {:?}", out.events);
    };
    assert_eq!(*status, TurnStatus::Failed);
    assert!(error.as_deref().unwrap().contains("not signed in"));
}

#[test]
fn a_signed_out_account_names_its_home_and_sign_in_command() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    let profile = keystone_agent::LaunchProfile::account(
        keystone_core::Provider::Claude,
        "Work",
        std::path::Path::new("/accounts/claude-work"),
    );
    driver.set_sign_in(profile.sign_in());
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(1), &mut out);
    for line in [
        r#"{"type":"system","subtype":"init","session_id":"4adbc14b-a833-445f-8c45-dd16939b8e9b","capabilities":[]}"#,
        r#"{"type":"assistant","message":{"id":"f64cb87e","model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"Not logged in · Please run /login"}]},"parent_tool_use_id":null,"error":"authentication_failed","is_api_error_message":true}"#,
        r#"{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error","result":"Not logged in · Please run /login"}"#,
    ] {
        driver.on_line(line.as_bytes(), &mut out);
    }
    let Some((_, Event::TurnCompleted { error, .. })) = out.events.last() else {
        panic!("expected the turn to end, got {:?}", out.events);
    };
    assert_eq!(
        error.as_deref(),
        Some(
            "Claude Code is not signed in (Not logged in · Please run /login). The account \u{201c}Work\u{201d} uses /accounts/claude-work. Sign it in from a terminal with `CLAUDE_CONFIG_DIR=/accounts/claude-work claude auth login`, then send again."
        )
    );
}

#[test]
fn unknown_control_requests_are_refused() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    out.writes.clear();
    driver.on_line(
        br#"{"type":"control_request","request_id":"r9","request":{"subtype":"elicitation"}}"#,
        &mut out,
    );
    let reply = parse(&out.writes[0]);
    assert_eq!(reply["response"]["subtype"], "error");
    assert_eq!(reply["response"]["request_id"], "r9");
}

#[test]
fn exit_fails_the_running_turn() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(1), &mut out);
    driver.on_exit("Claude Code stopped with exit code 1", &mut out);
    let ended: Vec<&Event> = out.events.iter().rev().take(2).map(|(_, e)| e).collect();
    assert_eq!(
        ended,
        vec![
            &Event::ModelLost,
            &Event::TurnCompleted {
                status: TurnStatus::Failed,
                error: Some("Claude Code stopped with exit code 1".into()),
                stopped: false,
            },
        ]
    );
}

#[test]
fn a_message_stopped_before_it_started_ends_the_turn_without_a_result() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(1), &mut out);
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    let uuid = "6b657973-746f-46e6-8005-000000000001";
    // Claude 2.1.300 sends no result for a message cancelled while queued.
    for line in [
        format!(r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"queued"}}"#),
        format!(r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"cancelled"}}"#),
        format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"keystone-2","response":{{"still_queued":[],"cancelled":["{uuid}"]}}}}}}"#
        ),
    ] {
        driver.on_line(line.as_bytes(), &mut out);
    }
    assert!(!driver.is_running());
    assert_eq!(
        out.events.last().map(|(_, e)| e),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None,
            stopped: true,
        })
    );
}

/// Claude answers `initialize`, reporting the mode a process launched in `launched`
/// runs in. Nothing is written to Claude before.
fn ready(driver: &mut ClaudeDriver, out: &mut Outputs, launched: ApprovalMode) {
    let mode = keystone_agent::claude::permission_mode(launched).unwrap_or("default");
    initialize(driver, out, mode);
}

/// Claude answers `initialize`, reporting that it runs in `mode`.
fn initialize(driver: &mut ClaudeDriver, out: &mut Outputs, mode: &str) {
    driver.on_line(
        format!(r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"keystone-1","response":{{"current_permission_mode":"{mode}"}}}}}}"#).as_bytes(),
        out,
    );
}

fn lifecycle(n: u64, state: &str) -> String {
    let uuid = keystone_agent::claude::message_uuid(SEED, n);
    format!(r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"{state}"}}"#)
}

fn feed(driver: &mut ClaudeDriver, out: &mut Outputs, lines: &[String]) {
    for line in lines {
        driver.on_line(line.as_bytes(), out);
    }
}

fn user_writes(out: &Outputs) -> usize {
    out.writes
        .iter()
        .filter(|w| parse(w)["type"] == "user")
        .count()
}

fn turn_ends(out: &Outputs) -> Vec<TurnStatus> {
    out.events
        .iter()
        .filter_map(|(_, e)| match e {
            Event::TurnCompleted { status, .. } => Some(*status),
            _ => None,
        })
        .collect()
}

/// Without lifecycle frames, a steer's aborted first `result` would end the turn while
/// the steer still ran, so the driver refuses to steer instead.
#[test]
fn a_claude_without_lifecycle_frames_is_not_steered() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(4), &mut out);
    feed(
        &mut driver,
        &mut out,
        &[
            r#"{"type":"system","subtype":"init","session_id":"s","capabilities":[]}"#.into(),
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m1"}},"parent_tool_use_id":null}"#.into(),
        ],
    );
    assert!(driver.steer_blocked().unwrap().contains("msg_lifecycle_v1"));
    driver.command(send(5), &mut out);
    assert_eq!(user_writes(&out), 1, "the steer was not sent");
    assert!(out.events.iter().any(
        |(_, e)| matches!(e, Event::Warning { message } if message.contains("msg_lifecycle_v1"))
    ));
    assert!(driver.is_running());
    feed(
        &mut driver,
        &mut out,
        &[r#"{"type":"result","subtype":"success","is_error":false,"result":"1 2 3"}"#.into()],
    );
    assert_eq!(turn_ends(&out), [TurnStatus::Completed]);
    // Once idle, a message starts the next turn as usual.
    assert_eq!(driver.steer_blocked(), None);
    driver.command(send(5), &mut out);
    assert_eq!(user_writes(&out), 2);
}

/// Before the first frame or `init`, Keystone cannot tell whether steering is safe.
/// The connection refuses such a steer; one that reaches the driver anyway, such as
/// after the connection replaced the process, waits for Claude to say.
#[test]
fn a_steer_before_claude_reports_anything_waits_for_it() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(4), &mut out);
    assert!(driver.steer_blocked().is_some());
    driver.command(send(5), &mut out);
    assert_eq!(user_writes(&out), 1);
    // The first frame settles it, and the held steer goes out.
    feed(&mut driver, &mut out, &[lifecycle(1, "queued")]);
    assert_eq!(driver.steer_blocked(), None);
    assert_eq!(user_writes(&out), 2);
    assert!(
        !out.events
            .iter()
            .any(|(_, e)| matches!(e, Event::Warning { .. }))
    );
    driver.command(send(6), &mut out);
    assert_eq!(user_writes(&out), 3);
}

/// A message held when its turn ends starts the next turn only once the app has seen
/// that end, so the files the turn left can be read first.
#[test]
fn a_held_follow_up_waits_until_the_turn_end_is_released() {
    for hold in [false, true] {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
        if hold {
            driver.hold_follow_ups();
        }
        ready(&mut driver, &mut out, ApprovalMode::Provider);
        driver.command(send(4), &mut out);
        driver.command(send(5), &mut out);
        assert_eq!(user_writes(&out), 1);
        feed(
            &mut driver,
            &mut out,
            &[r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#.into()],
        );
        assert_eq!(turn_ends(&out), [TurnStatus::Completed]);
        assert!(driver.is_running(), "the follow-up's turn started");
        assert_eq!(user_writes(&out), if hold { 1 } else { 2 });
        driver.release_follow_ups(&mut out);
        assert_eq!(user_writes(&out), 2);
        driver.release_follow_ups(&mut out);
        assert_eq!(user_writes(&out), 2);
    }
}

/// A held follow-up moved to a replacement process stays held until the release.
#[test]
fn a_held_follow_up_stays_held_in_a_replacement_driver() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    driver.hold_follow_ups();
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(4), &mut out);
    driver.command(send(5), &mut out);
    feed(
        &mut driver,
        &mut out,
        &[r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#.into()],
    );
    assert!(driver.end_unseen());
    let carried = driver.take_unsent();
    assert_eq!(carried.len(), 1);

    let mut out = Outputs::default();
    let mut replacement = ClaudeDriver::new(SESSION, true, SEED, ApprovalMode::Provider, &mut out);
    replacement.hold_follow_ups();
    ready(&mut replacement, &mut out, ApprovalMode::Provider);
    replacement.set_end_unseen();
    replacement.carry(carried, &mut out);
    assert_eq!(user_writes(&out), 0, "held until released");
    replacement.release_follow_ups(&mut out);
    assert_eq!(user_writes(&out), 1);
}

/// Stop cancels a follow-up that was queued behind a message whose result already
/// came in. The turn did not finish its work, so it is not reported as completed.
#[test]
fn stopping_a_queued_follow_up_interrupts_the_turn() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(1), &mut out);
    feed(
        &mut driver,
        &mut out,
        &[
            lifecycle(1, "queued"),
            lifecycle(1, "started"),
            r#"{"type":"system","subtype":"init","session_id":"s","capabilities":["msg_lifecycle_v1"]}"#.into(),
            r#"{"type":"result","subtype":"success","is_error":false,"result":"HELLO KEYSTONE"}"#.into(),
        ],
    );
    // Sent after the result but before the first message's terminal frame.
    driver.command(send(2), &mut out);
    assert_eq!(user_writes(&out), 2);
    feed(
        &mut driver,
        &mut out,
        &[lifecycle(2, "queued"), lifecycle(1, "completed")],
    );
    assert!(driver.is_running(), "the follow-up is still queued");
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    feed(
        &mut driver,
        &mut out,
        &[
            lifecycle(2, "cancelled"),
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{"still_queued":[],"cancelled":[]}}}"#.into(),
        ],
    );
    assert!(!driver.is_running());
    assert_eq!(turn_ends(&out), [TurnStatus::Interrupted]);
}

fn turn_end(out: &Outputs) -> Option<(TurnStatus, bool)> {
    out.events.iter().rev().find_map(|(_, e)| match e {
        Event::TurnCompleted {
            status, stopped, ..
        } => Some((*status, *stopped)),
        _ => None,
    })
}

/// The turn's end says whether the user's Stop ended it. A denial lets the turn go on,
/// a failure stays a failure, and a Stop after the turn ended reports nothing.
#[test]
fn the_turn_end_says_whether_the_user_stopped_it() {
    let init = r#"{"type":"system","subtype":"init","session_id":"s","capabilities":["msg_lifecycle_v1"]}"#;
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);

    driver.command(send(1), &mut out);
    feed(
        &mut driver,
        &mut out,
        &[
            lifecycle(1, "queued"),
            lifecycle(1, "started"),
            init.into(),
            can_use_tool("r1"),
        ],
    );
    let request = asked(&out)[0].clone();
    driver.command(
        Command::Answer {
            session: SESSION,
            request,
            allow: false,
        },
        &mut out,
    );
    assert!(driver.is_running(), "a denial lets the turn go on");
    feed(
        &mut driver,
        &mut out,
        &[
            r#"{"type":"result","subtype":"success","is_error":false,"result":"DONE"}"#.into(),
            lifecycle(1, "completed"),
        ],
    );
    assert_eq!(turn_end(&out), Some((TurnStatus::Completed, false)));

    driver.command(send(2), &mut out);
    feed(
        &mut driver,
        &mut out,
        &[lifecycle(2, "queued"), lifecycle(2, "started")],
    );
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    feed(
        &mut driver,
        &mut out,
        &[
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["aborted"]}"#.into(),
            lifecycle(2, "completed"),
        ],
    );
    assert_eq!(turn_end(&out), Some((TurnStatus::Interrupted, true)));

    // The next turn fails on its own: the earlier Stop does not carry over.
    driver.command(send(3), &mut out);
    feed(
        &mut driver,
        &mut out,
        &[
            lifecycle(3, "queued"),
            lifecycle(3, "started"),
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["overloaded"]}"#.into(),
            lifecycle(3, "completed"),
        ],
    );
    assert_eq!(turn_end(&out), Some((TurnStatus::Failed, false)));

    // Stop after the turn ended: nothing is written and nothing is reported.
    let (writes, events) = (out.writes.len(), out.events.len());
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    assert_eq!((out.writes.len(), out.events.len()), (writes, events));

    // A crash after Stop is still a failure.
    driver.command(send(4), &mut out);
    feed(
        &mut driver,
        &mut out,
        &[lifecycle(4, "queued"), lifecycle(4, "started")],
    );
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    driver.on_exit("Claude Code stopped with exit code 1", &mut out);
    assert_eq!(
        turn_end(&out).map(|(status, _)| status),
        Some(TurnStatus::Failed)
    );
}

fn can_use_tool(id: &str) -> String {
    format!(
        r#"{{"type":"control_request","request_id":"{id}","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{"command":"ls"}},"tool_use_id":"t-{id}"}}}}"#
    )
}

fn asked(out: &Outputs) -> Vec<keystone_agent::RequestId> {
    out.events
        .iter()
        .filter_map(|(_, e)| match e {
            Event::ApprovalRequested(approval) => Some(approval.request.clone()),
            _ => None,
        })
        .collect()
}

fn answer(driver: &mut ClaudeDriver, request: keystone_agent::RequestId) -> Vec<Value> {
    let mut out = Outputs::default();
    driver.command(
        Command::Answer {
            session: SESSION,
            request,
            allow: true,
        },
        &mut out,
    );
    out.writes.iter().map(|w| parse(w)).collect()
}

/// An answer is sent only for the request the user answered, and only while Claude
/// still waits for it: not after Claude withdrew it, and not to a later process that
/// reuses its ID.
#[test]
fn stale_answers_are_never_sent() {
    let mut out = Outputs::default();
    let mut first = ClaudeDriver::new(SESSION, false, 1, ApprovalMode::Provider, &mut out);
    ready(&mut first, &mut out, ApprovalMode::Provider);
    first.command(send(1), &mut out);
    feed(&mut first, &mut out, &[can_use_tool("r1")]);
    let withdrawn = asked(&out)[0].clone();
    feed(
        &mut first,
        &mut out,
        &[r#"{"type":"control_cancel_request","request_id":"r1"}"#.into()],
    );
    assert!(answer(&mut first, withdrawn.clone()).is_empty());

    feed(&mut first, &mut out, &[can_use_tool("r2")]);
    let old = asked(&out)[1].clone();
    first.on_exit("Claude Code stopped", &mut out);

    // The next process numbers its requests the same way.
    let mut out = Outputs::default();
    let mut second = ClaudeDriver::new(SESSION, true, 2, ApprovalMode::Provider, &mut out);
    ready(&mut second, &mut out, ApprovalMode::Provider);
    second.command(send(1), &mut out);
    feed(&mut second, &mut out, &[can_use_tool("r2")]);
    let current = asked(&out)[0].clone();
    assert_ne!(current, old);
    assert!(answer(&mut second, old).is_empty());
    assert!(answer(&mut second, withdrawn).is_empty());
    let sent = answer(&mut second, current);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["response"]["request_id"], "r2");
    assert_eq!(sent[0]["response"]["response"]["behavior"], "allow");
}

fn configure(driver: &mut ClaudeDriver, mode: ApprovalMode) {
    driver.command(
        Command::Configure {
            session: SESSION,
            approval: keystone_agent::ApprovalState {
                mode,
                overridden: false,
            },
        },
        &mut Outputs::default(),
    );
}

/// The `set_permission_mode` requests written, by mode.
fn modes_set(out: &Outputs) -> Vec<String> {
    out.writes
        .iter()
        .map(|w| parse(w))
        .filter(|w| w["request"]["subtype"] == "set_permission_mode")
        .map(|w| w["request"]["mode"].as_str().unwrap().to_owned())
        .collect()
}

fn mode_set_reply(out: &Outputs, result: &str) -> String {
    let request = out
        .writes
        .iter()
        .map(|w| parse(w))
        .rfind(|w| w["request"]["subtype"] == "set_permission_mode")
        .unwrap()["request_id"]
        .clone();
    format!(r#"{{"type":"control_response","response":{{"request_id":{request},{result}}}}}"#)
}

/// Ends the turn whose last message was the `n`th sent.
fn end_turn(driver: &mut ClaudeDriver, out: &mut Outputs, first: u64, last: u64) {
    let mut lines = vec![];
    for n in first..=last {
        lines.push(lifecycle(n, "started"));
    }
    lines.push(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#.into());
    for n in first..=last {
        lines.push(lifecycle(n, "completed"));
    }
    feed(driver, out, &lines);
}

/// A mode is set when a turn starts, and the turn's message waits for Claude to
/// confirm it. A steer after a change waits for the new mode too.
#[test]
fn a_mode_is_set_before_the_turn_and_its_message_waits_for_claude() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    configure(&mut driver, ApprovalMode::ReadOnly);
    let mut out = Outputs::default();
    driver.command(send(1), &mut out);
    assert_eq!(modes_set(&out), ["plan"]);
    assert_eq!(user_writes(&out), 0);
    // A steer sent meanwhile waits behind the first message.
    driver.command(send(2), &mut out);
    assert_eq!(user_writes(&out), 0);
    let reply = mode_set_reply(&out, r#""subtype":"success","response":{"mode":"plan"}"#);
    feed(&mut driver, &mut out, &[reply]);
    assert_eq!(user_writes(&out), 1);
    feed(&mut driver, &mut out, &[lifecycle(1, "queued")]);
    assert_eq!(user_writes(&out), 2);

    // A steer after a change waits for Claude to confirm the new mode, during the turn.
    configure(&mut driver, ApprovalMode::Auto);
    driver.command(send(3), &mut out);
    assert_eq!(
        (modes_set(&out), user_writes(&out)),
        (vec!["plan".into(), "acceptEdits".into()], 2)
    );
    let reply = mode_set_reply(
        &out,
        r#""subtype":"success","response":{"mode":"acceptEdits"}"#,
    );
    feed(&mut driver, &mut out, &[reply]);
    assert_eq!(user_writes(&out), 3);
    end_turn(&mut driver, &mut out, 1, 3);
    assert_eq!(turn_ends(&out), [TurnStatus::Completed]);

    // The next turn needs nothing set.
    let mut out = Outputs::default();
    driver.command(send(4), &mut out);
    assert_eq!((modes_set(&out).len(), user_writes(&out)), (0, 1));
    assert!(!driver.needs_restart());
}

/// Claude refusing the mode fails the turn; nothing runs under the old one.
#[test]
fn a_refused_mode_fails_the_turn_without_sending() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    configure(&mut driver, ApprovalMode::Auto);
    let mut out = Outputs::default();
    driver.command(send(1), &mut out);
    let reply = mode_set_reply(&out, r#""subtype":"error","error":"no""#);
    feed(&mut driver, &mut out, &[reply]);
    assert_eq!(user_writes(&out), 0);
    assert!(out.events.iter().any(|(_, e)| matches!(e,
        Event::TurnCompleted { status: TurnStatus::Failed, error: Some(error), stopped: false }
            if error.contains("approval mode"))));
    assert!(!driver.is_running());
}

/// Stop before Claude confirms the mode ends the turn with nothing sent.
#[test]
fn stop_while_the_mode_is_set_sends_nothing() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    initialize(&mut driver, &mut out, "acceptEdits");
    configure(&mut driver, ApprovalMode::Ask);
    let mut out = Outputs::default();
    driver.command(send(1), &mut out);
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    assert_eq!(turn_ends(&out), [TurnStatus::Interrupted]);
    let reply = mode_set_reply(&out, r#""subtype":"success","response":{"mode":"default"}"#);
    feed(&mut driver, &mut out, &[reply]);
    assert_eq!(user_writes(&out), 0);
}

/// Approving Claude's `ExitPlanMode` leaves plan mode; Read only returns at the next
/// turn.
#[test]
fn leaving_plan_mode_is_undone_at_the_next_turn() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::ReadOnly, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::ReadOnly);
    let mut out = Outputs::default();
    // Launched in plan mode, so nothing needs setting.
    driver.command(send(1), &mut out);
    assert_eq!((modes_set(&out).len(), user_writes(&out)), (0, 1));
    feed(
        &mut driver,
        &mut out,
        &[
            format!(
                r#"{{"type":"system","subtype":"init","session_id":"{CONVERSATION}","capabilities":["msg_lifecycle_v1"],"permissionMode":"plan"}}"#
            ),
            r#"{"type":"system","subtype":"status","status":null,"permissionMode":"default"}"#
                .into(),
        ],
    );
    end_turn(&mut driver, &mut out, 1, 1);
    let mut out = Outputs::default();
    driver.command(send(2), &mut out);
    assert_eq!(
        (modes_set(&out), user_writes(&out)),
        (vec!["plan".into()], 0)
    );
}

/// Which changes need a new process. Claude enters `bypassPermissions` only when
/// launched in it, a process launched in it is never lowered in place, and the user's
/// own settings come back only in a process launched with no mode.
#[test]
fn some_modes_need_a_new_process() {
    use ApprovalMode::*;
    let restart = |launched, to| {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(SESSION, false, SEED, launched, &mut out);
        configure(&mut driver, to);
        driver.needs_restart()
    };
    for (launched, to, expected) in [
        (Provider, Provider, false),
        (Provider, ReadOnly, false),
        (Provider, Auto, false),
        (Provider, FullAccess, true),
        (Ask, Auto, false),
        (Auto, Provider, true),
        (FullAccess, FullAccess, false),
        (FullAccess, Auto, true),
        (FullAccess, Provider, true),
    ] {
        assert_eq!(restart(launched, to), expected, "{launched:?} to {to:?}");
    }

    // A mode Keystone set in a process launched without one also needs a new process
    // to return to the user's settings.
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, Provider, &mut out);
    initialize(&mut driver, &mut out, "acceptEdits");
    configure(&mut driver, Ask);
    driver.command(send(1), &mut out);
    configure(&mut driver, Provider);
    assert!(driver.needs_restart());
    assert_eq!(
        keystone_agent::claude::args(FullAccess)[12..],
        ["--permission-mode", "bypassPermissions"]
    );
    assert_eq!(keystone_agent::claude::args(Provider).len(), 12);
}

/// Stop drops a steer that waits for Claude to report lifecycle support.
#[test]
fn stop_drops_a_held_steer() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(4), &mut out);
    driver.command(send(5), &mut out);
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    feed(&mut driver, &mut out, &[lifecycle(1, "queued")]);
    assert_eq!(user_writes(&out), 1);
}

/// The ID of the latest `set_permission_mode` request written.
fn mode_request(out: &Outputs) -> Value {
    out.writes
        .iter()
        .map(|w| parse(w))
        .rfind(|w| w["request"]["subtype"] == "set_permission_mode")
        .unwrap()["request_id"]
        .clone()
}

fn mode_answer(request: &Value, mode: &str) -> String {
    format!(
        r#"{{"type":"control_response","response":{{"request_id":{request},"subtype":"success","response":{{"mode":"{mode}"}}}}}}"#
    )
}

/// Review round 2, finding 2. Claude confirming the mode of an older selection does
/// not release the waiting message: the latest selection's mode is set first.
#[test]
fn a_confirmation_for_an_older_selection_releases_nothing() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    configure(&mut driver, ApprovalMode::Auto);
    let mut out = Outputs::default();
    driver.command(send(1), &mut out);
    let auto = mode_request(&out);
    configure(&mut driver, ApprovalMode::ReadOnly);
    feed(&mut driver, &mut out, &[mode_answer(&auto, "acceptEdits")]);
    assert_eq!(user_writes(&out), 0, "not sent under acceptEdits");
    assert_eq!(modes_set(&out), ["acceptEdits", "plan"]);
    let plan = mode_request(&out);
    feed(&mut driver, &mut out, &[mode_answer(&plan, "plan")]);
    assert_eq!(user_writes(&out), 1);
}

/// Review round 2, finding 5. Stop while Claude has not answered a mode request ends
/// the turn, and leaves no gate: the next message waits only for that answer, then for
/// its own mode.
#[test]
fn stop_leaves_no_gate_behind() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Auto, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Auto);
    configure(&mut driver, ApprovalMode::ReadOnly);
    let mut out = Outputs::default();
    driver.command(send(1), &mut out);
    let plan = mode_request(&out);
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    assert_eq!(turn_ends(&out), [TurnStatus::Interrupted]);
    configure(&mut driver, ApprovalMode::Auto);
    driver.command(send(2), &mut out);
    // Claude's mode is unknown until it answers: the plan request may still apply.
    assert_eq!(user_writes(&out), 0);
    feed(&mut driver, &mut out, &[mode_answer(&plan, "plan")]);
    assert_eq!(user_writes(&out), 0, "not sent under plan");
    let auto = mode_request(&out);
    feed(&mut driver, &mut out, &[mode_answer(&auto, "acceptEdits")]);
    assert_eq!(user_writes(&out), 1);
    assert!(driver.is_running());
}

/// Review round 2, finding 3. Under Read only `ExitPlanMode` is refused at once, with
/// a reason for Claude and a warning for the user; under other modes the user decides.
#[test]
fn read_only_refuses_exit_plan_mode() {
    let exit_plan = r#"{"type":"control_request","request_id":"x1","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"p"},"tool_use_id":"t1"}}"#;
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::ReadOnly, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::ReadOnly);
    driver.command(send(1), &mut out);
    let mut out = Outputs::default();
    driver.on_line(exit_plan.as_bytes(), &mut out);
    assert!(asked(&out).is_empty());
    let [answer] = &out.writes[..] else {
        panic!("{:?}", out.writes)
    };
    let answer = parse(answer);
    assert_eq!(answer["response"]["request_id"], "x1");
    assert_eq!(answer["response"]["response"]["behavior"], "deny");
    assert!(
        answer["response"]["response"]["message"]
            .as_str()
            .unwrap()
            .contains("Read only")
    );
    assert!(
        out.events
            .iter()
            .any(|(_, e)| matches!(e, Event::Warning { .. }))
    );

    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Auto, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Auto);
    driver.command(send(1), &mut out);
    let mut out = Outputs::default();
    driver.on_line(exit_plan.as_bytes(), &mut out);
    assert_eq!(asked(&out).len(), 1);
    assert!(out.writes.is_empty());
}

/// A mode request Claude never answers fails the waiting turn with nothing sent, and
/// the process is replaced before the next message.
#[test]
fn an_unanswered_mode_request_fails_the_turn_and_marks_the_process() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    configure(&mut driver, ApprovalMode::Auto);
    let mut out = Outputs::default();
    driver.command(send(1), &mut out);
    // The answered `initialize` is timed too, and its timeout changes nothing.
    let [initialize, request] = &driver.take_timed_requests()[..] else {
        panic!("two requests to time")
    };
    driver.request_timed_out(initialize, &mut out);
    assert!(!driver.needs_restart());
    driver.request_timed_out(request, &mut out);
    assert_eq!(turn_ends(&out), [TurnStatus::Failed]);
    assert_eq!(user_writes(&out), 0);
    assert!(driver.needs_restart());
    // A late answer does not make the process usable again.
    let late = mode_answer(&mode_request(&out), "acceptEdits");
    feed(&mut driver, &mut out, &[late]);
    assert!(driver.needs_restart());
}

/// A deterministic random source for the interleaving test.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// A Claude process for the interleaving test. It reads Keystone's lines in order,
/// when the test lets it, and answers as Claude 2.1.300 does.
struct Sim {
    driver: ClaudeDriver,
    /// Lines written to this process and not read yet.
    unread: std::collections::VecDeque<String>,
    /// Lines it wrote and Keystone has not read yet.
    unsent: std::collections::VecDeque<String>,
    mode: String,
    bypass: bool,
    /// The mode `ExitPlanMode` returns to.
    before_plan: String,
    /// Messages read and not finished.
    running: Vec<u64>,
    /// Whether `ExitPlanMode` waits for an answer. The turn cannot end meanwhile.
    asking: bool,
    seed: u64,
}

const USER_MODE: &str = "acceptEdits";

impl Sim {
    fn launch(mode: ApprovalMode, seed: u64, out: &mut Outputs) -> Self {
        let flag = keystone_agent::claude::permission_mode(mode).unwrap_or(USER_MODE);
        let driver = ClaudeDriver::new(SESSION, true, seed, mode, out);
        Self {
            driver,
            unread: Default::default(),
            unsent: Default::default(),
            mode: flag.into(),
            bypass: mode == ApprovalMode::FullAccess,
            before_plan: flag.into(),
            running: vec![],
            asking: false,
            seed,
        }
    }

    fn set_mode(&mut self, mode: &str) {
        if mode == "plan" && self.mode != "plan" {
            self.before_plan = self.mode.clone();
        }
        self.mode = mode.into();
    }

    /// Reads one line Keystone wrote. Returns the mode a user message runs in.
    fn read(&mut self) -> Option<(u64, String)> {
        let line = parse(&self.unread.pop_front()?);
        let control = |id: &Value, body: Value| {
            serde_json::json!({"type": "control_response", "response":
                {"subtype": "success", "request_id": id, "response": body}})
            .to_string()
        };
        match (line["type"].as_str(), line["request"]["subtype"].as_str()) {
            (Some("control_request"), Some("initialize")) => {
                let answer = control(
                    &line["request_id"],
                    serde_json::json!({"current_permission_mode": self.mode}),
                );
                self.unsent.push_back(answer);
            }
            (Some("control_request"), Some("set_permission_mode")) => {
                let mode = line["request"]["mode"].as_str().unwrap().to_owned();
                if mode == "bypassPermissions" && !self.bypass {
                    self.unsent.push_back(serde_json::json!({"type": "control_response", "response":
                        {"subtype": "error", "request_id": line["request_id"], "error": "bypass_not_launched"}}).to_string());
                } else {
                    self.set_mode(&mode);
                    let answer = control(&line["request_id"], serde_json::json!({"mode": mode}));
                    self.unsent.push_back(answer);
                    self.unsent.push_back(format!(
                        r#"{{"type":"system","subtype":"status","permissionMode":"{mode}"}}"#
                    ));
                }
            }
            (Some("control_request"), Some("interrupt")) => {
                if std::mem::take(&mut self.asking) {
                    self.unsent.push_back(
                        r#"{"type":"control_cancel_request","request_id":"exit-plan"}"#.into(),
                    );
                }
                for n in std::mem::take(&mut self.running) {
                    self.unsent.push_back(self.lifecycle(n, "cancelled"));
                }
                self.unsent.push_back(
                    r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["interrupted"]}"#.into(),
                );
                let answer = control(&line["request_id"], serde_json::json!({}));
                self.unsent.push_back(answer);
            }
            (Some("control_response"), _) => {
                // An approved `ExitPlanMode` returns to the mode before plan mode.
                if line["response"]["request_id"] == "exit-plan"
                    && std::mem::take(&mut self.asking)
                    && line["response"]["response"]["behavior"] == "allow"
                    && self.mode == "plan"
                {
                    let mode = self.before_plan.clone();
                    self.set_mode(&mode);
                    self.unsent.push_back(format!(
                        r#"{{"type":"system","subtype":"status","permissionMode":"{mode}"}}"#
                    ));
                }
            }
            (Some("user"), _) => {
                let uuid = line["uuid"].as_str().unwrap();
                let n = u64::from_str_radix(&uuid[uuid.len() - 12..], 16).unwrap();
                self.running.push(n);
                self.unsent.push_back(self.lifecycle(n, "queued"));
                self.unsent.push_back(self.lifecycle(n, "started"));
                self.unsent.push_back(format!(
                    r#"{{"type":"system","subtype":"init","session_id":"s","capabilities":["msg_lifecycle_v1"],"permissionMode":"{}"}}"#,
                    self.mode
                ));
                return Some((n, self.mode.clone()));
            }
            _ => {}
        }
        None
    }

    fn lifecycle(&self, n: u64, state: &str) -> String {
        let uuid = keystone_agent::claude::message_uuid(self.seed, n);
        format!(r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"{state}"}}"#)
    }

    /// Finishes the messages it is running.
    fn finish(&mut self) {
        if self.running.is_empty() || self.asking {
            return;
        }
        self.unsent.push_back(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#.into(),
        );
        for n in std::mem::take(&mut self.running) {
            self.unsent.push_back(self.lifecycle(n, "completed"));
        }
    }
}

/// Random interleavings of mode changes, messages, steers, Stop, `ExitPlanMode`, and
/// Claude's answers, with the connection's process replacement. Every message Claude
/// reads runs in the mode of the selection that was latest when it was written.
#[test]
fn messages_only_ever_run_in_the_mode_selected_when_they_were_written() {
    use ApprovalMode::*;
    let modes = [Provider, ReadOnly, Ask, Auto, FullAccess];
    let mut checked = 0;
    for run in 0..3000u64 {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (run + 1).wrapping_mul(0x2545_f491_4f6c_dd1d));
        let mut selected = modes[rng.below(5) as usize];
        let mut seed = 1;
        let mut out = Outputs::default();
        let mut sim = Sim::launch(selected, seed, &mut out);
        // What each written message must run in, by message number.
        let mut expected: HashMap<u64, String> = HashMap::new();
        let mut text = 0;
        let route = |sim: &mut Sim,
                     out: &mut Outputs,
                     selected: ApprovalMode,
                     expected: &mut HashMap<u64, String>| {
            for line in out.writes.drain(..) {
                let message = parse(&line);
                if message["type"] == "user" {
                    let uuid = message["uuid"].as_str().unwrap();
                    let n = u64::from_str_radix(&uuid[uuid.len() - 12..], 16).unwrap();
                    let mode =
                        keystone_agent::claude::permission_mode(selected).unwrap_or(USER_MODE);
                    expected.insert(n, mode.into());
                }
                sim.unread.push_back(line);
            }
        };
        for _ in 0..100 {
            match rng.below(9) {
                0 => {
                    selected = modes[rng.below(5) as usize];
                    sim.driver.command(
                        Command::Configure {
                            session: SESSION,
                            approval: keystone_agent::ApprovalState {
                                mode: selected,
                                overridden: true,
                            },
                        },
                        &mut out,
                    );
                }
                1 | 2 => {
                    // A message, as the connection sends it: an idle process that cannot
                    // run the mode is replaced first.
                    if !sim.driver.is_running() && sim.driver.needs_restart() {
                        seed += 1;
                        route(&mut sim, &mut out, selected, &mut expected);
                        sim = Sim::launch(selected, seed, &mut out);
                        expected.clear();
                    }
                    if sim.driver.steer_blocked().is_none() {
                        text += 1;
                        sim.driver.command(
                            Command::Send {
                                session: SESSION,
                                text: format!("m{text}"),
                                images: Vec::new(),
                            },
                            &mut out,
                        );
                    }
                }
                3 => sim
                    .driver
                    .command(Command::Interrupt { session: SESSION }, &mut out),
                4 | 5 => {
                    if let Some((n, mode)) = sim.read() {
                        assert_eq!(
                            Some(&mode),
                            expected.get(&n),
                            "run {run}: message {n} ran in the wrong mode"
                        );
                        checked += 1;
                    }
                }
                6 => {
                    if let Some(line) = sim.unsent.pop_front() {
                        let before = out.events.len();
                        sim.driver.on_line(line.as_bytes(), &mut out);
                        // The user allows whatever Claude asks.
                        let asked: Vec<_> = out.events[before..]
                            .iter()
                            .filter_map(|(_, e)| match e {
                                Event::ApprovalRequested(a) => Some(a.request.clone()),
                                _ => None,
                            })
                            .collect();
                        for request in asked {
                            sim.driver.command(
                                Command::Answer {
                                    session: SESSION,
                                    request,
                                    allow: true,
                                },
                                &mut out,
                            );
                        }
                    }
                }
                7 => sim.finish(),
                _ => {
                    if !sim.running.is_empty() && sim.mode == "plan" && !sim.asking {
                        sim.asking = true;
                        sim.unsent.push_back(r#"{"type":"control_request","request_id":"exit-plan","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{},"tool_use_id":"e"}}"#.into());
                    }
                }
            }
            route(&mut sim, &mut out, selected, &mut expected);
            // The connection replaces a process a waiting turn cannot run in.
            if sim.driver.wants_restart() {
                let carried = sim.driver.take_unsent();
                seed += 1;
                sim = Sim::launch(selected, seed, &mut out);
                expected.clear();
                sim.driver.carry(carried, &mut out);
                route(&mut sim, &mut out, selected, &mut expected);
            }
        }
    }
    assert!(checked > 5_000, "only {checked} messages were checked");
}

#[test]
fn the_initialize_answer_lists_models_with_the_default_folded_out() {
    use keystone_agent::claude::initialize_catalog;
    let line = br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-catalog","response":{"commands":[],"current_permission_mode":"default","models":[
        {"value":"default","displayName":"Default (recommended)","description":"Opus for most tasks","resolvedModel":"claude-opus-5-5"},
        {"value":"opus","displayName":"Opus","description":"","resolvedModel":"claude-opus-5-5","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max"],"supportsFastMode":true},
        {"value":"sonnet","displayName":"Sonnet","description":"","resolvedModel":"claude-sonnet-5-5","supportsEffort":false,"supportedEffortLevels":["low"]},
        {"value":"haiku","displayName":"Haiku","description":"Fast","resolvedModel":"claude-haiku-5-5"}]}}}"#;
    assert_eq!(
        initialize_catalog(line),
        Some(Catalog::Ready {
            models: vec![
                ModelInfo {
                    model: "opus".into(),
                    label: "Opus".into(),
                    resolved: Some("claude-opus-5-5".into()),
                    efforts: ["low", "medium", "high", "xhigh", "max"]
                        .map(String::from)
                        .into(),
                    default_effort: None,
                    speeds: vec![
                        Speed {
                            value: "standard".into(),
                            label: "Standard".into(),
                        },
                        Speed {
                            value: "fast".into(),
                            label: "Fast".into(),
                        },
                    ],
                },
                // Levels listed without `supportsEffort` are not offered.
                ModelInfo {
                    model: "sonnet".into(),
                    label: "Sonnet".into(),
                    resolved: Some("claude-sonnet-5-5".into()),
                    ..Default::default()
                },
                ModelInfo {
                    model: "haiku".into(),
                    label: "Haiku".into(),
                    resolved: Some("claude-haiku-5-5".into()),
                    ..Default::default()
                },
            ],
            default_model: Some("claude-opus-5-5".into()),
        })
    );
    let error = br#"{"type":"control_response","response":{"subtype":"error","request_id":"keystone-catalog","error":"not signed in"}}"#;
    assert_eq!(
        initialize_catalog(error),
        Some(Catalog::Failed("not signed in".into()))
    );
    assert_eq!(
        initialize_catalog(br#"{"type":"system","subtype":"init","session_id":"x"}"#),
        None
    );
}

#[test]
fn init_reports_the_model_once_per_process() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    driver.command(send(1), &mut out);
    let init = format!(
        r#"{{"type":"system","subtype":"init","session_id":"{CONVERSATION}","capabilities":["msg_lifecycle_v1"],"model":"claude-opus-5-5"}}"#
    );
    driver.on_line(init.as_bytes(), &mut out);
    driver.on_line(init.as_bytes(), &mut out);
    let reported: Vec<&Event> = out
        .events
        .iter()
        .filter_map(|(_, e)| matches!(e, Event::ModelReported { .. }).then_some(e))
        .collect();
    assert_eq!(
        reported,
        vec![&Event::ModelReported {
            model: "claude-opus-5-5".into()
        }]
    );
}

fn model_choice(model: Option<&str>, effort: Option<&str>, speed: Option<&str>) -> ModelChoice {
    ModelChoice {
        model: model.map(str::to_owned),
        effort: effort.map(str::to_owned),
        speed: speed.map(str::to_owned),
    }
}

fn choose(driver: &mut ClaudeDriver, out: &mut Outputs, choice: ModelChoice) {
    driver.command(
        Command::Choose {
            session: SESSION,
            choice,
        },
        out,
    );
}

/// The control requests written with `subtype`, by their `request` body.
fn requests(out: &Outputs, subtype: &str) -> Vec<Value> {
    out.writes
        .iter()
        .map(|w| parse(w))
        .filter(|w| w["request"]["subtype"] == subtype)
        .map(|w| w["request"].clone())
        .collect()
}

fn reply_to(out: &Outputs, subtype: &str, result: &str) -> String {
    let request = out
        .writes
        .iter()
        .map(|w| parse(w))
        .rfind(|w| w["request"]["subtype"] == subtype)
        .unwrap()["request_id"]
        .clone();
    format!(r#"{{"type":"control_response","response":{{"request_id":{request},{result}}}}}"#)
}

#[test]
fn launch_args_carry_the_model_effort_and_fast_mode_setting() {
    use keystone_agent::claude::choice_args;
    assert!(choice_args(&ModelChoice::default()).is_empty());
    assert_eq!(
        choice_args(&model_choice(Some("opus"), Some("max"), Some("fast"))),
        [
            "--model",
            "opus",
            "--effort",
            "max",
            "--settings",
            r#"{"fastMode":true}"#
        ]
    );
    assert_eq!(
        choice_args(&model_choice(None, None, Some("standard"))),
        ["--settings", r#"{"fastMode":false}"#]
    );
    // A speed the Claude catalog never lists passes nothing.
    assert!(choice_args(&model_choice(None, None, Some("priority"))).is_empty());
}

/// A change waits for the next turn: before its first message, Claude is asked to
/// switch, and the message is written only once every request is answered. Steers go
/// into the running turn unheld, and only what differs from the process is asked.
#[test]
fn a_changed_choice_is_applied_before_the_next_turn_and_its_message_waits() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    driver.set_launch_choice(model_choice(Some("opus"), Some("low"), None));
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    let mut out = Outputs::default();
    driver.command(send(1), &mut out);
    assert_eq!(user_writes(&out), 1, "launched with the choice already");
    assert!(requests(&out, "set_model").is_empty());
    end_turn(&mut driver, &mut out, 1, 1);

    let mut out = Outputs::default();
    choose(
        &mut driver,
        &mut out,
        model_choice(Some("sonnet"), Some("high"), Some("fast")),
    );
    assert!(out.writes.is_empty(), "nothing is asked between turns");
    driver.command(send(2), &mut out);
    assert_eq!(
        requests(&out, "set_model"),
        [serde_json::json!({"subtype": "set_model", "model": "sonnet"})]
    );
    assert_eq!(
        requests(&out, "apply_flag_settings"),
        [serde_json::json!({"subtype": "apply_flag_settings",
            "settings": {"effortLevel": "high", "fastMode": true}})]
    );
    assert_eq!(user_writes(&out), 0);
    let model = reply_to(
        &out,
        "set_model",
        r#""subtype":"success","response":{"model":"claude-sonnet-5-5"}"#,
    );
    feed(&mut driver, &mut out, &[model]);
    assert_eq!(user_writes(&out), 0, "the settings are still unanswered");
    assert!(out.events.iter().any(|(_, e)| *e
        == Event::ModelReported {
            model: "claude-sonnet-5-5".into()
        }));
    let settings = reply_to(&out, "apply_flag_settings", r#""subtype":"success""#);
    feed(&mut driver, &mut out, &[settings]);
    assert_eq!(user_writes(&out), 1);
    feed(&mut driver, &mut out, &[lifecycle(2, "queued")]);

    // A change during the turn holds no steer and asks nothing until the next turn.
    choose(
        &mut driver,
        &mut out,
        model_choice(Some("sonnet"), Some("max"), Some("fast")),
    );
    driver.command(send(3), &mut out);
    assert_eq!(user_writes(&out), 2);
    assert_eq!(requests(&out, "apply_flag_settings").len(), 1);
    end_turn(&mut driver, &mut out, 2, 3);

    let mut out = Outputs::default();
    driver.command(send(4), &mut out);
    assert!(requests(&out, "set_model").is_empty());
    assert_eq!(
        requests(&out, "apply_flag_settings"),
        [serde_json::json!({"subtype": "apply_flag_settings",
            "settings": {"effortLevel": "max"}})]
    );
    assert_eq!(user_writes(&out), 0);

    // Back to Default sends nothing: the process keeps what it has.
    let settings = reply_to(&out, "apply_flag_settings", r#""subtype":"success""#);
    feed(&mut driver, &mut out, &[settings]);
    end_turn(&mut driver, &mut out, 4, 4);
    let mut out = Outputs::default();
    choose(&mut driver, &mut out, ModelChoice::default());
    driver.command(send(5), &mut out);
    assert_eq!(
        (out.writes.len(), user_writes(&out)),
        (1, 1),
        "{:?}",
        out.writes
    );
}

#[test]
fn a_refused_change_fails_the_turn_with_nothing_written() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    let mut out = Outputs::default();
    choose(
        &mut driver,
        &mut out,
        model_choice(Some("mythos"), None, None),
    );
    driver.command(send(1), &mut out);
    let refused = reply_to(
        &out,
        "set_model",
        r#""subtype":"error","error":"model not available""#,
    );
    feed(&mut driver, &mut out, &[refused]);
    assert_eq!(user_writes(&out), 0);
    assert!(out.events.iter().any(|(_, e)| *e
        == Event::TurnCompleted {
            status: TurnStatus::Failed,
            error: Some("Claude Code refused the model change: model not available. Choose another model, effort or speed.".into()),
            stopped: false
        }));
    assert!(!driver.is_running());
}

#[test]
fn an_unanswered_change_fails_the_turn_and_marks_the_process() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    ready(&mut driver, &mut out, ApprovalMode::Provider);
    let mut out = Outputs::default();
    choose(
        &mut driver,
        &mut out,
        model_choice(None, Some("high"), None),
    );
    driver.command(send(1), &mut out);
    let [_initialize, request] = &driver.take_timed_requests()[..] else {
        panic!("two requests to time")
    };
    driver.request_timed_out(request, &mut out);
    assert_eq!(user_writes(&out), 0);
    assert!(out.events.iter().any(|(_, e)| *e
        == Event::TurnCompleted {
            status: TurnStatus::Failed,
            error: Some("Claude Code did not confirm the model change in time. Send again.".into()),
            stopped: false
        }));
    assert!(driver.needs_restart());
}

/// The account decides whether fast mode runs, and `init` says so: Keystone warns once
/// per process instead of showing Fast as running.
#[test]
fn fast_mode_the_account_turned_off_is_warned_about_once() {
    let init = |state: &str| {
        format!(
            r#"{{"type":"system","subtype":"init","session_id":"{CONVERSATION}","capabilities":["msg_lifecycle_v1"],"model":"claude-opus-5-5","fast_mode_state":"{state}","fast_mode_disabled_reason":"extra_usage_disabled"}}"#
        )
    };
    let warnings = |out: &Outputs| -> Vec<String> {
        out.events
            .iter()
            .filter_map(|(_, e)| match e {
                Event::Warning { message } => Some(message.clone()),
                _ => None,
            })
            .collect()
    };
    for (speed, state, warned) in [
        ("fast", "off", true),
        ("fast", "on", false),
        ("fast", "cooldown", false),
        ("standard", "off", false),
    ] {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
        driver.set_launch_choice(model_choice(None, None, Some(speed)));
        ready(&mut driver, &mut out, ApprovalMode::Provider);
        driver.command(send(1), &mut out);
        feed(&mut driver, &mut out, &[init(state), init(state)]);
        let expected: Vec<String> = if warned {
            vec!["Claude Code did not turn on fast mode (extra_usage_disabled).".into()]
        } else {
            Vec::new()
        };
        assert_eq!(warnings(&out), expected, "{speed} {state}");
    }
}
