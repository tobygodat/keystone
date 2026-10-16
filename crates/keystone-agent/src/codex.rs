//! The Codex driver: `codex app-server` JSON-RPC, as a state machine with no IO.
//!
//! One app-server hosts every Codex session. Each Keystone session maps to one Codex
//! thread. Lines go in through [`CodexDriver::on_line`], user actions through
//! [`CodexDriver::command`], and both produce lines to write and [`Event`]s per session.
//! Checked against codex-cli 0.162.1.

use crate::driver::{Command, Outputs};
use crate::item::{Approval, Event, Item, ItemKind, ItemStatus, RequestId, TurnStatus, cap_output};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;

/// Notifications Keystone does not show. Codex skips sending them.
const OPT_OUT: [&str; 6] = [
    "remoteControl/status/changed",
    "account/updated",
    "account/rateLimits/updated",
    "mcpServer/startupStatus/updated",
    "thread/status/changed",
    "thread/tokenUsage/updated",
];

enum Pending {
    Initialize,
    Thread {
        session: u64,
        resume: bool,
    },
    TurnStart {
        session: u64,
    },
    Steer {
        session: u64,
        text: String,
        turn: String,
        /// The session's stop count when the steer was sent.
        epoch: u64,
    },
    Ignore,
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Turn {
    #[default]
    Idle,
    /// `turn/start` was sent; the turn has no ID to steer or stop yet.
    Starting,
    Running(String),
}

#[derive(Default)]
struct Session {
    thread: Option<String>,
    opening: bool,
    turn: Turn,
    /// Messages accepted but not yet delivered, in order: sent before the thread
    /// existed, or while a turn was starting and had no ID to steer.
    waiting: Vec<String>,
    /// Stop was asked while the turn was starting. It is sent once the turn has an ID.
    stop_requested: bool,
    /// The user ended the running turn, with Stop or a denial that cancels it. Its
    /// `turn/completed` reports it, so it never carries over to another turn.
    stopped: bool,
    /// Counts stops. A steer sent before a stop must not start a turn after it.
    epoch: u64,
    /// Open approval requests and the decision that denies each one.
    approvals: HashMap<RequestId, &'static str>,
}

pub struct CodexDriver {
    next_id: u64,
    ready: bool,
    before_ready: Vec<String>,
    pending: HashMap<u64, Pending>,
    sessions: HashMap<u64, Session>,
    threads: HashMap<String, u64>,
    /// Messages Keystone did not recognize. Never fatal.
    pub unknown: u64,
}

impl CodexDriver {
    /// Starts the handshake. Write `out.writes` before anything else.
    pub fn new(client_version: &str, out: &mut Outputs) -> Self {
        let mut driver = Self {
            next_id: 0,
            ready: false,
            before_ready: Vec::new(),
            pending: HashMap::new(),
            sessions: HashMap::new(),
            threads: HashMap::new(),
            unknown: 0,
        };
        let line = driver.request(
            "initialize",
            json!({
                "clientInfo": {"name": "keystone", "title": "Keystone", "version": client_version},
                "capabilities": {"optOutNotificationMethods": OPT_OUT},
            }),
            Pending::Initialize,
        );
        out.writes.push(line);
        driver
    }

    /// Whether the session's conversation is open or opening in this process.
    pub fn is_open(&self, session: u64) -> bool {
        self.sessions
            .get(&session)
            .is_some_and(|s| s.thread.is_some() || s.opening)
    }

    pub fn is_running(&self, session: u64) -> bool {
        self.sessions
            .get(&session)
            .is_some_and(|s| s.turn != Turn::Idle)
    }

    pub fn command(&mut self, command: Command, out: &mut Outputs) {
        match command {
            Command::Open {
                session,
                cwd,
                resume,
            } => {
                self.sessions.entry(session).or_default().opening = true;
                self.open_thread(session, cwd, resume, out);
            }
            Command::Send { session, text } => self.send(session, text, out),
            Command::Interrupt { session } => self.interrupt(session, out),
            Command::Answer {
                session,
                request,
                allow,
            } => {
                let Some(state) = self.sessions.get_mut(&session) else {
                    return;
                };
                let Some(deny) = state.approvals.remove(&request) else {
                    return;
                };
                let decision = if allow { "accept" } else { deny };
                // "decline" lets the turn go on; "cancel" ends it.
                state.stopped |= decision == "cancel";
                out.writes.push(format!(
                    r#"{{"id":{},"result":{{"decision":"{decision}"}}}}"#,
                    request.0
                ));
            }
        }
    }

    /// Handles one line of app-server output.
    pub fn on_line(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(envelope) = serde_json::from_slice::<Envelope>(line) else {
            self.unknown += 1;
            return;
        };
        match (envelope.method, envelope.id) {
            (Some(method), Some(id)) => self.on_server_request(&method, id, line, out),
            (Some(method), None) => self.on_notification(&method, line, out),
            (None, Some(id)) => self.on_response(id, envelope.error, line, out),
            (None, None) => self.unknown += 1,
        }
    }

    /// The app-server exited. Every running turn fails, and every thread must be
    /// opened again in a new process.
    pub fn on_exit(&mut self, reason: &str, out: &mut Outputs) {
        for (&session, state) in &mut self.sessions {
            if std::mem::take(&mut state.turn) != Turn::Idle {
                out.event(
                    session,
                    Event::TurnCompleted {
                        status: TurnStatus::Failed,
                        error: Some(reason.to_owned()),
                        stopped: false,
                    },
                );
            } else if state.opening || !state.waiting.is_empty() {
                out.event(
                    session,
                    Event::Failed {
                        message: reason.to_owned(),
                    },
                );
            }
        }
        self.sessions.clear();
        self.threads.clear();
        self.pending.clear();
        self.ready = false;
    }

    fn request(&mut self, method: &str, params: Value, pending: Pending) -> String {
        self.next_id += 1;
        self.pending.insert(self.next_id, pending);
        json!({"id": self.next_id, "method": method, "params": params}).to_string()
    }

    fn write(&mut self, line: String, out: &mut Outputs) {
        if self.ready {
            out.writes.push(line);
        } else {
            self.before_ready.push(line);
        }
    }

    fn open_thread(
        &mut self,
        session: u64,
        cwd: String,
        resume: Option<String>,
        out: &mut Outputs,
    ) {
        let (method, params) = match &resume {
            Some(thread) => (
                "thread/resume",
                json!({"threadId": thread, "cwd": cwd, "excludeTurns": true}),
            ),
            None => ("thread/start", json!({"cwd": cwd})),
        };
        let line = self.request(
            method,
            params,
            Pending::Thread {
                session,
                resume: resume.is_some(),
            },
        );
        self.write(line, out);
    }

    /// Delivers a message: a new turn when idle, a steer while a turn runs, or a
    /// place in line while the thread opens or the turn starts.
    fn send(&mut self, session: u64, text: String, out: &mut Outputs) {
        let state = self.sessions.entry(session).or_default();
        match (state.thread.clone(), state.turn.clone()) {
            (None, _) if !state.opening => out.event(
                session,
                Event::Failed {
                    message: "This conversation is not open in Codex".into(),
                },
            ),
            (None, _) | (Some(_), Turn::Starting) => state.waiting.push(text),
            (Some(thread), Turn::Running(turn)) => {
                let epoch = state.epoch;
                let line = self.request(
                    "turn/steer",
                    json!({"threadId": thread, "expectedTurnId": turn, "input": text_input(&text)}),
                    Pending::Steer {
                        session,
                        text,
                        turn,
                        epoch,
                    },
                );
                self.write(line, out);
            }
            (Some(thread), Turn::Idle) => {
                // Starting, so further messages wait for the ID instead of starting a
                // parallel turn.
                state.turn = Turn::Starting;
                let line = self.request(
                    "turn/start",
                    json!({"threadId": thread, "input": text_input(&text)}),
                    Pending::TurnStart { session },
                );
                self.write(line, out);
            }
        }
    }

    fn interrupt(&mut self, session: u64, out: &mut Outputs) {
        let Some(state) = self.sessions.get_mut(&session) else {
            return;
        };
        let dropped = !std::mem::take(&mut state.waiting).is_empty();
        state.epoch += 1;
        match (state.thread.clone(), state.turn.clone()) {
            (Some(thread), Turn::Running(turn)) => {
                state.stopped = true;
                let line = self.request(
                    "turn/interrupt",
                    json!({"threadId": thread, "turnId": turn}),
                    Pending::Ignore,
                );
                self.write(line, out);
            }
            // A running turn always has a thread.
            (None, Turn::Running(_)) => {}
            // Remembered, and sent once `turn/started` gives the turn an ID.
            (_, Turn::Starting) => state.stop_requested = true,
            // Nothing reached Codex yet; the dropped messages end the turn here. With
            // none, the last turn already ended and must keep its own end.
            (_, Turn::Idle) if dropped => out.event(
                session,
                Event::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    error: None,
                    stopped: true,
                },
            ),
            (_, Turn::Idle) => {}
        }
    }

    /// Delivers what was waiting for the thread or the turn ID, and a pending stop.
    fn flush(&mut self, session: u64, out: &mut Outputs) {
        let Some(state) = self.sessions.get_mut(&session) else {
            return;
        };
        if state.thread.is_none() {
            return;
        }
        if let Turn::Running(_) = state.turn
            && std::mem::take(&mut state.stop_requested)
        {
            return self.interrupt(session, out);
        }
        if state.turn == Turn::Starting {
            return;
        }
        let waiting = std::mem::take(&mut state.waiting);
        for text in waiting {
            self.send(session, text, out);
        }
    }

    fn on_response(&mut self, id: Value, error: Option<RpcError>, line: &[u8], out: &mut Outputs) {
        let Some(pending) = id.as_u64().and_then(|id| self.pending.remove(&id)) else {
            self.unknown += 1;
            return;
        };
        match pending {
            Pending::Initialize => {
                self.ready = true;
                out.writes.push(r#"{"method":"initialized"}"#.to_owned());
                out.writes.append(&mut self.before_ready);
            }
            Pending::Thread { session, resume } => {
                let thread = serde_json::from_slice::<Response<ThreadResult>>(line)
                    .ok()
                    .and_then(|r| r.result)
                    .map(|r| r.thread.id);
                match thread {
                    Some(thread) => self.thread_ready(session, thread, out),
                    // Never fall back to a fresh thread on its own: the user would see the
                    // old conversation while Codex had none of it.
                    None => {
                        let state = self.sessions.entry(session).or_default();
                        state.opening = false;
                        state.waiting.clear();
                        let reason = error.map_or("no thread in the reply".into(), |e| e.message);
                        let message = if resume {
                            format!(
                                "Could not resume this Codex conversation: {reason}. Start a new Codex session to continue."
                            )
                        } else {
                            format!("Codex did not start a conversation: {reason}")
                        };
                        out.event(session, Event::Failed { message });
                    }
                }
            }
            Pending::TurnStart { session } => {
                if let Some(error) = error {
                    if let Some(state) = self.sessions.get_mut(&session) {
                        state.turn = Turn::Idle;
                        state.waiting.clear();
                        state.stop_requested = false;
                    }
                    out.event(
                        session,
                        Event::TurnCompleted {
                            status: TurnStatus::Failed,
                            error: Some(error.message),
                            stopped: false,
                        },
                    );
                }
            }
            Pending::Steer {
                session,
                text,
                turn,
                epoch,
            } => {
                let Some(error) = error else { return };
                let Some(state) = self.sessions.get(&session) else {
                    return;
                };
                let still_running = state.turn == Turn::Running(turn.clone());
                if state.epoch != epoch {
                    // The user stopped the turn after sending this; never restart work.
                    out.event(
                        session,
                        Event::Warning {
                            message: "A message sent before Stop was not delivered".into(),
                        },
                    );
                } else if still_running {
                    // A real failure while the turn goes on. Report it; the turn continues.
                    out.event(
                        session,
                        Event::Warning {
                            message: format!("Could not steer the running turn: {}", error.message),
                        },
                    );
                } else {
                    // Codex reports `turn/completed` before rejecting a steer for that
                    // turn, so the turn has ended: the message starts the next one.
                    self.send(session, text, out);
                }
            }
            Pending::Ignore => {}
        }
    }

    fn thread_ready(&mut self, session: u64, thread: String, out: &mut Outputs) {
        self.threads.insert(thread.clone(), session);
        let state = self.sessions.entry(session).or_default();
        state.opening = false;
        state.thread = Some(thread.clone());
        out.event(session, Event::ThreadReady { thread_id: thread });
        if std::mem::take(&mut state.stop_requested) {
            state.waiting.clear();
        }
        self.flush(session, out);
    }

    fn session_for(&self, thread: &str) -> Option<u64> {
        self.threads.get(thread).copied()
    }

    fn on_notification(&mut self, method: &str, line: &[u8], out: &mut Outputs) {
        match method {
            "turn/started" => {
                let Some(note) = params::<TurnNote>(line) else {
                    return self.unknown += 1;
                };
                if let Some(session) = self.session_for(&note.thread_id) {
                    if let Some(state) = self.sessions.get_mut(&session) {
                        state.turn = Turn::Running(note.turn.id);
                    }
                    out.event(session, Event::TurnStarted);
                    self.flush(session, out);
                }
            }
            "turn/completed" => {
                let Some(note) = params::<TurnNote>(line) else {
                    return self.unknown += 1;
                };
                if let Some(session) = self.session_for(&note.thread_id) {
                    let mut stopped = false;
                    if let Some(state) = self.sessions.get_mut(&session) {
                        state.turn = Turn::Idle;
                        state.stop_requested = false;
                        stopped = std::mem::take(&mut state.stopped);
                        // Requests from this turn can no longer be answered.
                        for request in state.approvals.drain().map(|(r, _)| r) {
                            out.event(session, Event::ApprovalResolved { request });
                        }
                    }
                    let status = match note.turn.status.as_str() {
                        "completed" => TurnStatus::Completed,
                        "interrupted" => TurnStatus::Interrupted,
                        _ => TurnStatus::Failed,
                    };
                    let error = note.turn.error.map(|e| e.message);
                    out.event(
                        session,
                        Event::TurnCompleted {
                            status,
                            error,
                            stopped,
                        },
                    );
                    // Messages that waited for this turn's ID start the next turn.
                    self.flush(session, out);
                }
            }
            "item/started" | "item/completed" => {
                let Some(note) = params::<ItemNote>(line) else {
                    return self.unknown += 1;
                };
                if let Some(session) = self.session_for(&note.thread_id) {
                    let item = note.item.into_item(method == "item/started");
                    out.event(
                        session,
                        if method == "item/started" {
                            Event::ItemStarted(item)
                        } else {
                            Event::ItemCompleted(item)
                        },
                    );
                }
            }
            "item/agentMessage/delta"
            | "item/reasoning/summaryTextDelta"
            | "item/commandExecution/outputDelta" => {
                let Some(note) = params::<DeltaNote>(line) else {
                    return self.unknown += 1;
                };
                if let Some(session) = self.session_for(&note.thread_id) {
                    out.event(
                        session,
                        Event::ItemDelta {
                            item_id: note.item_id,
                            text: note.delta,
                        },
                    );
                }
            }
            "serverRequest/resolved" => {
                let Some(note) = params::<ResolvedNote>(line) else {
                    return self.unknown += 1;
                };
                if let Some(session) = self.session_for(&note.thread_id) {
                    let request = RequestId(note.request_id.to_string());
                    if let Some(state) = self.sessions.get_mut(&session) {
                        state.approvals.remove(&request);
                    }
                    out.event(session, Event::ApprovalResolved { request });
                }
            }
            "error" => {
                // `willRetry` errors are transient; the final failure arrives with
                // `turn/completed`.
                if let Some(note) = params::<ErrorNote>(line)
                    && !note.will_retry
                    && let Some(session) = self.session_for(&note.thread_id)
                {
                    out.event(
                        session,
                        Event::Failed {
                            message: note.error.message,
                        },
                    );
                }
            }
            _ => self.unknown += 1,
        }
    }

    fn on_server_request(&mut self, method: &str, id: Value, line: &[u8], out: &mut Outputs) {
        let request = RequestId(id.to_string());
        let approval = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                params::<ApprovalParams>(line)
            }
            _ => None,
        };
        let Some(params) = approval else {
            // Answer everything else with an error, so Codex never waits forever.
            out.writes.push(format!(
                r#"{{"id":{},"error":{{"code":-32601,"message":"Keystone does not support {method} yet"}}}}"#,
                request.0
            ));
            return;
        };
        let Some(session) = self.session_for(&params.thread_id) else {
            out.writes.push(format!(
                r#"{{"id":{},"result":{{"decision":"cancel"}}}}"#,
                request.0
            ));
            return;
        };
        // Codex lists the decisions it accepts. Prefer one that lets the turn continue.
        let offers = |name: &str| {
            params.available_decisions.is_empty()
                || params
                    .available_decisions
                    .iter()
                    .any(|d| d.as_str() == Some(name))
        };
        let deny = if offers("decline") {
            "decline"
        } else {
            "cancel"
        };
        let action = match (&params.command, method) {
            (Some(command), _) => command.clone(),
            (None, "item/fileChange/requestApproval") => "Apply file changes".to_owned(),
            (None, _) => "Run a command".to_owned(),
        };
        if let Some(state) = self.sessions.get_mut(&session) {
            state.approvals.insert(request.clone(), deny);
        }
        out.event(
            session,
            Event::ApprovalRequested(Approval {
                request,
                item_id: params.item_id,
                action,
                detail: params.reason.or(params.cwd),
            }),
        );
    }
}

fn text_input(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

fn params<'a, T: Deserialize<'a>>(line: &'a [u8]) -> Option<T> {
    serde_json::from_slice::<Params<T>>(line)
        .ok()
        .map(|p| p.params)
}

/// The first pass over every line: which kind of message it is. Bodies are skipped,
/// not built.
#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(borrow)]
    method: Option<Cow<'a, str>>,
    id: Option<Value>,
    error: Option<RpcError>,
    #[allow(dead_code)]
    params: Option<IgnoredAny>,
    #[allow(dead_code)]
    result: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

#[derive(Deserialize)]
struct Params<T> {
    params: T,
}

#[derive(Deserialize)]
struct Response<T> {
    result: Option<T>,
}

#[derive(Deserialize)]
struct ThreadResult {
    thread: Named,
}

#[derive(Deserialize)]
struct Named {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnNote {
    thread_id: String,
    turn: WireTurn,
}

#[derive(Deserialize)]
struct WireTurn {
    id: String,
    #[serde(default)]
    status: String,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemNote {
    thread_id: String,
    item: WireItem,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeltaNote {
    thread_id: String,
    item_id: String,
    delta: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolvedNote {
    thread_id: String,
    request_id: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorNote {
    thread_id: String,
    error: RpcError,
    #[serde(default)]
    will_retry: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalParams {
    thread_id: String,
    item_id: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    reason: Option<String>,
    #[serde(default)]
    available_decisions: Vec<Value>,
}

/// Every Codex item type in one flat shape. Fields a type does not use stay empty.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct WireItem {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    text: Option<String>,
    content: Vec<Value>,
    summary: Vec<Value>,
    command: Option<String>,
    status: Option<String>,
    aggregated_output: Option<String>,
    exit_code: Option<i32>,
    changes: Vec<Change>,
    server: Option<String>,
    tool: Option<String>,
}

#[derive(Deserialize, Default)]
struct Change {
    #[serde(default)]
    path: String,
}

impl WireItem {
    /// `started` items are in progress even when, like agent messages, they carry no
    /// status of their own.
    fn into_item(self, started: bool) -> Item {
        let status = match self.status.as_deref() {
            Some("inProgress") => ItemStatus::InProgress,
            Some("failed") => ItemStatus::Failed,
            Some("declined") => ItemStatus::Declined,
            _ if started => ItemStatus::InProgress,
            _ => ItemStatus::Completed,
        };
        let (kind, text) = match self.kind.as_str() {
            "userMessage" => (ItemKind::UserMessage, join_text(&self.content)),
            "agentMessage" => (ItemKind::AgentMessage, self.text.unwrap_or_default()),
            "reasoning" => (ItemKind::Reasoning, join_text(&self.summary)),
            "commandExecution" => (
                ItemKind::Command {
                    command: self.command.unwrap_or_default(),
                    exit_code: self.exit_code,
                },
                cap_output(self.aggregated_output.as_deref().unwrap_or_default()),
            ),
            "fileChange" => (
                ItemKind::FileChange {
                    paths: self.changes.into_iter().map(|c| c.path).collect(),
                },
                String::new(),
            ),
            "mcpToolCall" => (
                ItemKind::Tool {
                    name: format!(
                        "{}.{}",
                        self.server.unwrap_or_default(),
                        self.tool.unwrap_or_default()
                    ),
                },
                String::new(),
            ),
            other => (
                ItemKind::Other {
                    name: other.to_owned(),
                },
                String::new(),
            ),
        };
        Item {
            id: self.id,
            kind,
            text,
            status,
        }
    }
}

/// Text from a list of strings or `{"type": "text", "text": ..}` parts.
fn join_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| part.as_str().or_else(|| part.get("text")?.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}
