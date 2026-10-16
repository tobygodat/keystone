//! The Claude driver against recordings of real Claude Code 2.1.300 traffic: a plain
//! reply, an allowed and a denied command, a steer that aborts the reply, a steer
//! sent during a tool call, a stopped turn, a message stopped before it started, and
//! a resumed conversation. The recordings were made with the same message UUIDs the
//! driver generates from `SEED`.

use keystone_agent::claude::ClaudeDriver;
use keystone_agent::{Command, Event, ItemKind, ItemStatus, Outputs, TurnStatus};
use keystone_core::ApprovalMode;
use serde_json::Value;

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
    // Stop ends it as interrupted with no error.
    let ends: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TurnCompleted { status, error } => Some((*status, error.clone())),
            _ => None,
        })
        .collect();
    use TurnStatus::*;
    assert_eq!(
        ends,
        [
            (Completed, None),
            (Completed, None),
            (Completed, None),
            (Completed, None),
            (Interrupted, None)
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

/// The resume recording's one message UUID was renumbered to the first a driver makes.
#[test]
fn a_resumed_conversation_keeps_its_id() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, true, SEED, ApprovalMode::Provider, &mut out);
    driver.command(
        Command::Send {
            session: SESSION,
            text: "What exact phrase did I ask you to reply with in my first message? Answer with only that phrase.".into(),
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
            error: None
        })
    );
}

#[test]
fn a_conversation_claude_no_longer_has_fails_instead_of_starting_fresh() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, true, SEED, ApprovalMode::Provider, &mut out);
    driver.command(send(1), &mut out);
    // Claude 2.1.300's only output for an unknown `--resume` ID, before it exits 1.
    driver.on_line(
        br#"{"type":"result","subtype":"error_during_execution","duration_ms":0,"is_error":true,"num_turns":0,"session_id":"00000000-0000-4000-8000-000000000000","errors":["No conversation found with session ID: 00000000-0000-4000-8000-000000000000"]}"#,
        &mut out,
    );
    let Some((_, Event::TurnCompleted { status, error })) = out.events.last() else {
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
    let Some((_, Event::TurnCompleted { status, error })) = out.events.last() else {
        panic!("expected the turn to end, got {:?}", out.events);
    };
    assert_eq!(*status, TurnStatus::Failed);
    assert!(error.as_deref().unwrap().contains("not signed in"));
}

#[test]
fn unknown_control_requests_are_refused() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
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
    driver.command(send(1), &mut out);
    driver.on_exit("Claude Code stopped with exit code 1", &mut out);
    assert_eq!(
        out.events.last().map(|(_, e)| e),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Failed,
            error: Some("Claude Code stopped with exit code 1".into())
        })
    );
}

#[test]
fn a_message_stopped_before_it_started_ends_the_turn_without_a_result() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
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
            error: None
        })
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

/// Stop cancels a follow-up that was queued behind a message whose result already
/// came in. The turn did not finish its work, so it is not reported as completed.
#[test]
fn stopping_a_queued_follow_up_interrupts_the_turn() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
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
/// confirm it. A change while a turn runs waits for the next turn.
#[test]
fn a_mode_is_set_before_the_turn_and_its_message_waits_for_claude() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
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

    configure(&mut driver, ApprovalMode::Auto);
    driver.command(send(3), &mut out);
    assert_eq!((modes_set(&out).len(), user_writes(&out)), (1, 3));
    end_turn(&mut driver, &mut out, 1, 3);
    assert_eq!(turn_ends(&out), [TurnStatus::Completed]);

    let mut out = Outputs::default();
    driver.command(send(4), &mut out);
    assert_eq!(
        (modes_set(&out), user_writes(&out)),
        (vec!["acceptEdits".into()], 0)
    );
    assert!(!driver.needs_restart());
}

/// Claude refusing the mode fails the turn; nothing runs under the old one.
#[test]
fn a_refused_mode_fails_the_turn_without_sending() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    configure(&mut driver, ApprovalMode::Auto);
    let mut out = Outputs::default();
    driver.command(send(1), &mut out);
    let reply = mode_set_reply(&out, r#""subtype":"error","error":"no""#);
    feed(&mut driver, &mut out, &[reply]);
    assert_eq!(user_writes(&out), 0);
    assert!(out.events.iter().any(|(_, e)| matches!(e,
        Event::TurnCompleted { status: TurnStatus::Failed, error: Some(error) }
            if error.contains("approval mode"))));
    assert!(!driver.is_running());
}

/// Stop before Claude confirms the mode ends the turn with nothing sent.
#[test]
fn stop_while_the_mode_is_set_sends_nothing() {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
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
    driver.command(send(4), &mut out);
    driver.command(send(5), &mut out);
    driver.command(Command::Interrupt { session: SESSION }, &mut out);
    feed(&mut driver, &mut out, &[lifecycle(1, "queued")]);
    assert_eq!(user_writes(&out), 1);
}
