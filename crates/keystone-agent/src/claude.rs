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

use crate::driver::{ApprovalState, Command, Outputs};
use crate::item::{
    Approval, Catalog, Event, Item, ItemKind, ItemStatus, ModelInfo, RequestId, TurnStatus,
    cap_output,
};
use crate::mcp::{Caller, Issuer};
use keystone_core::ApprovalMode;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// The arguments Keystone always passes, before any `--permission-mode` and `--resume`.
/// Permission prompts come to Keystone as `can_use_tool` requests. With the default
/// approval mode no permission mode is set, so the user's own Claude configuration
/// decides what needs approval.
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

/// Claude's `--permission-mode` for an approval mode, or `None` to pass none.
/// `default` is the mode Claude 2.1.300 also calls `manual`.
pub fn permission_mode(mode: ApprovalMode) -> Option<&'static str> {
    match mode {
        ApprovalMode::Provider => None,
        ApprovalMode::ReadOnly => Some("plan"),
        ApprovalMode::Ask => Some("default"),
        ApprovalMode::Auto => Some("acceptEdits"),
        ApprovalMode::FullAccess => Some("bypassPermissions"),
    }
}

/// The arguments for a process launched in `mode`, before any `--resume`.
pub fn args(mode: ApprovalMode) -> Vec<&'static str> {
    let mut args = ARGS.to_vec();
    if let Some(mode) = permission_mode(mode) {
        args.extend(["--permission-mode", mode]);
    }
    args
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Launch {
    #[default]
    Chat,
    LayoutHelper,
}

const LAYOUT_HELPER_ARGS: [&str; 11] = [
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

impl Launch {
    pub(crate) fn approval(self, asked: ApprovalState) -> ApprovalState {
        match self {
            Launch::Chat => asked,
            Launch::LayoutHelper => ApprovalState {
                mode: ApprovalMode::ReadOnly,
                overridden: true,
            },
        }
    }

    pub(crate) fn args<'a>(
        self,
        mode: ApprovalMode,
        baseline: Option<&'a str>,
        model: Option<&'a str>,
        resume: Option<&'a str>,
    ) -> Vec<&'a str> {
        match self {
            Launch::Chat => {
                let mut args = args(mode);
                if let Some(baseline) = baseline {
                    args.extend(["--permission-mode", baseline]);
                }
                if let Some(model) = model {
                    args.extend(["--model", model]);
                }
                if let Some(resume) = resume {
                    args.extend(["--resume", resume]);
                }
                args
            }
            Launch::LayoutHelper => {
                let mut args = args(ApprovalMode::ReadOnly);
                args.extend(LAYOUT_HELPER_ARGS);
                args
            }
        }
    }
}

/// What Claude is told when the user denies a tool call.
const DENIED: &str = "The user denied this in Keystone.";

/// What Claude is told when it asks to leave plan mode under Read only. Approving
/// `ExitPlanMode` returns Claude to the mode it had before plan mode, which can write
/// files without asking, so Read only never allows it.
const READ_ONLY_PLAN: &str = "Keystone's approval mode is Read only, so Claude Code stays in plan mode and cannot make changes. Present the plan and stop; the user can choose another approval mode to carry it out.";

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
    /// The user stopped this turn. Its end reports it, so it never reaches another turn.
    stopping: bool,
    /// Messages not written yet: they wait for Claude to confirm the selected
    /// permission mode, or a steer waits to learn whether Claude reports lifecycle
    /// frames.
    held: Vec<QueuedMessage>,
    /// Whether the turn's first message was written.
    written: bool,
}

enum Request {
    Initialize,
    Mcp,
    Interrupt,
    /// A `set_permission_mode` for the selection with this version.
    SetMode {
        version: u64,
        target: &'static str,
    },
}

pub struct QueuedMessage {
    text: String,
    authorization: Option<(Issuer, Caller)>,
}

struct Pending {
    /// Claude's own ID for the request, which the answer must name.
    request_id: Value,
    input: Value,
    tool_use_id: Option<String>,
    /// Whether allowing it leaves plan mode.
    exits_plan: bool,
}

pub struct ClaudeDriver {
    cancel_queued: bool,
    session: u64,
    resume: bool,
    seed: u64,
    next_message: u64,
    next_request: u64,
    requests: HashMap<String, Request>,
    /// Claude's session ID, from `system/init`.
    conversation: Option<String>,
    /// The model `system/init` last reported.
    model: Option<String>,
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
    /// How to sign this process's account in, when it is not the default account.
    sign_in: Option<String>,
    /// The approval mode the process was launched with.
    launched: ApprovalMode,
    /// The approval mode the user selected last, and its version, which counts every
    /// change. Each `set_permission_mode` records the version it was sent for.
    mode: ApprovalMode,
    version: u64,
    /// Whether Claude answered `initialize`. No message is written before.
    initialized: bool,
    mcp_request: Option<Value>,
    mcp_ready: bool,
    mcp_enabled: bool,
    /// The permission mode Claude last confirmed: in its `initialize` answer, an
    /// answer to `set_permission_mode`, or a `system` report.
    confirmed: Option<String>,
    /// Whether Keystone changed the permission mode in this process.
    changed: bool,
    /// Whether the user allowed `ExitPlanMode` in the running turn. Claude's mode is
    /// then unknown until the turn ends, so steers wait for the next turn.
    leaving_plan: bool,
    /// Whether `initialize` or a `set_permission_mode` failed or went unanswered too
    /// long. The process cannot be relied on, so it is replaced before anything else
    /// is written to it.
    untrusted: bool,
    /// `initialize` and `set_permission_mode` requests sent since the connection last
    /// asked, so it can time them ([`ClaudeDriver::take_timed_requests`]).
    timed: Vec<String>,
    launch: Launch,
    provider_version: Option<String>,
    helper_init_confirmed: bool,
    refused: bool,
    /// Messages Keystone did not recognize. Never fatal.
    pub unknown: u64,
}

impl ClaudeDriver {
    /// Starts the handshake. `seed` makes message UUIDs unique to this process; tests
    /// pass a fixed one. `resume` says whether the process was started with `--resume`,
    /// and `mode` is the approval mode it was launched with ([`args`]).
    pub fn new(
        session: u64,
        resume: bool,
        seed: u64,
        mode: ApprovalMode,
        out: &mut Outputs,
    ) -> Self {
        Self::launched(session, resume, seed, mode, Launch::Chat, out)
    }

    /// Names the account and how to sign it in, for a signed-out error.
    pub fn set_sign_in(&mut self, sign_in: Option<&str>) {
        self.sign_in = sign_in.map(str::to_owned);
    }

    pub(crate) fn launched(
        session: u64,
        resume: bool,
        seed: u64,
        mode: ApprovalMode,
        launch: Launch,
        out: &mut Outputs,
    ) -> Self {
        let mut driver = Self {
            cancel_queued: true,
            session,
            resume,
            seed,
            next_message: 0,
            next_request: 0,
            requests: HashMap::new(),
            conversation: None,
            model: None,
            lifecycle: None,
            turn: None,
            message: None,
            blocks: HashMap::new(),
            tools: HashMap::new(),
            denied: HashSet::new(),
            approvals: HashMap::new(),
            api_error: None,
            sign_in: None,
            launched: mode,
            mode,
            version: 0,
            initialized: false,
            mcp_request: None,
            mcp_ready: true,
            mcp_enabled: false,
            confirmed: None,
            changed: false,
            leaving_plan: false,
            untrusted: false,
            timed: Vec::new(),
            launch,
            provider_version: None,
            helper_init_confirmed: false,
            refused: false,
            unknown: 0,
        };
        let line = driver.request(json!({"subtype": "initialize"}), Request::Initialize);
        out.writes.push(line);
        driver
    }

    pub fn is_running(&self) -> bool {
        self.turn.is_some()
    }

    pub(crate) fn set_mcp(&mut self, url: String, token: String) {
        self.mcp_request = Some(
            json!({"subtype": "mcp_set_servers", "servers": {"keystone": {
                "type": "http", "url": url, "headers": {"Authorization": format!("Bearer {token}")}
            }}}),
        );
        self.mcp_ready = false;
        self.mcp_enabled = true;
    }

    pub(crate) fn send_from(
        &mut self,
        text: String,
        issuer: Issuer,
        caller: Caller,
        out: &mut Outputs,
    ) {
        self.enqueue(
            QueuedMessage {
                text,
                authorization: Some((issuer, caller)),
            },
            out,
        );
    }

    pub(crate) fn steer(&mut self, text: String, out: &mut Outputs) -> Result<(), &'static str> {
        if !self.mode_confirmed()
            || self
                .turn
                .as_ref()
                .is_none_or(|turn| !turn.written || turn.stopping || !turn.held.is_empty())
        {
            return Err("busy: Claude Code cannot steer this turn immediately");
        }
        if self.steer_unsupported().is_some() {
            return Err("busy: Claude Code cannot safely steer this turn yet");
        }
        self.write(
            QueuedMessage {
                text,
                authorization: None,
            },
            out,
        );
        Ok(())
    }

    pub(crate) fn set_cancel_queued(&mut self, enabled: bool) {
        self.cancel_queued = enabled;
    }

    fn interrupt_request(&self) -> Value {
        let mut request = json!({"subtype": "interrupt"});
        if self.cancel_queued {
            request["cancel_queued"] = json!(true);
        }
        request
    }

    /// Whether a message in the selected approval mode needs a new process. A running
    /// process cannot enter `bypassPermissions` unless launched in it, and one
    /// launched in it is never lowered in place. The user's own settings come back
    /// only in a process Keystone never changed the mode of. A process whose mode is
    /// unknown is replaced too.
    pub fn needs_restart(&self) -> bool {
        self.untrusted
            || match self.mode {
                ApprovalMode::FullAccess => self.launched != ApprovalMode::FullAccess,
                _ if self.launched == ApprovalMode::FullAccess => true,
                ApprovalMode::Provider => self.launched != ApprovalMode::Provider || self.changed,
                _ => false,
            }
    }

    /// Whether a turn waits for a new process: it needs one and nothing of it was
    /// written. The connection replaces the process, carrying the turn's messages
    /// over with [`ClaudeDriver::take_unsent`] and [`ClaudeDriver::carry`].
    pub fn wants_restart(&self) -> bool {
        self.needs_restart() && self.turn.as_ref().is_some_and(|turn| !turn.written)
    }

    /// Takes the messages of a turn nothing of which was written, ending it without
    /// reporting an end: it goes on in the next process.
    pub fn take_unsent(&mut self) -> Vec<QueuedMessage> {
        match self.turn.take() {
            Some(turn) if !turn.written => turn.held,
            turn => {
                self.turn = turn;
                Vec::new()
            }
        }
    }

    /// Continues a turn carried over from a replaced process. Its start was reported.
    pub fn carry(&mut self, messages: Vec<QueuedMessage>, out: &mut Outputs) {
        if messages.is_empty() || self.turn.is_some() {
            return;
        }
        self.turn = Some(Turn {
            held: messages,
            ..Turn::default()
        });
        self.pump(out);
    }

    /// The `initialize` and `set_permission_mode` requests sent since the last call,
    /// by ID.
    pub fn take_timed_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.timed)
    }

    /// A timed request went unanswered too long. If Claude has still not answered it,
    /// the process is not usable: nothing more is written to it, and the messages
    /// waiting for it fail.
    pub fn request_timed_out(&mut self, id: &str, out: &mut Outputs) {
        let reason = match self.requests.get(id) {
            Some(Request::Initialize) => {
                "Claude Code did not start in time, so nothing was sent. Send again to restart it."
            }
            Some(Request::SetMode { .. }) => {
                "Claude Code did not confirm the approval mode in time, so nothing was sent. Send again to restart it."
            }
            Some(Request::Mcp) => {
                "Claude Code did not configure Keystone tools in time, so nothing was sent. Send again to restart it."
            }
            _ => return,
        };
        self.untrusted = true;
        self.mcp_request = None;
        self.fail_held(reason, out);
    }

    /// Why a message sent now could not steer the running turn, if it could not.
    pub fn steer_blocked(&self) -> Option<&'static str> {
        self.turn.as_ref().and_then(|_| self.steer_unsupported())
    }

    /// Why this process could not take a steer even once a turn runs, if it could not.
    pub fn steer_unsupported(&self) -> Option<&'static str> {
        match self.lifecycle {
            Some(true) if self.cancel_queued => None,
            Some(true) => Some(
                "This Claude Code version does not report queued-message cancellation (interrupt_cancel_queued_v1), so Keystone cannot safely steer a running turn. Wait for the turn to end or stop it, then send again.",
            ),
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
            // Checked again before every message is written, including those waiting.
            Command::Configure { approval, .. } => {
                let mode = self.launch.approval(approval).mode;
                if mode != self.mode {
                    self.mode = mode;
                    self.version += 1;
                }
                self.pump(out);
            }
            Command::Send { text, .. } => self.send(text, out),
            Command::Interrupt { .. } => {
                // Nothing reached Claude yet, so the turn ends here.
                if let Some(turn) = &mut self.turn
                    && !turn.written
                {
                    turn.stopping = true;
                    return self.finish(TurnStatus::Interrupted, None, out);
                }
                if let Some(turn) = &mut self.turn {
                    turn.stopping = true;
                    // Held steers never reach Claude, and `cancel_queued` drops those it
                    // has not started, so Stop ends all of the turn.
                    turn.held.clear();
                    let line = self.request(self.interrupt_request(), Request::Interrupt);
                    out.writes.push(line);
                }
            }
            Command::Answer { request, allow, .. } => {
                let Some(pending) = self.approvals.remove(&request) else {
                    return;
                };
                let response = if allow && self.launch == Launch::Chat {
                    // Claude leaves plan mode when the tool runs, at a time no later line
                    // is ordered against, so nothing more is written in this turn.
                    if pending.exits_plan {
                        self.leaving_plan = true;
                    }
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
        if self.refused {
            return;
        }
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
            ("system", Some("hook_started")) if self.launch == Launch::LayoutHelper => {
                self.refuse("ran a hook although Keystone turned hooks off", out)
            }
            // Claude reports a mode change, such as after an approved `ExitPlanMode`.
            ("system", Some("status")) => {
                if let Ok(Status {
                    permission_mode: Some(mode),
                }) = serde_json::from_slice(line)
                {
                    if self.launch == Launch::LayoutHelper && mode != "plan" {
                        return self.refuse(
                            &format!(
                                "reported permission mode {mode} although Keystone required plan"
                            ),
                            out,
                        );
                    }
                    self.confirmed = Some(mode);
                    self.pump(out);
                }
            }
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

    /// The process exited. A running turn fails, and the model it reported is gone
    /// with it.
    pub fn on_exit(&mut self, reason: &str, out: &mut Outputs) {
        self.mcp_request = None;
        if self.turn.is_some() {
            self.finish(TurnStatus::Failed, Some(reason.to_owned()), out);
        }
        out.event(self.session, Event::ModelLost);
    }

    pub(crate) fn credentials_failed(&self) -> bool {
        self.untrusted || self.refused
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
        if let Request::Initialize | Request::Mcp | Request::SetMode { .. } = purpose {
            self.timed.push(id.clone());
        }
        let line =
            json!({"type": "control_request", "request_id": id, "request": request}).to_string();
        self.requests.insert(id, purpose);
        line
    }

    /// Starts a turn, or steers the running one. Every message waits in the turn's
    /// `held` list and is written by [`ClaudeDriver::pump`].
    fn send(&mut self, text: String, out: &mut Outputs) {
        self.enqueue(
            QueuedMessage {
                text,
                authorization: None,
            },
            out,
        );
    }

    fn enqueue(&mut self, message: QueuedMessage, out: &mut Outputs) {
        match &mut self.turn {
            Some(turn) => turn.held.push(message),
            None => {
                self.turn = Some(Turn {
                    held: vec![message],
                    ..Turn::default()
                });
                out.event(self.session, Event::TurnStarted);
            }
        }
        self.pump(out);
    }

    /// Whether a user message may be written now: Claude answered `initialize`, no
    /// `set_permission_mode` is unanswered, and the mode Claude confirmed last is the
    /// one the latest selection needs. With Provider settings, Keystone must never
    /// have changed the mode of this process.
    fn mode_confirmed(&self) -> bool {
        !self.needs_restart()
            && self.initialized
            && self.mcp_ready
            && !self.leaving_plan
            && !self.mode_requested()
            && permission_mode(self.mode)
                .is_none_or(|target| self.confirmed.as_deref() == Some(target))
    }

    fn mode_requested(&self) -> bool {
        self.requests
            .values()
            .any(|r| matches!(r, Request::SetMode { .. }))
    }

    /// Writes the held messages if the selected mode is confirmed, or asks Claude for
    /// it when nothing else is pending. A process that needs replacing is left to the
    /// connection ([`ClaudeDriver::wants_restart`]).
    fn pump(&mut self, out: &mut Outputs) {
        if !self.mcp_ready {
            return;
        }
        if self.turn.as_ref().is_none_or(|turn| turn.held.is_empty()) {
            return;
        }
        if !self.mode_confirmed() {
            if self.initialized
                && !self.leaving_plan
                && !self.mode_requested()
                && !self.needs_restart()
                && let Some(target) = permission_mode(self.mode)
            {
                let line = self.request(
                    json!({"subtype": "set_permission_mode", "mode": target}),
                    Request::SetMode {
                        version: self.version,
                        target,
                    },
                );
                self.changed = true;
                out.writes.push(line);
            }
            return;
        }
        let Some(turn) = &mut self.turn else { return };
        let mut refused = Vec::new();
        for text in std::mem::take(&mut turn.held) {
            match &mut self.turn {
                // A steer waits until Claude says whether it reports lifecycle frames.
                Some(turn) if turn.written && self.lifecycle.is_none() => turn.held.push(text),
                Some(_) => self.write(text, out),
                // A refused launch ended the turn; what followed it starts the next one.
                None => refused.push(text),
            }
        }
        if !refused.is_empty() {
            self.turn = Some(Turn {
                held: refused,
                ..Turn::default()
            });
            out.event(self.session, Event::TurnStarted);
            self.pump(out);
        }
    }

    /// Fails the messages that wait: the turn, if none of it was written, or else
    /// its held steers.
    fn fail_held(&mut self, reason: &str, out: &mut Outputs) {
        let Some(turn) = &mut self.turn else { return };
        if !turn.written {
            return self.finish(TurnStatus::Failed, Some(reason.into()), out);
        }
        let n = std::mem::take(&mut turn.held).len();
        if n > 0 {
            out.event(
                self.session,
                Event::Warning {
                    message: format!("{n} message(s) not sent. {reason}"),
                },
            );
        }
    }

    /// Writes one message: the turn's first, or a steer. Only [`ClaudeDriver::pump`]
    /// calls it, after checking the mode; it is checked again here.
    fn write(&mut self, message: QueuedMessage, out: &mut Outputs) {
        if !self.mode_confirmed() {
            if let Some(turn) = &mut self.turn {
                turn.held.push(message);
            }
            return;
        }
        if message
            .authorization
            .as_ref()
            .is_some_and(|(issuer, caller)| !issuer.permits_launch(*caller, self.mode))
        {
            self.finish(
                TurnStatus::Failed,
                Some("The launching agent no longer has permission to send this message".into()),
                out,
            );
            return;
        }
        // Without lifecycle frames, the steered turn's first `result` would end it while
        // the steer still runs. The connection refuses these first; this is the backstop.
        if let Some(turn) = &self.turn
            && turn.written
            && let Some(reason) = self.steer_unsupported()
        {
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
                "message": {"role": "user", "content": message.text},
            "parent_tool_use_id": null,
            "uuid": uuid,
        });
        let tools = self.tools.is_empty();
        let Some(turn) = &mut self.turn else { return };
        if turn.written {
            // `now` aborts the response so the steer is read at once, but it would also
            // abort a running tool call, so wait for the tool to finish.
            message["priority"] = json!(if tools { "now" } else { "next" });
        }
        turn.written = true;
        turn.outstanding.insert(uuid, false);
        out.writes.push(message.to_string());
    }

    /// Output while idle belongs to a turn Claude started itself, such as after a
    /// background task finished.
    fn ensure_turn(&mut self, out: &mut Outputs) {
        if self.turn.is_none() {
            self.turn = Some(Turn {
                started: true,
                written: self.launch == Launch::Chat,
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

    /// Ends the turn. Steers still held after a completed turn start the next one;
    /// otherwise they are dropped, with a warning.
    fn finish(&mut self, status: TurnStatus, error: Option<String>, out: &mut Outputs) {
        self.leaving_plan = false;
        // Only Stop ends a Claude turn early: a denial lets the turn go on.
        let (held, written, stopped) =
            self.turn.take().map_or((Vec::new(), false, false), |turn| {
                (turn.held, turn.written, turn.stopping)
            });
        let carried = if status == TurnStatus::Completed {
            held
        } else {
            // A turn with nothing written reports its own reason.
            if !held.is_empty() && written && status == TurnStatus::Failed {
                out.event(
                    self.session,
                    Event::Warning {
                        message: format!(
                            "{} message(s) not sent, because the turn failed",
                            held.len()
                        ),
                    },
                );
            }
            Vec::new()
        };
        self.message = None;
        self.blocks.clear();
        self.tools.clear();
        self.api_error = None;
        // Requests from this turn can no longer be answered.
        for request in std::mem::take(&mut self.approvals).into_keys() {
            out.event(self.session, Event::ApprovalResolved { request });
        }
        out.event(
            self.session,
            Event::TurnCompleted {
                status,
                error,
                stopped,
            },
        );
        if !carried.is_empty() {
            self.turn = Some(Turn {
                held: carried,
                ..Turn::default()
            });
            out.event(self.session, Event::TurnStarted);
            self.pump(out);
        }
    }

    fn refuse(&mut self, observed: &str, out: &mut Outputs) {
        self.refused = true;
        self.untrusted = true;
        let claude = match &self.provider_version {
            Some(version) => format!("Claude Code {version}"),
            None => "Claude Code".to_owned(),
        };
        let written = self.turn.as_ref().is_some_and(|turn| turn.written);
        let message = format!(
            "{claude} {observed}. The request was {}.",
            if written {
                "stopped and its reply not used"
            } else {
                "not sent"
            }
        );
        if written {
            let line = self.request(self.interrupt_request(), Request::Interrupt);
            out.writes.push(line);
        }
        match self.turn {
            Some(_) => self.finish(TurnStatus::Failed, Some(message), out),
            None => out.event(self.session, Event::Failed { message }),
        }
    }

    fn on_init(&mut self, line: &[u8], out: &mut Outputs) {
        if self.launch == Launch::LayoutHelper {
            let init = serde_json::from_slice::<Value>(line).unwrap_or_default();
            if self.provider_version.is_none() {
                self.provider_version = init["claude_code_version"].as_str().map(str::to_owned);
            }
            if let Some(observed) = helper_init_problem(&init) {
                return self.refuse(&observed, out);
            }
            self.helper_init_confirmed = true;
        }
        let Ok(init) = serde_json::from_slice::<Init>(line) else {
            return self.unknown += 1;
        };
        // A frame seen before `init` already settled it.
        if self.lifecycle != Some(true) {
            self.lifecycle = Some(init.capabilities.iter().any(|c| c == "msg_lifecycle_v1"));
        }
        if init.permission_mode.is_some() {
            self.confirmed = init.permission_mode;
        }
        // Claude repeats `init` before each turn; report the ID and the model once.
        if self.conversation.as_deref() != Some(init.session_id.as_str()) {
            self.conversation = Some(init.session_id.clone());
            out.event(
                self.session,
                Event::ThreadReady {
                    thread_id: init.session_id,
                },
            );
        }
        if let Some(model) = init.model
            && self.model.as_deref() != Some(model.as_str())
        {
            self.model = Some(model.clone());
            out.event(self.session, Event::ModelReported { model });
        }
        self.pump(out);
    }

    fn on_lifecycle(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(frame) = serde_json::from_slice::<Lifecycle>(line) else {
            return self.unknown += 1;
        };
        self.lifecycle = Some(true);
        self.pump(out);
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
                Some("authentication_failed") => match &self.sign_in {
                    Some(sign_in) => format!(
                        "Claude Code is not signed in ({text}). {sign_in}, then send again."
                    ),
                    None => format!(
                        "Claude Code is not signed in ({text}). Run `claude` in a terminal to sign in, then send again."
                    ),
                },
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
        if request.subtype == "can_use_tool"
            && self.mcp_enabled
            && self.mcp_ready
            && request
                .tool_name
                .as_deref()
                .and_then(|name| name.strip_prefix("mcp__keystone__"))
                .is_some_and(|name| {
                    matches!(
                        name,
                        "capabilities"
                            | "session_launch"
                            | "session_send"
                            | "session_read"
                            | "session_wait"
                            | "session_interrupt"
                    )
                })
        {
            out.writes.push(json!({"type":"control_response", "response":{"subtype":"success", "request_id":control.request_id, "response":{"behavior":"allow", "updatedInput":request.input.unwrap_or(Value::Null)}}}).to_string());
            return;
        }
        if request.subtype == "can_use_tool"
            && request.tool_name.as_deref() == Some("ExitPlanMode")
            && self.mode == ApprovalMode::ReadOnly
        {
            if let Some(id) = &request.tool_use_id {
                self.denied.insert(id.clone());
            }
            out.writes.push(
                json!({"type": "control_response", "response": {"subtype": "success",
                    "request_id": control.request_id,
                    "response": {"behavior": "deny", "message": READ_ONLY_PLAN}}})
                .to_string(),
            );
            return out.event(
                self.session,
                Event::Warning {
                    message: "Claude Code asked to leave plan mode. Read only keeps it there, so it was refused; choose another approval mode to let it make changes.".into(),
                },
            );
        }
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
        let exits_plan = request.tool_name.as_deref() == Some("ExitPlanMode");
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
                exits_plan,
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
        let failed = match purpose {
            Request::Mcp => {
                let ready = response.subtype == "success"
                    && response.response.as_ref().is_some_and(|answer| {
                        answer
                            .added
                            .as_ref()
                            .is_some_and(|added| added.iter().any(|name| name == "keystone"))
                            && answer.errors.as_ref().is_some_and(|errors| {
                                errors.as_object().is_some_and(|errors| errors.is_empty())
                            })
                    });
                if ready {
                    self.mcp_ready = true;
                    return self.pump(out);
                }
                self.untrusted = true;
                self.fail_held(
                    "Claude Code could not configure Keystone tools; nothing was sent",
                    out,
                );
                return out.event(
                    self.session,
                    Event::Failed {
                        message: "Claude Code could not configure Keystone tools; nothing was sent"
                            .into(),
                    },
                );
            }
            Request::SetMode { version, target } => {
                return self.on_mode_set(response, version, target, out);
            }
            Request::Initialize if response.subtype != "error" => {
                if self.launch == Launch::LayoutHelper {
                    let answer = response.response.as_ref();
                    self.provider_version = answer
                        .and_then(|a| a.claude_code_version.as_ref()?.as_str())
                        .map(str::to_owned);
                    match answer.and_then(|a| a.commands.as_ref()?.as_array()) {
                        Some(commands) if commands.is_empty() => {}
                        Some(commands) => {
                            return self.refuse(
                                &format!(
                                    "listed {} slash commands or skills at startup although Keystone turned them off",
                                    commands.len()
                                ),
                                out,
                            );
                        }
                        None => {
                            return self.refuse(
                                "did not list its slash commands at startup, so Keystone could not confirm they were off",
                                out,
                            );
                        }
                    }
                }
                self.initialized = true;
                self.confirmed = response
                    .response
                    .and_then(|r| r.current_permission_mode)
                    .or_else(|| permission_mode(self.launched).map(str::to_owned));
                if let Some(request) = self.mcp_request.take() {
                    let line = self.request(request, Request::Mcp);
                    out.writes.push(line);
                }
                return self.pump(out);
            }
            Request::Initialize => "Claude Code did not start",
            Request::Interrupt => "Could not stop the turn",
        };
        if response.subtype != "error" {
            return;
        }
        let error = response.error.unwrap_or_else(|| "no reason given".into());
        let message = format!("{failed}: {error}");
        match purpose {
            Request::Initialize => {
                self.untrusted = true;
                self.mcp_request = None;
                if self.turn.is_some() {
                    self.finish(TurnStatus::Failed, Some(message.clone()), out);
                }
                out.event(self.session, Event::Failed { message });
            }
            _ => out.event(self.session, Event::Warning { message }),
        }
    }

    /// Claude answered a `set_permission_mode`. Its answer is what the process runs in
    /// now, whichever selection it was for. Held messages go out only once that is the
    /// latest selection's mode ([`ClaudeDriver::pump`]); a refusal of the latest
    /// selection fails them unsent, and an older one is followed by a new request.
    fn on_mode_set(
        &mut self,
        response: ControlResult,
        version: u64,
        target: &'static str,
        out: &mut Outputs,
    ) {
        if response.subtype == "error" {
            if version == self.version {
                let error = response.error.unwrap_or_else(|| "no reason given".into());
                return self.fail_held(
                    &format!(
                        "Claude Code did not accept the approval mode, so nothing was sent: {error}"
                    ),
                    out,
                );
            }
        } else {
            self.confirmed = Some(
                response
                    .response
                    .and_then(|r| r.mode)
                    .unwrap_or_else(|| target.to_owned()),
            );
        }
        self.pump(out);
    }

    fn on_result(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(result) = serde_json::from_slice::<ResultMessage>(line) else {
            return self.unknown += 1;
        };
        let failed = result.is_error || result.subtype != "success";
        if !failed && self.launch == Launch::LayoutHelper {
            if !self.initialized
                || !self.turn.as_ref().is_some_and(|turn| turn.written)
                || self.confirmed.as_deref() != Some("plan")
            {
                return self.refuse(
                    "did not complete initialization and a written helper request in plan mode, so Keystone could not confirm the helper restrictions",
                    out,
                );
            }
            if !self.helper_init_confirmed {
                return self.refuse(
                    "did not report system/init tools and MCP servers, so Keystone could not confirm the helper restrictions",
                    out,
                );
            }
        }
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

/// The catalog in Claude's answer to an `initialize` control request, or `None` when
/// `line` is not such an answer. The `default` entry says what the default resolves to
/// and is not itself a choice, so it becomes the catalog's default model.
pub fn initialize_catalog(line: &[u8]) -> Option<Catalog> {
    let Ok(ControlResponse { response }) = serde_json::from_slice::<ControlResponse>(line) else {
        return None;
    };
    if response.subtype == "error" {
        return Some(Catalog::Failed(
            response.error.unwrap_or_else(|| "no reason given".into()),
        ));
    }
    let models = response.response?.models?;
    let default_model = models
        .iter()
        .find(|m| m.value == "default")
        .and_then(|m| m.resolved_model.clone());
    let models = models
        .into_iter()
        .filter(|m| m.value != "default")
        .map(|m| ModelInfo {
            model: m.value,
            label: m.display_name,
            resolved: m.resolved_model,
        })
        .collect();
    Some(Catalog::Ready {
        models,
        default_model,
    })
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
    #[serde(rename = "permissionMode")]
    permission_mode: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "permissionMode")]
    permission_mode: Option<String>,
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
    response: Option<ModeSet>,
}

#[derive(Deserialize)]
struct ModeSet {
    added: Option<Vec<String>>,
    errors: Option<Value>,
    mode: Option<String>,
    current_permission_mode: Option<String>,
    commands: Option<Value>,
    claude_code_version: Option<Value>,
    models: Option<Vec<WireModel>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireModel {
    value: String,
    display_name: String,
    resolved_model: Option<String>,
}

fn helper_init_problem(init: &Value) -> Option<String> {
    if let Some(mode) = init["permissionMode"].as_str()
        && mode != "plan"
    {
        return Some(format!(
            "reported permission mode {mode} although Keystone required plan"
        ));
    }
    let mut listed = Vec::new();
    for (key, label, required) in [
        ("tools", "tools", true),
        ("mcp_servers", "MCP servers", true),
        ("slash_commands", "slash commands", false),
        ("skills", "skills", false),
    ] {
        match init.get(key).map(Value::as_array) {
            Some(Some(list)) if list.is_empty() => {}
            Some(Some(list)) => listed.push(format!("{} {label}", list.len())),
            None if !required => {}
            _ => {
                return Some(format!(
                    "did not list its {label}, so Keystone could not confirm they were off"
                ));
            }
        }
    }
    (!listed.is_empty()).then(|| {
        format!(
            "listed {} although Keystone turned them off",
            listed.join(", ")
        )
    })
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
    fn the_layout_helper_driver_stays_in_plan_mode_and_denies_every_request() {
        use ApprovalMode::*;
        let mut out = Outputs::default();
        let mut driver =
            ClaudeDriver::launched(1, false, 7, ReadOnly, Launch::LayoutHelper, &mut out);
        driver.on_line(
            br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{"commands":[],"current_permission_mode":"plan"}}}"#,
            &mut out,
        );
        for mode in [Ask, Auto, FullAccess, Provider] {
            let approval = ApprovalState {
                mode,
                overridden: true,
            };
            driver.command(
                Command::Configure {
                    session: 1,
                    approval,
                },
                &mut out,
            );
        }
        let mut out = Outputs::default();
        driver.command(
            Command::Send {
                session: 1,
                text: "a layout".into(),
            },
            &mut out,
        );
        assert!(
            matches!(out.writes.as_slice(), [line] if line.contains(r#""type":"user""#)),
            "only the message is written, in plan mode: {:?}",
            out.writes
        );
        driver.on_line(
            br#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"t1"}}"#,
            &mut out,
        );
        let request = out
            .events
            .iter()
            .find_map(|(_, e)| match e {
                Event::ApprovalRequested(approval) => Some(approval.request.clone()),
                _ => None,
            })
            .unwrap();
        let mut out = Outputs::default();
        driver.command(
            Command::Answer {
                session: 1,
                request,
                allow: true,
            },
            &mut out,
        );
        assert!(
            matches!(out.writes.as_slice(), [line] if line.contains(r#""behavior":"deny""#)),
            "{:?}",
            out.writes
        );
    }

    #[test]
    fn initialized_process_cannot_succeed_without_a_written_helper_turn() {
        for launch in [Launch::LayoutHelper, Launch::Chat] {
            let mut out = Outputs::default();
            let mut driver =
                ClaudeDriver::launched(1, false, 7, ApprovalMode::ReadOnly, launch, &mut out);
            for line in [
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{"commands":[],"current_permission_mode":"plan"}}}"#,
                r#"{"type":"system","subtype":"init","session_id":"s1","tools":[],"mcp_servers":[],"permissionMode":"plan"}"#,
                r#"{"type":"assistant","message":{"id":"layout-reply","content":[{"type":"text","text":"a layout"}]}}"#,
                r#"{"type":"result","subtype":"success"}"#,
            ] {
                driver.on_line(line.as_bytes(), &mut out);
            }
            let expected = if launch == Launch::LayoutHelper {
                TurnStatus::Failed
            } else {
                TurnStatus::Completed
            };
            assert!(
                out.events.iter().any(|(_, event)| matches!(
                    event, Event::TurnCompleted { status, .. } if *status == expected
                )),
                "{:?}",
                out.events
            );
            assert!(
                !out.writes
                    .iter()
                    .any(|line| line.contains(r#""type":"user""#))
            );
        }
    }

    #[test]
    fn exit_codes_are_read_from_failed_command_output() {
        assert_eq!(exit_code_in("Exit code 2\nls: no such file"), Some(2));
        assert_eq!(exit_code_in("permission denied"), None);
    }
}
