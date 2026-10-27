//! The Codex driver: `codex app-server` JSON-RPC, as a state machine with no IO.
//!
//! One app-server hosts every Codex session. Each Keystone session maps to one Codex
//! thread. Lines go in through [`CodexDriver::on_line`], user actions through
//! [`CodexDriver::command`], and both produce lines to write and [`Event`]s per session.
//! Checked against codex-cli 0.162.1; rate-limit and token-usage notifications against
//! the 0.163.0 app-server schema.

use crate::commands::{self, Commands};
use crate::driver::{ApprovalState, Command, Image, Outputs};
use crate::item::{
    Approval, Catalog, Event, Item, ItemKind, ItemStatus, ModelInfo, RequestId, TurnStatus,
    cap_output,
};
use crate::mcp::{Caller, Issuer};
use crate::surface;
use keystone_core::integrations::{Health, ServerStatus};
use keystone_core::{ApprovalMode, ForkPoint, LimitWindow};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const CATALOG_PAGES: u32 = 20;

/// Notifications Keystone does not show. Codex skips sending them.
const OPT_OUT: [&str; 3] = [
    "remoteControl/status/changed",
    "account/updated",
    "thread/status/changed",
];

/// A Codex approval policy and sandbox, sent with each `turn/start`. Codex keeps them
/// for the thread's later turns too.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    pub approval: ApprovalPolicy,
    pub sandbox: Sandbox,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

/// The Codex approval policy and sandbox for a mode. `None` sends nothing, so the user's
/// own Codex configuration applies.
pub fn policy(mode: ApprovalMode) -> Option<Policy> {
    let (approval, sandbox) = match mode {
        ApprovalMode::Provider => return None,
        ApprovalMode::ReadOnly => (ApprovalPolicy::OnRequest, Sandbox::ReadOnly),
        ApprovalMode::Ask => (ApprovalPolicy::Untrusted, Sandbox::WorkspaceWrite),
        ApprovalMode::Auto => (ApprovalPolicy::OnRequest, Sandbox::WorkspaceWrite),
        ApprovalMode::FullAccess => (ApprovalPolicy::Never, Sandbox::FullAccess),
    };
    Some(Policy { approval, sandbox })
}

impl Policy {
    fn params(self) -> (Value, Value) {
        let approval = match self.approval {
            ApprovalPolicy::Untrusted => "untrusted",
            ApprovalPolicy::OnRequest => "on-request",
            ApprovalPolicy::Never => "never",
        };
        let sandbox = match self.sandbox {
            Sandbox::ReadOnly => "readOnly",
            Sandbox::WorkspaceWrite => "workspaceWrite",
            Sandbox::FullAccess => "dangerFullAccess",
        };
        (json!(approval), json!({ "type": sandbox }))
    }
}

/// The user's own settings for a directory, as a fresh thread reports them.
#[derive(Clone, Debug, PartialEq)]
enum Baseline {
    Requested,
    Known(Value, Value),
    Unavailable,
}

enum Pending {
    Initialize,
    Thread {
        session: u64,
        resume: bool,
        /// Whether it forks another thread; `resume` is set too.
        fork: bool,
        injected: bool,
        /// The same `thread/start` without `dynamicTools`, to send if Codex refuses
        /// the tool.
        without_tools: Option<Value>,
    },
    TurnStart {
        session: u64,
    },
    /// A throwaway thread, opened only to read the user's settings for `cwd`.
    Baseline {
        cwd: String,
    },
    /// One page of `model/list`, asked for by `session`, with the models of the pages
    /// before it.
    Catalog {
        session: u64,
        models: Vec<ModelInfo>,
        page: u32,
    },
    /// `skills/list` for one folder, asked for by `session`.
    Commands {
        session: u64,
    },
    Steer {
        session: u64,
        message: QueuedMessage,
        turn: String,
        /// The session's stop count when the steer was sent.
        epoch: u64,
        strict: bool,
    },
    /// A read of the session's stored history for crash recovery. `fallback` marks
    /// the older `thread/read` form.
    History {
        session: u64,
        ticket: u64,
        thread: String,
        fallback: bool,
        /// The most turns kept.
        turns: u64,
    },
    /// `thread/list` for an import listing.
    Threads {
        ticket: u64,
    },
    /// The turn count of thread `index` in listing `ticket`.
    TurnCount {
        ticket: u64,
        index: usize,
    },
    Ignore,
}

/// The most turns counted for one conversation in an import listing.
const TURN_COUNT_LIMIT: u64 = 1000;

/// An import listing whose turn counts are still arriving.
struct Listing {
    threads: Vec<crate::import::Listed>,
    waiting: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Turn {
    #[default]
    Idle,
    /// `turn/start` was sent; the turn has no ID to steer or stop yet.
    Starting,
    Running(String),
}

struct QueuedMessage {
    text: String,
    images: Vec<Image>,
    authorization: Option<(Issuer, Caller)>,
}

#[derive(Default)]
struct Session {
    thread: Option<String>,
    opening: bool,
    turn: Turn,
    /// Messages accepted but not yet delivered, in order: sent before the thread
    /// existed, or while a turn was starting and had no ID to steer.
    waiting: Vec<QueuedMessage>,
    /// Stop was asked while the turn was starting. It is sent once the turn has an ID.
    stop_requested: bool,
    /// The user ended the running turn, with Stop or a denial that cancels it. Its
    /// `turn/completed` reports it, so it never carries over to another turn.
    stopped: bool,
    /// Counts stops. A steer sent before a stop must not start a turn after it.
    epoch: u64,
    /// Open approval requests and the decision that denies each one.
    approvals: HashMap<RequestId, &'static str>,
    /// `show_ui` calls that wait for a Button press: Codex's request ID by call ID.
    ui_calls: HashMap<String, RequestId>,
    /// Every `show_ui` call ID the running turn made, answered or not.
    ui_seen: HashSet<String>,
    cwd: String,
    /// The model the thread is opened with. `None` leaves the choice to Codex.
    model: Option<String>,
    /// The policy for new turns. `None` uses the user's settings.
    policy: Option<Policy>,
    mode: ApprovalMode,
    /// The policy the running or starting turn was started under. A message for a
    /// session whose policy changed since waits for the next turn instead of steering.
    turn_policy: Option<Policy>,
    /// The thread may hold an override from an earlier turn, so using the user's
    /// settings means sending them. It stays set for the thread: every turn under the
    /// user's settings sends them, so no failed or lost restore can leave an override
    /// behind.
    overridden: bool,
    /// Where the next open forks another thread, when it has no thread to resume.
    fork: Option<ForkPoint>,
}

pub struct CodexDriver {
    /// Sessions whose turn's end waits for [`CodexDriver::release_follow_ups`] before
    /// messages sent during that turn start the next one.
    holds: HashSet<u64>,
    /// Sessions whose last turn's end has not been released yet.
    unseen_ends: HashSet<u64>,
    initialize_params: Value,
    initialization_error: Option<String>,
    next_id: u64,
    ready: bool,
    before_ready: Vec<String>,
    pending: HashMap<u64, Pending>,
    sessions: HashMap<u64, Session>,
    threads: HashMap<String, u64>,
    /// The user's own approval policy and sandbox, per working directory.
    baselines: HashMap<String, Baseline>,
    mcp: HashMap<u64, Value>,
    /// Each session's MCP servers as its thread last reported starting them.
    mcp_status: HashMap<u64, BTreeMap<String, ServerStatus>>,
    /// Sessions whose threads were given Keystone's own `keystone` server.
    injected: HashSet<u64>,
    /// Whether new threads get the `show_ui` dynamic tool. Off once Codex refuses it.
    widgets: bool,
    /// Import listings waiting for their turn counts, by ticket.
    listings: HashMap<u64, Listing>,
    /// Messages Keystone did not recognize. Never fatal.
    pub unknown: u64,
}

impl CodexDriver {
    /// Starts the handshake. Write `out.writes` before anything else.
    pub fn new(client_version: &str, out: &mut Outputs) -> Self {
        Self::with_widgets(client_version, false, out)
    }

    /// Starts the handshake, offering the `show_ui` dynamic tool to new threads when
    /// `widgets` is set. The field is experimental in Codex, so the handshake opts into
    /// experimental API; a refusal leaves the tool out.
    pub fn with_widgets(client_version: &str, widgets: bool, out: &mut Outputs) -> Self {
        let mut params = json!({"clientInfo": {"name": "keystone", "title": "Keystone", "version": client_version}, "capabilities": {"optOutNotificationMethods": OPT_OUT}});
        if widgets {
            params["capabilities"]["experimentalApi"] = json!(true);
        }
        let mut driver = Self {
            holds: HashSet::new(),
            unseen_ends: HashSet::new(),
            initialize_params: params.clone(),
            initialization_error: None,
            next_id: 0,
            ready: false,
            before_ready: Vec::new(),
            pending: HashMap::new(),
            sessions: HashMap::new(),
            threads: HashMap::new(),
            baselines: HashMap::new(),
            mcp: HashMap::new(),
            mcp_status: HashMap::new(),
            injected: HashSet::new(),
            widgets,
            listings: HashMap::new(),
            unknown: 0,
        };
        let line = driver.request("initialize", params, Pending::Initialize);
        out.writes.push(line);
        driver
    }

    /// Whether the session's conversation is open or opening in this process.
    pub fn is_open(&self, session: u64) -> bool {
        self.sessions
            .get(&session)
            .is_some_and(|s| s.thread.is_some() || s.opening)
    }

    pub fn hold_follow_ups(&mut self, session: u64) {
        self.holds.insert(session);
    }

    /// Lets messages sent during the session's turn that ended last start the next
    /// one.
    pub fn release_follow_ups(&mut self, session: u64, out: &mut Outputs) {
        if self.unseen_ends.remove(&session) {
            self.flush(session, out);
        }
    }

    /// Whether a turn runs, or a held follow-up waits to start one.
    pub fn is_running(&self, session: u64) -> bool {
        self.sessions.get(&session).is_some_and(|s| {
            s.turn != Turn::Idle || (self.unseen_ends.contains(&session) && !s.waiting.is_empty())
        })
    }

    pub(crate) fn set_mcp(&mut self, session: u64, url: String, token: String) {
        self.mcp.insert(
            session,
            json!({"mcp_servers": {"keystone": {
                "url": url, "http_headers": {"Authorization": format!("Bearer {token}")},
                "tool_timeout_sec": 60
            }}}),
        );
    }

    pub(crate) fn send_from(
        &mut self,
        session: u64,
        text: String,
        issuer: Issuer,
        caller: Caller,
        out: &mut Outputs,
    ) {
        self.send(
            session,
            QueuedMessage {
                text,
                images: Vec::new(),
                authorization: Some((issuer, caller)),
            },
            out,
        );
    }

    pub(crate) fn steer(
        &mut self,
        session: u64,
        text: String,
        out: &mut Outputs,
    ) -> Result<(), &'static str> {
        let state = self
            .sessions
            .get(&session)
            .ok_or("busy: The target is not running")?;
        if !self.ready
            || state.stopped
            || state.stop_requested
            || state.policy != state.turn_policy
            || !state.waiting.is_empty()
        {
            return Err("busy: Codex cannot steer this turn immediately");
        }
        let (Some(thread), Turn::Running(turn)) = (&state.thread, &state.turn) else {
            return Err("busy: Codex has no running turn to steer");
        };
        let (thread, turn, epoch) = (thread.clone(), turn.clone(), state.epoch);
        let line = self.request(
            "turn/steer",
            json!({"threadId": thread, "expectedTurnId": turn, "input": text_input(&text)}),
            Pending::Steer {
                session,
                message: QueuedMessage {
                    text,
                    images: Vec::new(),
                    authorization: None,
                },
                turn,
                epoch,
                strict: true,
            },
        );
        out.writes.push(line);
        Ok(())
    }

    pub(crate) fn interrupt_now(
        &mut self,
        session: u64,
        out: &mut Outputs,
    ) -> Result<(), &'static str> {
        self.interrupt(session, out);
        Ok(())
    }

    pub fn command(&mut self, command: Command, out: &mut Outputs) {
        if let Some(message) = &self.initialization_error {
            match command {
                Command::Open { session, .. } | Command::Send { session, .. } => out.event(
                    session,
                    Event::Failed {
                        message: message.clone(),
                    },
                ),
                _ => {}
            }
            return;
        }
        match command {
            Command::Open {
                session,
                cwd,
                resume,
                model,
            } => {
                let state = self.sessions.entry(session).or_default();
                state.opening = true;
                state.cwd = cwd.clone();
                state.model = model;
                let fork = state.fork.take().filter(|_| resume.is_none());
                // A resumed or forked thread reports its own override, not the user's
                // settings, so read those from a fresh thread before the next turn needs
                // them.
                if resume.is_none() && fork.is_none() {
                    // A new thread holds no override.
                    state.overridden = false;
                } else if state.overridden {
                    self.request_baseline(&cwd, out);
                }
                self.open_thread(session, cwd, resume, fork, out);
            }
            Command::Configure {
                session,
                approval: ApprovalState { mode, overridden },
            } => {
                let open = self.is_open(session);
                let state = self.sessions.entry(session).or_default();
                state.policy = policy(mode);
                state.mode = mode;
                // Once open, this process knows what it sent the thread.
                if !open {
                    state.overridden |= overridden;
                }
            }
            Command::Send {
                session,
                text,
                images,
            } => self.send(
                session,
                QueuedMessage {
                    text,
                    images,
                    authorization: None,
                },
                out,
            ),
            Command::Interrupt { session } => self.interrupt(session, out),
            Command::Act {
                session,
                call,
                text,
            } => {
                match self
                    .sessions
                    .get_mut(&session)
                    .and_then(|state| state.ui_calls.remove(&call))
                {
                    Some(request) => out.writes.push(format!(
                        r#"{{"id":{},"result":{}}}"#,
                        request.0,
                        json!({"contentItems": [{"type": "inputText", "text": text}], "success": true})
                    )),
                    None => out.event(session, Event::ActionUndelivered { call, text }),
                }
            }
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

    /// Asks for the models Codex offers, following `nextCursor` through every page up
    /// to [`CATALOG_PAGES`]. The answer arrives as [`Event::Catalog`] for `session`.
    pub fn request_catalog(&mut self, session: u64, out: &mut Outputs) {
        if let Some(message) = &self.initialization_error {
            out.event(session, Event::Catalog(Catalog::Failed(message.clone())));
            return;
        }
        self.request_catalog_page(session, Vec::new(), 1, None, out);
    }

    /// Asks for the skills Codex offers in `cwd`. The answer arrives as
    /// [`Event::Commands`] for `session`.
    pub fn request_commands(&mut self, session: u64, cwd: &str, out: &mut Outputs) {
        if let Some(message) = &self.initialization_error {
            out.event(session, Event::Commands(Commands::Failed(message.clone())));
            return;
        }
        let line = self.request(
            "skills/list",
            json!({"cwds": [cwd]}),
            Pending::Commands { session },
        );
        self.write(line, out);
    }

    /// Reads the stored history of `thread` without resuming it or opening it for
    /// `session`. The answer arrives as [`Event::HistoryRead`] with `ticket`.
    pub fn read_history(&mut self, session: u64, thread: String, ticket: u64, out: &mut Outputs) {
        self.read_turns(session, thread, ticket, crate::recovery::codex::TURNS, out);
    }

    /// Reads the newest `turns` turns of `thread` as [`CodexDriver::read_history`]
    /// does.
    pub fn read_turns(
        &mut self,
        session: u64,
        thread: String,
        ticket: u64,
        turns: u64,
        out: &mut Outputs,
    ) {
        if let Some(message) = &self.initialization_error {
            out.event(
                session,
                Event::HistoryRead {
                    ticket,
                    result: Err(message.clone()),
                },
            );
            return;
        }
        let params = json!({"threadId": thread, "limit": turns,
            "sortDirection": "desc", "itemsView": "full"});
        let line = self.request(
            "thread/turns/list",
            params,
            Pending::History {
                session,
                ticket,
                thread,
                fallback: false,
                turns,
            },
        );
        self.write(line, out);
    }

    /// Lists the newest [`crate::import::LISTED`] conversations that ran in `cwd`,
    /// each with its turn count. Nothing is resumed or opened. The answer arrives as
    /// [`Event::ThreadsListed`] with `ticket`, for session 0.
    pub fn list_threads(&mut self, cwd: &str, ticket: u64, out: &mut Outputs) {
        if let Some(message) = &self.initialization_error {
            let result = Err(message.clone());
            out.event(0, Event::ThreadsListed { ticket, result });
            return;
        }
        // Codex repairs its own index while it scans unless told to read only that.
        let params = json!({"cwd": cwd, "limit": crate::import::LISTED,
            "sortKey": "updated_at", "sortDirection": "desc", "useStateDbOnly": true,
            "sourceKinds": ["cli", "vscode", "exec", "appServer"]});
        let line = self.request("thread/list", params, Pending::Threads { ticket });
        self.write(line, out);
    }

    fn listing_failed(&mut self, ticket: u64, message: String, out: &mut Outputs) {
        self.listings.remove(&ticket);
        let result = Err(format!("Codex could not list its conversations: {message}"));
        out.event(0, Event::ThreadsListed { ticket, result });
    }

    fn request_catalog_page(
        &mut self,
        session: u64,
        models: Vec<ModelInfo>,
        page: u32,
        cursor: Option<String>,
        out: &mut Outputs,
    ) {
        let params = match cursor {
            Some(cursor) => json!({"cursor": cursor}),
            None => json!({}),
        };
        let line = self.request(
            "model/list",
            params,
            Pending::Catalog {
                session,
                models,
                page,
            },
        );
        self.write(line, out);
    }

    /// The app-server exited. Every running turn fails, every thread must be opened
    /// again in a new process, every reported model is gone with the process, and an
    /// unanswered catalog request fails.
    pub fn on_exit(&mut self, reason: &str, out: &mut Outputs) {
        let mut listings = BTreeSet::new();
        for pending in self.pending.values() {
            match pending {
                Pending::Catalog { session, .. } => {
                    out.event(*session, Event::Catalog(Catalog::Failed(reason.to_owned())))
                }
                Pending::Commands { session } => out.event(
                    *session,
                    Event::Commands(Commands::Failed(reason.to_owned())),
                ),
                Pending::History {
                    session, ticket, ..
                } => out.event(
                    *session,
                    Event::HistoryRead {
                        ticket: *ticket,
                        result: Err(reason.to_owned()),
                    },
                ),
                Pending::Threads { ticket } | Pending::TurnCount { ticket, .. } => {
                    listings.insert(*ticket);
                }
                _ => {}
            }
        }
        for ticket in listings {
            self.listing_failed(ticket, reason.to_owned(), out);
        }
        for (&session, state) in &mut self.sessions {
            if std::mem::take(&mut state.turn) != Turn::Idle {
                out.event(session, Event::RecoveryNeeded);
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
            out.event(session, Event::ModelLost);
        }
        self.sessions.clear();
        self.threads.clear();
        self.pending.clear();
        self.baselines.clear();
        self.mcp.clear();
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

    /// Makes the session's next open fork `point`'s thread through its turn, when the
    /// session has no thread of its own to resume.
    pub fn fork_on_open(&mut self, session: u64, point: ForkPoint) {
        self.sessions.entry(session).or_default().fork = Some(point);
    }

    fn open_thread(
        &mut self,
        session: u64,
        cwd: String,
        resume: Option<String>,
        fork: Option<ForkPoint>,
        out: &mut Outputs,
    ) {
        let (method, mut params) = match (&resume, &fork) {
            (Some(thread), _) => (
                "thread/resume",
                json!({"threadId": thread, "cwd": cwd, "excludeTurns": true}),
            ),
            (None, Some(fork)) => (
                "thread/fork",
                json!({"threadId": fork.thread, "lastTurnId": fork.at, "cwd": cwd,
                    "excludeTurns": true}),
            ),
            (None, None) => ("thread/start", json!({"cwd": cwd})),
        };
        let forking = fork.is_some();
        let resume = resume.is_some() || forking;
        // A resumed thread keeps the tools it started with.
        if let Some(model) = self.sessions.get(&session).and_then(|s| s.model.clone()) {
            params["model"] = json!(model);
        }
        let injected = if let Some(config) = self.mcp.remove(&session) {
            params["config"] = config;
            self.injected.insert(session);
            true
        } else {
            self.injected.remove(&session);
            false
        };
        let without_tools = (self.widgets && !resume).then(|| {
            let plain = params.clone();
            params["dynamicTools"] = json!([surface::codex_tool()]);
            plain
        });
        let line = self.request(
            method,
            params,
            Pending::Thread {
                session,
                resume,
                fork: forking,
                injected,
                without_tools,
            },
        );
        self.write(line, out);
    }

    /// Delivers a message: a new turn when idle, a steer while a turn runs, or a
    /// place in line while the thread opens or the turn starts.
    fn send(&mut self, session: u64, message: QueuedMessage, out: &mut Outputs) {
        if let Some(message) = &self.initialization_error {
            return out.event(
                session,
                Event::Failed {
                    message: message.clone(),
                },
            );
        }
        let held = self.unseen_ends.contains(&session);
        let state = self.sessions.entry(session).or_default();
        match (state.thread.clone(), state.turn.clone()) {
            (Some(_), Turn::Idle) if held => state.waiting.push(message),
            (None, _) if !state.opening => out.event(
                session,
                Event::Failed {
                    message: "This conversation is not open in Codex".into(),
                },
            ),
            (None, _) | (Some(_), Turn::Starting) => state.waiting.push(message),
            // The running turn has another approval mode: the message starts the next
            // turn when this one ends, under the latest mode.
            (Some(_), Turn::Running(_)) if state.policy != state.turn_policy => {
                state.waiting.push(message)
            }
            (Some(thread), Turn::Running(turn)) => {
                let epoch = state.epoch;
                let line = self.request(
                    "turn/steer",
                    json!({"threadId": thread, "expectedTurnId": turn, "input": input(&message.text, &message.images)}),
                    Pending::Steer {
                        session,
                        message,
                        turn,
                        epoch,
                        strict: false,
                    },
                );
                self.write(line, out);
            }
            (Some(thread), Turn::Idle) => {
                let mut params =
                    json!({"threadId": thread, "input": input(&message.text, &message.images)});
                match (state.policy, state.overridden) {
                    (Some(policy), _) => {
                        let (approval, sandbox) = policy.params();
                        params["approvalPolicy"] = approval;
                        params["sandboxPolicy"] = sandbox;
                    }
                    (None, false) => {}
                    (None, true) => match self.baselines.get(&state.cwd) {
                        Some(Baseline::Known(approval, sandbox)) => {
                            params["approvalPolicy"] = approval.clone();
                            params["sandboxPolicy"] = sandbox.clone();
                        }
                        // Wait for the user's settings instead of running under an
                        // override they turned off.
                        Some(Baseline::Requested) => return state.waiting.push(message),
                        None => {
                            state.waiting.push(message);
                            let cwd = state.cwd.clone();
                            return self.request_baseline(&cwd, out);
                        }
                        Some(Baseline::Unavailable) => {
                            state.waiting.clear();
                            return out.event(
                                session,
                                Event::Failed {
                                    message: "Could not read your Codex approval settings. Choose an approval mode.".into(),
                                },
                            );
                        }
                    },
                }
                if message
                    .authorization
                    .as_ref()
                    .is_some_and(|(issuer, caller)| !issuer.permits_launch(*caller, state.mode))
                {
                    return out.event(
                        session,
                        Event::Failed {
                            message:
                                "The launching agent no longer has permission to send this message"
                                    .into(),
                        },
                    );
                }
                // Starting, so further messages wait for the ID instead of starting a
                // parallel turn.
                state.turn = Turn::Starting;
                state.overridden |= state.policy.is_some();
                state.turn_policy = state.policy;
                let line = self.request("turn/start", params, Pending::TurnStart { session });
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

    fn request_baseline(&mut self, cwd: &str, out: &mut Outputs) {
        if self.baselines.contains_key(cwd) {
            return;
        }
        self.baselines.insert(cwd.to_owned(), Baseline::Requested);
        let line = self.request(
            "thread/start",
            json!({"cwd": cwd, "ephemeral": true}),
            Pending::Baseline {
                cwd: cwd.to_owned(),
            },
        );
        self.write(line, out);
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
        for message in waiting {
            self.send(session, message, out);
        }
    }

    fn on_response(&mut self, id: Value, error: Option<RpcError>, line: &[u8], out: &mut Outputs) {
        let Some(pending) = id.as_u64().and_then(|id| self.pending.remove(&id)) else {
            self.unknown += 1;
            return;
        };
        match pending {
            Pending::Initialize => {
                if let Some(error) = error {
                    if self.initialize_params.get("capabilities").is_some()
                        && (error.code == Some(-32602)
                            || error.message.contains("optOutNotificationMethods")
                            || error.message.contains("capabilities"))
                    {
                        self.initialize_params
                            .as_object_mut()
                            .unwrap()
                            .remove("capabilities");
                        // The experimental opt-in went with it.
                        self.widgets = false;
                        let line = self.request(
                            "initialize",
                            self.initialize_params.clone(),
                            Pending::Initialize,
                        );
                        out.writes.push(line);
                    } else {
                        let message = format!(
                            "Codex could not initialize: {}. Check the CLI version, then restart Keystone.",
                            error.message
                        );
                        self.before_ready.clear();
                        self.on_exit(&message, out);
                        self.initialization_error = Some(message);
                    }
                    return;
                }
                self.ready = true;
                out.writes.push(r#"{"method":"initialized"}"#.to_owned());
                out.writes.append(&mut self.before_ready);
            }
            Pending::Baseline { cwd } => {
                let result = serde_json::from_slice::<Response<ThreadResult>>(line)
                    .ok()
                    .and_then(|r| r.result);
                match result.and_then(ThreadResult::settings) {
                    Some((approval, sandbox)) => {
                        self.baselines
                            .insert(cwd.clone(), Baseline::Known(approval, sandbox));
                    }
                    None => {
                        self.baselines.insert(cwd.clone(), Baseline::Unavailable);
                    }
                }
                let waiting: Vec<u64> = self
                    .sessions
                    .iter()
                    .filter(|(_, s)| s.cwd == cwd && !s.waiting.is_empty())
                    .map(|(&id, _)| id)
                    .collect();
                for session in waiting {
                    self.flush(session, out);
                }
            }
            Pending::Thread {
                session,
                resume,
                fork,
                injected,
                without_tools,
            } => {
                let response = serde_json::from_slice::<Response<ThreadResult>>(line).ok();
                let refused_tools = response
                    .as_ref()
                    .and_then(|r| r.error.as_ref()?.get("message")?.as_str())
                    .is_some_and(|message| message.contains("dynamicTools"));
                let result = response.and_then(|r| r.result);
                // Codex refused the experimental tool: start the thread without it. Any
                // other error is the thread's own and keeps the tool for later threads.
                if result.is_none()
                    && refused_tools
                    && let Some(params) = without_tools
                {
                    self.widgets = false;
                    let line = self.request(
                        "thread/start",
                        params,
                        Pending::Thread {
                            session,
                            resume,
                            fork,
                            injected,
                            without_tools: None,
                        },
                    );
                    return self.write(line, out);
                }
                // A new thread has no override yet, so it reports the user's settings.
                if !resume
                    && let Some(cwd) = self.sessions.get(&session).map(|s| s.cwd.clone())
                    && let Some((approval, sandbox)) =
                        result.as_ref().and_then(|r| r.clone().settings())
                {
                    self.baselines
                        .insert(cwd, Baseline::Known(approval, sandbox));
                }
                match result.map(|r| (r.thread.id, r.model)) {
                    Some((thread, model)) => {
                        self.thread_ready(session, thread, out);
                        if let Some(model) = model {
                            out.event(session, Event::ModelReported { model });
                        }
                    }
                    // Never fall back to a fresh thread on its own: the user would see the
                    // old conversation while Codex had none of it.
                    None => {
                        let state = self.sessions.entry(session).or_default();
                        state.opening = false;
                        state.waiting.clear();
                        let reason = if injected {
                            "MCP-enabled thread initialization failed".into()
                        } else {
                            error.map_or("no thread in the reply".into(), |e| e.message)
                        };
                        let message = if fork {
                            format!(
                                "Could not fork this Codex conversation: {reason}. Hand off (Ctrl+Shift+H) to continue it."
                            )
                        } else if resume {
                            format!(
                                "Could not resume this Codex conversation: {reason}. Start a new Codex session to continue."
                            )
                        } else {
                            format!("Codex did not start a conversation: {reason}")
                        };
                        out.event(session, Event::Failed { message });
                        out.event(session, Event::ModelLost);
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
            Pending::Catalog {
                session,
                mut models,
                page,
            } => {
                let listed = match error {
                    Some(error) => Err(error.message),
                    None => serde_json::from_slice::<Response<ModelList>>(line)
                        .ok()
                        .and_then(|r| r.result)
                        .ok_or_else(|| "no models in the reply".to_owned()),
                };
                let listed = match listed {
                    Ok(listed) => listed,
                    Err(message) => {
                        return out.event(session, Event::Catalog(Catalog::Failed(message)));
                    }
                };
                models.extend(listed.visible());
                match listed.next_cursor {
                    Some(_) if page >= CATALOG_PAGES => out.event(
                        session,
                        Event::Catalog(Catalog::Failed(format!(
                            "Codex returned more than {CATALOG_PAGES} pages of models"
                        ))),
                    ),
                    Some(cursor) => {
                        self.request_catalog_page(session, models, page + 1, Some(cursor), out)
                    }
                    // Codex's `isDefault` is not the user's configured default, so what
                    // the default runs as stays unknown.
                    None => out.event(
                        session,
                        Event::Catalog(Catalog::Ready {
                            models,
                            default_model: None,
                        }),
                    ),
                }
            }
            Pending::Commands { session } => {
                let commands = match error {
                    Some(error) => Commands::Failed(format!(
                        "Codex could not list its skills: {}",
                        error.message
                    )),
                    None => match serde_json::from_slice::<Response<Value>>(line)
                        .ok()
                        .and_then(|r| r.result)
                    {
                        Some(result) => Commands::Ready(commands::codex(&result)),
                        None => Commands::Failed("Codex listed no skills.".into()),
                    },
                };
                out.event(session, Event::Commands(commands));
            }
            Pending::Steer {
                session,
                message,
                turn,
                epoch,
                strict,
            } => {
                let Some(error) = error else { return };
                let Some(state) = self.sessions.get(&session) else {
                    return;
                };
                let still_running = state.turn == Turn::Running(turn.clone());
                if strict {
                    out.event(session, Event::Warning { message: "The agent message could not steer the running turn; it was not retried".into() });
                } else if state.epoch != epoch {
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
                    self.send(session, message, out);
                }
            }
            Pending::History {
                session,
                ticket,
                thread,
                fallback,
                turns,
            } => {
                if error.is_some() && !fallback {
                    // An app-server without turn pagination, or one that will not list
                    // full items for this thread, pages its newest items instead. Codex
                    // answers an unknown method with -32600, not -32601.
                    let line = self.request(
                        "thread/items/list",
                        json!({"threadId": thread, "sortDirection": "desc",
                            "limit": crate::recovery::codex::ITEMS}),
                        Pending::History {
                            session,
                            ticket,
                            thread,
                            fallback: true,
                            turns,
                        },
                    );
                    return self.write(line, out);
                }
                let result = match error {
                    Some(error) => Err(format!(
                        "Codex could not read this conversation's history: {}",
                        error.message
                    )),
                    None => serde_json::from_slice::<Response<Value>>(line)
                        .ok()
                        .and_then(|r| r.result)
                        .ok_or_else(|| "Codex sent no history in its reply".to_owned())
                        .and_then(|result| {
                            if fallback {
                                crate::recovery::codex::decode_items(&result)
                            } else {
                                crate::recovery::codex::decode(&result, turns as usize)
                            }
                        }),
                };
                out.event(session, Event::HistoryRead { ticket, result });
            }
            Pending::Threads { ticket } => {
                let listed = match error {
                    Some(error) => Err(error.message),
                    None => serde_json::from_slice::<Response<Value>>(line)
                        .ok()
                        .and_then(|r| r.result)
                        .ok_or_else(|| "it sent no list".to_owned())
                        .and_then(|result| crate::import::codex_threads(&result)),
                };
                let threads = match listed {
                    Ok(threads) => threads,
                    Err(message) => return self.listing_failed(ticket, message, out),
                };
                if threads.is_empty() {
                    let result = Ok(threads);
                    return out.event(0, Event::ThreadsListed { ticket, result });
                }
                for (index, thread) in threads.iter().enumerate() {
                    let params = json!({"threadId": thread.id, "limit": TURN_COUNT_LIMIT,
                        "sortDirection": "desc", "itemsView": "notLoaded"});
                    let line = self.request(
                        "thread/turns/list",
                        params,
                        Pending::TurnCount { ticket, index },
                    );
                    self.write(line, out);
                }
                let waiting = threads.len();
                self.listings.insert(ticket, Listing { threads, waiting });
            }
            Pending::TurnCount { ticket, index } => {
                // A thread Codex cannot count, such as on an app-server without turn
                // pagination, is listed without its count.
                let page = error
                    .is_none()
                    .then(|| serde_json::from_slice::<Response<Value>>(line).ok())
                    .flatten()
                    .and_then(|r| r.result)
                    .filter(|result| result["data"].is_array());
                let Some(listing) = self.listings.get_mut(&ticket) else {
                    return;
                };
                if let (Some(thread), Some(page)) = (listing.threads.get_mut(index), page) {
                    thread.turns = page["data"].as_array().map(Vec::len);
                    thread.more = !page["nextCursor"].is_null();
                }
                listing.waiting -= 1;
                if listing.waiting == 0 {
                    let mut threads = self
                        .listings
                        .remove(&ticket)
                        .expect("checked above")
                        .threads;
                    threads.retain(|thread| thread.turns != Some(0));
                    let result = Ok(threads);
                    out.event(0, Event::ThreadsListed { ticket, result });
                }
            }
            Pending::Ignore => {}
        }
    }

    fn thread_ready(&mut self, session: u64, thread: String, out: &mut Outputs) {
        self.threads.insert(thread.clone(), session);
        self.mcp_status.remove(&session);
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
                        state.ui_calls.clear();
                        state.ui_seen.clear();
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
                    // Codex says which limit refused the turn only in the message, so the
                    // limit carries no name or reset time.
                    if status == TurnStatus::Failed
                        && note.turn.error.as_ref().is_some_and(|e| {
                            e.codex_error_info.as_ref().and_then(Value::as_str)
                                == Some("usageLimitExceeded")
                        })
                    {
                        out.event(session, Event::UsageLimit(Default::default()));
                    }
                    let error = note.turn.error.map(|e| e.message);
                    out.event(
                        session,
                        Event::ForkPoint(ForkPoint {
                            thread: note.thread_id,
                            at: note.turn.id,
                        }),
                    );
                    out.event(
                        session,
                        Event::TurnCompleted {
                            status,
                            error,
                            stopped,
                        },
                    );
                    if self.holds.contains(&session) {
                        self.unseen_ends.insert(session);
                    }
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
                        state.ui_calls.retain(|_, waiting| *waiting != request);
                    }
                    out.event(session, Event::ApprovalResolved { request });
                }
            }
            // The account's limits, after a turn's responses. Account-wide, so it rides
            // on no session in particular.
            "account/rateLimits/updated" => {
                let Some(note) = params::<RateLimitsNote>(line) else {
                    return self.unknown += 1;
                };
                let windows = note.rate_limits.windows();
                if !windows.is_empty() {
                    out.event(0, Event::Usage(windows));
                }
            }
            "thread/tokenUsage/updated" => {
                let Some(note) = params::<TokenUsageNote>(line) else {
                    return self.unknown += 1;
                };
                if let Some(session) = self.session_for(&note.thread_id) {
                    out.event(
                        session,
                        Event::ContextUsage {
                            used: note.token_usage.last.total_tokens,
                            window: note.token_usage.model_context_window,
                        },
                    );
                }
            }
            // A thread starts each MCP server as it opens. Keystone's own server is left
            // out; every other server's latest state goes out as one list.
            "mcpServer/startupStatus/updated" => {
                let Some(note) = params::<McpStartupNote>(line) else {
                    return self.unknown += 1;
                };
                let Some(session) = note.thread_id.as_deref().and_then(|t| self.session_for(t))
                else {
                    return;
                };
                if note.name == "keystone" && self.injected.contains(&session) {
                    return;
                }
                let health = match (note.status.as_str(), note.failure_reason.as_deref()) {
                    ("starting", _) => Health::Pending,
                    ("ready", _) => Health::Connected,
                    (_, Some("reauthenticationRequired")) => Health::NeedsAuth,
                    ("failed" | "cancelled", _) => Health::Failed,
                    _ => return self.unknown += 1,
                };
                let servers = self.mcp_status.entry(session).or_default();
                servers.insert(
                    note.name.clone(),
                    ServerStatus {
                        name: note.name,
                        health,
                    },
                );
                out.event(
                    session,
                    Event::McpStatus(servers.values().cloned().collect()),
                );
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
        if method == "item/tool/call" {
            return self.on_tool_call(request, line, out);
        }
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

impl CodexDriver {
    /// A dynamic tool call. `show_ui` is answered at once unless its surface waits
    /// for a Button press; any other tool is refused.
    fn on_tool_call(&mut self, request: RequestId, line: &[u8], out: &mut Outputs) {
        let answer = |text: String, success: bool| {
            format!(
                r#"{{"id":{},"result":{}}}"#,
                request.0,
                json!({"contentItems": [{"type": "inputText", "text": text}], "success": success})
            )
        };
        let Some(call) = params::<ToolCallParams>(line) else {
            self.unknown += 1;
            return out.writes.push(answer(
                "Keystone could not read this tool call".into(),
                false,
            ));
        };
        let session = self.session_for(&call.thread_id);
        let (Some(session), true) = (session, call.tool == surface::TOOL) else {
            return out.writes.push(answer(
                format!("Keystone has no tool named {}", call.tool),
                false,
            ));
        };
        // Only the running turn's calls are shown; a late or repeated one has no turn
        // to answer it.
        let current = self.sessions.get_mut(&session).is_some_and(|state| {
            matches!(&state.turn, Turn::Running(turn)
                if call.turn_id.as_ref().is_none_or(|id| id == turn))
                && state.ui_seen.insert(call.call_id.clone())
        });
        if !current {
            return out.writes.push(answer(
                "Keystone shows widgets only for the running turn".into(),
                false,
            ));
        }
        match surface::parse(&call.call_id, &call.arguments) {
            Err(invalid) => out.writes.push(answer(invalid.to_message(), false)),
            Ok(surface) => {
                let waiting = surface.waiting();
                if waiting {
                    if let Some(state) = self.sessions.get_mut(&session) {
                        state.ui_calls.insert(call.call_id, request);
                    }
                } else {
                    out.writes
                        .push(answer(surface::shown_message(&surface), true));
                }
                out.event(session, Event::SurfaceShown { surface, waiting });
            }
        }
    }
}

fn text_input(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

/// A message's input items: its text, left out when empty beside images, then a
/// `localImage` per image, which Codex reads from its path.
fn input(text: &str, images: &[Image]) -> Value {
    let text =
        (!text.is_empty() || images.is_empty()).then(|| json!({"type": "text", "text": text}));
    let images = images
        .iter()
        .map(|image| json!({"type": "localImage", "path": image.path}));
    Value::Array(text.into_iter().chain(images).collect())
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
    #[serde(default)]
    code: Option<i64>,
    /// A turn error's kind, such as `usageLimitExceeded`: a string, or an object for
    /// the kinds that carry an HTTP status.
    #[serde(rename = "codexErrorInfo", default)]
    codex_error_info: Option<Value>,
}

#[derive(Deserialize)]
struct Params<T> {
    params: T,
}

#[derive(Deserialize)]
struct Response<T> {
    result: Option<T>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Clone, Deserialize)]
struct ThreadResult {
    thread: Named,
    #[serde(rename = "approvalPolicy")]
    approval_policy: Option<Value>,
    sandbox: Option<Value>,
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelList {
    data: Vec<WireModel>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireModel {
    model: String,
    display_name: String,
    #[serde(default)]
    hidden: bool,
}

impl ModelList {
    /// The page's visible models. Codex lists a hidden model for its own use, not for
    /// choosing.
    fn visible(&self) -> impl Iterator<Item = ModelInfo> + '_ {
        self.data.iter().filter(|m| !m.hidden).map(|m| ModelInfo {
            model: m.model.clone(),
            label: m.display_name.clone(),
            resolved: None,
        })
    }
}

impl ThreadResult {
    fn settings(self) -> Option<(Value, Value)> {
        Some((self.approval_policy?, self.sandbox?))
    }
}

#[derive(Clone, Deserialize)]
struct Named {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpStartupNote {
    thread_id: Option<String>,
    name: String,
    status: String,
    failure_reason: Option<String>,
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
struct RateLimitsNote {
    rate_limits: RateLimitSnapshot,
}

/// One limit's windows. Credits, plan and spend controls are not read.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitSnapshot {
    limit_id: Option<String>,
    limit_name: Option<String>,
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitWindow {
    /// 0-100.
    used_percent: f64,
    window_duration_mins: Option<u64>,
    /// Unix seconds.
    resets_at: Option<u64>,
}

impl RateLimitSnapshot {
    fn windows(&self) -> Vec<LimitWindow> {
        let limit = self.limit_id.as_deref().unwrap_or("codex");
        [("primary", &self.primary), ("secondary", &self.secondary)]
            .into_iter()
            .filter_map(|(slot, window)| {
                let window = window.as_ref()?;
                let duration = match window.window_duration_mins {
                    Some(300) => "5-hour".to_owned(),
                    Some(10_080) => "Weekly".to_owned(),
                    Some(minutes) if minutes % 1_440 == 0 => format!("{}-day", minutes / 1_440),
                    Some(minutes) if minutes % 60 == 0 => format!("{}-hour", minutes / 60),
                    Some(minutes) => format!("{minutes}-minute"),
                    None if slot == "primary" => "Primary".to_owned(),
                    None => "Secondary".to_owned(),
                };
                let label = match (&self.limit_name, limit) {
                    (_, "codex") => duration,
                    (Some(name), _) => format!("{name} {duration}"),
                    (None, id) => format!("{id} {duration}"),
                };
                Some(LimitWindow {
                    id: format!("{limit}/{slot}"),
                    label,
                    used_percent: window.used_percent,
                    resets_at: window.resets_at,
                })
            })
            .collect()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsageNote {
    thread_id: String,
    token_usage: ThreadTokenUsage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadTokenUsage {
    last: TokenBreakdown,
    model_context_window: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenBreakdown {
    total_tokens: u64,
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
struct ToolCallParams {
    thread_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    call_id: String,
    tool: String,
    #[serde(default)]
    arguments: Value,
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
pub(crate) struct WireItem {
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
    content_items: Option<Vec<Value>>,
}

#[derive(Deserialize, Default)]
struct Change {
    #[serde(default)]
    path: String,
}

impl WireItem {
    /// `started` items are in progress even when, like agent messages, they carry no
    /// status of their own.
    pub(crate) fn into_item(self, started: bool) -> Item {
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
            "dynamicToolCall" => (
                ItemKind::Tool {
                    name: self.tool.unwrap_or_default(),
                },
                cap_output(&join_text(&self.content_items.unwrap_or_default())),
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

#[cfg(test)]
#[path = "codex_mcp_tests.rs"]
mod mcp_tests;
