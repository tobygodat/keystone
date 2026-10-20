//! A scripted stand-in for `claude -p` in stream-json mode, for tests and UI
//! verification.
//!
//! It speaks the subset of Claude Code's stream-json and control protocol Keystone
//! uses and replies with labeled test text. A prompt containing "run" asks approval
//! for a command first. A message with priority "now" aborts the reply, as Claude does.
//! Set `KEYSTONE_FAKE_CLAUDE_DELAY_MS` to change the pause between streamed words,
//! and `KEYSTONE_FAKE_CLAUDE_SIGNED_OUT=1` to answer like a Claude with no login.
//! `KEYSTONE_FAKE_CLAUDE_REPLY` replaces the reply text exactly.
//! `KEYSTONE_FAKE_CLAUDE_NO_LIFECYCLE=1` acts like a Claude older than
//! `msg_lifecycle_v1`: no `command_lifecycle` frames and no such capability.
//! `--resume missing` fails like a conversation Claude no longer has.
//! In that mode only, `KEYSTONE_FAKE_CLAUDE_RELEASE_FILE` naming a path holds each
//! reply after its first streamed word until that file exists, so a test can act
//! mid-reply without a timing margin. A hold longer than [`RELEASE_LIMIT`] ends the
//! turn as an interruption. With lifecycle frames the setting is ignored: the hold
//! does not watch for steers, which only a lifecycle-capable Claude receives.
//!
//! Permission modes work like Claude Code 2.1.300's: `--permission-mode` at launch,
//! else `KEYSTONE_FAKE_CLAUDE_USER_MODE` (the user's own setting, `default` when unset),
//! and `set_permission_mode`, which refuses `bypassPermissions` unless the process was
//! launched in it. Only plan mode is kept across a `--resume` with no mode given (in
//! `.fake-claude-plan-<session>`, in `KEYSTONE_FAKE_CLAUDE_LOG_DIR` when set, else the
//! working directory). The `initialize` answer
//! and `init` report the mode, and each reply names the mode it ran under.
//! `KEYSTONE_FAKE_CLAUDE_HOLD_MODE_MS` delays answers to `set_permission_mode`. That
//! and the user's mode can also be set for one working directory, in its
//! `.fake-claude-hold-mode-ms` and `.fake-claude-user-mode` files. A
//! `.fake-claude-ignore-initialize` file holding `all` leaves every `initialize`
//! unanswered, and `probe` only Keystone's settings probe (`keystone-probe`).
//!
//! A prompt containing "exit plan" asks to approve `ExitPlanMode` first, as Claude does
//! in plan mode. Approved, the mode returns to the one before plan mode, as Claude
//! 2.1.300 does; denied, the reply says so.
//!
//! The `initialize` answer lists scripted models (`default`, `fake-opus`, `fake-haiku`).
//! `--model <value>` chooses one; `init` reports the resolved model (`fake-opus` when
//! none is given), and each reply ends with it.
//!
//! An answer applies only to the approval it names, and an approval Claude withdrew
//! takes no answer. Request IDs restart in each process, as a stale answer from an
//! earlier process would find them. With `KEYSTONE_FAKE_CLAUDE_LOG_DIR` set, each
//! answer received is logged to `<dir>/<pid>.answers` as `applied <id>` or
//! `ignored <id>`. When its directory holds a `.fake-claude-pids` file, each process
//! appends its ID to it, so a test can find a process it never saw start. A
//! `.fake-claude-argv` file, or the file `KEYSTONE_FAKE_CLAUDE_ARGV_LOG` names, gets
//! each process's arguments as a JSON array per line, and a `.fake-claude-messages`
//! file each user message it reads.
//!
//! `KEYSTONE_FAKE_ENV_RECEIPT` names a file each process appends its environment to.
//!
//! `init` lists tools unless launched with `--tools ""`, and no MCP servers, slash
//! commands or skills. `init-tools` and `init-commands` settings replace the JSON of
//! `init`'s `tools` and the `initialize` answer's `commands`; `missing` leaves the
//! field out. A `hook` setting of `1` reports a `SessionStart` hook before reading
//! anything, and a `reject-flag` setting names a flag to exit on at launch, as an
//! older Claude would.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[path = "support/orchestrator.rs"]
mod orchestrator;

type Out = Arc<Mutex<io::Stdout>>;

/// The longest a reply waits for its release file.
const RELEASE_LIMIT: Duration = Duration::from_secs(10);

const VERSION: &str = "2.1.300";

fn emit(out: &Out, message: Value) {
    let mut out = out.lock().unwrap();
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

#[derive(Clone, Copy, PartialEq)]
enum Abort {
    Steer,
    Stop,
}

struct Queued {
    uuid: String,
    text: String,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Queued>,
    /// Whether a message is being answered now.
    busy: bool,
    abort: Option<Abort>,
    /// The approval waiting for an answer: its request ID and where the answer goes.
    approval: Option<(String, Sender<bool>)>,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    mcp: Mutex<Option<orchestrator::Server>>,
    /// The permission mode, and whether the process may enter `bypassPermissions`.
    mode: Mutex<String>,
    /// The mode approving `ExitPlanMode` returns to.
    before_plan: Mutex<String>,
    /// Marks the conversation as in plan mode, for a later `--resume`.
    plan_file: PathBuf,
    bypass: bool,
    wake: Condvar,
    /// The resolved model the process runs on.
    model: String,
    /// Whether to send `command_lifecycle` frames.
    lifecycle: bool,
    /// Where a reply pauses after its first word until this file exists. Only set
    /// without lifecycle frames.
    release_file: Option<PathBuf>,
}

/// With `KEYSTONE_FAKE_ENV_RECEIPT` naming a file, appends one JSON line per process:
/// its ID, its arguments, and the account environment it was started with. Credential
/// variables are listed by name only when present, never with their values.
fn write_env_receipt(provider: &str) {
    let Some(path) = std::env::var_os("KEYSTONE_FAKE_ENV_RECEIPT") else {
        return;
    };
    let var = |name: &str| std::env::var(name).ok();
    let credentials: Vec<&str> = [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ]
    .into_iter()
    .filter(|name| std::env::var_os(name).is_some())
    .collect();
    let line = json!({
        "provider": provider,
        "pid": std::process::id(),
        "argv": std::env::args().skip(1).collect::<Vec<_>>(),
        "CLAUDE_CONFIG_DIR": var("CLAUDE_CONFIG_DIR"),
        "CODEX_HOME": var("CODEX_HOME"),
        "HOME": var("HOME"),
        "credentials": credentials,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

fn main() {
    write_env_receipt("claude");
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--version") {
        let version =
            std::env::var("KEYSTONE_FAKE_CLAUDE_VERSION").unwrap_or_else(|_| VERSION.into());
        println!("{version} (Claude Code)");
        return;
    }
    let resume = args
        .iter()
        .position(|a| a == "--resume")
        .and_then(|i| args.get(i + 1).cloned());
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    if resume.as_deref() == Some("missing") {
        emit(
            &out,
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
                   "errors": ["No conversation found with session ID: missing"]}),
        );
        eprintln!("No conversation found with session ID: missing");
        std::process::exit(1);
    }
    let session = resume.unwrap_or_else(|| format!("fake-claude-session-{}", std::process::id()));
    let delay = Duration::from_millis(
        std::env::var("KEYSTONE_FAKE_CLAUDE_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    );
    let signed_out = std::env::var("KEYSTONE_FAKE_CLAUDE_SIGNED_OUT").is_ok_and(|v| v == "1");
    let frames = !std::env::var("KEYSTONE_FAKE_CLAUDE_NO_LIFECYCLE").is_ok_and(|v| v == "1");
    let log = std::env::var_os("KEYSTONE_FAKE_CLAUDE_LOG_DIR")
        .map(|dir| PathBuf::from(dir).join(format!("{}.answers", std::process::id())));
    // A file left by an earlier process with the same ID is not this one's.
    if let Some(log) = &log {
        let _ = std::fs::write(log, "");
    }
    if let Ok(mut pids) = std::fs::OpenOptions::new()
        .append(true)
        .open(".fake-claude-pids")
    {
        let _ = writeln!(pids, "{}", std::process::id());
    }
    let argv_log = std::env::var_os("KEYSTONE_FAKE_CLAUDE_ARGV_LOG")
        .map_or_else(|| PathBuf::from(".fake-claude-argv"), PathBuf::from);
    let open = std::fs::OpenOptions::new().append(true).open(&argv_log);
    if let Ok(mut argv) = open {
        let _ = writeln!(argv, "{}", json!(args[1..]));
    }
    let launch_mode = args
        .iter()
        .position(|a| a == "--permission-mode")
        .and_then(|i| args.get(i + 1).cloned());
    let model = args
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| args.get(i + 1))
        .map_or("fake-opus", |value| resolve_model(value))
        .to_owned();
    // Settings from a file in the working directory apply to that directory alone.
    let setting = |name: &str| {
        std::fs::read_to_string(format!(".fake-claude-{name}"))
            .ok()
            .map(|v| v.trim().to_owned())
            .or_else(|| {
                std::env::var(format!(
                    "KEYSTONE_FAKE_CLAUDE_{}",
                    name.to_uppercase().replace('-', "_")
                ))
                .ok()
            })
    };
    let user_mode = setting("user-mode").unwrap_or_else(|| "default".into());
    let plan_file = std::env::var_os("KEYSTONE_FAKE_CLAUDE_LOG_DIR")
        .map_or_else(PathBuf::new, PathBuf::from)
        .join(format!(".fake-claude-plan-{session}"));
    let mode = launch_mode.clone().unwrap_or_else(|| {
        if plan_file.exists() {
            "plan".into()
        } else {
            user_mode.clone()
        }
    });
    let hold_mode = Duration::from_millis(
        setting("hold-mode-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    );
    let ignore_initialize = setting("ignore-initialize");
    if let Some(flag) = setting("reject-flag")
        && args.contains(&flag)
    {
        eprintln!("error: unknown option '{flag}'");
        std::process::exit(1);
    }
    let field = |name: &str, default: Value| match setting(name).as_deref() {
        Some("missing") => None,
        Some(json) => Some(serde_json::from_str(json).expect("valid JSON setting")),
        None => Some(default),
    };
    let no_tools = args
        .windows(2)
        .any(|w| w[0] == "--tools" && w[1].is_empty());
    let tools = field(
        "init-tools",
        if no_tools {
            json!([])
        } else {
            json!(["Bash", "Read", "Edit"])
        },
    );
    let commands = field("init-commands", json!([]));
    if setting("hook").as_deref() == Some("1") {
        emit(
            &out,
            json!({"type": "system", "subtype": "hook_started", "hook_id": "fake-hook",
                   "hook_name": "SessionStart:startup", "hook_event": "SessionStart",
                   "session_id": session}),
        );
    }
    let shared = Arc::new(Shared {
        state: Mutex::default(),
        mcp: Mutex::default(),
        bypass: launch_mode.as_deref() == Some("bypassPermissions"),
        before_plan: Mutex::new(if mode == "plan" {
            user_mode.clone()
        } else {
            mode.clone()
        }),
        plan_file,
        mode: Mutex::new(String::new()),
        wake: Condvar::new(),
        model,
        lifecycle: frames,
        release_file: if frames {
            None
        } else {
            std::env::var_os("KEYSTONE_FAKE_CLAUDE_RELEASE_FILE").map(PathBuf::from)
        },
    });
    set_mode(&shared, &mode);
    let skip_init = setting("skip-init").as_deref() == Some("1");
    let worker = {
        let (out, shared, session) = (out.clone(), shared.clone(), session.clone());
        thread::spawn(move || work(&out, &shared, &session, delay, signed_out, tools, skip_init))
    };

    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match message["type"].as_str() {
            Some("control_request") => {
                let id = message["request_id"].clone();
                let reply = |response: Value| {
                    emit(
                        &out,
                        json!({"type": "control_response", "response":
                            {"subtype": "success", "request_id": id, "response": response}}),
                    )
                };
                match message["request"]["subtype"].as_str() {
                    Some("initialize")
                        if ignore_initialize.as_deref() == Some("all")
                            || (ignore_initialize.as_deref() == Some("probe")
                                && id == "keystone-probe") => {}
                    Some("initialize") => {
                        if let Some(frames) = setting("initialize-frames") {
                            for frame in serde_json::from_str::<Vec<Value>>(&frames).unwrap() {
                                emit(&out, frame);
                            }
                            continue;
                        }
                        let mut answer = json!({"claude_code_version": VERSION,
                            "current_permission_mode": *shared.mode.lock().unwrap(),
                            "models": MODELS.iter().map(|(value, name, description, resolved)| json!({
                                "value": value, "displayName": name, "description": description,
                                "resolvedModel": resolved})).collect::<Vec<_>>()});
                        if let Some(commands) = &commands {
                            answer["commands"] = commands.clone();
                        }
                        reply(answer)
                    }
                    Some("mcp_set_servers") => {
                        let config = &message["request"]["servers"]["keystone"];
                        let server = orchestrator::Server::from_config(config, "headers");
                        let accepted = config["type"] == "http" && server.is_some();
                        *shared.mcp.lock().unwrap() = if accepted { server } else { None };
                        reply(if accepted {
                            json!({"added":["keystone"],"removed":[],"errors":{}})
                        } else {
                            json!({"added":[],"removed":[],"errors":{"keystone":"Invalid loopback MCP configuration"}})
                        });
                    }
                    Some("set_permission_mode") => {
                        thread::sleep(hold_mode);
                        let mode = message["request"]["mode"].as_str().unwrap_or_default();
                        let mode = if mode == "manual" { "default" } else { mode };
                        if mode == "bypassPermissions" && !shared.bypass {
                            emit(
                                &out,
                                json!({"type": "control_response", "response":
                                    {"subtype": "error", "request_id": id, "error_code": "bypass_not_launched",
                                     "error": "Cannot set permission mode to bypassPermissions because the session was not launched with --dangerously-skip-permissions"}}),
                            );
                        } else {
                            set_mode(&shared, mode);
                            reply(json!({"mode": mode}));
                            emit(
                                &out,
                                json!({"type": "system", "subtype": "status", "status": null,
                                       "permissionMode": mode, "session_id": session}),
                            );
                        }
                    }
                    Some("interrupt") => {
                        if let Some(path) = std::env::var_os("KEYSTONE_FAKE_CLAUDE_INTERRUPT_LOG") {
                            let _ = std::fs::write(path, message["request"].to_string());
                        }
                        let mut state = shared.state.lock().unwrap();
                        let cancelled: Vec<String> =
                            state.queue.drain(..).map(|q| q.uuid).collect();
                        for uuid in &cancelled {
                            lifecycle(&out, &shared, &session, uuid, "cancelled");
                        }
                        if state.busy {
                            state.abort = Some(Abort::Stop);
                        }
                        drop(state);
                        reply(json!({"still_queued": [], "cancelled": cancelled}));
                    }
                    _ => emit(
                        &out,
                        json!({"type": "control_response", "response":
                            {"subtype": "error", "request_id": id, "error": "not scripted"}}),
                    ),
                }
            }
            Some("control_response") => {
                let id = message["response"]["request_id"]
                    .as_str()
                    .unwrap_or_default();
                let allow = message["response"]["response"]["behavior"] == "allow";
                let mut state = shared.state.lock().unwrap();
                let applied = state
                    .approval
                    .as_ref()
                    .is_some_and(|(pending, _)| pending == id);
                if applied && let Some((_, tx)) = state.approval.take() {
                    let _ = tx.send(allow);
                }
                drop(state);
                if let Some(log) = &log {
                    let line = format!("{} {id}\n", if applied { "applied" } else { "ignored" });
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(log)
                        .and_then(|mut file| file.write_all(line.as_bytes()));
                }
            }
            Some("user") => {
                let uuid = message["uuid"].as_str().unwrap_or_default().to_owned();
                let text = message["message"]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if let Ok(mut messages) = std::fs::OpenOptions::new()
                    .append(true)
                    .open(".fake-claude-messages")
                {
                    let _ = writeln!(messages, "{}", json!(text));
                }
                if let Some(frames) = setting("turn-frames") {
                    for frame in serde_json::from_str::<Vec<Value>>(&frames).unwrap() {
                        emit(&out, frame);
                    }
                    continue;
                }
                lifecycle(&out, &shared, &session, &uuid, "queued");
                let mut state = shared.state.lock().unwrap();
                if state.busy && message["priority"] == "now" {
                    state.abort = Some(Abort::Steer);
                }
                state.queue.push_back(Queued { uuid, text });
                shared.wake.notify_all();
            }
            _ => {}
        }
    }
    shared.state.lock().unwrap().closed = true;
    shared.wake.notify_all();
    let _ = worker.join();
}

/// The scripted catalog: `value`, display name, description, and what it resolves to.
const MODELS: [(&str, &str, &str, &str); 3] = [
    (
        "default",
        "Default (recommended)",
        "Scripted default",
        "fake-opus",
    ),
    ("fake-opus", "Fake Opus", "Scripted model", "fake-opus"),
    (
        "fake-haiku",
        "Fake Haiku",
        "Scripted small model",
        "fake-haiku",
    ),
];

/// What a `--model` value runs as. An unknown value runs as itself, as Claude would
/// try to.
fn resolve_model(value: &str) -> &str {
    MODELS
        .iter()
        .find(|(v, ..)| *v == value)
        .map_or(value, |(.., resolved)| resolved)
}

/// Changes the permission mode, remembering plan mode for a later `--resume`.
fn set_mode(shared: &Shared, mode: &str) {
    let mut current = shared.mode.lock().unwrap();
    if mode == "plan" && *current != "plan" && !current.is_empty() {
        *shared.before_plan.lock().unwrap() = current.clone();
    }
    *current = mode.to_owned();
    if mode == "plan" {
        let _ = std::fs::write(&shared.plan_file, "");
    } else {
        let _ = std::fs::remove_file(&shared.plan_file);
    }
}

fn lifecycle(out: &Out, shared: &Shared, session: &str, uuid: &str, state: &str) {
    if !shared.lifecycle {
        return;
    }
    emit(
        out,
        json!({"type": "command_lifecycle", "command_uuid": uuid, "state": state, "session_id": session}),
    );
}

fn stream(out: &Out, event: Value) {
    emit(
        out,
        json!({"type": "stream_event", "event": event, "parent_tool_use_id": null}),
    );
}

fn work(
    out: &Out,
    shared: &Shared,
    session: &str,
    delay: Duration,
    signed_out: bool,
    tools: Option<Value>,
    skip_init: bool,
) {
    let mut turn = 0;
    loop {
        let next = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if let Some(next) = state.queue.pop_front() {
                    state.busy = true;
                    state.abort = None;
                    break next;
                }
                if state.closed {
                    return;
                }
                state = shared.wake.wait(state).unwrap();
            }
        };
        turn += 1;
        lifecycle(out, shared, session, &next.uuid, "started");
        let capabilities: &[&str] = if shared.lifecycle
            && std::env::var("KEYSTONE_FAKE_CLAUDE_NO_CANCEL_QUEUED")
                .is_ok_and(|value| value == "1")
        {
            &["msg_lifecycle_v1"]
        } else if shared.lifecycle {
            &["msg_lifecycle_v1", "interrupt_cancel_queued_v1"]
        } else {
            &[]
        };
        let mode = shared.mode.lock().unwrap().clone();
        let mut init = json!({"type": "system", "subtype": "init", "session_id": session,
               "capabilities": capabilities, "permissionMode": mode, "mcp_servers": [],
               "slash_commands": [], "skills": [], "claude_code_version": VERSION,
               "model": shared.model});
        if let Some(tools) = &tools {
            init["tools"] = tools.clone();
        }
        if !skip_init {
            emit(out, init);
        }
        if std::env::var("KEYSTONE_FAKE_CLAUDE_SUBAGENT_INIT").is_ok_and(|value| value == "1") {
            emit(
                out,
                json!({"type":"system", "subtype":"init", "parent_tool_use_id":"scripted-subagent", "capabilities":[]}),
            );
        }
        emit(
            out,
            json!({"type": "user", "message": {"role": "user", "content": next.text},
                   "parent_tool_use_id": null, "uuid": next.uuid, "isReplay": true}),
        );
        let ended = if signed_out {
            emit(
                out,
                json!({"type": "assistant", "message": {"id": format!("fake-{turn}"), "model": "<synthetic>",
                       "content": [{"type": "text", "text": "Not logged in · Please run /login"}]},
                       "parent_tool_use_id": null, "error": "authentication_failed", "is_api_error_message": true}),
            );
            emit(
                out,
                json!({"type": "result", "subtype": "success", "is_error": true,
                       "result": "Not logged in · Please run /login"}),
            );
            "cancelled"
        } else {
            answer(out, shared, turn, &next.uuid, &next.text, &mode, delay)
        };
        lifecycle(out, shared, session, &next.uuid, ended);
        shared.state.lock().unwrap().busy = false;
    }
}

/// Runs one message's turn and returns its lifecycle end state.
#[allow(clippy::too_many_arguments)]
fn answer(
    out: &Out,
    shared: &Shared,
    turn: u64,
    request: &str,
    text: &str,
    mode: &str,
    delay: Duration,
) -> &'static str {
    let mut mode = mode.to_owned();
    let mut left_plan = "";
    if text.contains("exit plan") && mode == "plan" {
        let (tx, rx): (Sender<bool>, Receiver<bool>) = mpsc::channel();
        let request = format!("fake-exit-plan-{turn}");
        shared.state.lock().unwrap().approval = Some((request.clone(), tx));
        emit(
            out,
            json!({"type": "control_request", "request_id": request, "request":
               {"subtype": "can_use_tool", "tool_name": "ExitPlanMode", "display_name": "ExitPlanMode",
                "input": {"plan": "Write the file."}, "tool_use_id": format!("fake-exit-toolu-{turn}")}}),
        );
        let allow = loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(allow) => break allow,
                Err(RecvTimeoutError::Disconnected) => break false,
                Err(RecvTimeoutError::Timeout) => {
                    if shared.state.lock().unwrap().abort == Some(Abort::Stop) {
                        shared.state.lock().unwrap().approval = None;
                        return stopped(out);
                    }
                }
            }
        };
        if allow {
            let before = shared.before_plan.lock().unwrap().clone();
            set_mode(shared, &before);
            mode = before;
            emit(
                out,
                json!({"type": "system", "subtype": "status", "status": null, "permissionMode": mode}),
            );
            left_plan = " Left plan mode.";
        } else {
            left_plan = " Stayed in plan mode.";
        }
    }
    let mode = mode.as_str();
    let model = &shared.model;
    let server = shared.mcp.lock().unwrap().clone();
    let request = format!("{}-{turn}-{request}", std::process::id());
    let scripted = orchestrator::reply(text, server.as_ref(), &request);
    let mut reply = scripted.or_else(|| std::env::var("KEYSTONE_FAKE_CLAUDE_REPLY").ok()).unwrap_or_else(|| {
        format!(
            "Claude test reply (test data): You wrote: {text} **Mode:** `{mode}`{left_plan} **Model:** `{model}`"
        )
    });
    if text.contains("long") {
        for n in 1..=200 {
            reply.push_str(&format!(" word{n}"));
        }
    }
    if text.contains("run") {
        let message = format!("fake-tool-{turn}");
        let tool = format!("fake-toolu-{turn}");
        let input = json!({"command": "echo scripted", "description": "Print a scripted line"});
        stream(
            out,
            json!({"type": "message_start", "message": {"id": message}}),
        );
        stream(
            out,
            json!({"type": "content_block_start", "index": 0, "content_block":
                   {"type": "tool_use", "id": tool, "name": "Bash", "input": {}}}),
        );
        emit(
            out,
            json!({"type": "assistant", "message": {"id": message, "content":
                   [{"type": "tool_use", "id": tool, "name": "Bash", "input": input}]}, "parent_tool_use_id": null}),
        );
        let (tx, rx): (Sender<bool>, Receiver<bool>) = mpsc::channel();
        let request = format!("fake-request-{turn}");
        // `bypassPermissions` runs it without asking.
        if mode == "bypassPermissions" {
            let _ = tx.send(true);
        } else {
            shared.state.lock().unwrap().approval = Some((request.clone(), tx));
        }
        if mode != "bypassPermissions" {
            emit(
                out,
                json!({"type": "control_request", "request_id": request, "request":
                   {"subtype": "can_use_tool", "tool_name": "Bash", "display_name": "Bash", "input": input,
                    "description": "Print a scripted line", "tool_use_id": tool}}),
            );
        }
        let allow = loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(allow) => break Some(allow),
                Err(RecvTimeoutError::Disconnected) => break None,
                Err(RecvTimeoutError::Timeout) => {
                    if shared.state.lock().unwrap().abort == Some(Abort::Stop) {
                        break None;
                    }
                }
            }
        };
        let Some(allow) = allow else {
            // Withdrawn: a late answer must not apply to it or to a later request.
            shared.state.lock().unwrap().approval = None;
            emit(
                out,
                json!({"type": "control_cancel_request", "request_id": request}),
            );
            return stopped(out);
        };
        let (content, is_error) = if allow {
            ("scripted", false)
        } else {
            ("The user denied this in Keystone.", true)
        };
        emit(
            out,
            json!({"type": "user", "message": {"role": "user", "content":
                   [{"type": "tool_result", "tool_use_id": tool, "content": content, "is_error": is_error}]},
                   "parent_tool_use_id": null}),
        );
    }
    let message = format!("fake-message-{turn}");
    stream(
        out,
        json!({"type": "message_start", "message": {"id": message}}),
    );
    stream(
        out,
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    );
    let mut sent = String::new();
    for (i, word) in reply.split(' ').enumerate() {
        let abort = shared.state.lock().unwrap().abort;
        if let Some(abort) = abort {
            return aborted(out, &message, &sent, abort);
        }
        let piece = if i == 0 {
            word.to_owned()
        } else {
            format!(" {word}")
        };
        sent.push_str(&piece);
        stream(
            out,
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": piece}}),
        );
        if i == 0 && !released(shared) {
            emit(
                out,
                json!({"type": "assistant", "message": {"id": message, "content":
                       [{"type": "text", "text": sent}]}, "parent_tool_use_id": null}),
            );
            return stopped(out);
        }
        thread::sleep(delay);
    }
    if orchestrator::is_child_message(text) {
        orchestrator::hold_child(|| {
            let state = shared.state.lock().unwrap();
            state.abort.is_some() || state.closed
        });
        if let Some(abort) = shared.state.lock().unwrap().abort {
            return aborted(out, &message, &sent, abort);
        }
    }
    emit(
        out,
        json!({"type": "assistant", "message": {"id": message, "content":
               [{"type": "text", "text": sent}]}, "parent_tool_use_id": null}),
    );
    stream(out, json!({"type": "content_block_stop", "index": 0}));
    emit(
        out,
        json!({"type": "result", "subtype": "success", "is_error": false, "result": sent}),
    );
    "completed"
}

fn aborted(out: &Out, message: &str, sent: &str, abort: Abort) -> &'static str {
    emit(
        out,
        json!({"type": "assistant", "message": {"id": message, "content":
               [{"type": "text", "text": sent}]}, "parent_tool_use_id": null}),
    );
    match abort {
        Abort::Stop => stopped(out),
        Abort::Steer => {
            emit(
                out,
                json!({"type": "result", "subtype": "success", "is_error": false,
                       "terminal_reason": "aborted_streaming", "result": sent}),
            );
            "cancelled"
        }
    }
}

/// Waits for the release file, if one is set. False when the wait ran out, the
/// turn was stopped, or stdin closed, so the reply must not finish as if released.
fn released(shared: &Shared) -> bool {
    let Some(file) = &shared.release_file else {
        return true;
    };
    let deadline = Instant::now() + RELEASE_LIMIT;
    loop {
        if file.exists() {
            return true;
        }
        let state = shared.state.lock().unwrap();
        if state.abort == Some(Abort::Stop) || state.closed {
            return false;
        }
        drop(state);
        if Instant::now() >= deadline {
            eprintln!("release file {} never appeared", file.display());
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn stopped(out: &Out) -> &'static str {
    emit(
        out,
        json!({"type": "user", "message": {"role": "user", "content":
               [{"type": "text", "text": "[Request interrupted by user]"}]}, "parent_tool_use_id": null}),
    );
    emit(
        out,
        json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
               "errors": ["interrupted"]}),
    );
    "cancelled"
}
