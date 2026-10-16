//! The Claude Code driver: `claude -p` stream-json with the control protocol the
//! official SDKs use, as a state machine with no IO.
//!
//! Claude runs one process per session, so one driver serves one session. Lines go in
//! through [`ClaudeDriver::on_line`], user actions through [`ClaudeDriver::command`],
//! and both produce lines to write and [`Event`]s. Checked against Claude Code 2.1.300.
//!
//! A turn ends at a `result` once every message sent during it has finished. Each sent
//! message carries a UUID, and `command_lifecycle` frames report when Claude starts,
//! completes, or cancels it. A steer can fold into the running turn (one `result`) or
//! abort it and run next (two), so counting results alone is not enough.

use crate::driver::{Command, Outputs};
use crate::item::{Approval, Event, Item, ItemKind, ItemStatus, RequestId, TurnStatus, cap_output};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// The arguments Keystone passes, before any `--resume`. Permission prompts come to
/// Keystone as `can_use_tool` requests. No permission mode is set, so the user's own
/// Claude configuration decides what needs approval.
pub const ARGS: [&str; 12] = [
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--replay-user-messages",
    "--permission-prompts",
    "host",
    "--permission-prompt-tool",
    "stdio",
];

/// Why a message cannot steer a turn before Claude says whether it reports lifecycle
/// frames.
pub const STEER_NOT_YET: &str = "Claude Code has not reported this turn's progress yet, so the message cannot steer it. Wait a moment and send again.";

/// What Claude is told when the user denies a tool call.
const DENIED: &str = "The user denied this in Keystone.";

#[derive(Default)]
struct Turn {
    /// Messages sent during this turn that Claude has not completed or dropped, and
    /// whether each has started.
    outstanding: HashMap<String, bool>,
    /// Whether any message started. Messages cancelled before starting get no result.
    started: bool,
    /// Whether Claude cancelled a message before it started, as Stop does to queued
    /// steers. Its work never ran, so an earlier message's result cannot complete the
    /// turn.
    unrun: bool,
    /// The latest result's status and error. A later result replaces it.
    result: Option<(TurnStatus, Option<String>)>,
    stopping: bool,
}

enum Request {
    Initialize,
    Interrupt,
}

struct Pending {
    /// Claude's own ID for the request, which the answer must name.
    request_id: Value,
    input: Value,
    tool_use_id: Option<String>,
}

pub struct ClaudeDriver {
    session: u64,
    resume: bool,
    seed: u64,
    next_message: u64,
    next_request: u64,
    requests: HashMap<String, Request>,
    /// Claude's session ID, from `system/init`.
    conversation: Option<String>,
    /// Whether Claude reports `command_lifecycle` frames: unknown until the first frame
    /// or `init`. Steering needs them to tell which `result` ends the turn.
    lifecycle: Option<bool>,
    turn: Option<Turn>,
    /// The streaming assistant message and how many of its blocks have arrived whole.
    message: Option<(String, usize)>,
    /// Item IDs of streaming text and thinking blocks, by block index.
    blocks: HashMap<usize, String>,
    /// Tool calls waiting for their result.
    tools: HashMap<String, ItemKind>,
    /// Tool calls the user denied, so their results show as declined.
    denied: HashSet<String>,
    approvals: HashMap<RequestId, Pending>,
    /// An API error Claude reported as a synthetic assistant message, for the result.
    api_error: Option<String>,
    /// Messages Keystone did not recognize. Never fatal.
    pub unknown: u64,
}

impl ClaudeDriver {
    /// Starts the handshake. `seed` makes message UUIDs unique to this process; tests
    /// pass a fixed one. `resume` says whether the process was started with `--resume`.
    pub fn new(session: u64, resume: bool, seed: u64, out: &mut Outputs) -> Self {
        let mut driver = Self {
            session,
            resume,
            seed,
            next_message: 0,
            next_request: 0,
            requests: HashMap::new(),
            conversation: None,
            lifecycle: None,
            turn: None,
            message: None,
            blocks: HashMap::new(),
            tools: HashMap::new(),
            denied: HashSet::new(),
            approvals: HashMap::new(),
            api_error: None,
            unknown: 0,
        };
        let line = driver.request(json!({"subtype": "initialize"}), Request::Initialize);
        out.writes.push(line);
        driver
    }

    pub fn is_running(&self) -> bool {
        self.turn.is_some()
    }

    /// Why a message sent now could not steer the running turn, if it could not.
    pub fn steer_blocked(&self) -> Option<&'static str> {
        self.turn.as_ref().and_then(|_| self.steer_unsupported())
    }

    /// Why this process could not take a steer even once a turn runs, if it could not.
    pub fn steer_unsupported(&self) -> Option<&'static str> {
        match self.lifecycle {
            Some(true) => None,
            None => Some(STEER_NOT_YET),
            Some(false) => Some(
                "This Claude Code version does not report message progress (msg_lifecycle_v1), so Keystone cannot steer a running turn. Wait for the turn to end or stop it, then send again.",
            ),
        }
    }

    pub fn command(&mut self, command: Command, out: &mut Outputs) {
        match command {
            // The process is started with the directory and `--resume`.
            Command::Open { .. } => {}
            Command::Send { text, .. } => self.send(text, out),
            Command::Interrupt { .. } => {
                if let Some(turn) = &mut self.turn {
                    turn.stopping = true;
                    // `cancel_queued` drops steers Claude has not started, so Stop ends
                    // all of the turn.
                    let line = self.request(
                        json!({"subtype": "interrupt", "cancel_queued": true}),
                        Request::Interrupt,
                    );
                    out.writes.push(line);
                }
            }
            Command::Answer { request, allow, .. } => {
                let Some(pending) = self.approvals.remove(&request) else {
                    return;
                };
                let response = if allow {
                    json!({"behavior": "allow", "updatedInput": pending.input})
                } else {
                    if let Some(id) = pending.tool_use_id {
                        self.denied.insert(id);
                    }
                    json!({"behavior": "deny", "message": DENIED})
                };
                out.writes.push(
                    json!({"type": "control_response", "response":
                        {"subtype": "success", "request_id": pending.request_id, "response": response}})
                    .to_string(),
                );
            }
        }
    }

    /// Handles one line of Claude's output.
    pub fn on_line(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(envelope) = serde_json::from_slice::<Envelope>(line) else {
            self.unknown += 1;
            return;
        };
        // Subagent traffic is shown through the tool call that started it.
        if envelope.parent_tool_use_id.is_some() {
            return;
        }
        match (&*envelope.kind, envelope.subtype.as_deref()) {
            ("system", Some("init")) => self.on_init(line, out),
            ("command_lifecycle", _) => self.on_lifecycle(line, out),
            ("stream_event", _) => self.on_stream(line, out),
            ("assistant", _) => self.on_assistant(line, out),
            ("user", _) => self.on_user(line, out),
            ("control_request", _) => self.on_control_request(line, out),
            ("control_cancel_request", _) => {
                if let Ok(cancel) = serde_json::from_slice::<CancelRequest>(line) {
                    let request = self.request_id(&cancel.request_id);
                    if self.approvals.remove(&request).is_some() {
                        out.event(self.session, Event::ApprovalResolved { request });
                    }
                }
            }
            ("control_response", _) => self.on_control_response(line, out),
            ("result", _) => self.on_result(line, out),
            // Status, usage, and notices Keystone does not show.
            ("system" | "rate_limit_event" | "keep_alive" | "prompt_suggestion", _) => {}
            _ => self.unknown += 1,
        }
    }

    /// The process exited. A running turn fails.
    pub fn on_exit(&mut self, reason: &str, out: &mut Outputs) {
        if self.turn.is_some() {
            self.finish(TurnStatus::Failed, Some(reason.to_owned()), out);
        }
    }

    /// Keystone's ID for one of Claude's requests. Claude's IDs are only unique within
    /// its process, so the seed keeps an answer meant for an earlier process from
    /// matching a request in this one.
    fn request_id(&self, id: &Value) -> RequestId {
        RequestId(format!("{:016x}:{id}", self.seed))
    }

    fn request(&mut self, request: Value, purpose: Request) -> String {
        self.next_request += 1;
        let id = format!("keystone-{}", self.next_request);
        let line =
            json!({"type": "control_request", "request_id": id, "request": request}).to_string();
        self.requests.insert(id, purpose);
        line
    }

    /// Starts a turn, or steers the running one. Claude queues a message sent before
    /// it is ready, so nothing waits here.
    fn send(&mut self, text: String, out: &mut Outputs) {
        // Without lifecycle frames, the steered turn's first `result` would end it while
        // the steer still runs. The connection refuses these first; this is the backstop.
        if let Some(reason) = self.steer_blocked() {
            out.event(
                self.session,
                Event::Warning {
                    message: reason.into(),
                },
            );
            return;
        }
        self.next_message += 1;
        let uuid = message_uuid(self.seed, self.next_message);
        let mut message = json!({
            "type": "user",
            "message": {"role": "user", "content": text},
            "parent_tool_use_id": null,
            "uuid": uuid,
        });
        if self.turn.is_some() {
            // `now` aborts the response so the steer is read at once, but it would also
            // abort a running tool call, so wait for the tool to finish.
            message["priority"] = json!(if self.tools.is_empty() { "now" } else { "next" });
        } else {
            self.turn = Some(Turn::default());
            out.event(self.session, Event::TurnStarted);
        }
        if let Some(turn) = &mut self.turn {
            turn.outstanding.insert(uuid, false);
        }
        out.writes.push(message.to_string());
    }

    /// Output while idle belongs to a turn Claude started itself, such as after a
    /// background task finished.
    fn ensure_turn(&mut self, out: &mut Outputs) {
        if self.turn.is_none() {
            self.turn = Some(Turn {
                started: true,
                ..Turn::default()
            });
            out.event(self.session, Event::TurnStarted);
        }
    }

    /// Ends the turn once its result is in and nothing it sent is still pending.
    fn maybe_finish(&mut self, out: &mut Outputs) {
        let Some(turn) = &self.turn else { return };
        let lifecycle = self.lifecycle == Some(true);
        if lifecycle && !turn.outstanding.is_empty() {
            return;
        }
        let (status, error) = match &turn.result {
            // A queued message was cancelled, so the turn did not finish its work.
            Some((TurnStatus::Completed, _)) if turn.unrun => (TurnStatus::Interrupted, None),
            Some(result) => result.clone(),
            // Older versions send no lifecycle frames, and no steers are sent to them, so
            // the turn ends at its one result.
            None if !lifecycle => return,
            // Everything sent was dropped before it started, so no result will come.
            None if turn.started => return,
            None if turn.stopping => (TurnStatus::Interrupted, None),
            None => (
                TurnStatus::Failed,
                Some("Claude Code dropped the message without running it".into()),
            ),
        };
        self.finish(status, error, out);
    }

    fn finish(&mut self, status: TurnStatus, error: Option<String>, out: &mut Outputs) {
        self.turn = None;
        self.message = None;
        self.blocks.clear();
        self.tools.clear();
        self.api_error = None;
        // Requests from this turn can no longer be answered.
        for request in std::mem::take(&mut self.approvals).into_keys() {
            out.event(self.session, Event::ApprovalResolved { request });
        }
        out.event(self.session, Event::TurnCompleted { status, error });
    }

    fn on_init(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(init) = serde_json::from_slice::<Init>(line) else {
            return self.unknown += 1;
        };
        // A frame seen before `init` already settled it.
        if self.lifecycle != Some(true) {
            self.lifecycle = Some(init.capabilities.iter().any(|c| c == "msg_lifecycle_v1"));
        }
        // Claude repeats `init` before each turn; report the ID once.
        if self.conversation.as_deref() != Some(init.session_id.as_str()) {
            self.conversation = Some(init.session_id.clone());
            out.event(
                self.session,
                Event::ThreadReady {
                    thread_id: init.session_id,
                },
            );
        }
    }

    fn on_lifecycle(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(frame) = serde_json::from_slice::<Lifecycle>(line) else {
            return self.unknown += 1;
        };
        self.lifecycle = Some(true);
        let Some(turn) = &mut self.turn else { return };
        match frame.state.as_str() {
            "started" => {
                turn.started = true;
                if let Some(started) = turn.outstanding.get_mut(&frame.command_uuid) {
                    *started = true;
                }
            }
            "completed" => {
                turn.outstanding.remove(&frame.command_uuid);
            }
            // A steer also cancels the message it aborts, after that message started.
            "cancelled" => {
                if turn.outstanding.remove(&frame.command_uuid) == Some(false) {
                    turn.unrun = true;
                }
            }
            "discarded" | "refused" => {
                if turn.outstanding.remove(&frame.command_uuid).is_some() {
                    out.event(
                        self.session,
                        Event::Warning {
                            message: format!("Claude Code {} a message", frame.state),
                        },
                    );
                }
            }
            _ => return,
        }
        self.maybe_finish(out);
    }

    fn on_stream(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(stream) = serde_json::from_slice::<Stream>(line) else {
            return self.unknown += 1;
        };
        match stream.event {
            StreamEvent::MessageStart { message } => {
                self.ensure_turn(out);
                self.message = Some((message.id, 0));
                self.blocks.clear();
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let Some((message, _)) = &self.message else {
                    return;
                };
                let (id, kind) = match content_block.kind.as_str() {
                    "text" => (format!("{message}:{index}"), ItemKind::AgentMessage),
                    "thinking" => (format!("{message}:{index}"), ItemKind::Reasoning),
                    "tool_use" => (
                        content_block.id.unwrap_or_default(),
                        tool_kind(&content_block.name.unwrap_or_default(), &Value::Null),
                    ),
                    _ => return,
                };
                if !matches!(kind, ItemKind::AgentMessage | ItemKind::Reasoning) {
                    self.tools.insert(id.clone(), kind.clone());
                } else {
                    self.blocks.insert(index, id.clone());
                }
                out.event(
                    self.session,
                    Event::ItemStarted(Item {
                        id,
                        kind,
                        text: String::new(),
                        status: ItemStatus::InProgress,
                    }),
                );
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                let text = delta.text.or(delta.thinking).unwrap_or_default();
                if let Some(id) = self.blocks.get(&index)
                    && !text.is_empty()
                {
                    out.event(
                        self.session,
                        Event::ItemDelta {
                            item_id: id.clone(),
                            text,
                        },
                    );
                }
            }
            StreamEvent::Other => {}
        }
    }

    /// A whole content block. Text and thinking complete here; a tool call is now known
    /// in full and waits for its result.
    fn on_assistant(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(assistant) = serde_json::from_slice::<Assistant>(line) else {
            return self.unknown += 1;
        };
        if assistant.error.is_some() {
            // Claude's own words about an API failure, not a reply. The result reports it.
            let text = join_text(&assistant.message.content);
            self.api_error = Some(match assistant.error.as_deref() {
                Some("authentication_failed") => format!(
                    "Claude Code is not signed in ({text}). Run `claude` in a terminal to sign in, then send again."
                ),
                _ => text,
            });
            return;
        }
        self.ensure_turn(out);
        let message = assistant.message.id;
        if self.message.as_ref().is_none_or(|(id, _)| *id != message) {
            self.message = Some((message.clone(), 0));
        }
        for block in assistant.message.content {
            let index = self.message.as_ref().map_or(0, |(_, n)| *n);
            if let Some((_, n)) = &mut self.message {
                *n += 1;
            }
            let kind_name = block.get("type").and_then(Value::as_str).unwrap_or("");
            let text_of = |key: &str| {
                block
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let item = match kind_name {
                "text" | "thinking" => Item {
                    id: format!("{message}:{index}"),
                    kind: if kind_name == "text" {
                        ItemKind::AgentMessage
                    } else {
                        ItemKind::Reasoning
                    },
                    text: text_of(kind_name),
                    status: ItemStatus::Completed,
                },
                "tool_use" => {
                    let id = text_of("id");
                    let kind = tool_kind(
                        block.get("name").and_then(Value::as_str).unwrap_or(""),
                        block.get("input").unwrap_or(&Value::Null),
                    );
                    self.tools.insert(id.clone(), kind.clone());
                    out.event(
                        self.session,
                        Event::ItemStarted(Item {
                            id,
                            kind,
                            text: String::new(),
                            status: ItemStatus::InProgress,
                        }),
                    );
                    continue;
                }
                _ => {
                    self.unknown += 1;
                    continue;
                }
            };
            self.blocks.remove(&index);
            out.event(self.session, Event::ItemCompleted(item));
        }
    }

    /// The echo of a sent message, or tool results.
    fn on_user(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(user) = serde_json::from_slice::<User>(line) else {
            return self.unknown += 1;
        };
        let parts = match user.message.content {
            Content::Text(text) if user.is_replay => {
                return self.echo(user.uuid, text, out);
            }
            Content::Text(_) => return,
            Content::Parts(parts) => parts,
        };
        if user.is_replay {
            return self.echo(user.uuid, join_text(&parts), out);
        }
        // Parts without a replay flag are tool results, or notes such as
        // "[Request interrupted by user]" that are not the user's words.
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let id = part
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let is_error = part
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = match part.get("content") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(parts)) => join_text(parts),
                _ => String::new(),
            };
            let declined = self.denied.remove(&id);
            let mut kind = self.tools.remove(&id).unwrap_or(ItemKind::Tool {
                name: "tool".into(),
            });
            if let ItemKind::Command { exit_code, .. } = &mut kind {
                *exit_code = match (declined, is_error) {
                    (true, _) => None,
                    (false, false) => Some(0),
                    (false, true) => exit_code_in(&text),
                };
            }
            let status = match (declined, is_error) {
                (true, _) => ItemStatus::Declined,
                (false, true) => ItemStatus::Failed,
                (false, false) => ItemStatus::Completed,
            };
            out.event(
                self.session,
                Event::ItemCompleted(Item {
                    id,
                    kind,
                    text: cap_output(&text),
                    status,
                }),
            );
        }
    }

    fn echo(&mut self, uuid: Option<String>, text: String, out: &mut Outputs) {
        let id = uuid.unwrap_or_else(|| {
            self.next_message += 1;
            message_uuid(self.seed, self.next_message)
        });
        out.event(
            self.session,
            Event::ItemCompleted(Item {
                id,
                kind: ItemKind::UserMessage,
                text,
                status: ItemStatus::Completed,
            }),
        );
    }

    fn on_control_request(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(control) = serde_json::from_slice::<ControlRequest>(line) else {
            return self.unknown += 1;
        };
        let request = control.request;
        if request.subtype != "can_use_tool" {
            // Answer everything else with an error, so Claude never waits forever.
            out.writes.push(
                json!({"type": "control_response", "response": {
                    "subtype": "error",
                    "request_id": control.request_id,
                    "error": format!("Keystone does not support {} yet", request.subtype),
                }})
                .to_string(),
            );
            return;
        }
        self.ensure_turn(out);
        let tool = request
            .display_name
            .or(request.tool_name)
            .unwrap_or_else(|| "a tool".into());
        let input = request.input.unwrap_or(Value::Null);
        let subject = [
            "command",
            "file_path",
            "notebook_path",
            "url",
            "pattern",
            "path",
        ]
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str));
        let action = match (tool.as_str(), subject) {
            ("Bash" | "PowerShell", Some(command)) => command.to_owned(),
            (_, Some(subject)) => format!("{tool} {subject}"),
            (_, None) => tool,
        };
        let detail = request
            .description
            .filter(|d| *d != action)
            .or(request.blocked_path);
        let id = self.request_id(&control.request_id);
        self.approvals.insert(
            id.clone(),
            Pending {
                request_id: control.request_id,
                input,
                tool_use_id: request.tool_use_id.clone(),
            },
        );
        out.event(
            self.session,
            Event::ApprovalRequested(Approval {
                request: id,
                item_id: request.tool_use_id,
                action,
                detail,
            }),
        );
    }

    fn on_control_response(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(control) = serde_json::from_slice::<ControlResponse>(line) else {
            return self.unknown += 1;
        };
        let response = control.response;
        // Claude also echoes Keystone's answers to its own requests; those match nothing.
        let Some(purpose) = response
            .request_id
            .as_str()
            .and_then(|id| self.requests.remove(id))
        else {
            return;
        };
        if response.subtype != "error" {
            return;
        }
        let error = response.error.unwrap_or_else(|| "no reason given".into());
        let event = match purpose {
            Request::Initialize => Event::Failed {
                message: format!("Claude Code did not start: {error}"),
            },
            Request::Interrupt => Event::Warning {
                message: format!("Could not stop the turn: {error}"),
            },
        };
        out.event(self.session, event);
    }

    fn on_result(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(result) = serde_json::from_slice::<ResultMessage>(line) else {
            return self.unknown += 1;
        };
        let failed = result.is_error || result.subtype != "success";
        let reason = || {
            if result.errors.is_empty() {
                result
                    .result
                    .clone()
                    .unwrap_or_else(|| result.subtype.clone())
            } else {
                result.errors.join("; ")
            }
        };
        // A result before `init` means the conversation never opened, such as a
        // `--resume` of a conversation Claude no longer has. Never fall back to a fresh
        // one: the user would see the old conversation while Claude had none of it.
        if self.conversation.is_none() && failed {
            let message = if self.resume {
                format!(
                    "Could not resume this Claude Code conversation: {}. Start a new Claude session to continue.",
                    reason()
                )
            } else {
                format!("Claude Code did not start a conversation: {}", reason())
            };
            return match self.turn {
                Some(_) => self.finish(TurnStatus::Failed, Some(message), out),
                None => out.event(self.session, Event::Failed { message }),
            };
        }
        let api_error = self.api_error.take();
        let Some(turn) = &mut self.turn else {
            if failed {
                out.event(
                    self.session,
                    Event::Warning {
                        message: api_error.unwrap_or_else(reason),
                    },
                );
            }
            return;
        };
        turn.result = Some(match (failed, turn.stopping) {
            (false, _) => (TurnStatus::Completed, None),
            (true, true) => (TurnStatus::Interrupted, None),
            (true, false) => (TurnStatus::Failed, Some(api_error.unwrap_or_else(reason))),
        });
        self.maybe_finish(out);
    }
}

/// A UUID that names one sent message. Claude echoes it in lifecycle frames.
pub fn message_uuid(seed: u64, n: u64) -> String {
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        seed >> 32,
        (seed >> 16) & 0xffff,
        (seed >> 4) & 0xfff,
        seed & 0xf,
        n & 0xffff_ffff_ffff
    )
}

fn tool_kind(name: &str, input: &Value) -> ItemKind {
    let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    match name {
        "Bash" | "PowerShell" => ItemKind::Command {
            command: text("command").unwrap_or_default(),
            exit_code: None,
        },
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ItemKind::FileChange {
            paths: text("file_path")
                .or_else(|| text("notebook_path"))
                .into_iter()
                .collect(),
        },
        _ => ItemKind::Tool {
            name: name.to_owned(),
        },
    }
}

/// Claude starts a failed command's output with "Exit code N".
fn exit_code_in(text: &str) -> Option<i32> {
    text.strip_prefix("Exit code ")?
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .next()?
        .parse()
        .ok()
}

/// Text from a list of `{"type": "text", "text": ..}` parts.
fn join_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text")?.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first pass over every line: which kind of message it is. Bodies are skipped,
/// not built.
#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    subtype: Option<Cow<'a, str>>,
    parent_tool_use_id: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct Init {
    session_id: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Deserialize)]
struct Lifecycle {
    command_uuid: String,
    state: String,
}

#[derive(Deserialize)]
struct Stream {
    event: StreamEvent,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    MessageStart {
        message: Named,
    },
    ContentBlockStart {
        index: usize,
        content_block: Block,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Named {
    id: String,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    text: Option<String>,
    thinking: Option<String>,
}

#[derive(Deserialize)]
struct Assistant {
    message: AssistantMessage,
    error: Option<String>,
}

#[derive(Deserialize)]
struct AssistantMessage {
    id: String,
    #[serde(default)]
    content: Vec<Value>,
}

#[derive(Deserialize)]
struct User {
    message: UserMessage,
    uuid: Option<String>,
    #[serde(rename = "isReplay", default)]
    is_replay: bool,
}

#[derive(Deserialize)]
struct UserMessage {
    content: Content,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Parts(Vec<Value>),
}

#[derive(Deserialize)]
struct ControlRequest {
    request_id: Value,
    request: ControlBody,
}

#[derive(Deserialize)]
struct ControlBody {
    subtype: String,
    tool_name: Option<String>,
    display_name: Option<String>,
    input: Option<Value>,
    description: Option<String>,
    blocked_path: Option<String>,
    tool_use_id: Option<String>,
}

#[derive(Deserialize)]
struct CancelRequest {
    request_id: Value,
}

#[derive(Deserialize)]
struct ControlResponse {
    response: ControlResult,
}

#[derive(Deserialize)]
struct ControlResult {
    subtype: String,
    request_id: Value,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ResultMessage {
    subtype: String,
    #[serde(default)]
    is_error: bool,
    result: Option<String>,
    #[serde(default)]
    errors: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_uuids_are_well_formed_and_distinct() {
        let a = message_uuid(0x6b65_7973_746f_6e65, 1);
        assert_eq!(a, "6b657973-746f-46e6-8005-000000000001");
        assert_ne!(a, message_uuid(0x6b65_7973_746f_6e65, 2));
    }

    #[test]
    fn exit_codes_are_read_from_failed_command_output() {
        assert_eq!(exit_code_in("Exit code 2\nls: no such file"), Some(2));
        assert_eq!(exit_code_in("permission denied"), None);
    }
}
