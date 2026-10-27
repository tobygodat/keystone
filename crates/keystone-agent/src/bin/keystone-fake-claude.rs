//! A scripted stand-in for `claude -p` in stream-json mode, for tests and UI
//! verification.
//!
//! It speaks the subset of Claude Code's stream-json and control protocol Keystone
//! uses and replies with labeled test text. A prompt containing "run" asks approval
//! for a command first. A message with priority "now" aborts the reply, as Claude does.
//! Set `KEYSTONE_FAKE_CLAUDE_DELAY_MS` (or a working directory's `.fake-claude-delay-ms`)
//! to change the pause between streamed words,
//! and `KEYSTONE_FAKE_CLAUDE_SIGNED_OUT=1` to answer like a Claude with no login.
//! `KEYSTONE_FAKE_CLAUDE_REPLY` replaces the reply text exactly.
//! `KEYSTONE_FAKE_CLAUDE_WRITE` names a path, relative to the working directory, that
//! each turn reports a completed Write of before its reply. Nothing is written.
//! `KEYSTONE_FAKE_CLAUDE_NO_LIFECYCLE=1` acts like a Claude older than
//! `msg_lifecycle_v1`: no `command_lifecycle` frames and no such capability.
//! `--resume missing` fails like a conversation Claude no longer has.
//! `--resume <session> --fork-session` continues that conversation under a new session
//! ID, and `--resume-session-at <uuid>` keeps it only through that record. In store
//! mode the fork's file starts with a copy of the source's records, written with the
//! first message as Claude Code 2.1.301 does, and an unknown session or record fails
//! as it does there. Each assistant and user frame carries the UUID its record has.
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
//! A user message's content may be a list of text and base64 `image` blocks; with
//! `KEYSTONE_FAKE_IMAGE_RECEIPT` set, each image's media type and byte hash is appended
//! there (see `support/images.rs`), and the reply names them.
//!
//! An account whose config home holds a `.fake-usage-limit` file is at its usage limit
//! (see `support/limit.rs`): each turn fails on it, in Claude Code 2.1.301's frames.
//!
//! A prompt containing "usage NN" gives its reply's last assistant message token usage
//! (NN/2 % of a 200,000-token context) and, before its `result`, a synthetic, labelled
//! `rate_limit_event` with the 5-hour window at NN%. `get_usage` is not scripted.
//!
//! `init` lists tools unless launched with `--tools ""`, the MCP servers the fixture
//! config turns on (see [`mcp_servers`]), and no slash commands or skills. The `initialize` answer's `commands` lists the skills and
//! commands under `$CLAUDE_CONFIG_DIR` and the working directory's `.claude`, as Claude
//! Code 2.1.301 does, then two built-ins; none with `--disable-slash-commands`.
//! `init-tools` and `init-commands` settings replace the JSON of `init`'s `tools` and
//! the `initialize` answer's `commands`; `missing` leaves the field out. A `hook` setting of `1` reports a `SessionStart` hook before reading
//! anything, and a `reject-flag` setting names a flag to exit on at launch, as an
//! older Claude would.
//!
//! With `KEYSTONE_FAKE_CLAUDE_STORE=1` the session is recorded as Claude Code stores
//! it: JSON Lines in `<CLAUDE_CONFIG_DIR or ~/.claude>/projects/<cwd, non-alphanumerics
//! as ->/<session>.jsonl`, one record per user message, whole content block and tool
//! result, chained by `parentUuid`. Message and tool IDs include the process ID, so a
//! resumed session never reuses one. A prompt containing "crash" records its whole
//! reply after the first streamed word, then exits, as a Claude that dies mid-stream
//! after saving.
//!
//! An `idle` setting of `1` reports what idle eviction needs, as the published Agent
//! SDK types describe it: `session_state_changed` (`running` as a turn starts, `idle`
//! once nothing is queued), and `background_tasks_changed` whenever its task set
//! changes. Every `initialize` answer then carries `session_state` and the pending
//! approval and dialog lists, and a repeated `initialize` is followed by a snapshot
//! of the task set. A `quiesce` setting of `missing` leaves those fields out,
//! `running` reports the session running, and `no-snapshot` sends no snapshot. While
//! `.fake-claude-hold-snapshot` exists, the next such answer consumes it, creates
//! `.fake-claude-holding-snapshot`, and sends its snapshot only once
//! `.fake-claude-release-snapshot` exists, removing both, so a test can act between
//! the two. Also in this mode, the lines of a
//! `.fake-claude-emit` file are written out as they are, `$SESSION` replaced by the
//! session ID, as soon as the file appears, and the file is removed. In this
//! mode a prompt containing "background task" or "ambient task" starts a task that a
//! foreground Bash call hands to the background and that runs until
//! `.fake-claude-release-background` (or `-ambient`) exists in the working directory;
//! "hook" starts a hook that outlives the turn until `.fake-claude-release-hook`
//! exists; and "hold tool" keeps a Bash call running until `.fake-claude-release-tool`
//! exists. A `child` setting of `1` starts a `sleep` child at launch and appends its
//! ID to `.fake-claude-children`, so a test can check the whole tree ends.
//!
//! `mcp_set_servers` takes Keystone's `keystone` HTTP server and its `keystone_ui` SDK
//! server, as Claude Code 2.1.301 does. For the SDK server it then sends `initialize`,
//! `notifications/initialized` and `tools/list` as `mcp_message` control requests, and
//! a prompt that asks for a widget (see `support/widgets.rs`) calls `show_ui`: it asks
//! permission with `can_use_tool`, sends `tools/call` with the tool use ID in `_meta`,
//! waits for the answer, and replies with what each call returned.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[path = "support/claude_config.rs"]
mod config_cli;
#[path = "support/images.rs"]
mod images;
#[path = "support/orchestrator.rs"]
mod orchestrator;
#[path = "support/limit.rs"]
mod support_limit;
#[path = "support/widgets.rs"]
mod widgets;

type Out = Arc<Mutex<io::Stdout>>;

/// The session's store file and the last record written, when recording.
struct Store {
    path: PathBuf,
    session: String,
    cwd: String,
    last: Option<String>,
    next: u64,
    /// A fork's copy of its source's records, written before its first record.
    copied: Vec<String>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

/// Where `session` is stored in the working directory's project, in store mode.
fn store_path(session: &str) -> Option<(PathBuf, String)> {
    if !std::env::var("KEYSTONE_FAKE_CLAUDE_STORE").is_ok_and(|v| v == "1") {
        return None;
    }
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))?;
    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let folder: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = root
        .join("projects")
        .join(folder)
        .join(format!("{session}.jsonl"));
    Some((path, cwd))
}

/// The records a fork of `source` starts with, through `at` when given, or why Claude
/// Code would refuse the fork. Nothing to check outside store mode.
fn fork_records(source: &str, at: Option<&str>, session: &str) -> Result<Vec<String>, String> {
    let Some((path, _)) = store_path(source) else {
        return Ok(Vec::new());
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Err(format!("No conversation found with session ID: {source}"));
    };
    let mut records = Vec::new();
    for line in text.lines() {
        let Ok(mut record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let last = at.is_some() && record["uuid"].as_str() == at;
        record["sessionId"] = json!(session);
        records.push(record.to_string());
        if last {
            return Ok(records);
        }
    }
    match at {
        Some(at) => Err(format!("No message found with message.uuid of: {at}")),
        None => Ok(records),
    }
}

fn open_store(session: &str, copied: Vec<String>) {
    let Some((path, cwd)) = store_path(session) else {
        return;
    };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    // A resumed session continues the chain where its file ends, and a fork where its
    // copy does.
    let last = std::fs::read_to_string(&path)
        .ok()
        .into_iter()
        .chain(copied.last().cloned())
        .last()
        .and_then(|text| {
            text.lines().rev().find_map(|line| {
                serde_json::from_str::<Value>(line).ok()?["uuid"]
                    .as_str()
                    .map(str::to_owned)
            })
        });
    *STORE.lock().unwrap() = Some(Store {
        path,
        session: session.to_owned(),
        cwd,
        last,
        next: 0,
        copied,
    });
}

static RECORDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Appends a record of `kind` ("user" or "assistant") with `message`, under `uuid`
/// when given, and returns the record's UUID, which its frame carries. Outside store
/// mode nothing is written.
fn store(kind: &str, message: Value, uuid: Option<&str>) -> String {
    let next = RECORDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let uuid = uuid.filter(|uuid| !uuid.is_empty()).map_or_else(
        || format!("fake-record-{}-{next}", std::process::id()),
        str::to_owned,
    );
    let mut guard = STORE.lock().unwrap();
    let Some(store) = guard.as_mut() else {
        return uuid;
    };
    store.next += 1;
    let record = json!({"type": kind, "uuid": uuid, "parentUuid": store.last, "isSidechain": false,
        "sessionId": store.session, "cwd": store.cwd, "version": VERSION, "message": message});
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&store.path)
    {
        for copied in std::mem::take(&mut store.copied) {
            let _ = writeln!(file, "{copied}");
        }
        let _ = writeln!(file, "{record}");
    }
    store.last = Some(uuid.clone());
    uuid
}

/// The longest a reply waits for its release file.
const RELEASE_LIMIT: Duration = Duration::from_secs(10);

const VERSION: &str = "2.1.300";

/// The MCP servers `init` reports: with `KEYSTONE_FAKE_MCP_CONFIG=1` and
/// `CLAUDE_CONFIG_DIR` naming a fixture, every server the account and the working
/// directory turn on, as Keystone's inventory reads them. A stdio server whose command
/// is `false` has `failed`, as the real CLI reports it; every other one is `connected`.
/// Otherwise none, so a test run from a shell with a real `CLAUDE_CONFIG_DIR` never
/// reads it.
fn mcp_servers() -> Value {
    use keystone_core::integrations::{Detail, Location, State, Transport, read};
    if !std::env::var("KEYSTONE_FAKE_MCP_CONFIG").is_ok_and(|v| v == "1") {
        return json!([]);
    }
    let Some(config_dir) = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from) else {
        return json!([]);
    };
    let Ok(cwd) = std::env::current_dir() else {
        return json!([]);
    };
    let location = Location::Claude {
        global_file: config_dir.join(".claude.json"),
        config_dir,
    };
    let inventory = read(&keystone_core::AccountId::new("fake"), &location, &cwd);
    inventory
        .items
        .iter()
        .filter(|item| item.state == State::On && item.overridden_by.is_none())
        .filter_map(|item| match &item.detail {
            Detail::McpServer(server) => {
                let failed = matches!(&server.transport,
                    Transport::Stdio { command, .. } if command == "false");
                Some(json!({"name": item.name,
                    "status": if failed { "failed" } else { "connected" }}))
            }
            _ => None,
        })
        .collect()
}

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
    /// What the reply says about the images sent with the text.
    images: String,
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
    /// Whether the `keystone_ui` server answered `tools/list` with `show_ui`, or `None`
    /// while it is connecting.
    widgets: Mutex<Option<bool>>,
    /// Control requests this process sent that wait for an answer, by request ID.
    calls: Mutex<std::collections::HashMap<String, Sender<Value>>>,
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
    /// Whether to report session state and background tasks (the `idle` setting).
    idle: bool,
    /// The live background tasks, as `background_tasks_changed` lists them.
    tasks: Mutex<Vec<Value>>,
    session_state: Mutex<&'static str>,
}

impl Shared {
    fn set_session_state(&self, out: &Out, session: &str, state: &'static str) {
        if !self.idle {
            return;
        }
        *self.session_state.lock().unwrap() = state;
        emit(
            out,
            json!({"type": "system", "subtype": "session_state_changed", "state": state,
                   "uuid": format!("fake-state-{}", std::process::id()), "session_id": session}),
        );
    }

    fn emit_tasks(&self, out: &Out, session: &str) {
        let tasks = self.tasks.lock().unwrap().clone();
        emit(
            out,
            json!({"type": "system", "subtype": "background_tasks_changed", "tasks": tasks,
                   "uuid": format!("fake-tasks-{}", std::process::id()), "session_id": session}),
        );
    }
}

/// Waits until `.fake-claude-release-<name>` exists in the working directory.
fn wait_for_release(name: &str) {
    let file = PathBuf::from(format!(".fake-claude-release-{name}"));
    while !file.exists() {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Starts a background task the turn's Bash call hands off, which ends once released.
fn start_task(out: &Out, shared: &Arc<Shared>, session: &str, turn: u64, ambient: bool) {
    let name = if ambient { "ambient" } else { "background" };
    let task = format!("fake-task-{}-{turn}", std::process::id());
    let run = format!("{task}-run-1");
    let mut entry = json!({"task_id": task, "run_id": run, "task_type": "local_bash",
        "description": format!("Scripted {name} task")});
    if ambient {
        entry["ambient"] = json!(true);
    }
    shared.tasks.lock().unwrap().push(entry);
    shared.emit_tasks(out, session);
    emit(
        out,
        json!({"type": "system", "subtype": "task_started", "task_id": task, "run_id": run,
               "task_type": "local_bash", "description": format!("Scripted {name} task"),
               "ambient": ambient, "session_id": session}),
    );
    let (out, shared, session) = (out.clone(), shared.clone(), session.to_owned());
    thread::spawn(move || {
        wait_for_release(name);
        emit(
            &out,
            json!({"type": "system", "subtype": "task_notification", "task_id": task, "run_id": run,
                   "status": "completed", "summary": "done", "session_id": session}),
        );
        shared
            .tasks
            .lock()
            .unwrap()
            .retain(|entry| entry["task_id"] != task.as_str());
        shared.emit_tasks(&out, &session);
    });
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
    config_cli::claude(&std::env::args().skip(1).collect::<Vec<_>>());
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
    let new_session = format!("fake-claude-session-{}", std::process::id());
    let (session, copied) = match resume {
        Some(source) if args.iter().any(|a| a == "--fork-session") => {
            let at = args
                .iter()
                .position(|a| a == "--resume-session-at")
                .and_then(|i| args.get(i + 1));
            match fork_records(&source, at.map(String::as_str), &new_session) {
                Ok(copied) => (new_session, copied),
                Err(error) => {
                    emit(
                        &out,
                        json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
                               "errors": [error]}),
                    );
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        Some(source) => (source, Vec::new()),
        None => (new_session, Vec::new()),
    };
    open_store(&session, copied);
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
    let delay = Duration::from_millis(
        setting("delay-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    );
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
    let commands = field(
        "init-commands",
        if args.iter().any(|a| a == "--disable-slash-commands") {
            json!([])
        } else {
            listed_commands()
        },
    );
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
        widgets: Mutex::new(Some(false)),
        calls: Mutex::default(),
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
        idle: setting("idle").as_deref() == Some("1"),
        tasks: Mutex::default(),
        session_state: Mutex::new("idle"),
    });
    set_mode(&shared, &mode);
    let skip_init = setting("skip-init").as_deref() == Some("1");
    let quiesce = setting("quiesce");
    let mut initializes = 0;
    if setting("child").as_deref() == Some("1")
        && let Ok(child) = std::process::Command::new("sleep").arg("3600").spawn()
        && let Ok(mut children) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(".fake-claude-children")
    {
        let _ = writeln!(children, "{}", child.id());
    }
    let worker = {
        let (out, shared, session) = (out.clone(), shared.clone(), session.clone());
        thread::spawn(move || work(&out, &shared, &session, delay, signed_out, tools, skip_init))
    };
    if shared.idle {
        let (out, session) = (out.clone(), session.clone());
        thread::spawn(move || {
            let file = PathBuf::from(".fake-claude-emit");
            loop {
                if let Ok(text) = std::fs::read_to_string(&file) {
                    let _ = std::fs::remove_file(&file);
                    for line in text.lines().filter(|line| !line.trim().is_empty()) {
                        let line = line.replace("$SESSION", &session);
                        emit(&out, serde_json::from_str(&line).unwrap());
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
    }

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
                        initializes += 1;
                        if !shared.idle {
                            reply(answer);
                            continue;
                        }
                        let mut response =
                            json!({"subtype": "success", "request_id": id, "response": answer});
                        if quiesce.as_deref() != Some("missing") {
                            let pending: Vec<Value> = shared
                                .state
                                .lock()
                                .unwrap()
                                .approval
                                .iter()
                                .map(|(request, _)| json!({"type": "control_request", "request_id": request}))
                                .collect();
                            let state = if quiesce.as_deref() == Some("running") {
                                "running"
                            } else {
                                *shared.session_state.lock().unwrap()
                            };
                            response["pending_permission_requests"] = json!(pending);
                            response["pending_user_dialog_requests"] = json!([]);
                            response["response"]["session_state"] = json!(state);
                        }
                        emit(
                            &out,
                            json!({"type": "control_response", "response": response}),
                        );
                        match quiesce.as_deref() {
                            _ if initializes == 1 => {}
                            Some("no-snapshot") => {}
                            _ if std::fs::remove_file(".fake-claude-hold-snapshot").is_ok() => {
                                let (out, shared, session) =
                                    (out.clone(), shared.clone(), session.clone());
                                thread::spawn(move || {
                                    let _ = std::fs::write(".fake-claude-holding-snapshot", "");
                                    wait_for_release("snapshot");
                                    let _ = std::fs::remove_file(".fake-claude-release-snapshot");
                                    shared.emit_tasks(&out, &session);
                                    let _ = std::fs::remove_file(".fake-claude-holding-snapshot");
                                });
                            }
                            _ => shared.emit_tasks(&out, &session),
                        }
                    }
                    Some("mcp_set_servers") => {
                        let servers = &message["request"]["servers"];
                        let mut added = Vec::new();
                        let mut errors = serde_json::Map::new();
                        let config = &servers["keystone"];
                        if !config.is_null() {
                            let server = orchestrator::Server::from_config(config, "headers");
                            let accepted = config["type"] == "http" && server.is_some();
                            *shared.mcp.lock().unwrap() = if accepted { server } else { None };
                            if accepted {
                                added.push("keystone");
                            } else {
                                errors.insert(
                                    "keystone".into(),
                                    json!("Invalid loopback MCP configuration"),
                                );
                            }
                        }
                        let ui = &servers["keystone_ui"];
                        if !ui.is_null() {
                            if ui["type"] == "sdk" && ui["name"] == "keystone_ui" {
                                added.push("keystone_ui");
                            } else {
                                errors.insert(
                                    "keystone_ui".into(),
                                    json!("Invalid SDK MCP configuration"),
                                );
                            }
                        }
                        reply(json!({"added": added, "removed": [], "errors": errors}));
                        if added.contains(&"keystone_ui") {
                            *shared.widgets.lock().unwrap() = None;
                            let (out, shared) = (out.clone(), shared.clone());
                            thread::spawn(move || connect_widgets(&out, &shared));
                        }
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
                if let Some(tx) = shared.calls.lock().unwrap().remove(id) {
                    let _ = tx.send(message["response"].clone());
                    continue;
                }
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
                let content = &message["message"]["content"];
                let blocks = content.as_array().cloned().unwrap_or_default();
                let text = match content.as_str() {
                    Some(text) => text.to_owned(),
                    None => blocks
                        .iter()
                        .filter(|block| block["type"] == "text")
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                let images =
                    images::record(&blocks.iter().filter_map(image_block).collect::<Vec<_>>());
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
                state.queue.push_back(Queued { uuid, text, images });
                shared.wake.notify_all();
            }
            _ => {}
        }
    }
    shared.state.lock().unwrap().closed = true;
    shared.wake.notify_all();
    let _ = worker.join();
}

/// The `initialize` answer's `commands`, as Claude Code 2.1.301 lists them: skills,
/// then commands, from `$CLAUDE_CONFIG_DIR` (never `~/.claude`) and the working
/// directory's `.claude`, their source a description suffix, then two built-ins.
fn listed_commands() -> Value {
    let mut listed = Vec::new();
    let user = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let project = std::env::current_dir().ok().map(|cwd| cwd.join(".claude"));
    for (root, tag) in [(user, "user"), (project, "project")] {
        let Some(root) = root else { continue };
        let mut found = Vec::new();
        if let Ok(skills) = std::fs::read_dir(root.join("skills")) {
            for skill in skills.flatten() {
                let file = skill.path().join("SKILL.md");
                if file.is_file() {
                    found.push((skill.file_name().to_string_lossy().into_owned(), file));
                }
            }
        }
        found.sort();
        let skills = found.len();
        let mut stack = vec![root.join("commands")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "md") {
                    let relative = path.strip_prefix(root.join("commands")).unwrap();
                    let name = relative
                        .with_extension("")
                        .to_string_lossy()
                        .replace('/', ":");
                    found.push((name, path));
                }
            }
        }
        found[skills..].sort();
        for (name, file) in found {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            let front = |key: &str| {
                text.lines()
                    .skip(1)
                    .take_while(|line| *line != "---")
                    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
                    .map(|value| {
                        value
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .to_owned()
                    })
                    .unwrap_or_default()
            };
            listed.push(
                json!({"name": name, "description": format!("{} ({tag})", front("description")),
                "argumentHint": front("argument-hint")}),
            );
        }
    }
    listed.push(json!({"name": "compact", "description": "Free up context by summarizing the conversation so far",
        "argumentHint": "<optional custom summarization instructions>", "builtin": true}));
    listed.push(
        json!({"name": "context", "description": "Show current context usage",
        "argumentHint": "", "builtin": true}),
    );
    Value::Array(listed)
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
    shared: &Arc<Shared>,
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
               "capabilities": capabilities, "permissionMode": mode, "mcp_servers": mcp_servers(),
               "slash_commands": [], "skills": [], "claude_code_version": VERSION,
               "model": shared.model});
        if let Some(tools) = &tools {
            init["tools"] = tools.clone();
        }
        if !skip_init {
            emit(out, init);
        }
        shared.set_session_state(out, session, "running");
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
        store(
            "user",
            json!({"role": "user", "content": next.text}),
            Some(&next.uuid),
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
            answer(
                out,
                shared,
                session,
                turn,
                &next.uuid,
                &next.text,
                &next.images,
                &mode,
                delay,
            )
        };
        lifecycle(out, shared, session, &next.uuid, ended);
        let mut state = shared.state.lock().unwrap();
        state.busy = false;
        let queued = !state.queue.is_empty();
        drop(state);
        if !queued {
            shared.set_session_state(out, session, "idle");
        }
    }
}

/// Sends a control request and waits for its answer, or `None` if the turn is
/// stopped first, after withdrawing the request as Claude does.
fn ask(out: &Out, shared: &Shared, id: &str, request: Value) -> Option<Value> {
    let (tx, rx) = mpsc::channel();
    shared.calls.lock().unwrap().insert(id.to_owned(), tx);
    emit(
        out,
        json!({"type": "control_request", "request_id": id, "request": request}),
    );
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(answer) => return Some(answer),
            Err(RecvTimeoutError::Disconnected) => return None,
            Err(RecvTimeoutError::Timeout) => {
                let state = shared.state.lock().unwrap();
                if state.abort == Some(Abort::Stop) || state.closed {
                    drop(state);
                    shared.calls.lock().unwrap().remove(id);
                    emit(
                        out,
                        json!({"type": "control_cancel_request", "request_id": id}),
                    );
                    return None;
                }
            }
        }
    }
}

fn mcp_message(message: Value) -> Value {
    json!({"subtype": "mcp_message", "server_name": "keystone_ui", "message": message})
}

/// Connects to the `keystone_ui` SDK server as Claude Code does once it is added.
fn connect_widgets(out: &Out, shared: &Shared) {
    let pid = std::process::id();
    let handshake = [
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "claude-code", "version": VERSION}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    ];
    let mut listed = Value::Null;
    for (n, message) in handshake.into_iter().enumerate() {
        let id = format!("fake-mcp-{pid}-{n}");
        let Some(answer) = ask(out, shared, &id, mcp_message(message)) else {
            return;
        };
        listed = answer["response"]["mcp_response"].clone();
    }
    *shared.widgets.lock().unwrap() = Some(
        listed["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "show_ui")),
    );
}

/// One `show_ui` call: the tool use, its permission check, `tools/call`, and its
/// result. Returns the result text, or `None` when the turn was stopped.
fn show_ui(out: &Out, shared: &Shared, turn: u64, n: usize, arguments: Value) -> Option<String> {
    let pid = std::process::id();
    let message = format!("fake-ui-{pid}-{turn}-{n}");
    let tool = format!("fake-ui-toolu-{pid}-{turn}-{n}");
    let name = "mcp__keystone_ui__show_ui";
    stream(
        out,
        json!({"type": "message_start", "message": {"id": message}}),
    );
    stream(
        out,
        json!({"type": "content_block_start", "index": 0, "content_block":
               {"type": "tool_use", "id": tool, "name": name, "input": {}}}),
    );
    let content = json!([{"type": "tool_use", "id": tool, "name": name, "input": arguments}]);
    emit(
        out,
        json!({"type": "assistant", "message": {"id": message, "content": content},
               "parent_tool_use_id": null}),
    );
    store(
        "assistant",
        json!({"id": message, "role": "assistant", "content": content}),
        None,
    );
    let permission = format!("fake-ui-permission-{pid}-{turn}-{n}");
    let allowed = ask(
        out,
        shared,
        &permission,
        json!({"subtype": "can_use_tool", "tool_name": name, "display_name": "show_ui",
               "input": arguments, "tool_use_id": tool}),
    )?;
    let (text, is_error) = if allowed["response"]["behavior"] == "allow" {
        let id = format!("fake-ui-call-{pid}-{turn}-{n}");
        let answer = ask(
            out,
            shared,
            &id,
            mcp_message(
                json!({"jsonrpc": "2.0", "id": 2 + n, "method": "tools/call", "params": {
                    "name": "show_ui", "arguments": arguments,
                    "_meta": {"claudecode/toolUseId": tool}}}),
            ),
        )?;
        let result = &answer["response"]["mcp_response"]["result"];
        (
            result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            result["isError"] == true,
        )
    } else {
        ("The user denied this in Keystone.".to_owned(), true)
    };
    let result = json!([{"type": "tool_result", "tool_use_id": tool,
        "content": [{"type": "text", "text": text}], "is_error": is_error}]);
    emit(
        out,
        json!({"type": "user", "message": {"role": "user", "content": result},
               "parent_tool_use_id": null}),
    );
    store("user", json!({"role": "user", "content": result}), None);
    Some(text)
}

/// Runs one message's turn and returns its lifecycle end state.
#[allow(clippy::too_many_arguments)]
fn answer(
    out: &Out,
    shared: &Arc<Shared>,
    session: &str,
    turn: u64,
    request: &str,
    text: &str,
    images: &str,
    mode: &str,
    delay: Duration,
) -> &'static str {
    if let Some(reset) = support_limit::limited_until(support_limit::claude_home()) {
        return limit(out, session, turn, reset);
    }
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
            "Claude test reply (test data): You wrote: {text} **Mode:** `{mode}`{left_plan} **Model:** `{model}`{images}"
        )
    });
    if text.contains("long") {
        for n in 1..=200 {
            reply.push_str(&format!(" word{n}"));
        }
    }
    if shared.idle {
        idle_scripts(out, shared, session, turn, text);
    }
    if text.contains("run") {
        let message = format!("fake-tool-{}-{turn}", std::process::id());
        let tool = format!("fake-toolu-{}-{turn}", std::process::id());
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
        let uuid = store(
            "assistant",
            json!({"id": message, "role": "assistant", "content":
                   [{"type": "tool_use", "id": tool, "name": "Bash", "input": input}]}),
            None,
        );
        emit(
            out,
            json!({"type": "assistant", "message": {"id": message, "content":
                   [{"type": "tool_use", "id": tool, "name": "Bash", "input": input}]}, "parent_tool_use_id": null,
                   "uuid": uuid}),
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
        let uuid = store(
            "user",
            json!({"role": "user", "content":
                   [{"type": "tool_result", "tool_use_id": tool, "content": content, "is_error": is_error}]}),
            None,
        );
        emit(
            out,
            json!({"type": "user", "message": {"role": "user", "content":
                   [{"type": "tool_result", "tool_use_id": tool, "content": content, "is_error": is_error}]},
                   "parent_tool_use_id": null, "uuid": uuid}),
        );
    }
    if let Some(path) = std::env::var_os("KEYSTONE_FAKE_CLAUDE_WRITE") {
        let path = std::env::current_dir().unwrap_or_default().join(path);
        let message = format!("fake-write-{}-{turn}", std::process::id());
        let tool = format!("fake-write-toolu-{}-{turn}", std::process::id());
        let content = json!([{"type": "tool_use", "id": tool, "name": "Write",
                              "input": {"file_path": path, "content": ""}}]);
        let uuid = store(
            "assistant",
            json!({"id": message, "role": "assistant", "content": content}),
            None,
        );
        emit(
            out,
            json!({"type": "assistant", "message": {"id": message, "content": content},
                   "parent_tool_use_id": null, "uuid": uuid}),
        );
        let result = json!([{"type": "tool_result", "tool_use_id": tool,
                             "content": "File written.", "is_error": false}]);
        let uuid = store("user", json!({"role": "user", "content": result}), None);
        emit(
            out,
            json!({"type": "user", "message": {"role": "user", "content": result},
                   "parent_tool_use_id": null, "uuid": uuid}),
        );
    }
    let calls = widgets::calls(text);
    if !calls.is_empty() {
        // A turn waits for the server to connect, as Claude Code does.
        let started = Instant::now();
        let widgets = loop {
            match *shared.widgets.lock().unwrap() {
                Some(widgets) => break widgets,
                None if started.elapsed() > Duration::from_secs(5) => break false,
                None => {}
            }
            thread::sleep(Duration::from_millis(5));
        };
        let mut results = Vec::new();
        for (n, arguments) in calls.into_iter().enumerate().filter(|_| widgets) {
            match show_ui(out, shared, turn, n, arguments) {
                Some(result) => results.push(result),
                None => return stopped(out),
            }
        }
        reply = widgets::reply(&results);
    }
    let message = format!("fake-message-{}-{turn}", std::process::id());
    let text_record = |text: &str| {
        store(
            "assistant",
            json!({"id": message, "role": "assistant", "content": [{"type": "text", "text": text}]}),
            None,
        )
    };
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
            text_record(&sent);
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
        if i == 0 && text.contains("crash") {
            text_record(&reply);
            std::process::exit(3);
        }
        if i == 0 && !released(shared) {
            emit(
                out,
                json!({"type": "assistant", "message": {"id": message, "content":
                       [{"type": "text", "text": sent}]}, "parent_tool_use_id": null}),
            );
            text_record(&sent);
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
            text_record(&sent);
            return aborted(out, &message, &sent, abort);
        }
    }
    let usage = usage_percent(text);
    let mut message_json = json!({"id": message, "content": [{"type": "text", "text": sent}]});
    let mut result =
        json!({"type": "result", "subtype": "success", "is_error": false, "result": sent});
    if let Some(percent) = usage {
        message_json["usage"] = json!({"input_tokens": 0, "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": percent * 1000, "output_tokens": 0});
        result["modelUsage"] = json!({model: {"contextWindow": 200_000}});
    }
    let uuid = text_record(&sent);
    emit(
        out,
        json!({"type": "assistant", "message": message_json, "parent_tool_use_id": null,
               "uuid": uuid}),
    );
    stream(out, json!({"type": "content_block_stop", "index": 0}));
    if let Some(percent) = usage {
        emit_rate_limit(out, session, percent);
    }
    emit(out, result);
    "completed"
}

/// The percentage a prompt such as "usage 85" asks the fake to report.
fn usage_percent(text: &str) -> Option<u64> {
    let rest = &text[text.find("usage ")? + "usage ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// A synthetic, labelled `rate_limit_event`: the 5-hour window at `percent`, resetting
/// in two hours, and the weekly one at 20%, resetting in four days.
fn emit_rate_limit(out: &Out, session: &str, percent: u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let fraction = percent as f64 / 100.0;
    let (five_hour, weekly) = (now + 2 * 3600, now + 4 * 86_400);
    emit(
        out,
        json!({"type": "rate_limit_event", "keystone_fake": "synthetic test data", "rate_limit_info": {
            "status": if percent >= 80 { "allowed_warning" } else { "allowed" },
            "resetsAt": five_hour, "rateLimitType": "five_hour", "utilization": fraction,
            "unifiedWindows": {"five_hour": {"utilization": fraction, "resetsAt": five_hour},
                               "seven_day": {"utilization": 0.2, "resetsAt": weekly}},
            "isUsingOverage": false},
            "uuid": format!("fake-rate-limit-{}", std::process::id()), "session_id": session}),
    );
}

/// The idle-mode prompts: a Bash call handed to a background or ambient task, a hook
/// that outlives the turn, and a Bash call held until released.
fn idle_scripts(out: &Out, shared: &Arc<Shared>, session: &str, turn: u64, text: &str) {
    let bash = |id: &str, result: Option<&str>| {
        let message = format!("fake-bash-{}-{turn}-{id}", std::process::id());
        let tool = format!("fake-bash-toolu-{}-{turn}-{id}", std::process::id());
        emit(
            out,
            json!({"type": "assistant", "message": {"id": message, "content":
                   [{"type": "tool_use", "id": tool, "name": "Bash", "input": {"command": "sleep 600"}}]},
                   "parent_tool_use_id": null}),
        );
        let result = result.map_or_else(
            || {
                wait_for_release("tool");
                "held tool done".to_owned()
            },
            str::to_owned,
        );
        emit(
            out,
            json!({"type": "user", "message": {"role": "user", "content":
                   [{"type": "tool_result", "tool_use_id": tool, "content": result, "is_error": false}]},
                   "parent_tool_use_id": null}),
        );
    };
    for (word, ambient) in [("background task", false), ("ambient task", true)] {
        if text.contains(word) {
            start_task(out, shared, session, turn, ambient);
            bash(
                word,
                Some("Command running in background with ID: fake-task"),
            );
        }
    }
    if text.contains("hook") {
        let hook = format!("fake-hook-{}-{turn}", std::process::id());
        emit(
            out,
            json!({"type": "system", "subtype": "hook_started", "hook_id": hook,
                   "hook_name": "PostToolUse:Bash", "hook_event": "PostToolUse", "session_id": session}),
        );
        let (out, session) = (out.clone(), session.to_owned());
        thread::spawn(move || {
            wait_for_release("hook");
            emit(
                &out,
                json!({"type": "system", "subtype": "hook_response", "hook_id": hook,
                       "hook_name": "PostToolUse:Bash", "hook_event": "PostToolUse", "output": "",
                       "stdout": "", "stderr": "", "exit_code": 0, "outcome": "success",
                       "session_id": session}),
            );
        });
    }
    if text.contains("hold tool") {
        bash("held", None);
    }
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

/// The account's usage limit refuses the request, in Claude Code 2.1.301's frames: a
/// rejected `rate_limit_event`, a synthetic assistant message with `error`
/// `rate_limit` and `api_error` `usage_limit_reached`, then an error result.
fn limit(out: &Out, session: &str, turn: u64, reset: u64) -> &'static str {
    let info = json!({"status": "rejected", "rateLimitType": "five_hour", "resetsAt": reset,
                      "isUsingOverage": false});
    emit(
        out,
        json!({"type": "rate_limit_event", "keystone_fake": "synthetic test data",
               "rate_limit_info": info, "uuid": format!("fake-limit-{}-{turn}", std::process::id()),
               "session_id": session}),
    );
    let text = "You've hit your 5-hour limit (synthetic test data)";
    emit(
        out,
        json!({"type": "assistant", "message": {"id": format!("fake-limit-{}-{turn}", std::process::id()),
               "model": "<synthetic>", "content": [{"type": "text", "text": text}]},
               "parent_tool_use_id": null, "error": "rate_limit", "is_api_error_message": true,
               "api_error_status": 429, "api_error": "usage_limit_reached",
               "api_error_params": {"rate_limit_info": info}}),
    );
    emit(
        out,
        json!({"type": "result", "subtype": "success", "is_error": true, "result": text,
               "api_error_status": 429, "api_error": "usage_limit_reached"}),
    );
    "completed"
}

fn stopped(out: &Out) -> &'static str {
    let uuid = store(
        "user",
        json!({"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}),
        None,
    );
    emit(
        out,
        json!({"type": "user", "message": {"role": "user", "content":
               [{"type": "text", "text": "[Request interrupted by user]"}]}, "parent_tool_use_id": null,
               "uuid": uuid}),
    );
    emit(
        out,
        json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
               "errors": ["interrupted"]}),
    );
    "cancelled"
}

/// A Claude `image` block: its media type and the hash of its decoded bytes.
fn image_block(block: &Value) -> Option<Value> {
    if block["type"] != "image" {
        return None;
    }
    let source = &block["source"];
    let data = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        source["data"].as_str().unwrap_or_default(),
    )
    .unwrap_or_default();
    Some(json!({"provider": "claude", "source": source["type"],
        "media_type": source["media_type"], "sha256": images::sha256(&data), "bytes": data.len()}))
}
