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

use crate::driver::{ApprovalState, Command, Image, Outputs};
use crate::item::{
    Approval, Catalog, Event, Item, ItemKind, ItemStatus, ModelInfo, RequestId, Speed, TurnStatus,
    cap_output,
};
use crate::mcp::{Caller, Issuer};
use crate::question::{Answer, Choice, Field, Form, Input, Question};
use crate::surface;
use keystone_core::integrations::{Health, ServerStatus};
use keystone_core::{ApprovalMode, LimitWindow, ModelChoice, UsageLimit};
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
pub const STEER_NOT_YET: &str = "Claude Code cannot steer this turn yet. Send again in a moment.";

/// The request ID of the `get_usage` a chat process is asked once Claude answers
/// `initialize`. It is outside the `keystone-N` numbering, so no other request's ID
/// moves, and it is never timed or waited for: no answer, an error or an answer
/// Keystone cannot read only leaves the account's usage as it was.
const USAGE_REQUEST: &str = "keystone-usage";

/// The speed values the Claude driver lists for a model with fast mode. They stand for
/// Claude's `fastMode: false` and `fastMode: true`.
pub const STANDARD_SPEED: &str = "standard";
pub const FAST_SPEED: &str = "fast";

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

/// The launch arguments for a model choice: `--model`, `--effort`, and fast mode as a
/// flag setting, which a process started for the SDK needs before it runs fast. A speed
/// this driver does not list passes nothing.
pub fn choice_args(choice: &ModelChoice) -> Vec<&str> {
    let mut args = Vec::new();
    if let Some(model) = &choice.model {
        args.extend(["--model", model]);
    }
    if let Some(effort) = &choice.effort {
        args.extend(["--effort", effort]);
    }
    match choice.speed.as_deref().and_then(fast_mode) {
        Some(true) => args.extend(["--settings", r#"{"fastMode":true}"#]),
        Some(false) => args.extend(["--settings", r#"{"fastMode":false}"#]),
        None => {}
    }
    args
}

/// Claude's `fastMode` setting for a speed this driver lists.
fn fast_mode(speed: &str) -> Option<bool> {
    match speed {
        FAST_SPEED => Some(true),
        STANDARD_SPEED => Some(false),
        _ => None,
    }
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
        choice: &'a ModelChoice,
        resume: Option<&'a str>,
        fork: Option<&'a keystone_core::ForkPoint>,
    ) -> Vec<&'a str> {
        match self {
            Launch::Chat => {
                let mut args = args(mode);
                if let Some(baseline) = baseline {
                    args.extend(["--permission-mode", baseline]);
                }
                args.extend(choice_args(choice));
                match (resume, fork) {
                    (Some(resume), _) => args.extend(["--resume", resume]),
                    (None, Some(fork)) => args.extend([
                        "--resume",
                        &fork.thread,
                        "--fork-session",
                        "--resume-session-at",
                        &fork.at,
                    ]),
                    (None, None) => {}
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
pub(crate) const DENIED: &str = "The user denied this in Keystone.";

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
    /// The UUID of the turn's latest transcript entry Claude reported: an assistant
    /// message, a tool result or a message it read. A fork through this turn keeps
    /// everything up to and including it.
    last_entry: Option<String>,
}

/// How long Claude lets a `show_ui` call wait for the user before failing it.
const WIDGET_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

enum Request {
    Initialize,
    /// `mcp_set_servers`; whether it registers the widget server, and whether it
    /// follows one that did, which may have added the orchestrator's already.
    Mcp {
        widgets: bool,
        retry: bool,
    },
    Interrupt,
    /// A `set_permission_mode` for the selection with this version.
    SetMode {
        version: u64,
        target: &'static str,
    },
    /// A `set_model` or `apply_flag_settings` for the model choice with this version,
    /// and the fields it changes.
    Choose {
        version: u64,
        change: ModelChoice,
    },
    /// A repeated `initialize` asking whether the process is quiescent, for idle
    /// eviction ([`ClaudeDriver::check_quiescence`]).
    Quiesce,
}

/// A quiescence check in flight: its request, whether Claude answered it, and the
/// outcome once known.
struct Quiesce {
    id: String,
    answered: bool,
    result: Option<Result<(), String>>,
}

pub struct QueuedMessage {
    text: String,
    images: Vec<Image>,
    authorization: Option<(Issuer, Caller)>,
}

struct Pending {
    /// Claude's own ID for the request, which the answer must name.
    request_id: Value,
    input: Value,
    tool_use_id: Option<String>,
    /// Whether allowing it leaves plan mode.
    exits_plan: bool,
    /// An MCP elicitation with no fields, which asks only to go ahead: allowing it
    /// accepts the form empty, and denying it declines.
    consent: bool,
}

/// A question waiting for the user, and what its answer is written as.
struct Asked {
    request_id: Value,
    fields: Vec<Field>,
    native: Native,
}

enum Native {
    /// `AskUserQuestion`: the tool's input, sent back with the answers keyed by each
    /// question's text.
    Ask {
        input: Value,
        keys: Vec<String>,
        tool_use_id: Option<String>,
    },
    /// An MCP form, answered with its properties' values.
    Form(Form),
}

/// What Claude is told when the user declines its question.
const DECLINED: &str = "The user declined to answer.";

pub struct ClaudeDriver {
    cancel_queued: bool,
    /// A turn's end waits for [`ClaudeDriver::release_follow_ups`] before messages
    /// sent during that turn start the next one.
    hold_follow_ups: bool,
    /// The last turn's end has not been released yet.
    end_unseen: bool,
    session: u64,
    resume: bool,
    /// Whether the process was started to fork another conversation.
    forking: bool,
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
    questions: HashMap<RequestId, Asked>,
    /// An API error Claude reported as a synthetic assistant message, for the result.
    api_error: Option<String>,
    /// The usage limit that refused this turn's request, for its end.
    limit: Option<UsageLimit>,
    /// The windows `rate_limit_event`s reported in this process. A later `get_usage`
    /// answer leaves them alone: it was asked for first, so it is taken as the older
    /// reading.
    reported: HashSet<String>,
    /// The limit the latest rejected `rate_limit_event` named. Claude sends one when an
    /// account's limit refuses a request, before the error it reports.
    rejected: Option<UsageLimit>,
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
    /// The orchestrator server's configuration, until Claude accepts it.
    mcp_request: Option<Value>,
    mcp_ready: bool,
    mcp_enabled: bool,
    /// Whether the `show_ui` server is offered: the version allows it and Claude has
    /// not refused it.
    widgets: bool,
    /// `show_ui` calls that wait for a Button press, by tool call ID: Claude's control
    /// request ID and the JSON-RPC ID the answer names.
    ui_calls: HashMap<String, (Value, Value)>,
    /// The permission mode Claude last confirmed: in its `initialize` answer, an
    /// answer to `set_permission_mode`, or a `system` report.
    confirmed: Option<String>,
    /// Whether Keystone changed the permission mode in this process.
    changed: bool,
    /// Whether the user allowed `ExitPlanMode` in the running turn. Claude's mode is
    /// then unknown until the turn ends, so steers wait for the next turn.
    leaving_plan: bool,
    /// The model choice the process runs with: what it was launched with, then each
    /// change Claude accepted.
    applied: ModelChoice,
    /// The model choice the user selected last, and its version, which counts every
    /// change. Each change request records the version it was sent for.
    selected: ModelChoice,
    choice_version: u64,
    /// Whether Claude was reported not running the fast mode it was asked for, since
    /// the speed last changed.
    fast_warned: bool,
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
    /// Counts everything Claude or the user did in this process, apart from keep-alives
    /// and the replies a quiescence check expects. Idle eviction is valid only for the
    /// count it was proven at.
    activity: u64,
    /// The last `session_state_changed` state, or the state a quiescence check's answer
    /// reported.
    session_state: Option<String>,
    /// How many background tasks the conversation's own last `background_tasks_changed`
    /// listed, ambient ones included; `None` until a valid one arrives in this process.
    tasks: Option<usize>,
    /// Hooks that reported `hook_started` and not yet `hook_response`.
    hooks: HashSet<String>,
    /// A safety frame Keystone could not read, or a task list it could not attribute
    /// to this conversation, such as a subagent's. Only a passed quiescence check
    /// clears it.
    vetoed: bool,
    quiesce: Option<Quiesce>,
    /// A check's task snapshot can arrive after the connection took its failed
    /// outcome. Keep that snapshot separate so it is not counted as activity.
    snapshot_owed: bool,
    /// The context the latest main-conversation API call filled, in tokens, and the
    /// model it ran on, until the turn's `result` reports the model's window.
    context: Option<(u64, Option<String>)>,
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

    /// Says the process was launched with `choice` ([`choice_args`]), so it needs no
    /// change before its first turn.
    pub fn set_launch_choice(&mut self, choice: ModelChoice) {
        self.applied = choice.clone();
        self.selected = choice;
    }

    /// Says the process was started with `--fork-session`, for a failure to open.
    pub fn set_forking(&mut self) {
        self.forking = true;
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
            hold_follow_ups: false,
            end_unseen: false,
            session,
            resume,
            forking: false,
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
            questions: HashMap::new(),
            api_error: None,
            limit: None,
            reported: HashSet::new(),
            rejected: None,
            sign_in: None,
            launched: mode,
            mode,
            version: 0,
            initialized: false,
            mcp_request: None,
            mcp_ready: true,
            mcp_enabled: false,
            widgets: false,
            ui_calls: HashMap::new(),
            confirmed: None,
            changed: false,
            applied: ModelChoice::default(),
            selected: ModelChoice::default(),
            choice_version: 0,
            fast_warned: false,
            leaving_plan: false,
            untrusted: false,
            timed: Vec::new(),
            launch,
            provider_version: None,
            helper_init_confirmed: false,
            refused: false,
            activity: 0,
            session_state: None,
            tasks: None,
            hooks: HashSet::new(),
            vetoed: false,
            quiesce: None,
            snapshot_owed: false,
            context: None,
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
        self.mcp_request = Some(json!({
            "type": "http", "url": url, "headers": {"Authorization": format!("Bearer {token}")}
        }));
        self.mcp_ready = false;
        self.mcp_enabled = true;
    }

    /// Offers the `show_ui` tool as an SDK-hosted MCP server once Claude answers
    /// `initialize`. Messages wait until Claude accepts or refuses it; a refusal only
    /// leaves the tool out. Set before the process answers anything.
    pub fn set_widgets(&mut self, enabled: bool) {
        if self.launch != Launch::Chat || self.initialized {
            return;
        }
        self.widgets = enabled;
        self.mcp_ready = !enabled && self.mcp_request.is_none();
    }

    /// Asks Claude to add Keystone's servers: the orchestrator's and the widget server.
    fn request_servers(&mut self, retry: bool, out: &mut Outputs) {
        let mut servers = serde_json::Map::new();
        if let Some(server) = &self.mcp_request {
            servers.insert("keystone".into(), server.clone());
        }
        if self.widgets {
            servers.insert(
                surface::SERVER.into(),
                json!({"type": "sdk", "name": surface::SERVER, "timeout": WIDGET_TIMEOUT_MS,
                    "disableAutoBackground": true}),
            );
        }
        if servers.is_empty() {
            return;
        }
        let widgets = self.widgets;
        let line = self.request(
            json!({"subtype": "mcp_set_servers", "servers": servers}),
            Request::Mcp { widgets, retry },
        );
        out.writes.push(line);
    }

    /// Claude answered nothing usable for a request with the widget server. The
    /// orchestrator's is asked for alone; without one, messages go on with no
    /// `show_ui` tool.
    fn drop_widgets(&mut self, out: &mut Outputs) {
        self.widgets = false;
        if self.mcp_request.is_some() {
            return self.request_servers(true, out);
        }
        self.mcp_ready = true;
        self.pump(out);
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
                images: Vec::new(),
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
        // A refused steer changes nothing, so only a written one is activity.
        self.activity += 1;
        self.write(
            QueuedMessage {
                text,
                images: Vec::new(),
                authorization: None,
            },
            out,
        );
        Ok(())
    }

    pub(crate) fn set_cancel_queued(&mut self, enabled: bool) {
        self.cancel_queued = enabled;
    }

    pub fn hold_follow_ups(&mut self) {
        self.hold_follow_ups = true;
    }

    /// Whether the last turn's end waits for [`ClaudeDriver::release_follow_ups`].
    pub fn end_unseen(&self) -> bool {
        self.end_unseen
    }

    /// Holds what this driver carries from a replaced one whose end was not released.
    pub fn set_end_unseen(&mut self) {
        self.end_unseen = true;
    }

    /// Lets messages sent during the turn that ended last start the next one.
    pub fn release_follow_ups(&mut self, out: &mut Outputs) {
        if std::mem::take(&mut self.end_unseen) {
            self.pump(out);
        }
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
        self.activity += 1;
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
        if let Some(Request::Mcp { widgets: true, .. }) = self.requests.get(id) {
            self.requests.remove(id);
            return self.drop_widgets(out);
        }
        let reason = match self.requests.get(id) {
            Some(Request::Initialize) => "Claude Code did not start in time. Send again.",
            Some(Request::SetMode { .. }) => {
                "Claude Code did not confirm the approval mode in time. Send again."
            }
            Some(Request::Choose { .. }) => {
                "Claude Code did not confirm the model change in time. Send again."
            }
            Some(Request::Mcp { .. }) => {
                "Claude Code did not configure Keystone tools in time. Send again."
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
                "This Claude Code version cannot steer a running turn (it lacks interrupt_cancel_queued_v1). Send after the turn ends.",
            ),
            None => Some(STEER_NOT_YET),
            Some(false) => Some(
                "This Claude Code version cannot steer a running turn (it lacks msg_lifecycle_v1). Send after the turn ends.",
            ),
        }
    }

    pub fn command(&mut self, command: Command, out: &mut Outputs) {
        self.activity += 1;
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
            // Checked before the first message of each turn is written.
            Command::Choose { choice, .. } => {
                if self.launch == Launch::Chat && choice != self.selected {
                    self.selected = choice;
                    self.choice_version += 1;
                }
                self.pump(out);
            }
            Command::Send { text, images, .. } => self.send(text, images, out),
            Command::Act { call, text, .. } => match self.ui_calls.remove(&call) {
                Some((request_id, id)) => out.writes.push(mcp_response(
                    &request_id,
                    json!({"jsonrpc": "2.0", "id": id, "result":
                        {"content": [{"type": "text", "text": text}]}}),
                )),
                None => out.event(self.session, Event::ActionUndelivered { call, text }),
            },
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
                let response = if pending.consent {
                    if allow {
                        json!({"action": "accept", "content": {}})
                    } else {
                        json!({"action": "decline"})
                    }
                } else if allow && self.launch == Launch::Chat {
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
            Command::Reply {
                request, answers, ..
            } => {
                let Some(asked) = self.questions.remove(&request) else {
                    return;
                };
                let response = match (asked.native, answers) {
                    (Native::Ask { input, keys, .. }, Some(answers)) => {
                        let blank = Answer::default();
                        let answered: serde_json::Map<String, Value> = keys
                            .into_iter()
                            .zip(&asked.fields)
                            .enumerate()
                            .map(|(index, (key, field))| {
                                let values = field.values(answers.get(index).unwrap_or(&blank));
                                (key, json!(values.join(", ")))
                            })
                            .collect();
                        let mut input = input;
                        if let Some(object) = input.as_object_mut() {
                            object.insert("answers".into(), Value::Object(answered));
                        }
                        json!({"behavior": "allow", "updatedInput": input})
                    }
                    (Native::Ask { tool_use_id, .. }, None) => {
                        if let Some(id) = tool_use_id {
                            self.denied.insert(id);
                        }
                        json!({"behavior": "deny", "message": DECLINED})
                    }
                    (Native::Form(form), Some(answers)) => {
                        json!({"action": "accept", "content": form.content(&answers)})
                    }
                    (Native::Form(_), None) => json!({"action": "decline"}),
                };
                out.writes.push(
                    json!({"type": "control_response", "response":
                        {"subtype": "success", "request_id": asked.request_id, "response": response}})
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
            self.activity += 1;
            return;
        };
        // Like a `rate_limit_event`, the answer says nothing about this process's work.
        if envelope.kind == "control_response"
            && let Ok(answer) = serde_json::from_slice::<UsageAnswer>(line)
            && answer.response.request_id == USAGE_REQUEST
        {
            let mut windows = if answer.response.subtype == "success" {
                usage_windows(&answer.response.response)
            } else {
                Vec::new()
            };
            windows.retain(|window| !self.reported.contains(&window.id));
            if !windows.is_empty() {
                out.event(self.session, Event::Usage(windows));
            }
            return;
        }
        if !self.passive(&envelope, line) {
            self.activity += 1;
        }
        // Before the subagent filter: a task or hook a subagent runs is still work.
        if envelope.kind == "system" {
            self.track_safety(&envelope, line);
        }
        // The account's limits, whichever conversation's call reported them.
        if envelope.kind == "rate_limit_event" {
            return match serde_json::from_slice::<RateLimitEvent>(line) {
                Ok(event) => {
                    // Evidence for this turn's end only: a rejection between turns says
                    // nothing about the next one. Nor does one that extra usage carries past.
                    if self.turn.is_some()
                        && event.rate_limit_info.status.as_deref() == Some("rejected")
                        && !event.rate_limit_info.is_using_overage
                    {
                        self.rejected = Some(event.rate_limit_info.limit(false));
                    }
                    let windows = event.rate_limit_info.windows();
                    self.reported
                        .extend(windows.iter().map(|window| window.id.clone()));
                    if !windows.is_empty() {
                        out.event(self.session, Event::Usage(windows));
                    }
                }
                Err(_) => self.unknown += 1,
            };
        }
        // Subagent traffic is shown through the tool call that started it.
        if envelope.parent_tool_use_id.is_some() {
            return;
        }
        match (&*envelope.kind, envelope.subtype.as_deref()) {
            ("system", Some("init")) => self.on_init(line, out),
            // Claude Code resumes a compacted conversation from the boundary on, so an
            // entry before it can no longer be resumed at.
            ("system", Some("compact_boundary")) => {
                if let Some(turn) = &mut self.turn {
                    turn.last_entry = None;
                }
                out.event(self.session, Event::ForkPointsLost)
            }
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
                    let session = self.session;
                    self.ui_calls.retain(|call, (request_id, _)| {
                        let waits = *request_id != cancel.request_id;
                        if !waits {
                            out.event(session, Event::SurfaceStopped { call: call.clone() });
                        }
                        waits
                    });
                    let request = self.request_id(&cancel.request_id);
                    if self.approvals.remove(&request).is_some()
                        | self.questions.remove(&request).is_some()
                    {
                        out.event(self.session, Event::ApprovalResolved { request });
                    }
                }
            }
            ("control_response", _) => self.on_control_response(line, out),
            ("result", _) => self.on_result(line, out),
            // Status and notices Keystone does not show.
            ("system" | "keep_alive" | "prompt_suggestion", _) => {}
            _ => self.unknown += 1,
        }
    }

    /// The process exited. A running turn fails, and the model it reported is gone
    /// with it.
    pub fn on_exit(&mut self, reason: &str, out: &mut Outputs) {
        self.mcp_request = None;
        self.ui_calls.clear();
        if let Some(turn) = &mut self.turn {
            // What the process wrote before it died is unknown, so the turn has no
            // point to fork at.
            turn.last_entry = None;
            out.event(self.session, Event::RecoveryNeeded);
            self.finish(TurnStatus::Failed, Some(reason.to_owned()), out);
        }
        out.event(self.session, Event::ModelLost);
    }

    pub(crate) fn credentials_failed(&self) -> bool {
        self.untrusted || self.refused
    }

    /// The conversation Claude reported, once `system/init` named it.
    pub fn conversation(&self) -> Option<&str> {
        self.conversation.as_deref()
    }

    /// How much has happened in this process; see the `activity` field. Anything that
    /// changes it invalidates an idle proof made before.
    pub fn activity(&self) -> u64 {
        self.activity
    }

    /// Why this process is not idle by what Keystone itself observed, if it is not.
    /// `None` is not a proof of idle: that also needs [`ClaudeDriver::check_quiescence`]
    /// to succeed at the same [`ClaudeDriver::activity`].
    pub fn idle_blocker(&self) -> Option<&'static str> {
        self.check_blocker().or(self
            .vetoed
            .then_some("Claude Code sent activity Keystone could not read or attribute"))
    }

    /// Why a quiescence check cannot be asked yet. Activity Keystone could not read or
    /// attribute does not stop one: the check is the refresh that can clear it.
    pub fn check_blocker(&self) -> Option<&'static str> {
        let check = self.quiesce.as_ref().map(|check| check.id.as_str());
        let requests = self.requests.iter().any(|(id, request)| {
            !matches!(request, Request::Quiesce) || check != Some(id.as_str())
        });
        [
            (self.launch != Launch::Chat, "it is the layout helper"),
            (self.untrusted || self.refused, "it must be replaced"),
            (!self.initialized, "it has not finished starting"),
            (requests, "a control request is unanswered"),
            (!self.mcp_ready, "Keystone tools are being configured"),
            (self.turn.is_some(), "a turn is running"),
            (!self.approvals.is_empty(), "an approval is waiting"),
            (!self.tools.is_empty(), "a tool call is running"),
            (!self.ui_calls.is_empty(), "a widget waits for the user"),
            (!self.hooks.is_empty(), "a hook is running"),
            (
                self.session_state.as_deref().is_some_and(|s| s != "idle"),
                "Claude Code reports it is not idle",
            ),
            (
                self.tasks.is_some_and(|tasks| tasks > 0),
                "a background task is running",
            ),
            (self.conversation.is_none(), "it has no conversation ID yet"),
        ]
        .into_iter()
        .find_map(|(blocked, reason)| blocked.then_some(reason))
    }

    /// Asks Claude whether this process is quiescent, with a repeated `initialize`.
    /// It succeeds only on a successful answer reporting the session idle with empty
    /// pending-approval and pending-dialog lists, followed by the conversation's own
    /// empty background-task snapshot. Claude Code 2.1.300, 2.1.301 and 2.1.305 put
    /// `session_state` in the answer's `response` and the lists beside it. Passing proves nothing about
    /// scheduled tasks or hooks yet to fire, so no profile evicts on it alone; see
    /// `docs/design/providers.md`. The outcome
    /// arrives through [`ClaudeDriver::take_quiescence`]. Anything else observed in
    /// between counts as activity, which the caller compares.
    pub fn check_quiescence(&mut self, out: &mut Outputs) {
        self.quiesce = None;
        let line = self.request(json!({"subtype": "initialize"}), Request::Quiesce);
        self.quiesce = Some(Quiesce {
            id: format!("keystone-{}", self.next_request),
            answered: false,
            result: None,
        });
        out.writes.push(line);
    }

    /// The outcome of the quiescence check, once known.
    pub fn take_quiescence(&mut self) -> Option<Result<(), String>> {
        let result = self.quiesce.as_mut()?.result.take()?;
        self.quiesce = None;
        Some(result)
    }

    /// Forgets the quiescence check in flight and any snapshot owed to it. Its request
    /// stays open until Claude answers, holding the next check back; the late answer
    /// proves nothing and is activity.
    pub fn abandon_quiescence(&mut self) {
        self.snapshot_owed = false;
        self.quiesce = None;
    }

    fn on_quiescence(&mut self, response: ControlResult) {
        self.snapshot_owed = true;
        if !self
            .quiesce
            .as_ref()
            .is_some_and(|check| response.request_id == check.id.as_str())
        {
            return;
        }
        let empty = |list: &Option<Vec<Value>>, what: &str| match list {
            Some(list) if list.is_empty() => Ok(()),
            Some(_) => Err(format!("Claude Code has {what} pending")),
            None => Err(format!("Claude Code did not report its pending {what}")),
        };
        let checked = if response.subtype != "success" {
            Err(format!(
                "Claude Code refused the quiescence check: {}",
                response.error.unwrap_or_else(|| "no reason given".into())
            ))
        } else {
            let state = response.response.and_then(|answer| answer.session_state);
            empty(&response.pending_permission_requests, "approvals")
                .and_then(|()| empty(&response.pending_user_dialog_requests, "dialogs"))
                .and_then(|()| match state.as_deref() {
                    Some("idle") => Ok(()),
                    Some(state) => Err(format!("Claude Code reports the session {state}")),
                    None => Err("Claude Code did not report the session state".into()),
                })
        };
        if checked.is_ok() {
            self.session_state = Some("idle".into());
        }
        if let Some(check) = &mut self.quiesce {
            match checked {
                Ok(()) => check.answered = true,
                Err(reason) => check.result = Some(Err(reason)),
            }
        }
    }

    /// Whether a line is one a quiescence check expects, or a keep-alive, rather than
    /// activity.
    fn passive(&self, envelope: &Envelope, line: &[u8]) -> bool {
        match (&*envelope.kind, envelope.subtype.as_deref()) {
            ("keep_alive" | "rate_limit_event", _) => true,
            ("control_response", _) => {
                let check = self.quiesce.as_ref();
                serde_json::from_slice::<ControlResponse>(line).is_ok_and(|control| {
                    check.is_some_and(|check| {
                        !check.answered && control.response.request_id == check.id.as_str()
                    })
                })
            }
            ("system", Some("background_tasks_changed")) => {
                self.snapshot_owed && self.own_tasks(envelope, line).is_some()
            }
            _ => false,
        }
    }

    /// How many tasks a `background_tasks_changed` frame lists, if it is readable and
    /// this conversation's own. Only such a frame replaces the task set or completes a
    /// quiescence check.
    fn own_tasks(&self, envelope: &Envelope, line: &[u8]) -> Option<usize> {
        let frame = serde_json::from_slice::<TasksChanged>(line).ok()?;
        self.owns(envelope, frame.session_id.as_deref())
            .then_some(frame.tasks.len())
    }

    /// Whether a safety frame names this conversation itself: no parent tool call and
    /// this session ID.
    fn owns(&self, envelope: &Envelope, session: Option<&str>) -> bool {
        envelope.parent_tool_use_id.is_none()
            && session.is_some()
            && session == self.conversation.as_deref()
    }

    /// Updates what Claude reported of its own work: its session state, background
    /// tasks (each snapshot of its own replaces the last) and running hooks. A frame
    /// Keystone cannot read, or a task list it cannot attribute to this conversation,
    /// leaves the process resident until a quiescence check passes.
    fn track_safety(&mut self, envelope: &Envelope, line: &[u8]) {
        match envelope.subtype.as_deref() {
            Some("session_state_changed") => match serde_json::from_slice::<SessionState>(line) {
                Ok(frame) if self.owns(envelope, frame.session_id.as_deref()) => {
                    self.session_state = Some(frame.state)
                }
                _ => self.vetoed = true,
            },
            Some("background_tasks_changed") => {
                let Some(tasks) = self.own_tasks(envelope, line) else {
                    self.vetoed = true;
                    return;
                };
                self.tasks = Some(tasks);
                self.snapshot_owed = false;
                if let Some(check) = &mut self.quiesce
                    && check.answered
                    && check.result.is_none()
                {
                    check.result = Some(match tasks {
                        0 => Ok(()),
                        n => Err(format!("{n} background task(s) are running")),
                    });
                    // A passed check supersedes activity Keystone could not read or
                    // attribute before.
                    if tasks == 0 {
                        self.vetoed = false;
                    }
                }
            }
            Some("hook_started") => match serde_json::from_slice::<Hook>(line) {
                Ok(hook) => {
                    self.hooks.insert(hook.hook_id);
                }
                Err(_) => self.vetoed = true,
            },
            Some("hook_response") => match serde_json::from_slice::<Hook>(line) {
                Ok(hook) => {
                    self.hooks.remove(&hook.hook_id);
                }
                Err(_) => self.vetoed = true,
            },
            _ => {}
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
        if let Request::Initialize
        | Request::Mcp { .. }
        | Request::SetMode { .. }
        | Request::Choose { .. } = purpose
        {
            self.timed.push(id.clone());
        }
        let line =
            json!({"type": "control_request", "request_id": id, "request": request}).to_string();
        self.requests.insert(id, purpose);
        line
    }

    /// Starts a turn, or steers the running one. Every message waits in the turn's
    /// `held` list and is written by [`ClaudeDriver::pump`].
    fn send(&mut self, text: String, images: Vec<Image>, out: &mut Outputs) {
        self.enqueue(
            QueuedMessage {
                text,
                images,
                authorization: None,
            },
            out,
        );
    }

    fn enqueue(&mut self, message: QueuedMessage, out: &mut Outputs) {
        self.activity += 1;
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

    /// The fields of the selected choice Claude does not run yet. A `None` field is
    /// never sent, and neither is a speed this driver does not list.
    fn choice_change(&self) -> ModelChoice {
        let differs = |selected: &Option<String>, applied: &Option<String>| {
            selected
                .clone()
                .filter(|value| Some(value) != applied.as_ref())
        };
        ModelChoice {
            model: differs(&self.selected.model, &self.applied.model),
            effort: differs(&self.selected.effort, &self.applied.effort),
            speed: differs(&self.selected.speed, &self.applied.speed)
                .filter(|speed| fast_mode(speed).is_some()),
        }
    }

    fn choice_requested(&self) -> bool {
        self.requests
            .values()
            .any(|r| matches!(r, Request::Choose { .. }))
    }

    /// Whether a turn's first message may be written: Claude runs the selected choice
    /// and no change is unanswered.
    fn choice_applied(&self) -> bool {
        !self.choice_requested() && self.choice_change() == ModelChoice::default()
    }

    /// Asks Claude to run the selected choice, when it differs and no change is
    /// unanswered: `set_model` for the model, one `apply_flag_settings` for effort and
    /// speed.
    fn request_choice(&mut self, out: &mut Outputs) {
        if !self.initialized || self.needs_restart() || self.choice_requested() {
            return;
        }
        let change = self.choice_change();
        let version = self.choice_version;
        if let Some(model) = &change.model {
            let line = self.request(
                json!({"subtype": "set_model", "model": model}),
                Request::Choose {
                    version,
                    change: ModelChoice {
                        model: Some(model.clone()),
                        ..ModelChoice::default()
                    },
                },
            );
            out.writes.push(line);
        }
        let mut settings = serde_json::Map::new();
        if let Some(effort) = &change.effort {
            settings.insert("effortLevel".into(), json!(effort));
        }
        if let Some(fast) = change.speed.as_deref().and_then(fast_mode) {
            settings.insert("fastMode".into(), json!(fast));
        }
        if !settings.is_empty() {
            let line = self.request(
                json!({"subtype": "apply_flag_settings", "settings": settings}),
                Request::Choose {
                    version,
                    change: ModelChoice {
                        model: None,
                        ..change
                    },
                },
            );
            out.writes.push(line);
        }
    }

    /// Writes the held messages if the selected mode is confirmed, or asks Claude for
    /// it when nothing else is pending. A process that needs replacing is left to the
    /// connection ([`ClaudeDriver::wants_restart`]).
    fn pump(&mut self, out: &mut Outputs) {
        if !self.mcp_ready {
            return;
        }
        if self.turn.as_ref().is_none_or(|turn| turn.held.is_empty()) || self.end_unseen {
            return;
        }
        // A choice change applies from a turn's start; steers go into the running turn.
        let first = self.turn.as_ref().is_some_and(|turn| !turn.written);
        if first {
            self.request_choice(out);
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
        if first && !self.choice_applied() {
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
        let first = self.turn.as_ref().is_some_and(|turn| !turn.written);
        if !self.mode_confirmed() || (first && !self.choice_applied()) {
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
                "message": {"role": "user", "content": content(message.text, &message.images)},
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
        let (held, written, stopped, last_entry) = self
            .turn
            .take()
            .map_or((Vec::new(), false, false, None), |turn| {
                (turn.held, turn.written, turn.stopping, turn.last_entry)
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
        self.ui_calls.clear();
        self.api_error = None;
        self.rejected = None;
        // Requests from this turn can no longer be answered.
        let questions = std::mem::take(&mut self.questions).into_keys();
        for request in std::mem::take(&mut self.approvals)
            .into_keys()
            .chain(questions)
        {
            out.event(self.session, Event::ApprovalResolved { request });
        }
        if let Some(limit) = self.limit.take()
            && status == TurnStatus::Failed
        {
            out.event(self.session, Event::UsageLimit(limit));
        }
        if let (Launch::Chat, Some(thread), Some(at)) =
            (self.launch, self.conversation.clone(), last_entry)
        {
            out.event(
                self.session,
                Event::ForkPoint(keystone_core::ForkPoint { thread, at }),
            );
        }
        out.event(
            self.session,
            Event::TurnCompleted {
                status,
                error,
                stopped,
            },
        );
        self.end_unseen = self.hold_follow_ups;
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
        // The account, not only the request, decides whether fast mode runs.
        if self.applied.speed.as_deref() == Some(FAST_SPEED)
            && !self.fast_warned
            && let Some(state) = &init.fast_mode_state
            && state != "on"
            && state != "cooldown"
        {
            self.fast_warned = true;
            let reason = init
                .fast_mode_disabled_reason
                .clone()
                .unwrap_or_else(|| state.clone());
            out.event(
                self.session,
                Event::Warning {
                    message: format!("Claude Code did not turn on fast mode ({reason})."),
                },
            );
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
        if let Some(servers) = init.mcp_servers.as_ref().and_then(Value::as_array)
            && self.launch == Launch::Chat
        {
            let ours = |name: &str| {
                (name == "keystone" && self.mcp_enabled)
                    || (name == surface::SERVER && self.widgets)
            };
            let servers = servers
                .iter()
                .filter_map(|server| {
                    let name = server["name"].as_str().filter(|name| !ours(name))?;
                    let health = match server["status"].as_str()? {
                        "connected" => Health::Connected,
                        "pending" => Health::Pending,
                        "needs-auth" => Health::NeedsAuth,
                        "failed" => Health::Failed,
                        "disabled" => Health::Disabled,
                        _ => return None,
                    };
                    Some(ServerStatus {
                        name: name.to_owned(),
                        health,
                    })
                })
                .collect();
            out.event(self.session, Event::McpStatus(servers));
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
            // Claude 2.1.301 names an account's usage limit with `api_error`; without it,
            // a rate-limit error after a rejected `rate_limit_event` is taken as one.
            self.limit = match (assistant.error.as_deref(), assistant.api_error.as_deref()) {
                (_, Some(USAGE_LIMIT)) => Some(
                    assistant
                        .api_error_params
                        .and_then(|params| params.rate_limit_info)
                        .map(|info| info.limit(false))
                        .or_else(|| self.rejected.clone())
                        .unwrap_or_default(),
                ),
                (Some("rate_limit"), None) => self.rejected.clone().map(|limit| UsageLimit {
                    inferred: true,
                    ..limit
                }),
                _ => None,
            };
            return;
        }
        if let Some(uuid) = &assistant.uuid {
            self.ensure_turn(out);
            if let Some(turn) = &mut self.turn {
                turn.last_entry = Some(uuid.clone());
            }
        }
        if let Some(usage) = &assistant.message.usage {
            let used = usage.input_tokens
                + usage.cache_creation_input_tokens
                + usage.cache_read_input_tokens
                + usage.output_tokens;
            // A message with no tokens at all, such as a synthetic one, held no context.
            if used > 0 {
                self.context = Some((used, assistant.message.model.clone()));
            }
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
        if let (Some(turn), Some(uuid)) = (&mut self.turn, &user.uuid) {
            turn.last_entry = Some(uuid.clone());
        }
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
        if request.subtype == "mcp_message" {
            return self.on_mcp_message(control.request_id, request, out);
        }
        // Showing a widget changes nothing but Keystone's own transcript.
        if request.subtype == "can_use_tool"
            && self.widgets
            && request.tool_name.as_deref() == Some(surface::CLAUDE_TOOL)
        {
            out.writes
                .push(mcp_allow(&control.request_id, request.input));
            return;
        }
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
            out.writes
                .push(mcp_allow(&control.request_id, request.input));
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
                    message: "Claude Code asked to leave plan mode. Choose another approval mode to let it make changes.".into(),
                },
            );
        }
        if request.subtype == "can_use_tool"
            && request.tool_name.as_deref() == Some("AskUserQuestion")
        {
            return self.on_ask(control.request_id, request, out);
        }
        if request.subtype == "elicitation" {
            return self.on_elicitation(control.request_id, request, out);
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
                consent: false,
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

    /// `AskUserQuestion`, shown as a question. Claude offers typed "Other" text with
    /// every question itself, so each field takes it.
    fn on_ask(&mut self, request_id: Value, request: ControlBody, out: &mut Outputs) {
        let input = request.input.unwrap_or(Value::Null);
        let asked = input
            .get("questions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, question)| {
                let text = |key: &str| {
                    question
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                };
                let prompt = text("question")?;
                let header =
                    text("header").map_or_else(|| format!("Question {}", index + 1), str::to_owned);
                let options: Vec<Choice> = question
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| {
                        let label = option.get("label")?.as_str()?.trim();
                        let description = option
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|d| !d.is_empty() && *d != label);
                        (!label.is_empty()).then(|| Choice {
                            label: label.to_owned(),
                            description: description.map(str::to_owned),
                        })
                    })
                    .collect();
                let input = if options.is_empty() {
                    Input::Text
                } else {
                    Input::Choice {
                        options,
                        multiple: question.get("multiSelect") == Some(&json!(true)),
                        other: true,
                    }
                };
                let field = Field {
                    prompt: (prompt != header).then(|| prompt.to_owned()),
                    header,
                    input,
                    required: true,
                    bounds: Default::default(),
                };
                Some((prompt.to_owned(), field))
            })
            .collect::<Option<Vec<_>>>()
            .filter(|asked| !asked.is_empty());
        let Some(asked) = asked else {
            if let Some(id) = &request.tool_use_id {
                self.denied.insert(id.clone());
            }
            out.writes.push(
                json!({"type": "control_response", "response": {"subtype": "success",
                    "request_id": request_id,
                    "response": {"behavior": "deny", "message": "Keystone could not read these questions."}}})
                .to_string(),
            );
            return out.event(
                self.session,
                Event::Warning {
                    message: "Claude Code asked questions Keystone could not read, so they were declined.".into(),
                },
            );
        };
        self.ensure_turn(out);
        let (keys, fields): (Vec<_>, Vec<_>) = asked.into_iter().unzip();
        let id = self.request_id(&request_id);
        self.questions.insert(
            id.clone(),
            Asked {
                request_id,
                fields: fields.clone(),
                native: Native::Ask {
                    input,
                    keys,
                    tool_use_id: request.tool_use_id,
                },
            },
        );
        out.event(
            self.session,
            Event::QuestionAsked(Question {
                request: id,
                server: None,
                message: None,
                fields,
            }),
        );
    }

    /// An MCP server's request for input. A form with fields is a question; one with
    /// none only asks to go ahead, so it is an approval. Anything Keystone cannot show
    /// is declined, and the transcript says so.
    fn on_elicitation(&mut self, request_id: Value, request: ControlBody, out: &mut Outputs) {
        let server = request.mcp_server_name.unwrap_or_else(|| "unnamed".into());
        let message = request
            .message
            .as_ref()
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned);
        let form = match request.mode.as_deref() {
            Some("form") | None => Form::read(
                request.requested_schema.as_ref().unwrap_or(&Value::Null),
                request.property_order.as_deref(),
            ),
            Some(mode) => Err(format!(
                "it asks for a {mode} answer, which Keystone can't give"
            )),
        };
        let form = match form {
            Ok(form) => form,
            Err(reason) => {
                out.writes.push(
                    json!({"type": "control_response", "response": {"subtype": "success",
                        "request_id": request_id, "response": {"action": "decline"}}})
                    .to_string(),
                );
                return out.event(
                    self.session,
                    Event::Warning {
                        message: format!(
                            "Declined a request from the {server} MCP server: {reason}."
                        ),
                    },
                );
            }
        };
        self.ensure_turn(out);
        let id = self.request_id(&request_id);
        if form.fields.is_empty() {
            self.approvals.insert(
                id.clone(),
                Pending {
                    request_id,
                    input: Value::Null,
                    tool_use_id: None,
                    exits_plan: false,
                    consent: true,
                },
            );
            return out.event(
                self.session,
                Event::ApprovalRequested(Approval {
                    request: id,
                    item_id: None,
                    action: message.unwrap_or_else(|| "Continue".into()),
                    detail: Some(format!("From the {server} MCP server")),
                }),
            );
        }
        let fields = form.fields.clone();
        self.questions.insert(
            id.clone(),
            Asked {
                request_id,
                fields: fields.clone(),
                native: Native::Form(form),
            },
        );
        out.event(
            self.session,
            Event::QuestionAsked(Question {
                request: id,
                server: Some(server),
                message,
                fields,
            }),
        );
    }

    /// One MCP message for the widget server, answered at once except a `show_ui` call
    /// that waits for a Button press.
    fn on_mcp_message(&mut self, request_id: Value, request: ControlBody, out: &mut Outputs) {
        if request.server_name.as_deref() != Some(surface::SERVER) {
            out.writes.push(
                json!({"type": "control_response", "response": {"subtype": "error",
                    "request_id": request_id,
                    "error": format!("Keystone hosts no SDK MCP server named {}",
                        request.server_name.as_deref().unwrap_or("(none)"))}})
                .to_string(),
            );
            return;
        }
        let message = request.message.unwrap_or_default();
        let Some(id) = message.get("id").cloned() else {
            // A notification: its answer is an empty result.
            return out.writes.push(mcp_response(
                &request_id,
                json!({"jsonrpc": "2.0", "id": 0, "result": {}}),
            ));
        };
        let params = message.get("params").cloned().unwrap_or_default();
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": surface::SERVER, "version": env!("CARGO_PKG_VERSION")}})),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": [surface::mcp_tool()]})),
            "tools/call" if params["name"] == surface::TOOL => {
                return self.on_show_ui(request_id, id, &params, out);
            }
            "tools/call" => Err((-32602, "Unknown tool")),
            _ => Err((-32601, "Method not found")),
        };
        let rpc = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        out.writes.push(mcp_response(&request_id, rpc));
    }

    fn on_show_ui(&mut self, request_id: Value, id: Value, params: &Value, out: &mut Outputs) {
        let call = params["_meta"]["claudecode/toolUseId"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let id = request_id
                    .as_str()
                    .map_or_else(|| request_id.to_string(), str::to_owned);
                format!("keystone-ui-{id}")
            });
        let arguments = params.get("arguments").cloned().unwrap_or_default();
        let text = |text: String| json!({"content": [{"type": "text", "text": text}]});
        match surface::parse(&call, &arguments) {
            Err(invalid) => {
                let mut result = text(invalid.to_message());
                result["isError"] = json!(true);
                out.writes.push(mcp_response(
                    &request_id,
                    json!({"jsonrpc": "2.0", "id": id, "result": result}),
                ));
            }
            Ok(surface) => {
                let waiting = surface.waiting();
                if waiting {
                    self.ui_calls.insert(call, (request_id, id));
                } else {
                    out.writes.push(mcp_response(
                        &request_id,
                        json!({"jsonrpc": "2.0", "id": id, "result":
                            text(surface::shown_message(&surface))}),
                    ));
                }
                out.event(self.session, Event::SurfaceShown { surface, waiting });
            }
        }
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
            Request::Mcp { widgets, retry } => {
                // Each server is judged on its own: a refused widget server only leaves
                // `show_ui` out. Claude lists only the servers a request changed, so a
                // retry may find the orchestrator's added already.
                let answer = response
                    .response
                    .as_ref()
                    .filter(|_| response.subtype == "success");
                let added = |name: &str| {
                    answer
                        .and_then(|a| a.added.as_ref())
                        .is_some_and(|added| added.iter().any(|a| a == name))
                };
                let errors: Option<Vec<&String>> = answer
                    .and_then(|a| a.errors.as_ref()?.as_object())
                    .map(|errors| errors.keys().collect());
                let orchestrator = self.mcp_request.is_none()
                    || (errors
                        .as_ref()
                        .is_some_and(|errors| errors.iter().all(|name| *name == surface::SERVER))
                        && (retry || added("keystone")));
                if orchestrator {
                    let widgets_added = errors.as_ref().is_some_and(|errors| errors.is_empty())
                        && added(surface::SERVER);
                    self.widgets = widgets && widgets_added;
                    self.mcp_request = None;
                    self.mcp_ready = true;
                    return self.pump(out);
                }
                if widgets && answer.is_none() {
                    return self.drop_widgets(out);
                }
                self.untrusted = true;
                self.fail_held("Claude Code could not configure Keystone tools", out);
                return out.event(
                    self.session,
                    Event::Failed {
                        message: "Claude Code could not configure Keystone tools".into(),
                    },
                );
            }
            Request::SetMode { version, target } => {
                return self.on_mode_set(response, version, target, out);
            }
            Request::Choose { version, change } => {
                return self.on_choice_set(response, version, change, out);
            }
            Request::Quiesce => return self.on_quiescence(response),
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
                self.request_servers(false, out);
                if self.launch == Launch::Chat {
                    out.writes.push(
                        json!({"type": "control_request", "request_id": USAGE_REQUEST,
                            "request": {"subtype": "get_usage", "skip_behaviors": true}})
                        .to_string(),
                    );
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
                    &format!("Claude Code did not accept the approval mode: {error}"),
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

    /// Claude answered a `set_model` or `apply_flag_settings`. A success is what the
    /// process runs now, whichever selection it was for, and `set_model`'s answer names
    /// the model it resolved to. A refusal of the latest selection fails the waiting
    /// turn unsent; an older one is followed by a new request.
    fn on_choice_set(
        &mut self,
        response: ControlResult,
        version: u64,
        change: ModelChoice,
        out: &mut Outputs,
    ) {
        if response.subtype == "error" {
            if version == self.choice_version {
                let error = response.error.unwrap_or_else(|| "no reason given".into());
                return self.fail_held(
                    &format!(
                        "Claude Code refused the model change: {error}. Choose another model, effort or speed."
                    ),
                    out,
                );
            }
            return self.pump(out);
        }
        if change.model.is_some() {
            self.applied.model = change.model;
            match response.response.and_then(|r| r.model) {
                Some(model) => {
                    if self.model.as_deref() != Some(model.as_str()) {
                        self.model = Some(model.clone());
                        out.event(self.session, Event::ModelReported { model });
                    }
                }
                // The next `init` reports it.
                None => {
                    if self.model.take().is_some() {
                        out.event(self.session, Event::ModelLost);
                    }
                }
            }
        }
        if change.effort.is_some() {
            self.applied.effort = change.effort;
        }
        if change.speed.is_some() {
            self.applied.speed = change.speed;
            self.fast_warned = false;
        }
        self.pump(out);
    }

    fn on_result(&mut self, line: &[u8], out: &mut Outputs) {
        let Ok(result) = serde_json::from_slice::<ResultMessage>(line) else {
            return self.unknown += 1;
        };
        let failed = result.is_error || result.subtype != "success";
        if let Some((used, model)) = self.context.take() {
            // Only the window of the model that filled it; another model's would mislead.
            let window = model
                .or_else(|| self.model.clone())
                .and_then(|model| result.model_usage.get(&model))
                .and_then(|usage| usage.context_window);
            out.event(self.session, Event::ContextUsage { used, window });
        }
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
            let message = if self.forking {
                format!(
                    "Could not fork this Claude Code conversation: {}. Hand off ({}) to continue it.",
                    reason(),
                    crate::HAND_OFF_KEYS
                )
            } else if self.resume {
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
        if !failed {
            self.limit = None;
        } else if self.limit.is_none() && result.api_error.as_deref() == Some(USAGE_LIMIT) {
            self.limit = Some(self.rejected.clone().unwrap_or_default());
        }
        let Some(turn) = &mut self.turn else {
            self.limit = None;
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
            efforts: if m.supports_effort {
                m.supported_effort_levels
            } else {
                Vec::new()
            },
            default_effort: None,
            speeds: if m.supports_fast_mode {
                [(STANDARD_SPEED, "Standard"), (FAST_SPEED, "Fast")]
                    .map(|(value, label)| Speed {
                        value: value.into(),
                        label: label.into(),
                    })
                    .into()
            } else {
                Vec::new()
            },
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

/// The answer to an `mcp_message` control request.
fn mcp_response(request_id: &Value, rpc: Value) -> String {
    json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": request_id, "response": {"mcp_response": rpc}}})
    .to_string()
}

fn mcp_allow(request_id: &Value, input: Option<Value>) -> String {
    json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": request_id, "response": {"behavior": "allow",
        "updatedInput": input.unwrap_or(Value::Null)}}})
    .to_string()
}

pub(crate) fn tool_kind(name: &str, input: &Value) -> ItemKind {
    let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    match name {
        surface::CLAUDE_TOOL => ItemKind::Tool {
            name: surface::TOOL.into(),
        },
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
pub(crate) fn exit_code_in(text: &str) -> Option<i32> {
    text.strip_prefix("Exit code ")?
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .next()?
        .parse()
        .ok()
}

/// A user message's content: its text alone, as Keystone always sent it, or with images
/// a text block when there is text, then a base64 `image` block per image.
fn content(text: String, images: &[Image]) -> Value {
    if images.is_empty() {
        return json!(text);
    }
    let text = (!text.is_empty()).then(|| json!({"type": "text", "text": text}));
    let images = images.iter().map(|image| {
        json!({"type": "image", "source": {"type": "base64", "media_type": image.media_type,
            "data": image.base64.as_deref().unwrap_or_default()}})
    });
    Value::Array(text.into_iter().chain(images).collect())
}

/// Text from a list of `{"type": "text", "text": ..}` parts.
pub(crate) fn join_text(parts: &[Value]) -> String {
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
    mcp_servers: Option<Value>,
    #[serde(rename = "permissionMode")]
    permission_mode: Option<String>,
    model: Option<String>,
    fast_mode_state: Option<String>,
    fast_mode_disabled_reason: Option<String>,
}

#[derive(Deserialize)]
struct SessionState {
    state: String,
    session_id: Option<String>,
}

/// A `background_tasks_changed` frame. Each entry must name its task and type, or the
/// frame is unreadable.
#[derive(Deserialize)]
struct TasksChanged {
    tasks: Vec<TaskEntry>,
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct TaskEntry {
    #[serde(rename = "task_id")]
    _task_id: String,
    #[serde(rename = "task_type")]
    _task_type: String,
}

#[derive(Deserialize)]
struct Hook {
    hook_id: String,
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
    api_error: Option<String>,
    api_error_params: Option<ApiErrorParams>,
    uuid: Option<String>,
}

/// The `api_error` kind Claude gives a request an account's usage limit refused. Its
/// `api_error_params.rate_limit_info` has the shape of a `rate_limit_event`'s.
const USAGE_LIMIT: &str = "usage_limit_reached";

#[derive(Deserialize)]
struct ApiErrorParams {
    rate_limit_info: Option<RateLimitInfo>,
}

#[derive(Deserialize)]
struct AssistantMessage {
    id: String,
    #[serde(default)]
    content: Vec<Value>,
    model: Option<String>,
    usage: Option<TokenUsage>,
}

/// The tokens one API call used, which together are what its context held.
#[derive(Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct ModelUsage {
    #[serde(rename = "contextWindow")]
    context_window: Option<u64>,
}

/// A `rate_limit_event`: what the latest API response said about the account's
/// limits. Only windows, percentages and reset times are read.
#[derive(Deserialize)]
struct RateLimitEvent {
    rate_limit_info: RateLimitInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitInfo {
    status: Option<String>,
    resets_at: Option<u64>,
    rate_limit_type: Option<String>,
    #[serde(default)]
    is_using_overage: bool,
    /// A fraction of the window, 0-1, and above 1 past the limit.
    utilization: Option<f64>,
    #[serde(default)]
    unified_windows: Option<HashMap<String, Option<UnifiedWindow>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnifiedWindow {
    utilization: Option<f64>,
    resets_at: Option<u64>,
}

impl RateLimitInfo {
    /// The limit this report names and when it resets.
    fn limit(&self, inferred: bool) -> UsageLimit {
        UsageLimit {
            label: self
                .rate_limit_type
                .as_deref()
                .map(|kind| limit_window(kind, 0.0, None).label),
            resets_at: self.resets_at,
            inferred,
        }
    }

    /// Every window the event carries: each of `unifiedWindows`, and the limiting
    /// window it names when that is not among them. A rejected window with no
    /// utilization is full.
    fn windows(&self) -> Vec<LimitWindow> {
        let mut windows: Vec<LimitWindow> = self
            .unified_windows
            .iter()
            .flatten()
            .filter_map(|(id, window)| {
                let window = window.as_ref()?;
                Some(limit_window(
                    id,
                    window.utilization? * 100.0,
                    window.resets_at,
                ))
            })
            .collect();
        windows.sort_by(|a, b| a.id.cmp(&b.id));
        if let Some(kind) = &self.rate_limit_type
            && !windows.iter().any(|w| &w.id == kind)
        {
            let used = match (self.utilization, self.status.as_deref()) {
                (Some(fraction), _) => Some(fraction * 100.0),
                (None, Some("rejected")) => Some(100.0),
                _ => None,
            };
            if let Some(used) = used {
                windows.push(limit_window(kind, used, self.resets_at));
            }
        }
        windows
    }
}

/// The label of a window Claude's `rate_limit_event` names, if Keystone knows it.
fn window_label(id: &str) -> Option<&'static str> {
    Some(match id {
        "five_hour" => "5-hour",
        "seven_day" => "Weekly",
        "seven_day_opus" => "Weekly Opus",
        "seven_day_sonnet" => "Weekly Sonnet",
        "seven_day_overage_included" => "Weekly with extra usage",
        "overage" => "Extra usage",
        _ => return None,
    })
}

fn limit_window(id: &str, used_percent: f64, resets_at: Option<u64>) -> LimitWindow {
    LimitWindow {
        id: id.to_owned(),
        label: window_label(id).unwrap_or(id).to_owned(),
        used_percent,
        resets_at,
    }
}

/// A `control_response` read only as far as its request ID, so an answer of any shape
/// is recognized as the `get_usage` one.
#[derive(Deserialize)]
struct UsageAnswer {
    response: UsageResult,
}

#[derive(Deserialize)]
struct UsageResult {
    #[serde(default)]
    subtype: Value,
    request_id: Value,
    #[serde(default)]
    response: Value,
}

/// The windows of a `get_usage` answer, as Claude Code 2.1.301 reports them:
/// `rate_limits_available` and `rate_limits`, each window's `utilization` a percentage
/// and `resets_at` an RFC 3339 time. The schema is experimental, so each window is read
/// on its own, and one Keystone cannot read is left out. So is one Keystone has no label
/// for, such as `model_scoped`, which would otherwise show under its raw ID.
fn usage_windows(answer: &Value) -> Vec<LimitWindow> {
    let Some(limits) = answer["rate_limits"]
        .as_object()
        .filter(|_| answer["rate_limits_available"] == true)
    else {
        return Vec::new();
    };
    let mut windows: Vec<LimitWindow> = limits
        .iter()
        .filter_map(|(id, window)| {
            window_label(id)?;
            let used = window["utilization"].as_f64()?;
            let resets_at = window["resets_at"].as_str().and_then(rfc3339_seconds);
            Some(limit_window(id, used, resets_at))
        })
        .collect();
    windows.sort_by(|a, b| a.id.cmp(&b.id));
    windows
}

/// Unix seconds for an RFC 3339 time from 1970 to 9999, such as
/// `2026-10-21T07:00:00.123+00:00`. Anything else, an impossible date included, is
/// `None`.
fn rfc3339_seconds(text: &str) -> Option<u64> {
    let digits = |part: &str, width: usize, range: std::ops::RangeInclusive<i64>| {
        (part.len() == width && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
            .filter(|n| range.contains(n))
    };
    let (date, time) = (text.get(..10)?, text.get(11..)?);
    if !matches!(text.as_bytes()[10], b'T' | b't' | b' ') {
        return None;
    }
    let &[year, month, day] = &date.split('-').collect::<Vec<_>>()[..] else {
        return None;
    };
    let (year, month) = (digits(year, 4, 1970..=9999)?, digits(month, 2, 1..=12)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    let day = digits(day, 2, 1..=days_in_month)?;
    let (clock, zone) = time.split_at(time.find(['Z', 'z', '+', '-'])?);
    let (clock, fraction) = clock.split_once('.').unwrap_or((clock, "0"));
    if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let &[hour, minute, second] = &clock.split(':').collect::<Vec<_>>()[..] else {
        return None;
    };
    let seconds = digits(hour, 2, 0..=23)? * 3600 + digits(minute, 2, 0..=59)? * 60;
    let seconds = seconds + digits(second, 2, 0..=60)?;
    let offset = match zone {
        "Z" | "z" => 0,
        _ => {
            let (hours, minutes) = zone[1..].split_once(':')?;
            let offset = digits(hours, 2, 0..=23)? * 3600 + digits(minutes, 2, 0..=59)? * 60;
            if zone.starts_with('-') {
                -offset
            } else {
                offset
            }
        }
    };
    // Days since 1970-01-01 from the civil date (Howard Hinnant's algorithm), the
    // inverse of [`surface::iso8601`]. Every field is bounded, so nothing overflows.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + seconds - offset).ok()
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
    server_name: Option<String>,
    message: Option<Value>,
    mcp_server_name: Option<String>,
    mode: Option<String>,
    requested_schema: Option<Value>,
    property_order: Option<Vec<String>>,
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
    pending_permission_requests: Option<Vec<Value>>,
    pending_user_dialog_requests: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct ModeSet {
    added: Option<Vec<String>>,
    errors: Option<Value>,
    mode: Option<String>,
    model: Option<String>,
    current_permission_mode: Option<String>,
    commands: Option<Value>,
    claude_code_version: Option<Value>,
    models: Option<Vec<WireModel>>,
    session_state: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireModel {
    value: String,
    display_name: String,
    resolved_model: Option<String>,
    #[serde(default)]
    supports_effort: bool,
    #[serde(default)]
    supported_effort_levels: Vec<String>,
    #[serde(default)]
    supports_fast_mode: bool,
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
    #[serde(rename = "modelUsage", default)]
    model_usage: HashMap<String, ModelUsage>,
    result: Option<String>,
    #[serde(default)]
    errors: Vec<String>,
    api_error: Option<String>,
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
                images: Vec::new(),
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

    fn usage_events(out: &Outputs) -> Vec<Vec<LimitWindow>> {
        out.events
            .iter()
            .filter_map(|(_, event)| match event {
                Event::Usage(windows) => Some(windows.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rate_limit_events_become_usage_windows_without_counting_as_activity() {
        let (mut driver, mut out) = idle_driver();
        out.events.clear();
        let activity = driver.activity();
        for line in [
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1800000000,"rateLimitType":"five_hour","utilization":0.85,"overageDisabledReason":"org_level_disabled"},"uuid":"u","session_id":"s1"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","utilization":0.4,"unifiedWindows":{"seven_day":{"utilization":0.12,"resetsAt":1800500000},"five_hour":{"utilization":0.4,"resetsAt":1800000000}}},"uuid":"u","session_id":"s1"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day_opus","resetsAt":1800600000},"uuid":"u","session_id":"s1"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"},"uuid":"u","session_id":"s1"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":null,"resetsAt":1800000000},"seven_day":{"utilization":0.5,"resetsAt":1800500000}}},"uuid":"u","session_id":"s1"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.9,"resetsAt":1800000000,"unifiedWindows":{"seven_day":null,"five_hour":{"utilization":0.9,"resetsAt":1800000000}}},"uuid":"u","session_id":"s1"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"seven_day","utilization":0.3,"resetsAt":1800500000,"unifiedWindows":null},"uuid":"u","session_id":"s1"}"#,
        ] {
            driver.on_line(line.as_bytes(), &mut out);
        }
        assert_eq!(driver.activity(), activity);
        let usage = usage_events(&out);
        assert_eq!(usage.len(), 6, "{usage:?}");
        assert_eq!(
            usage[4],
            vec![limit_window("five_hour", 90.0, Some(1_800_000_000))]
        );
        assert_eq!(
            usage[5],
            vec![limit_window("seven_day", 30.0, Some(1_800_500_000))]
        );
        assert_eq!(
            usage[3],
            vec![limit_window("seven_day", 50.0, Some(1_800_500_000))]
        );
        assert_eq!(
            usage[0],
            vec![limit_window("five_hour", 85.0, Some(1_800_000_000))]
        );
        assert_eq!(
            usage[1],
            vec![
                limit_window("five_hour", 40.0, Some(1_800_000_000)),
                limit_window("seven_day", 12.0, Some(1_800_500_000)),
            ]
        );
        assert_eq!(usage[1][1].label, "Weekly");
        assert_eq!(
            usage[2],
            vec![limit_window("seven_day_opus", 100.0, Some(1_800_600_000))]
        );
    }

    /// A `get_usage` answer as Claude Code 2.1.301 sends one, with synthetic values.
    fn usage_answer(subtype: &str, response: &str) -> String {
        format!(
            r#"{{"type":"control_response","response":{{"subtype":"{subtype}","request_id":"keystone-usage","response":{response}}}}}"#
        )
    }

    #[test]
    fn a_chat_process_asks_get_usage_after_initialize_and_reports_its_windows() {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(1, false, 7, ApprovalMode::Provider, &mut out);
        assert_eq!(out.writes.len(), 1, "only initialize before its answer");
        out.writes.clear();
        driver.on_line(
            br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{}}}"#,
            &mut out,
        );
        let asked: Vec<Value> = out
            .writes
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            asked,
            vec![
                json!({"type": "control_request", "request_id": "keystone-usage",
                "request": {"subtype": "get_usage", "skip_behaviors": true}})
            ]
        );
        let (mut driver, mut out) = idle_driver();
        out.events.clear();
        let activity = driver.activity();
        driver.on_line(
            usage_answer(
                "success",
                r#"{"rate_limits_available":true,"rate_limits":{
                    "five_hour":{"utilization":41.0,"resets_at":"2027-01-15T08:00:00.000Z"},
                    "seven_day":{"utilization":12.5,"resets_at":"2027-01-19T08:00:00.123456+00:00"},
                    "seven_day_sonnet":{"utilization":3,"resets_at":null},
                    "seven_day_opus":{"utilization":null,"resets_at":null},
                    "model_scoped":{"utilization":7.0,"resets_at":"2027-01-19T08:00:00Z"}}}"#,
            )
            .as_bytes(),
            &mut out,
        );
        assert_eq!(
            usage_events(&out),
            vec![vec![
                limit_window("five_hour", 41.0, Some(1_800_000_000)),
                limit_window("seven_day", 12.5, Some(1_800_345_600)),
                limit_window("seven_day_sonnet", 3.0, None),
            ]]
        );
        assert_eq!(usage_events(&out)[0][0].label, "5-hour");
        assert_eq!(driver.activity(), activity);
        assert_eq!(driver.unknown, 0);
        assert_eq!(driver.idle_blocker(), None);
    }

    #[test]
    fn a_get_usage_error_or_unreadable_answer_is_ignored_quietly() {
        let (mut driver, mut out) = idle_driver();
        out.events.clear();
        out.writes.clear();
        let activity = driver.activity();
        for line in [
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"keystone-usage","error":"Unknown subtype: get_usage"}}"#.to_owned(),
            usage_answer("success", r#"{"rate_limits_available":false,"rate_limits":null}"#),
            usage_answer(
                "success",
                r#"{"rate_limits_available":false,"rate_limits":{"five_hour":{"utilization":50,"resets_at":null}}}"#,
            ),
            usage_answer("success", r#"{"rate_limits_available":true,"rate_limits":[1,2]}"#),
            usage_answer(
                "success",
                r#"{"rate_limits_available":true,"rate_limits":{"five_hour":"full","seven_day":{"utilization":"12"},"model_scoped":{"opus":{"utilization":5}}}}"#,
            ),
            usage_answer("success", r#""a string""#),
            usage_answer("success", r#"{"mode":7,"models":"none","session_state":false}"#),
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-usage"}}"#.to_owned(),
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"keystone-usage","error":"no","response":{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":50,"resets_at":null}}}}}"#.to_owned(),
            r#"{"type":"control_response","response":{"request_id":"keystone-usage","response":{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":50,"resets_at":null}}}}}"#.to_owned(),
            r#"{"type":"control_response","response":{"request_id":"keystone-usage","subtype":7}}"#.to_owned(),
            r#"{"type":"control_response","response":{"request_id":"keystone-usage","subtype":{"ok":true},"response":{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":50,"resets_at":null}}}}}"#.to_owned(),
        ] {
            driver.on_line(line.as_bytes(), &mut out);
        }
        assert!(out.events.is_empty(), "{:?}", out.events);
        assert!(out.writes.is_empty(), "{:?}", out.writes);
        assert_eq!(driver.activity(), activity);
        assert_eq!(driver.unknown, 0);
        assert_eq!(driver.idle_blocker(), None);
        // An unreadable reset time keeps the window, without one.
        driver.on_line(
            usage_answer(
                "success",
                r#"{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":9,"resets_at":"soon"},"seven_day":{"utilization":2,"resets_at":"2027-01-19T9223372036854775807:00:00Z"}}}"#,
            )
            .as_bytes(),
            &mut out,
        );
        assert_eq!(
            usage_events(&out),
            vec![vec![
                limit_window("five_hour", 9.0, None),
                limit_window("seven_day", 2.0, None),
            ]]
        );
    }

    #[test]
    fn the_layout_helper_never_asks_get_usage() {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::launched(
            1,
            false,
            7,
            ApprovalMode::ReadOnly,
            Launch::LayoutHelper,
            &mut out,
        );
        out.writes.clear();
        driver.on_line(
            br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{"commands":[],"current_permission_mode":"plan"}}}"#,
            &mut out,
        );
        assert!(
            !out.writes.iter().any(|line| line.contains("get_usage")),
            "{:?}",
            out.writes
        );
    }

    #[test]
    fn a_get_usage_answer_during_the_first_turn_or_a_check_changes_neither() {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(1, false, 7, ApprovalMode::Provider, &mut out);
        driver.on_line(
            br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{}}}"#,
            &mut out,
        );
        driver.command(
            Command::Send {
                session: 1,
                text: "hi".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        driver.on_line(
            br#"{"type":"system","subtype":"init","session_id":"s1","capabilities":["msg_lifecycle_v1"]}"#,
            &mut out,
        );
        assert!(driver.is_running());
        let (activity, blocker) = (driver.activity(), driver.idle_blocker());
        let mut out = Outputs::default();
        let answer = usage_answer(
            "success",
            r#"{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":30,"resets_at":"2027-01-15T08:00:00Z"}}}"#,
        );
        driver.on_line(answer.as_bytes(), &mut out);
        assert!(driver.is_running());
        assert_eq!(
            (driver.activity(), driver.idle_blocker()),
            (activity, blocker)
        );
        assert!(out.writes.is_empty());
        assert_eq!(
            usage_events(&out),
            vec![vec![limit_window("five_hour", 30.0, Some(1_800_000_000))]]
        );
        assert_eq!(out.events.len(), 1);

        let (mut driver, _) = idle_driver();
        let activity = driver.activity();
        assert_eq!(
            check(&mut driver, &[QUIET, answer.as_str(), NO_TASKS]),
            Some(Ok(()))
        );
        assert_eq!(driver.activity(), activity);
    }

    #[test]
    fn a_late_get_usage_answer_keeps_the_windows_a_rate_limit_event_reported() {
        let (mut driver, mut out) = idle_driver();
        out.events.clear();
        driver.on_line(
            br#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","utilization":0.4,"resetsAt":1800000000},"uuid":"u","session_id":"s1"}"#,
            &mut out,
        );
        driver.on_line(
            usage_answer(
                "success",
                r#"{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":30,"resets_at":"2027-01-15T08:00:00Z"},"seven_day":{"utilization":12,"resets_at":"2027-01-19T08:00:00Z"}}}"#,
            )
            .as_bytes(),
            &mut out,
        );
        assert_eq!(
            usage_events(&out),
            vec![
                vec![limit_window("five_hour", 40.0, Some(1_800_000_000))],
                vec![limit_window("seven_day", 12.0, Some(1_800_345_600))],
            ]
        );
    }

    #[test]
    fn rfc3339_times_become_unix_seconds() {
        for seconds in [0, 951_868_799, 1_800_000_000, 4_102_444_800] {
            assert_eq!(
                rfc3339_seconds(&surface::iso8601(seconds * 1000)),
                Some(seconds)
            );
        }
        for (text, seconds) in [
            ("2027-01-19T08:00:00+00:00", Some(1_800_345_600)),
            ("2027-01-19T03:00:00.999999-05:00", Some(1_800_345_600)),
            ("2027-01-19t13:30:00+05:30", Some(1_800_345_600)),
            ("2027-01-19 08:00:00Z", Some(1_800_345_600)),
            ("2027-13-19T08:00:00Z", None),
            ("2027-01-19T08:00:00", None),
            ("2027-01-19", None),
            ("2028-02-29T00:00:00Z", Some(1_835_395_200)),
            ("1969-12-31T23:59:59Z", None),
            ("1970-01-01T00:00:00+01:00", None),
            ("2027-02-29T00:00:00Z", None),
            ("2027-02-31T00:00:00Z", None),
            ("2027-01-19T24:00:00Z", None),
            ("2027-01-19T08:60:00Z", None),
            ("2027-01-19T08:00:00+24:00", None),
            ("2027-01-19T08:00:00:17Z", None),
            ("2027-01-19T08:00:00.Z", None),
            ("2027-1-19T08:00:00Z", None),
            ("99999999999999-01-01T00:00:00Z", None),
            ("2027-01-19T9223372036854775807:00:00Z", None),
            ("2027-01-19T08:00:00+9223372036854775807:00", None),
            ("2027-01-19é08:00:00Z", None),
            ("", None),
        ] {
            assert_eq!(rfc3339_seconds(text), seconds, "{text}");
        }
    }

    /// A second turn after [`idle_driver`]'s, fed `lines` then its lifecycle end: the
    /// usage limits and turn ends it reports, in order.
    fn second_turn(driver: &mut ClaudeDriver, turn: u64, lines: &[&str]) -> Vec<Event> {
        let lifecycle = |state: &str| {
            format!(
                r#"{{"type":"command_lifecycle","command_uuid":"{}","state":"{state}"}}"#,
                message_uuid(7, turn)
            )
        };
        let mut out = Outputs::default();
        driver.command(
            Command::Send {
                session: 1,
                text: "again".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        out.events.clear();
        driver.on_line(lifecycle("started").as_bytes(), &mut out);
        for line in lines {
            driver.on_line(line.as_bytes(), &mut out);
        }
        driver.on_line(lifecycle("completed").as_bytes(), &mut out);
        out.events
            .into_iter()
            .map(|(_, event)| event)
            .filter(|event| matches!(event, Event::UsageLimit(_) | Event::TurnCompleted { .. }))
            .collect()
    }

    /// Claude 2.1.301's frames for a request an account's usage limit refused, as its
    /// stream-json output builds them: a synthetic assistant message with `error`
    /// `rate_limit` and `api_error` `usage_limit_reached`, then an error result.
    const LIMIT_MESSAGE: &str = r#"{"type":"assistant","message":{"id":"limit","model":"<synthetic>","content":[{"type":"text","text":"You've hit your 5-hour limit · resets 3pm"}]},"parent_tool_use_id":null,"error":"rate_limit","is_api_error_message":true,"api_error_status":429,"api_error":"usage_limit_reached","api_error_params":{"rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1800000000,"isUsingOverage":false}}}"#;
    const LIMIT_RESULT: &str = r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your 5-hour limit · resets 3pm","api_error_status":429,"api_error":"usage_limit_reached"}"#;

    fn failed(events: &[Event]) -> Option<&str> {
        events.iter().find_map(|event| match event {
            Event::TurnCompleted {
                status: TurnStatus::Failed,
                error,
                ..
            } => Some(error.as_deref().unwrap_or("")),
            _ => None,
        })
    }

    #[test]
    fn a_usage_limit_error_ends_the_turn_naming_the_limit_and_its_reset() {
        let (mut driver, _) = idle_driver();
        let events = second_turn(&mut driver, 2, &[LIMIT_MESSAGE, LIMIT_RESULT]);
        assert_eq!(
            events[0],
            Event::UsageLimit(UsageLimit {
                label: Some("5-hour".into()),
                resets_at: Some(1_800_000_000),
                inferred: false,
            })
        );
        assert_eq!(
            failed(&events),
            Some("You've hit your 5-hour limit · resets 3pm")
        );
        assert_eq!(events.len(), 2, "{events:?}");
    }

    #[test]
    fn a_rate_limit_error_after_a_rejected_report_is_a_labelled_inferred_limit() {
        let (mut driver, _) = idle_driver();
        let events = second_turn(
            &mut driver,
            2,
            &[
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day","resetsAt":1800500000},"uuid":"u","session_id":"s1"}"#,
                r#"{"type":"assistant","message":{"id":"limit","content":[{"type":"text","text":"limit"}]},"parent_tool_use_id":null,"error":"rate_limit"}"#,
                r#"{"type":"result","subtype":"success","is_error":true,"result":"limit"}"#,
            ],
        );
        assert_eq!(
            events[0],
            Event::UsageLimit(UsageLimit {
                label: Some("Weekly".into()),
                resets_at: Some(1_800_500_000),
                inferred: true,
            })
        );
        assert_eq!(failed(&events), Some("limit"));
        let overage = second_turn(
            &mut driver,
            3,
            &[
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1800000000,"isUsingOverage":true},"uuid":"u","session_id":"s1"}"#,
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"slow down"}]},"parent_tool_use_id":null,"error":"rate_limit"}"#,
                r#"{"type":"result","subtype":"success","is_error":true,"result":"x"}"#,
            ],
        );
        assert_eq!(
            overage.len(),
            1,
            "a rejection extra usage carries is no limit: {overage:?}"
        );
    }

    #[test]
    fn other_rate_limit_errors_and_later_failures_are_not_usage_limits() {
        let (mut driver, _) = idle_driver();
        let throttle = second_turn(
            &mut driver,
            2,
            &[
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"API Error: Request rejected (429) · slow down"}]},"parent_tool_use_id":null,"error":"rate_limit"}"#,
                r#"{"type":"result","subtype":"success","is_error":true,"result":"x"}"#,
            ],
        );
        assert!(failed(&throttle).is_some());
        assert_eq!(
            throttle.len(),
            1,
            "a throttle is no usage limit: {throttle:?}"
        );
        let mut out = Outputs::default();
        driver.on_line(
            br#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1800000000},"uuid":"u","session_id":"s1"}"#,
            &mut out,
        );
        driver.on_line(LIMIT_MESSAGE.as_bytes(), &mut out);
        driver.on_line(LIMIT_RESULT.as_bytes(), &mut out);
        let later = second_turn(
            &mut driver,
            3,
            &[
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"slow down"}]},"parent_tool_use_id":null,"error":"rate_limit"}"#,
                r#"{"type":"result","subtype":"success","is_error":true,"result":"x"}"#,
            ],
        );
        assert_eq!(
            later.len(),
            1,
            "a limit or rejection reported between turns never reaches the next: {later:?}"
        );
    }

    #[test]
    fn a_result_reports_the_context_the_last_call_filled() {
        let (mut driver, mut out) = idle_driver();
        driver.command(
            Command::Send {
                session: 1,
                text: "again".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        out.events.clear();
        for line in [
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"fake-opus","capabilities":["msg_lifecycle_v1"]}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"a"}],"usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":1000,"output_tokens":5}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m2","model":"fake-opus","content":[{"type":"text","text":"b"}],"usage":{"input_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":90000,"output_tokens":6}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m2b","model":"<synthetic>","content":[],"usage":{"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m3","content":[],"usage":{"input_tokens":999999}},"parent_tool_use_id":"toolu_sub"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","modelUsage":{"fake-haiku":{"contextWindow":100000},"fake-opus":{"contextWindow":200000}}}"#,
        ] {
            driver.on_line(line.as_bytes(), &mut out);
        }
        let context: Vec<_> = out
            .events
            .iter()
            .filter(|(_, e)| matches!(e, Event::ContextUsage { .. }))
            .collect();
        assert_eq!(
            context,
            vec![&(
                1,
                Event::ContextUsage {
                    used: 90_010,
                    window: Some(200_000)
                }
            )]
        );
        // A window reported only for other models is not borrowed.
        out.events.clear();
        driver.command(
            Command::Send {
                session: 1,
                text: "third".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        for line in [
            r#"{"type":"assistant","message":{"id":"m4","model":"claude-other[1m]","content":[{"type":"text","text":"c"}],"usage":{"input_tokens":180000}},"parent_tool_use_id":null}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","modelUsage":{"fake-opus":{"contextWindow":1000000}}}"#,
        ] {
            driver.on_line(line.as_bytes(), &mut out);
        }
        assert!(out.events.contains(&(
            1,
            Event::ContextUsage {
                used: 180_000,
                window: None
            }
        )));
    }

    #[test]
    fn a_turn_ends_with_the_fork_point_of_its_last_transcript_entry() {
        let (mut driver, mut out) = idle_driver();
        driver.command(
            Command::Send {
                session: 1,
                text: "again".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        out.events.clear();
        for line in [
            r#"{"type":"command_lifecycle","command_uuid":"00000000-0000-4000-8007-000000000002","state":"started"}"#,
            r#"{"type":"user","message":{"role":"user","content":"again"},"uuid":"prompt-1","isReplay":true,"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"uuid":"entry-1","parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"sub","content":[{"type":"text","text":"sub"}]},"uuid":"subagent-entry","parent_tool_use_id":"t1"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a.txt"}]},"uuid":"result-1","parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"done"}]},"uuid":"entry-2","parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"API Error"}]},"uuid":"error-entry","error":"rate_limit","parent_tool_use_id":null}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
            r#"{"type":"command_lifecycle","command_uuid":"00000000-0000-4000-8007-000000000002","state":"completed"}"#,
        ] {
            driver.on_line(line.as_bytes(), &mut out);
        }
        let end = out
            .events
            .iter()
            .position(|(_, e)| matches!(e, Event::TurnCompleted { .. }))
            .expect("the turn ended");
        assert_eq!(
            out.events[end - 1],
            (
                1,
                Event::ForkPoint(keystone_core::ForkPoint {
                    thread: "s1".into(),
                    at: "entry-2".into(),
                })
            )
        );
        assert_eq!(
            out.events
                .iter()
                .filter(|(_, e)| matches!(e, Event::ForkPoint(_)))
                .count(),
            1
        );
    }

    #[test]
    fn a_compaction_loses_earlier_fork_points_and_a_crash_saves_none() {
        let (mut driver, mut out) = idle_driver();
        driver.command(
            Command::Send {
                session: 1,
                text: "again".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        out.events.clear();
        for line in [
            r#"{"type":"command_lifecycle","command_uuid":"00000000-0000-4000-8007-000000000002","state":"started"}"#,
            r#"{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"before"}]},"uuid":"entry-0","parent_tool_use_id":null}"#,
            r#"{"type":"system","subtype":"compact_boundary","session_id":"s1","uuid":"boundary","compact_metadata":{"trigger":"auto","pre_tokens":1}}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["overloaded"]}"#,
            r#"{"type":"command_lifecycle","command_uuid":"00000000-0000-4000-8007-000000000002","state":"completed"}"#,
        ] {
            driver.on_line(line.as_bytes(), &mut out);
        }
        assert!(out.events.contains(&(1, Event::ForkPointsLost)));
        assert!(
            out.events
                .iter()
                .any(|(_, e)| matches!(e, Event::TurnCompleted { .. })),
            "{:?}",
            out.events
        );
        assert!(
            !out.events
                .iter()
                .any(|(_, e)| matches!(e, Event::ForkPoint(_))),
            "{:?}",
            out.events
        );
        driver.command(
            Command::Send {
                session: 1,
                text: "more".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        out.events.clear();
        for line in [
            r#"{"type":"command_lifecycle","command_uuid":"00000000-0000-4000-8007-000000000003","state":"started"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"partial"}]},"uuid":"entry-1","parent_tool_use_id":null}"#,
        ] {
            driver.on_line(line.as_bytes(), &mut out);
        }
        driver.on_exit("killed", &mut out);
        assert!(
            out.events
                .iter()
                .any(|(_, e)| matches!(e, Event::TurnCompleted { .. })),
            "{:?}",
            out.events
        );
        assert!(
            !out.events
                .iter()
                .any(|(_, e)| matches!(e, Event::ForkPoint(_))),
            "{:?}",
            out.events
        );
    }

    #[test]
    fn a_fork_resumes_the_source_as_a_new_session_cut_at_its_point() {
        let fork = keystone_core::ForkPoint {
            thread: "source".into(),
            at: "entry-2".into(),
        };
        let none = ModelChoice::default();
        let args = Launch::Chat.args(ApprovalMode::Provider, None, &none, None, Some(&fork));
        assert_eq!(
            args[ARGS.len()..],
            [
                "--resume",
                "source",
                "--fork-session",
                "--resume-session-at",
                "entry-2"
            ]
        );
        // The fork's own conversation, once known, is resumed instead.
        let args = Launch::Chat.args(
            ApprovalMode::Provider,
            None,
            &none,
            Some("fork"),
            Some(&fork),
        );
        assert_eq!(args[ARGS.len()..], ["--resume", "fork"]);
        let helper =
            Launch::LayoutHelper.args(ApprovalMode::Provider, None, &none, None, Some(&fork));
        assert!(!helper.contains(&"--fork-session"));

        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(1, true, 0, ApprovalMode::Provider, &mut out);
        driver.set_forking();
        driver.on_line(
            br#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["No message found with message.uuid of: entry-2"]}"#,
            &mut out,
        );
        assert!(out.events.iter().any(|(_, e)| matches!(e,
            Event::Failed { message } if message.starts_with("Could not fork this Claude Code conversation: No message found"))),
            "{:?}", out.events);
    }

    /// A driver past `initialize` and one completed turn, so nothing Keystone itself
    /// sees keeps it busy.
    fn idle_driver() -> (ClaudeDriver, Outputs) {
        let mut out = Outputs::default();
        let mut driver = ClaudeDriver::new(1, false, 7, ApprovalMode::Provider, &mut out);
        for line in [
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{}}}"#,
        ] {
            driver.on_line(line.as_bytes(), &mut out);
        }
        driver.command(
            Command::Send {
                session: 1,
                text: "hi".into(),
                images: Vec::new(),
            },
            &mut out,
        );
        for line in [
            r#"{"type":"system","subtype":"init","session_id":"s1","capabilities":["msg_lifecycle_v1"]}"#,
            r#"{"type":"command_lifecycle","command_uuid":"00000000-0000-4000-8007-000000000001","state":"started"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
            r#"{"type":"command_lifecycle","command_uuid":"00000000-0000-4000-8007-000000000001","state":"completed"}"#,
        ] {
            driver.on_line(line.as_bytes(), &mut out);
        }
        assert!(!driver.is_running());
        (driver, out)
    }

    const QUIET: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{"session_state":"idle"},"pending_permission_requests":[],"pending_user_dialog_requests":[]}}"#;
    const UNATTRIBUTED: &str = "Claude Code sent activity Keystone could not read or attribute";
    const NO_TASKS: &str =
        r#"{"type":"system","subtype":"background_tasks_changed","session_id":"s1","tasks":[]}"#;

    fn check(driver: &mut ClaudeDriver, lines: &[&str]) -> Option<Result<(), String>> {
        let mut out = Outputs::default();
        driver.check_quiescence(&mut out);
        assert!(
            out.writes[0].contains(r#""request_id":"keystone-2""#),
            "{:?}",
            out.writes
        );
        for line in lines {
            driver.on_line(line.as_bytes(), &mut out);
        }
        driver.take_quiescence()
    }

    /// The regression the naive predicate misses: the turn's `result` is in, so
    /// `is_running` is false, while a background task it started still runs.
    #[test]
    fn a_task_still_running_after_the_result_blocks_eviction() {
        let (mut driver, mut out) = idle_driver();
        assert_eq!(driver.idle_blocker(), None);
        driver.on_line(
            br#"{"type":"system","subtype":"background_tasks_changed","session_id":"s1","tasks":[{"task_id":"t1","run_id":"r1","task_type":"local_bash","description":"build"}]}"#,
            &mut out,
        );
        assert!(!driver.is_running());
        assert_eq!(driver.idle_blocker(), Some("a background task is running"));
        // An ambient task counts too, whatever the SDK says about activity indicators.
        driver.on_line(
            br#"{"type":"system","subtype":"background_tasks_changed","session_id":"s1","tasks":[{"task_id":"t2","task_type":"monitor","description":"watch","ambient":true}]}"#,
            &mut out,
        );
        assert_eq!(driver.idle_blocker(), Some("a background task is running"));
        // A completion edge clears nothing: only a snapshot replaces the set.
        driver.on_line(
            br#"{"type":"system","subtype":"task_notification","task_id":"t2","status":"completed"}"#,
            &mut out,
        );
        assert_eq!(driver.idle_blocker(), Some("a background task is running"));
        driver.on_line(NO_TASKS.as_bytes(), &mut out);
        assert_eq!(driver.idle_blocker(), None);
    }

    #[test]
    fn subagent_tasks_hooks_and_session_state_block_and_count_as_activity() {
        let (mut driver, mut out) = idle_driver();
        let before = driver.activity();
        // A subagent's frames are not shown, but its work is still work.
        driver.on_line(
            br#"{"type":"system","subtype":"background_tasks_changed","parent_tool_use_id":"toolu_1","tasks":[{"task_id":"a1","task_type":"local_agent","description":"sub"}]}"#,
            &mut out,
        );
        assert!(driver.activity() > before);
        assert_eq!(driver.idle_blocker(), Some(UNATTRIBUTED));
        assert_eq!(check(&mut driver, &[QUIET, NO_TASKS]), Some(Ok(())));
        assert_eq!(driver.idle_blocker(), None);
        driver.on_line(
            br#"{"type":"system","subtype":"hook_started","hook_id":"h1","hook_name":"Stop","hook_event":"Stop"}"#,
            &mut out,
        );
        assert_eq!(driver.idle_blocker(), Some("a hook is running"));
        // A response for another hook leaves the first one running.
        driver.on_line(
            br#"{"type":"system","subtype":"hook_response","hook_id":"h0","outcome":"success"}"#,
            &mut out,
        );
        assert_eq!(driver.idle_blocker(), Some("a hook is running"));
        driver.on_line(
            br#"{"type":"system","subtype":"hook_response","hook_id":"h1","outcome":"success"}"#,
            &mut out,
        );
        assert_eq!(driver.idle_blocker(), None);
        driver.on_line(
            br#"{"type":"system","subtype":"session_state_changed","session_id":"s1","state":"requires_action"}"#,
            &mut out,
        );
        assert_eq!(
            driver.idle_blocker(),
            Some("Claude Code reports it is not idle")
        );
        driver.on_line(
            br#"{"type":"system","subtype":"session_state_changed","session_id":"s1","state":"idle"}"#,
            &mut out,
        );
        assert_eq!(driver.idle_blocker(), None);
        // Keep-alives are not activity; anything unrecognized is.
        let quiet = driver.activity();
        driver.on_line(br#"{"type":"keep_alive"}"#, &mut out);
        assert_eq!(driver.activity(), quiet);
        driver.on_line(br#"{"type":"system","subtype":"cron_fired"}"#, &mut out);
        assert!(driver.activity() > quiet);
    }

    #[test]
    fn an_unreadable_safety_frame_vetoes_until_a_passed_check() {
        let (mut driver, mut out) = idle_driver();
        driver.on_line(
            br#"{"type":"system","subtype":"background_tasks_changed","session_id":"s1","tasks":[{"description":"no id"}]}"#,
            &mut out,
        );
        assert_eq!(driver.idle_blocker(), Some(UNATTRIBUTED));
        assert_eq!(driver.check_blocker(), None, "the check is the refresh");
        assert_eq!(check(&mut driver, &[QUIET, NO_TASKS]), Some(Ok(())));
        assert_eq!(driver.idle_blocker(), None);
    }

    /// Only the conversation's own session state counts. A subagent's, another
    /// session's or an unnamed `idle` lowers nothing and holds the process until a
    /// check passes.
    #[test]
    fn only_the_conversations_own_session_state_counts() {
        for foreign in [
            r#"{"type":"system","subtype":"session_state_changed","parent_tool_use_id":"toolu_1","session_id":"s1","state":"idle"}"#,
            r#"{"type":"system","subtype":"session_state_changed","session_id":"s2","state":"idle"}"#,
            r#"{"type":"system","subtype":"session_state_changed","state":"idle"}"#,
        ] {
            let (mut driver, mut out) = idle_driver();
            driver.on_line(
                br#"{"type":"system","subtype":"session_state_changed","session_id":"s1","state":"running"}"#,
                &mut out,
            );
            driver.on_line(foreign.as_bytes(), &mut out);
            assert_eq!(
                driver.idle_blocker(),
                Some("Claude Code reports it is not idle"),
                "{foreign}"
            );
            driver.on_line(
                br#"{"type":"system","subtype":"session_state_changed","session_id":"s1","state":"idle"}"#,
                &mut out,
            );
            assert_eq!(driver.idle_blocker(), Some(UNATTRIBUTED), "{foreign}");
            assert_eq!(
                check(&mut driver, &[QUIET, NO_TASKS]),
                Some(Ok(())),
                "{foreign}"
            );
            assert_eq!(driver.idle_blocker(), None, "{foreign}");
        }
    }

    /// Only the conversation's own snapshot proves it quiescent. A subagent's, another
    /// session's or an unnamed one is activity and proves nothing, even when empty.
    #[test]
    fn only_the_conversations_own_task_list_completes_a_check() {
        for foreign in [
            r#"{"type":"system","subtype":"background_tasks_changed","parent_tool_use_id":"toolu_1","session_id":"s1","tasks":[]}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","parent_tool_use_id":"toolu_1","session_id":"s2","tasks":[]}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","session_id":"s2","tasks":[]}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
        ] {
            let (mut driver, _) = idle_driver();
            let activity = driver.activity();
            assert_eq!(check(&mut driver, &[QUIET, foreign]), None, "{foreign}");
            assert_eq!(driver.activity(), activity + 1, "{foreign}");
            assert_eq!(driver.idle_blocker(), Some(UNATTRIBUTED), "{foreign}");
            let mut out = Outputs::default();
            driver.on_line(NO_TASKS.as_bytes(), &mut out);
            assert_eq!(driver.take_quiescence(), Some(Ok(())), "{foreign}");
            assert_eq!(driver.idle_blocker(), None, "{foreign}");
        }
    }

    #[test]
    fn a_check_passes_only_on_its_own_answer_then_an_empty_snapshot() {
        let (mut driver, _) = idle_driver();
        let activity = driver.activity();
        // A snapshot before the answer is stale: it completes nothing.
        assert_eq!(check(&mut driver, &[NO_TASKS]), None);
        let mut out = Outputs::default();
        driver.on_line(QUIET.as_bytes(), &mut out);
        assert_eq!(driver.take_quiescence(), None, "waits for the snapshot");
        driver.on_line(NO_TASKS.as_bytes(), &mut out);
        assert_eq!(driver.take_quiescence(), Some(Ok(())));
        // The stale snapshot was activity; the answer and its snapshot were not.
        assert_eq!(driver.activity(), activity + 1);

        let (mut driver, _) = idle_driver();
        let activity = driver.activity();
        assert_eq!(check(&mut driver, &[QUIET, NO_TASKS]), Some(Ok(())));
        assert_eq!(
            driver.activity(),
            activity,
            "a passed check is not activity"
        );

        for (answer, snapshot, reason) in [
            (
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{}}}"#,
                NO_TASKS,
                "did not report its pending approvals",
            ),
            (
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{},"pending_permission_requests":[],"pending_user_dialog_requests":[]}}"#,
                NO_TASKS,
                "did not report the session state",
            ),
            (
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{"session_state":"requires_action"},"pending_permission_requests":[{"request_id":"p"}],"pending_user_dialog_requests":[]}}"#,
                NO_TASKS,
                "approvals pending",
            ),
            (
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{"session_state":"running"},"pending_permission_requests":[],"pending_user_dialog_requests":[]}}"#,
                NO_TASKS,
                "reports the session running",
            ),
            (
                QUIET,
                r#"{"type":"system","subtype":"background_tasks_changed","session_id":"s1","tasks":[{"task_id":"t","task_type":"local_bash","description":"d"}]}"#,
                "1 background task(s) are running",
            ),
            (
                r#"{"type":"control_response","response":{"subtype":"error","request_id":"keystone-2","error":"nope"}}"#,
                NO_TASKS,
                "refused",
            ),
        ] {
            let (mut driver, _) = idle_driver();
            let result = check(&mut driver, &[answer, snapshot]);
            assert!(
                matches!(&result, Some(Err(message)) if message.contains(reason)),
                "{reason}: {result:?}"
            );
        }
    }

    /// Claude Code 2.1.300, 2.1.301 and 2.1.305 answer a repeated `initialize` with
    /// `session_state` inside the answer's `response`, beside the version and mode, and
    /// the pending lists beside `request_id`. Recorded offline from the real 2.1.301 with
    /// no credentials (TOB-44, run `init-plain-2.1.301`), abridged to the fields shown,
    /// `request_id` renumbered and the snapshot's `session_id` renamed. #44's connect
    /// receipt (`tests/fixtures/provider-connect-probes.json`) has the same nesting. A
    /// `session_state` beside `request_id`, where no CLI puts it, reports nothing.
    #[test]
    fn a_check_reads_session_state_where_claude_code_sends_it() {
        let recorded = r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{"output_style":"default","account":{"tokenSource":"none","apiProvider":"firstParty"},"pid":37665,"claude_code_version":"2.1.301","current_permission_mode":"auto","remote_control_auto_enable":false,"remote_control_available":true,"fast_mode_state":"off","session_state":"idle","capabilities":["ui_surface_v1","mcp_call_result_not_for_model_v1"]},"pending_permission_requests":[],"pending_user_dialog_requests":[]}}"#;
        let snapshot = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[],"uuid":"2b048f76-afa3-4cc5-b198-2c92bc07b6db","session_id":"s1"}"#;
        let (mut driver, _) = idle_driver();
        assert_eq!(check(&mut driver, &[recorded, snapshot]), Some(Ok(())));

        let beside = r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{},"pending_permission_requests":[],"pending_user_dialog_requests":[],"session_state":"idle"}}"#;
        let (mut driver, _) = idle_driver();
        let result = check(&mut driver, &[beside, snapshot]);
        assert!(
            matches!(&result, Some(Err(m)) if m.contains("did not report the session state")),
            "{result:?}"
        );
    }

    /// Claude follows its answer to a check with the conversation's task snapshot,
    /// passed or not. A failed check's snapshot is still its own: not activity, so the
    /// failure is not taken for news that starts the next check (TOB-333), whether the
    /// connection takes the outcome before the snapshot arrives or after. It proves
    /// nothing either, and the conversation's next snapshot is activity again.
    #[test]
    fn a_failed_checks_own_snapshot_is_not_activity_and_proves_nothing() {
        for answer in [
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{}}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{"session_state":"running"},"pending_permission_requests":[],"pending_user_dialog_requests":[]}}"#,
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"keystone-2","error":"nope"}}"#,
        ] {
            for taken_first in [true, false] {
                let case = format!("{answer} taken first: {taken_first}");
                let (mut driver, mut out) = idle_driver();
                driver.on_line(
                    br#"{"type":"system","subtype":"background_tasks_changed","parent_tool_use_id":"toolu_1","tasks":[]}"#,
                    &mut out,
                );
                let activity = driver.activity();
                let lines: &[&str] = if taken_first {
                    &[answer]
                } else {
                    &[answer, NO_TASKS]
                };
                let result = check(&mut driver, lines);
                assert!(matches!(result, Some(Err(_))), "{case}: {result:?}");
                if taken_first {
                    driver.on_line(NO_TASKS.as_bytes(), &mut out);
                    assert_eq!(driver.take_quiescence(), None, "{case}");
                }
                assert_eq!(driver.activity(), activity, "{case}");
                assert_eq!(driver.idle_blocker(), Some(UNATTRIBUTED), "{case}");
                driver.on_line(NO_TASKS.as_bytes(), &mut out);
                assert_eq!(driver.activity(), activity + 1, "{case}");
                assert_eq!(driver.idle_blocker(), Some(UNATTRIBUTED), "{case}");
            }
        }
    }

    /// While a failed check's snapshot is owed, anything else is still activity: a
    /// subagent's, another session's, an unnamed or an unreadable task list, and a
    /// second snapshot of the conversation's own. A snapshot listing a task still
    /// blocks, owed or not.
    #[test]
    fn only_one_own_snapshot_is_owed_to_a_failed_check() {
        let running = r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{"session_state":"running"},"pending_permission_requests":[],"pending_user_dialog_requests":[]}}"#;
        for foreign in [
            r#"{"type":"system","subtype":"background_tasks_changed","parent_tool_use_id":"toolu_1","session_id":"s1","tasks":[]}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","session_id":"s2","tasks":[]}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","session_id":"s1","tasks":[{"description":"no id"}]}"#,
        ] {
            let (mut driver, mut out) = idle_driver();
            let activity = driver.activity();
            assert!(matches!(check(&mut driver, &[running]), Some(Err(_))));
            driver.on_line(foreign.as_bytes(), &mut out);
            assert_eq!(driver.activity(), activity + 1, "{foreign}");
            driver.on_line(NO_TASKS.as_bytes(), &mut out);
            assert_eq!(driver.activity(), activity + 1, "{foreign}");
            assert_eq!(driver.idle_blocker(), Some(UNATTRIBUTED), "{foreign}");
            driver.on_line(NO_TASKS.as_bytes(), &mut out);
            assert_eq!(driver.activity(), activity + 2, "{foreign}");
        }

        let (mut driver, mut out) = idle_driver();
        assert!(matches!(check(&mut driver, &[running]), Some(Err(_))));
        driver.on_line(
            br#"{"type":"system","subtype":"background_tasks_changed","session_id":"s1","tasks":[{"task_id":"t","task_type":"local_bash","description":"d"}]}"#,
            &mut out,
        );
        assert_eq!(driver.idle_blocker(), Some("a background task is running"));
    }

    /// A check abandoned after its answer, such as one whose snapshot timed out, owes
    /// nothing: a snapshot arriving later is activity.
    #[test]
    fn a_snapshot_after_an_abandoned_answered_check_is_activity() {
        let running = r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-2","response":{"session_state":"running"},"pending_permission_requests":[],"pending_user_dialog_requests":[]}}"#;
        for answer in [QUIET, running] {
            let (mut driver, mut out) = idle_driver();
            let activity = driver.activity();
            check(&mut driver, &[answer]);
            driver.abandon_quiescence();
            driver.on_line(NO_TASKS.as_bytes(), &mut out);
            assert_eq!(driver.take_quiescence(), None, "{answer}");
            assert_eq!(driver.activity(), activity + 1, "{answer}");
        }
    }

    /// An agent's steer refused because nothing runs leaves the idle proof standing:
    /// the connection returns the refusal without revisiting idle state.
    #[test]
    fn a_refused_steer_is_not_activity() {
        let (mut driver, mut out) = idle_driver();
        let activity = driver.activity();
        assert!(driver.steer("nudge".into(), &mut out).is_err());
        assert_eq!(driver.activity(), activity);
        assert!(out.writes.iter().all(|line| !line.contains("nudge")));
    }

    #[test]
    fn an_abandoned_check_ignores_its_late_answer() {
        let (mut driver, _) = idle_driver();
        assert_eq!(check(&mut driver, &[]), None);
        driver.abandon_quiescence();
        let activity = driver.activity();
        let mut out = Outputs::default();
        driver.on_line(QUIET.as_bytes(), &mut out);
        driver.on_line(NO_TASKS.as_bytes(), &mut out);
        assert_eq!(driver.take_quiescence(), None);
        assert!(
            driver.activity() > activity,
            "a late answer is not expected"
        );
        assert_eq!(driver.idle_blocker(), None);
    }

    #[test]
    fn an_abandoned_checks_snapshot_stays_passive_after_the_next_check_starts() {
        let (mut driver, mut out) = idle_driver();
        assert_eq!(check(&mut driver, &[]), None);
        driver.abandon_quiescence();
        driver.on_line(QUIET.as_bytes(), &mut out);
        assert_eq!(driver.check_blocker(), None);
        let activity = driver.activity();
        driver.check_quiescence(&mut out);
        driver.on_line(NO_TASKS.as_bytes(), &mut out);
        assert_eq!(
            driver.activity(),
            activity,
            "the abandoned check's own snapshot"
        );
    }

    #[test]
    fn the_next_check_waits_for_an_abandoned_checks_answer() {
        let (mut driver, mut out) = idle_driver();
        assert_eq!(check(&mut driver, &[]), None);
        driver.abandon_quiescence();
        assert_eq!(
            driver.check_blocker(),
            Some("a control request is unanswered")
        );
        let activity = driver.activity();
        driver.on_line(QUIET.as_bytes(), &mut out);
        assert_eq!(driver.activity(), activity + 1);
        assert_eq!(driver.check_blocker(), None);
        driver.on_line(NO_TASKS.as_bytes(), &mut out);
        assert_eq!(driver.activity(), activity + 1, "its own snapshot");

        driver.check_quiescence(&mut out);
        let next = QUIET.replace("keystone-2", "keystone-3");
        driver.on_line(next.as_bytes(), &mut out);
        driver.on_line(NO_TASKS.as_bytes(), &mut out);
        assert_eq!(driver.take_quiescence(), Some(Ok(())));
        assert_eq!(driver.activity(), activity + 1);
    }

    #[test]
    fn exit_codes_are_read_from_failed_command_output() {
        assert_eq!(exit_code_in("Exit code 2\nls: no such file"), Some(2));
        assert_eq!(exit_code_in("permission denied"), None);
    }
}
