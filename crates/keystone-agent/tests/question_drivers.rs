//! Both drivers against recordings of real questions: Claude Code 2.1.316's
//! `AskUserQuestion` and MCP `elicitation`, and codex-cli 0.166.0's
//! `item/tool/requestUserInput` and `mcpServer/elicitation/request`, each answered and
//! declined. `scripts/probe-questions.py` made the recordings and the fixtures in
//! `tests/fixtures/questions/`; each expected answer below is the one the probe sent
//! and the provider accepted.

use keystone_agent::claude::ClaudeDriver;
use keystone_agent::codex::CodexDriver;
use keystone_agent::{
    Answer, Command, Event, Input, ItemKind, ItemStatus, Outputs, Question, RequestId, TurnStatus,
};
use keystone_core::ApprovalMode;
use serde_json::{Value, json};

const SESSION: u64 = 5;
const SEED: u64 = 0x6b65_7973_746f_6e65;
const THREAD: &str = "01a1418d-549d-7553-bd08-4183fac44aa7";

fn parse(line: &str) -> Value {
    serde_json::from_str(line).unwrap()
}

fn answer(chosen: &[usize], text: &str) -> Answer {
    Answer {
        chosen: chosen.to_vec(),
        text: text.into(),
    }
}

fn send() -> Command {
    Command::Send {
        session: SESSION,
        text: "asked".into(),
        images: Vec::new(),
    }
}

/// What a recording asked, in order, and everything the driver did while replaying it.
struct Replay {
    asked: Vec<Event>,
    out: Outputs,
}

impl Replay {
    fn events(&self) -> Vec<&Event> {
        self.out.events.iter().map(|(_, e)| e).collect()
    }

    fn question(&self) -> &Question {
        self.asked
            .iter()
            .find_map(|e| match e {
                Event::QuestionAsked(question) => Some(question),
                _ => None,
            })
            .expect("the recording asks a question")
    }

    fn completed(&self) -> bool {
        self.events().contains(&&Event::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
            stopped: false,
        })
    }
}

/// Replays a Claude recording, sending the message when Claude reports it queued. Each
/// approval is allowed, and each question gets `reply`.
fn claude(fixture: &str, reply: Option<Vec<Answer>>) -> Replay {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    driver.on_line(
        br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{"current_permission_mode":"default"}}}"#,
        &mut out,
    );
    let mut asked = Vec::new();
    for line in fixture.lines() {
        let message = parse(line);
        if message["type"] == "command_lifecycle" && message["state"] == "queued" {
            driver.command(send(), &mut out);
        }
        let before = out.events.len();
        driver.on_line(line.as_bytes(), &mut out);
        let new: Vec<Event> = out.events[before..]
            .iter()
            .map(|(_, e)| e.clone())
            .collect();
        for event in new {
            match &event {
                Event::ApprovalRequested(approval) => driver.command(
                    Command::Answer {
                        session: SESSION,
                        request: approval.request.clone(),
                        allow: true,
                    },
                    &mut out,
                ),
                Event::QuestionAsked(question) => driver.command(
                    Command::Reply {
                        session: SESSION,
                        request: question.request.clone(),
                        answers: reply.clone(),
                    },
                    &mut out,
                ),
                _ => continue,
            }
            asked.push(event);
        }
    }
    Replay { asked, out }
}

/// The answer the Claude driver wrote to the control request `id`.
fn claude_answer(out: &Outputs, id: &str) -> Value {
    out.writes
        .iter()
        .map(|w| parse(w))
        .find(|w| w["type"] == "control_response" && w["response"]["request_id"] == id)
        .map(|w| w["response"]["response"].clone())
        .unwrap_or_else(|| panic!("no answer to {id}"))
}

fn recorded_request(fixture: &str, subtype: &str) -> Value {
    fixture
        .lines()
        .map(parse)
        .find(|m| m["type"] == "control_request" && m["request"]["subtype"] == subtype)
        .unwrap()
}

const CLAUDE_ASK: &str = include_str!("fixtures/questions/claude-2.1.316-ask-answered.jsonl");
const CLAUDE_ASK_DECLINED: &str =
    include_str!("fixtures/questions/claude-2.1.316-ask-declined.jsonl");
const CLAUDE_FORM: &str = include_str!("fixtures/questions/claude-2.1.316-elicit-accepted.jsonl");
const CLAUDE_FORM_DECLINED: &str =
    include_str!("fixtures/questions/claude-2.1.316-elicit-declined.jsonl");

#[test]
fn claude_ask_user_question_is_answered_in_its_own_format() {
    let replay = claude(
        CLAUDE_ASK,
        Some(vec![answer(&[1], ""), answer(&[0, 2], "")]),
    );
    let question = replay.question();
    assert_eq!(question.server, None);
    let headers: Vec<_> = question.fields.iter().map(|f| f.header.as_str()).collect();
    assert_eq!(headers, ["Color", "Fruit"]);
    assert_eq!(
        question.fields[0].prompt.as_deref(),
        Some("Which color do you prefer?")
    );
    assert!(matches!(
        &question.fields[0].input,
        Input::Choice { options, multiple: false, other: true } if options.len() == 2
    ));
    assert!(matches!(
        &question.fields[1].input,
        Input::Choice { options, multiple: true, other: true }
            if options.iter().map(|o| o.label.as_str()).eq(["Apple", "Banana", "Cherry"])
                && options[0].description.is_none()
    ));

    let request = recorded_request(CLAUDE_ASK, "can_use_tool");
    let mut expected = request["request"]["input"].clone();
    expected["answers"] =
        json!({"Which color do you prefer?": "Blue", "Which fruits do you like?": "Apple, Cherry"});
    assert_eq!(
        claude_answer(&replay.out, request["request_id"].as_str().unwrap()),
        json!({"behavior": "allow", "updatedInput": expected})
    );
    assert!(replay.events().iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.kind == ItemKind::Tool { name: "AskUserQuestion".into() }
            && item.status == ItemStatus::Completed && item.text.contains("\"Blue\""))));
    assert!(replay.completed());
}

#[test]
fn declining_claude_questions_denies_the_tool_and_claude_goes_on() {
    let replay = claude(CLAUDE_ASK_DECLINED, None);
    let request = recorded_request(CLAUDE_ASK_DECLINED, "can_use_tool");
    assert_eq!(
        claude_answer(&replay.out, request["request_id"].as_str().unwrap()),
        json!({"behavior": "deny", "message": "The user declined to answer."})
    );
    assert!(replay.events().iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.kind == ItemKind::Tool { name: "AskUserQuestion".into() }
            && item.status == ItemStatus::Declined)));
    assert!(
        replay.completed(),
        "Claude replies after a declined question"
    );
}

#[test]
fn a_claude_mcp_form_is_a_question_in_the_servers_order() {
    let answers = vec![
        answer(&[1], ""),
        answer(&[0, 2], ""),
        answer(&[], "extra crisp"),
        answer(&[0], ""),
    ];
    let replay = claude(CLAUDE_FORM, Some(answers));
    // The tool call asks for approval first, then the server asks its question.
    assert!(matches!(replay.asked[0], Event::ApprovalRequested(_)));
    let question = replay.question();
    assert_eq!(question.server.as_deref(), Some("probe"));
    assert_eq!(
        question.message.as_deref(),
        Some("Choose a plan for the probe")
    );
    let headers: Vec<_> = question.fields.iter().map(|f| f.header.as_str()).collect();
    assert_eq!(headers, ["Size", "Toppings", "Note", "Rush order"]);
    assert_eq!(
        question
            .fields
            .iter()
            .map(|f| f.required)
            .collect::<Vec<_>>(),
        [true, false, false, false]
    );
    let request = recorded_request(CLAUDE_FORM, "elicitation");
    assert_eq!(
        claude_answer(&replay.out, request["request_id"].as_str().unwrap()),
        json!({"action": "accept", "content": {"size": "large", "toppings": ["cheese", "basil"], "note": "extra crisp", "rush": true}})
    );
    assert!(replay.completed());
}

#[test]
fn declining_a_claude_mcp_form_declines_it() {
    let replay = claude(CLAUDE_FORM_DECLINED, None);
    let request = recorded_request(CLAUDE_FORM_DECLINED, "elicitation");
    assert_eq!(
        claude_answer(&replay.out, request["request_id"].as_str().unwrap()),
        json!({"action": "decline"})
    );
    assert!(replay.completed());
}

/// A Claude driver running a turn, with what it wrote and reported cleared.
fn claude_running() -> (ClaudeDriver, Outputs) {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    driver.on_line(
        br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{"current_permission_mode":"default"}}}"#,
        &mut out,
    );
    driver.command(send(), &mut out);
    (driver, Outputs::default())
}

#[test]
fn a_withdrawn_claude_question_takes_no_answer_and_its_turn_end_resolves_the_rest() {
    let (mut driver, mut out) = claude_running();
    let ask = |id: &str| {
        json!({"type": "control_request", "request_id": id, "request": {"subtype": "can_use_tool",
            "tool_name": "AskUserQuestion", "tool_use_id": format!("t-{id}"), "input": {"questions": [
                {"question": "Ship it?", "header": "Ship", "options": [{"label": "Yes", "description": "Yes"}, {"label": "No", "description": "No"}]}
            ]}}})
        .to_string()
    };
    driver.on_line(ask("q1").as_bytes(), &mut out);
    driver.on_line(ask("q2").as_bytes(), &mut out);
    let requests: Vec<RequestId> = out
        .events
        .iter()
        .filter_map(|(_, e)| match e {
            Event::QuestionAsked(q) => Some(q.request.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 2);

    let mut out = Outputs::default();
    driver.on_line(
        br#"{"type":"control_cancel_request","request_id":"q1"}"#,
        &mut out,
    );
    assert_eq!(
        out.events,
        vec![(
            SESSION,
            Event::ApprovalResolved {
                request: requests[0].clone()
            }
        )]
    );
    driver.command(
        Command::Reply {
            session: SESSION,
            request: requests[0].clone(),
            answers: Some(vec![answer(&[0], "")]),
        },
        &mut out,
    );
    assert!(
        out.writes.is_empty(),
        "a withdrawn question takes no answer"
    );

    let mut out = Outputs::default();
    driver.on_line(
        br#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s"}"#,
        &mut out,
    );
    assert!(out.events.contains(&(
        SESSION,
        Event::ApprovalResolved {
            request: requests[1].clone()
        }
    )));
}

#[test]
fn claude_forms_keystone_cannot_show_are_declined_and_an_empty_one_is_an_approval() {
    let (mut driver, mut out) = claude_running();
    driver.on_line(
        br#"{"type":"control_request","request_id":"u1","request":{"subtype":"elicitation","mcp_server_name":"docs","message":"Sign in","mode":"url","url":"https://example.com"}}"#,
        &mut out,
    );
    assert_eq!(claude_answer(&out, "u1"), json!({"action": "decline"}));
    assert!(out.events.contains(&(
        SESSION,
        Event::Warning {
            message: "Declined a request from the docs MCP server: it asks for a url answer, which Keystone can't give.".into()
        }
    )));

    let mut out = Outputs::default();
    driver.on_line(
        br#"{"type":"control_request","request_id":"e1","request":{"subtype":"elicitation","mcp_server_name":"docs","message":"Go ahead?","mode":"form","requested_schema":{"type":"object","properties":{}}}}"#,
        &mut out,
    );
    let Some((_, Event::ApprovalRequested(approval))) = out.events.last() else {
        panic!("an empty form is an approval: {:?}", out.events);
    };
    assert_eq!(approval.action, "Go ahead?");
    assert_eq!(approval.detail.as_deref(), Some("From the docs MCP server"));
    let request = approval.request.clone();
    driver.command(
        Command::Answer {
            session: SESSION,
            request,
            allow: true,
        },
        &mut out,
    );
    assert_eq!(
        claude_answer(&out, "e1"),
        json!({"action": "accept", "content": {}})
    );
}

/// A Codex driver with the session's thread open and a message sent.
fn codex_opened() -> CodexDriver {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/probe".into(),
            resume: None,
        },
        &mut out,
    );
    driver.on_line(br#"{"id":1,"result":{"userAgent":"test"}}"#, &mut out);
    let response = format!(r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}"}}}}}}"#);
    driver.on_line(response.as_bytes(), &mut out);
    driver.command(send(), &mut out);
    driver
}

/// Replays a Codex recording. Each approval is allowed, and each question gets `reply`.
fn codex(fixture: &str, reply: Option<Vec<Answer>>) -> Replay {
    let mut driver = codex_opened();
    let mut out = Outputs::default();
    let mut asked = Vec::new();
    for line in fixture.lines() {
        let before = out.events.len();
        driver.on_line(line.as_bytes(), &mut out);
        let new: Vec<Event> = out.events[before..]
            .iter()
            .map(|(_, e)| e.clone())
            .collect();
        for event in new {
            match &event {
                Event::ApprovalRequested(approval) => driver.command(
                    Command::Answer {
                        session: SESSION,
                        request: approval.request.clone(),
                        allow: true,
                    },
                    &mut out,
                ),
                Event::QuestionAsked(question) => driver.command(
                    Command::Reply {
                        session: SESSION,
                        request: question.request.clone(),
                        answers: reply.clone(),
                    },
                    &mut out,
                ),
                _ => continue,
            }
            asked.push(event);
        }
    }
    Replay { asked, out }
}

/// The result the Codex driver wrote for server request `id`.
fn codex_answer(out: &Outputs, id: u64) -> Value {
    out.writes
        .iter()
        .map(|w| parse(w))
        .find(|w| w["id"] == id && w.get("method").is_none())
        .map(|w| w["result"].clone())
        .unwrap_or_else(|| panic!("no answer to {id}"))
}

const CODEX_ASK: &str = include_str!("fixtures/questions/codex-0.166.0-ask-answered.jsonl");
const CODEX_ASK_DECLINED: &str =
    include_str!("fixtures/questions/codex-0.166.0-ask-declined.jsonl");
const CODEX_FORM: &str = include_str!("fixtures/questions/codex-0.166.0-elicit-accepted.jsonl");
const CODEX_FORM_DECLINED: &str =
    include_str!("fixtures/questions/codex-0.166.0-elicit-declined.jsonl");

#[test]
fn codex_request_user_input_is_answered_by_question_id() {
    let replay = codex(
        CODEX_ASK,
        Some(vec![
            answer(&[], "probe-notes.txt"),
            answer(&[], "probe-notes.txt"),
        ]),
    );
    let question = replay.question();
    assert_eq!(question.server, None);
    let headers: Vec<_> = question.fields.iter().map(|f| f.header.as_str()).collect();
    assert_eq!(headers, ["Color", "Name"]);
    assert!(matches!(
        &question.fields[0].input,
        Input::Choice { options, multiple: false, other: true }
            if options[1].label == "Blue" && options[1].description.is_some()
    ));
    assert_eq!(
        codex_answer(&replay.out, 0),
        json!({"answers": {"color": {"answers": ["probe-notes.txt"]}, "name": {"answers": ["probe-notes.txt"]}}})
    );
    assert!(replay.events().contains(&&Event::ApprovalResolved {
        request: question.request.clone()
    }));
    assert!(replay.completed());
}

#[test]
fn declining_codex_questions_sends_no_answers_and_codex_goes_on() {
    let replay = codex(CODEX_ASK_DECLINED, None);
    assert_eq!(codex_answer(&replay.out, 0), json!({"answers": {}}));
    assert!(replay.completed());
}

#[test]
fn a_codex_mcp_tool_approval_is_an_approval_and_its_form_a_question() {
    let answers = vec![
        answer(&[], "extra crisp"),
        answer(&[0], ""),
        answer(&[1], ""),
        answer(&[0, 2], ""),
    ];
    let replay = codex(CODEX_FORM, Some(answers));
    let Event::ApprovalRequested(approval) = &replay.asked[0] else {
        panic!("the tool call asks for approval first: {:?}", replay.asked);
    };
    assert_eq!(
        approval.action,
        "Allow the probe MCP server to run tool \"choose_plan\"?"
    );
    assert_eq!(
        codex_answer(&replay.out, 0),
        json!({"action": "accept", "content": {}})
    );
    let question = replay.question();
    assert_eq!(question.server.as_deref(), Some("probe"));
    // Codex sends the schema's properties in alphabetical order.
    let headers: Vec<_> = question.fields.iter().map(|f| f.header.as_str()).collect();
    assert_eq!(headers, ["Note", "Rush order", "Size", "Toppings"]);
    assert_eq!(
        codex_answer(&replay.out, 1),
        json!({"action": "accept", "content": {"size": "large", "toppings": ["cheese", "basil"], "note": "extra crisp", "rush": true}})
    );
    assert!(replay.completed());
}

#[test]
fn declining_a_codex_mcp_form_declines_it() {
    let replay = codex(CODEX_FORM_DECLINED, None);
    assert_eq!(codex_answer(&replay.out, 1), json!({"action": "decline"}));
    assert!(replay.completed());
}

#[test]
fn codex_questions_for_secrets_are_declined_and_a_turn_end_resolves_open_ones() {
    let mut driver = codex_opened();
    let mut out = Outputs::default();
    let secret = format!(
        r#"{{"id":7,"method":"item/tool/requestUserInput","params":{{"threadId":"{THREAD}","turnId":"t","itemId":"i","questions":[{{"id":"key","header":"Key","question":"API key?","isOther":true,"isSecret":true,"options":[{{"label":"None","description":"No key"}}]}}]}}}}"#
    );
    driver.on_line(secret.as_bytes(), &mut out);
    assert_eq!(codex_answer(&out, 7), json!({"answers": {}}));
    assert!(matches!(&out.events[..], [(_, Event::Warning { .. })]));

    let mut out = Outputs::default();
    let ask = format!(
        r#"{{"id":8,"method":"item/tool/requestUserInput","params":{{"threadId":"{THREAD}","turnId":"t","itemId":"i","questions":[{{"id":"go","header":"Go","question":"Go on?","options":[{{"label":"Yes","description":"Yes"}}]}}]}}}}"#
    );
    driver.on_line(ask.as_bytes(), &mut out);
    driver.on_line(
        format!(
            r#"{{"method":"turn/started","params":{{"threadId":"{THREAD}","turn":{{"id":"t"}}}}}}"#
        )
        .as_bytes(),
        &mut out,
    );
    let mut out = Outputs::default();
    driver.on_line(
        format!(r#"{{"method":"turn/completed","params":{{"threadId":"{THREAD}","turn":{{"id":"t","status":"interrupted"}}}}}}"#).as_bytes(),
        &mut out,
    );
    assert!(out.events.contains(&(
        SESSION,
        Event::ApprovalResolved {
            request: RequestId("8".into())
        }
    )));
    driver.command(
        Command::Reply {
            session: SESSION,
            request: RequestId("8".into()),
            answers: Some(vec![answer(&[0], "")]),
        },
        &mut out,
    );
    assert!(
        out.writes.is_empty(),
        "a question its turn ended takes no answer"
    );
}

/// The approval and the question each driver asked, in order.
fn approval_then_question(out: &Outputs) -> (RequestId, RequestId) {
    let approval = out.events.iter().find_map(|(_, e)| match e {
        Event::ApprovalRequested(approval) => Some(approval.request.clone()),
        _ => None,
    });
    let question = out.events.iter().find_map(|(_, e)| match e {
        Event::QuestionAsked(question) => Some(question.request.clone()),
        _ => None,
    });
    (
        approval.expect("an approval waits"),
        question.expect("a question waits"),
    )
}

#[test]
fn answering_a_question_while_an_approval_waits_never_answers_the_approval() {
    let (mut driver, mut out) = claude_running();
    driver.on_line(
        br#"{"type":"control_request","request_id":"a1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf build"},"tool_use_id":"t-a1"}}"#,
        &mut out,
    );
    driver.on_line(
        json!({"type": "control_request", "request_id": "q1", "request": {"subtype": "can_use_tool",
            "tool_name": "AskUserQuestion", "tool_use_id": "t-q1", "input": {"questions": [
                {"question": "Ship it?", "header": "Ship", "options": [{"label": "Yes", "description": "Yes"}, {"label": "No", "description": "No"}]}
            ]}}})
        .to_string()
        .as_bytes(),
        &mut out,
    );
    let (approval, question) = approval_then_question(&out);
    let mut out = Outputs::default();
    // A reply that names the approval is not an answer to it.
    driver.command(
        Command::Reply {
            session: SESSION,
            request: approval.clone(),
            answers: Some(vec![answer(&[0], "")]),
        },
        &mut out,
    );
    assert!(out.writes.is_empty(), "a reply never answers an approval");
    driver.command(
        Command::Reply {
            session: SESSION,
            request: question,
            answers: Some(vec![answer(&[0], "")]),
        },
        &mut out,
    );
    let written: Vec<Value> = out.writes.iter().map(|w| parse(w)).collect();
    assert_eq!(
        written.len(),
        1,
        "only the question is answered: {written:?}"
    );
    assert_eq!(written[0]["response"]["request_id"], "q1");
    // The approval still waits, and Deny still denies it.
    let mut out = Outputs::default();
    driver.command(
        Command::Answer {
            session: SESSION,
            request: approval,
            allow: false,
        },
        &mut out,
    );
    assert_eq!(claude_answer(&out, "a1")["behavior"], "deny");

    let mut driver = codex_opened();
    let mut out = Outputs::default();
    let approval = format!(
        r#"{{"id":40,"method":"item/commandExecution/requestApproval","params":{{"threadId":"{THREAD}","itemId":"exec","command":"rm -rf build"}}}}"#
    );
    driver.on_line(approval.as_bytes(), &mut out);
    let ask = json!({"id": 41, "method": "item/tool/requestUserInput", "params": {"threadId": THREAD,
        "turnId": "t", "itemId": "call", "questions": [{"id": "ship", "header": "Ship", "question": "Ship it?",
        "isOther": false, "isSecret": false, "options": [{"label": "Yes", "description": "Yes"}]}]}});
    driver.on_line(ask.to_string().as_bytes(), &mut out);
    let (approval, question) = approval_then_question(&out);
    let mut out = Outputs::default();
    driver.command(
        Command::Reply {
            session: SESSION,
            request: approval.clone(),
            answers: Some(vec![answer(&[0], "")]),
        },
        &mut out,
    );
    assert!(out.writes.is_empty(), "a reply never answers an approval");
    driver.command(
        Command::Reply {
            session: SESSION,
            request: question,
            answers: Some(vec![answer(&[0], "")]),
        },
        &mut out,
    );
    assert_eq!(out.writes.len(), 1, "only the question is answered");
    assert_eq!(
        codex_answer(&out, 41),
        json!({"answers": {"ship": {"answers": ["Yes"]}}})
    );
    let mut out = Outputs::default();
    driver.command(
        Command::Answer {
            session: SESSION,
            request: approval,
            allow: false,
        },
        &mut out,
    );
    assert_eq!(codex_answer(&out, 40)["decision"], "decline");
}
