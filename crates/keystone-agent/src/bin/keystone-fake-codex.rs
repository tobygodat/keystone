//! A scripted stand-in for `codex app-server`, for tests and UI verification.
//!
//! It speaks the same JSON-RPC subset Keystone uses and replies with labeled test
//! text. A prompt containing "run" asks approval for a command first. Set
//! `KEYSTONE_FAKE_CODEX_DELAY_MS` to change the pause between streamed words.
//!
//! Each thread keeps the approval policy and sandbox of its last `turn/start`
//! override, as Codex does, and each reply ends with them. With
//! `KEYSTONE_FAKE_CODEX_STATE` naming a file, they are kept there, so a later process
//! resumes a thread with its override like real Codex. A turn whose prompt contains
//! "end on steer" ends 300 ms after a steer arrives and then rejects it, as Codex does
//! for a steer that reaches a turn that just ended. `KEYSTONE_FAKE_CODEX_REPLY`
//! replaces the reply text exactly.
//!
//! `model/list` returns a scripted catalog (`fake-default`, `fake-fast`, and a hidden
//! entry) in two pages, as Codex pages: the first answer holds `fake-default` and
//! `"nextCursor": "page-2"`, the answer to cursor `page-2` holds the rest with a null
//! cursor, and any other cursor is an "invalid cursor" error. With
//! `KEYSTONE_FAKE_CODEX_NO_CATALOG=1` every `model/list` is a JSON-RPC error. `thread/start`
//! and `thread/resume` reply with the thread's model: the `model` they were given, else
//! `fake-default`, remembered per thread for the process. Each reply ends with it.
//! `skills/list` answers the skills in `$CODEX_HOME/skills` and the folder's
//! `.agents/skills` and `.codex/skills`, then one scripted system skill.
//! `turn/start` takes `localImage` input items beside the text; with
//! `KEYSTONE_FAKE_IMAGE_RECEIPT` set, each one's path and the hash of the file there is
//! appended to it (see `support/images.rs`), and the reply names them.
//! `KEYSTONE_FAKE_ENV_RECEIPT` names a file each process appends its environment to, and
//! a `CODEX_HOME` that is not a directory fails the start, as the real CLI does.
//!
//! Stored history: `thread/turns/list`, `thread/items/list` (newest first) and
//! `thread/read` answer from
//! `<CODEX_HOME or ~/.codex>/keystone-fake-threads/<thread>.json`, a `{"turns": [..]}`
//! file a test may write itself; a missing file is an unknown thread. With
//! `KEYSTONE_FAKE_CODEX_STORE=1` each turn is recorded there as it runs: the turn when
//! it starts, each completed item, and its end. `KEYSTONE_FAKE_CODEX_NO_TURNS_LIST=1`
//! answers `thread/turns/list` as an unknown method, like an older Codex: -32600, which
//! is what the real app-server sends for a method it does not know.
//! `thread/list` lists the stored threads whose file also holds a `thread` record
//! (`name`, `preview`, `cwd`, `createdAt`, `updatedAt`) whose `cwd` the filter names.
//! `thread/turns/list` with `itemsView: "notLoaded"` leaves each turn's items out, and
//! names a `nextCursor` when turns are left past `limit`.
//! `KEYSTONE_FAKE_CODEX_METHOD_LOG` names a file that gets every method received, one
//! per line.
//! `KEYSTONE_FAKE_CODEX_THREAD_LOG` names a file that gets each request naming a
//! `threadId` as `<method> <threadId>`, one per line. A prompt containing "crash"
//! records its whole reply as completed after
//! the first streamed word, then exits, as a Codex that dies mid-stream after saving.
//!
//! A thread started with a `show_ui` entry in `dynamicTools` calls it through
//! `item/tool/call` when the prompt asks (see `support/widgets.rs`), waits for the
//! answer, and replies with what each call returned. `dynamicTools` is refused, as
//! Codex 0.163 refuses it, unless `initialize` opted into `experimentalApi`.
//!
//! `thread/fork` starts a new thread with the source's model and override and
//! `forkedFromId`, as Codex 0.163.0 does. With a stored source it keeps the source's
//! turns through `lastTurnId` (all of them without one) under the same turn IDs, and
//! an unknown turn fails with "turn not found"; in store mode a source with no stored
//! history is an unknown thread.
//!
//! `thread/start`, `thread/resume` and `thread/fork` are followed by `mcpServer/startupStatus/updated`
//! for each MCP server the thread gets (see [`start_mcp_servers`]).
//!
//! An account whose `CODEX_HOME` holds a `.fake-usage-limit` file is at its usage
//! limit (see `support/limit.rs`): each turn fails with `usageLimitExceeded`.
//!
//! A prompt containing "usage NN" ends its turn with synthetic, labelled
//! `account/rateLimits/updated` (the 5-hour window at NN%) and `thread/tokenUsage/updated`
//! notifications.

use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[path = "support/codex_config.rs"]
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

/// The skills `skills/list` answers for `cwd`, as Codex 0.163.0 lists them: those in
/// `$CODEX_HOME/skills` (never `~/.codex`) as `user`, those in the folder's
/// `.agents/skills` and `.codex/skills` as `repo`, then one scripted `system` skill.
fn skills(cwd: &str) -> Vec<Value> {
    let home = std::env::var_os("CODEX_HOME").map(std::path::PathBuf::from);
    let cwd = std::path::Path::new(cwd);
    let roots = [
        (home.map(|h| h.join("skills")), "user"),
        (Some(cwd.join(".agents/skills")), "repo"),
        (Some(cwd.join(".codex/skills")), "repo"),
    ];
    let mut listed = Vec::new();
    for (root, scope) in roots {
        let Some(root) = root else { continue };
        let mut found: Vec<_> = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join("SKILL.md"))
            .filter(|file| file.is_file())
            .collect();
        found.sort();
        for file in found {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            let front = |key: &str| {
                text.lines()
                    .skip(1)
                    .take_while(|line| *line != "---")
                    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
                    .unwrap_or_default()
                    .to_owned()
            };
            listed.push(
                json!({"name": front("name"), "description": front("description"),
                "path": file, "scope": scope, "enabled": true, "pluginId": null}),
            );
        }
    }
    listed.push(
        json!({"name": "fake-system-skill", "description": "Scripted system skill",
        "path": "/fake/system/SKILL.md", "scope": "system", "enabled": true, "pluginId": null}),
    );
    listed
}

/// Reports starting each MCP server the thread gets, as Codex 0.163.0 does after
/// answering `thread/start` or `thread/resume`: Keystone's own when the thread's config
/// adds it, then, with `CODEX_HOME` set, every server the account and the thread's
/// folder turn on, as Keystone's inventory reads them, when `KEYSTONE_FAKE_MCP_CONFIG=1`
/// says `CODEX_HOME` is a fixture. A stdio server whose command is `false` fails; every
/// other one is ready. Otherwise only Keystone's, so a test run from a shell with a real
/// `CODEX_HOME` never reads it.
fn start_mcp_servers(out: &Out, thread: &str, params: &Value) {
    use keystone_core::integrations::{Detail, Location, State, Transport, read};
    let mut servers: Vec<(String, bool)> = Vec::new();
    if params["config"]["mcp_servers"].get("keystone").is_some() {
        servers.push(("keystone".into(), false));
    }
    let fixture = std::env::var("KEYSTONE_FAKE_MCP_CONFIG").is_ok_and(|v| v == "1");
    if let (true, Some(home), Some(cwd)) = (
        fixture,
        std::env::var_os("CODEX_HOME").map(std::path::PathBuf::from),
        params["cwd"].as_str(),
    ) {
        let location = Location::Codex {
            home,
            user_home: None,
        };
        let inventory = read(
            &keystone_core::AccountId::new("fake"),
            &location,
            std::path::Path::new(cwd),
        );
        for item in inventory
            .items
            .iter()
            .filter(|item| item.state == State::On && item.overridden_by.is_none())
        {
            if let Detail::McpServer(server) = &item.detail {
                let failed = matches!(&server.transport,
                    Transport::Stdio { command, .. } if command == "false");
                servers.push((item.name.clone(), failed));
            }
        }
    }
    let note = |name: &str, status: &str, error: Option<&str>| {
        json!({"method": "mcpServer/startupStatus/updated", "params": {"threadId": thread,
            "name": name, "status": status, "error": error, "failureReason": null}})
    };
    for (name, _) in &servers {
        emit(out, note(name, "starting", None));
    }
    for (name, failed) in &servers {
        emit(
            out,
            if *failed {
                note(name, "failed", Some("scripted MCP startup failure"))
            } else {
                note(name, "ready", None)
            },
        );
    }
}

fn emit(out: &Out, message: Value) {
    let mut out = out.lock().unwrap();
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

fn notify(out: &Out, method: &str, params: Value) {
    if std::env::var("KEYSTONE_FAKE_CODEX_STORE").is_ok_and(|v| v == "1") {
        record(method, &params);
    }
    emit(out, json!({"method": method, "params": params}));
}

/// Where a thread's stored history lives.
fn thread_file(thread: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".codex"))
        })?;
    let safe = !thread.is_empty()
        && thread
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    safe.then(|| {
        home.join("keystone-fake-threads")
            .join(format!("{thread}.json"))
    })
}

fn stored_turns(thread: &str) -> Option<Vec<Value>> {
    let bytes = std::fs::read(thread_file(thread)?).ok()?;
    let stored: Value = serde_json::from_slice(&bytes).ok()?;
    stored["turns"].as_array().cloned()
}

/// The stored threads `thread/list` returns: those whose file has a `thread` record
/// whose `cwd` the `cwd` filter names, most recently updated first, up to `limit`.
fn listed_threads(params: &Value) -> Vec<Value> {
    let Some(dir) = thread_file("x").and_then(|file| file.parent().map(ToOwned::to_owned)) else {
        return Vec::new();
    };
    let cwds: Vec<&str> = match &params["cwd"] {
        Value::String(cwd) => vec![cwd.as_str()],
        Value::Array(cwds) => cwds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let mut threads: Vec<Value> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let stored: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            let id = entry.path().file_stem()?.to_str()?.to_owned();
            let thread = &stored["thread"];
            let cwd = thread["cwd"].as_str()?;
            (cwds.is_empty() || cwds.contains(&cwd)).then(|| {
                json!({"id": id, "name": thread["name"], "preview": thread["preview"],
                    "cwd": cwd, "createdAt": thread["createdAt"], "updatedAt": thread["updatedAt"],
                    "turns": []})
            })
        })
        .collect();
    threads.sort_by_key(|thread| std::cmp::Reverse(thread["updatedAt"].as_i64()));
    threads.truncate(params["limit"].as_u64().unwrap_or(50) as usize);
    threads
}

/// Records a turn's start, a completed item, or the turn's end in its thread's store.
fn record(method: &str, params: &Value) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    let thread = params["threadId"].as_str().unwrap_or_default();
    let Some(path) = thread_file(thread) else {
        return;
    };
    let mut turns = stored_turns(thread).unwrap_or_default();
    match method {
        "turn/started" => turns.push(json!({"id": params["turn"]["id"], "status": "inProgress",
            "itemsView": "full", "items": []})),
        "item/completed" => {
            let Some(turn) = turns.iter_mut().rev().find(|t| t["id"] == params["turnId"]) else {
                return;
            };
            let items = turn["items"].as_array_mut().unwrap();
            items.retain(|item| item["id"] != params["item"]["id"]);
            items.push(params["item"].clone());
        }
        "turn/completed" => {
            let Some(turn) = turns
                .iter_mut()
                .rev()
                .find(|t| t["id"] == params["turn"]["id"])
            else {
                return;
            };
            turn["status"] = params["turn"]["status"].clone();
        }
        _ => return,
    }
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, json!({"turns": turns}).to_string()).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
}

struct Turn {
    interrupt: AtomicBool,
    /// Ended by the main loop; the turn's thread stops without reporting anything.
    silent: AtomicBool,
    end_on_steer: bool,
    steer: Mutex<Option<String>>,
    /// Where the client's answer to the turn's open server request goes.
    answers: Mutex<Option<Sender<Value>>>,
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
    write_env_receipt("codex");
    if std::env::args().any(|arg| arg == "--version") {
        let version =
            std::env::var("KEYSTONE_FAKE_CODEX_VERSION").unwrap_or_else(|_| "0.162.1".into());
        println!("codex-cli {version}");
        return;
    }
    config_cli::codex(&std::env::args().skip(1).collect::<Vec<_>>());
    // As codex 0.163.0 does, refuse a CODEX_HOME that does not exist.
    if let Some(home) = std::env::var_os("CODEX_HOME")
        && !std::path::Path::new(&home).is_dir()
    {
        eprintln!(
            "Error: CODEX_HOME points to {:?}, but that path does not exist",
            std::path::Path::new(&home)
        );
        std::process::exit(1);
    }
    let delay = Duration::from_millis(
        std::env::var("KEYSTONE_FAKE_CODEX_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    );
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let next = Arc::new(AtomicU64::new(1));
    let turns: Arc<Mutex<std::collections::HashMap<String, Arc<Turn>>>> = Arc::default();
    // Each thread keeps the approval policy and sandbox of its last override, as Codex does.
    let state = std::env::var_os("KEYSTONE_FAKE_CODEX_STATE").map(std::path::PathBuf::from);
    let mut policies: std::collections::HashMap<String, (Value, Value)> = state
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let defaults = || (json!("on-request"), json!({"type": "workspaceWrite"}));
    let mut models: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut servers: std::collections::HashMap<String, orchestrator::Server> =
        std::collections::HashMap::new();
    let no_catalog = std::env::var("KEYSTONE_FAKE_CODEX_NO_CATALOG").is_ok_and(|v| v == "1");
    let mut experimental = false;
    let mut widget_threads: std::collections::HashSet<String> = Default::default();

    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to one of our approval or tool requests.
            for turn in turns.lock().unwrap().values() {
                if let Some(tx) = turn.answers.lock().unwrap().as_ref() {
                    let _ = tx.send(message.clone());
                }
            }
            continue;
        };
        if let Some(path) = std::env::var_os("KEYSTONE_FAKE_CODEX_METHOD_LOG")
            && let Ok(mut log) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let _ = writeln!(log, "{method}");
        }
        if let Some(path) = std::env::var_os("KEYSTONE_FAKE_CODEX_THREAD_LOG")
            && let Some(thread) = params["threadId"].as_str()
            && let Ok(mut log) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let _ = writeln!(log, "{method} {thread}");
        }
        let reply = |result: Value| emit(&out, json!({"id": id, "result": result}));
        match method {
            "thread/turns/list"
                if std::env::var("KEYSTONE_FAKE_CODEX_NO_TURNS_LIST").is_ok_and(|v| v == "1") =>
            {
                emit(
                    &out,
                    json!({"id": id, "error": {"code": -32600, "message": "Invalid request: unknown variant `thread/turns/list`"}}),
                )
            }
            "thread/turns/list" | "thread/read" | "thread/items/list" => {
                let thread = params["threadId"].as_str().unwrap_or_default();
                match stored_turns(thread) {
                    None => emit(
                        &out,
                        json!({"id": id, "error": {"code": -32600, "message": format!("thread not found: {thread}")}}),
                    ),
                    Some(turns) if method == "thread/read" => {
                        reply(json!({"thread": {"id": thread,
                        "turns": if params["includeTurns"] == true { turns } else { Vec::new() }}}))
                    }
                    Some(turns) if method == "thread/items/list" => {
                        // Codex 0.163's ThreadItemEntry wraps each item with its turn's ID.
                        let mut items: Vec<Value> = turns
                            .iter()
                            .flat_map(|turn| {
                                let id = turn["id"].clone();
                                let items = turn["items"].as_array().cloned().unwrap_or_default();
                                items.into_iter().map(move |item| {
                                    json!({"turnId": id, "item": item,
                                        "startedAtMs": null, "completedAtMs": null})
                                })
                            })
                            .collect();
                        items.reverse();
                        let limit = params["limit"].as_u64().unwrap_or(50) as usize;
                        let more = items.len() > limit;
                        items.truncate(limit);
                        reply(json!({"data": items, "nextCursor": more.then_some("older"),
                            "backwardsCursor": null}))
                    }
                    Some(mut turns) => {
                        turns.reverse();
                        let limit = params["limit"].as_u64().unwrap_or(50) as usize;
                        let more = turns.len() > limit;
                        turns.truncate(limit);
                        if params["itemsView"] == "notLoaded" {
                            for turn in &mut turns {
                                turn["items"] = json!([]);
                                turn["itemsView"] = json!("notLoaded");
                            }
                        }
                        reply(json!({"data": turns, "nextCursor": more.then_some("older"),
                            "backwardsCursor": null}))
                    }
                }
            }
            "thread/list" => reply(json!({"data": listed_threads(&params),
                "nextCursor": null, "backwardsCursor": null})),
            "initialize" => {
                if let Some(path) = std::env::var_os("KEYSTONE_FAKE_CODEX_INITIALIZE_LOG") {
                    let _ = std::fs::write(path, params.to_string());
                }
                experimental = params["capabilities"]["experimentalApi"] == true;
                if params.get("capabilities").is_some()
                    && std::env::var("KEYSTONE_FAKE_CODEX_NO_NOTIFICATION_FILTER")
                        .is_ok_and(|value| value == "1")
                {
                    emit(
                        &out,
                        json!({"id": id, "error": {"code": -32602, "message": "optOutNotificationMethods is not supported"}}),
                    );
                    continue;
                }
                reply(json!({"userAgent": "keystone-fake-codex"}));
            }
            "initialized" => {}
            "thread/start" if params.get("dynamicTools").is_some() && !experimental => emit(
                &out,
                json!({"id": id, "error": {"code": -32600,
                    "message": "thread/start.dynamicTools requires experimentalApi capability"}}),
            ),
            "thread/start" => {
                let n = next.fetch_add(1, Ordering::Relaxed);
                let thread = format!("fake-thread-{n}");
                if params["dynamicTools"]
                    .as_array()
                    .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "show_ui"))
                {
                    widget_threads.insert(thread.clone());
                }
                let (approval, sandbox) = defaults();
                let model = thread_model(&mut models, &thread, &params);
                thread_server(&mut servers, &thread, &params);
                reply(json!({"thread": {"id": thread},
                    "approvalPolicy": approval, "sandbox": sandbox, "model": model}));
                start_mcp_servers(&out, &thread, &params);
            }
            "thread/fork" => {
                let source = params["threadId"].as_str().unwrap_or_default().to_owned();
                let last = params["lastTurnId"].as_str();
                let kept = match (stored_turns(&source), last) {
                    (Some(turns), Some(last)) => turns
                        .iter()
                        .position(|turn| turn["id"] == last)
                        .map(|end| Some(turns[..=end].to_vec()))
                        .ok_or_else(|| format!("turn not found: {last}")),
                    (Some(turns), None) => Ok(Some(turns)),
                    (None, _)
                        if std::env::var("KEYSTONE_FAKE_CODEX_STORE").is_ok_and(|v| v == "1") =>
                    {
                        Err(format!("no rollout found for thread id {source}"))
                    }
                    (None, _) => Ok(None),
                };
                let kept = match kept {
                    Ok(kept) => kept,
                    Err(message) => {
                        emit(
                            &out,
                            json!({"id": id, "error": {"code": -32600, "message": message}}),
                        );
                        continue;
                    }
                };
                let n = next.fetch_add(1, Ordering::Relaxed);
                let thread = format!("fake-thread-{n}");
                if let (Some(turns), Some(path)) = (kept, thread_file(&thread)) {
                    let _ = std::fs::create_dir_all(path.parent().unwrap());
                    let _ = std::fs::write(path, json!({"turns": turns}).to_string());
                }
                let (approval, sandbox) = policies.get(&source).cloned().unwrap_or_else(defaults);
                policies.insert(thread.clone(), (approval.clone(), sandbox.clone()));
                if let Some(path) = &state {
                    let _ = std::fs::write(path, serde_json::to_vec(&policies).unwrap());
                }
                if let Some(model) = models.get(&source).cloned() {
                    models.entry(thread.clone()).or_insert(model);
                }
                let model = thread_model(&mut models, &thread, &params);
                thread_server(&mut servers, &thread, &params);
                reply(json!({"thread": {"id": thread, "forkedFromId": source},
                    "approvalPolicy": approval, "sandbox": sandbox, "model": model}));
                start_mcp_servers(&out, &thread, &params);
            }
            "thread/resume" => {
                let thread = params["threadId"].as_str().unwrap_or_default().to_owned();
                let (approval, sandbox) = policies.get(&thread).cloned().unwrap_or_else(defaults);
                let model = thread_model(&mut models, &thread, &params);
                thread_server(&mut servers, &thread, &params);
                reply(json!({"thread": {"id": thread},
                    "approvalPolicy": approval, "sandbox": sandbox, "model": model}));
                start_mcp_servers(&out, &thread, &params);
            }
            "model/list" if no_catalog => emit(
                &out,
                json!({"id": id, "error": {"code": -32000, "message": "scripted catalog failure"}}),
            ),
            "model/list" => match params["cursor"].as_str() {
                None => reply(json!({"data": [
                    {"id": "fake-default", "model": "fake-default", "displayName": "Fake Default",
                     "description": "Scripted default", "hidden": false, "isDefault": true}
                ], "nextCursor": "page-2"})),
                Some("page-2") => reply(json!({"data": [
                    {"id": "fake-fast", "model": "fake-fast", "displayName": "Fake Fast",
                     "description": "Scripted second model", "hidden": false, "isDefault": false},
                    {"id": "fake-hidden", "model": "fake-hidden", "displayName": "Hidden",
                     "hidden": true, "isDefault": false}
                ], "nextCursor": null})),
                Some(_) => emit(
                    &out,
                    json!({"id": id, "error": {"code": -32600, "message": "invalid cursor"}}),
                ),
            },
            "skills/list" => {
                let cwd = params["cwds"][0].as_str().unwrap_or_default().to_owned();
                reply(json!({"data": [{"cwd": cwd, "skills": skills(&cwd)}]}));
            }
            "turn/start" => {
                let thread = params["threadId"].as_str().unwrap_or_default().to_owned();
                let input = params["input"].as_array().cloned().unwrap_or_default();
                let text = input
                    .iter()
                    .find(|item| item["type"] == "text")
                    .and_then(|item| item["text"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                let images =
                    images::record(&input.iter().filter_map(image_item).collect::<Vec<_>>());
                if !params["approvalPolicy"].is_null() {
                    policies.insert(
                        thread.clone(),
                        (
                            params["approvalPolicy"].clone(),
                            params["sandboxPolicy"].clone(),
                        ),
                    );
                    if let Some(path) = &state {
                        let _ = std::fs::write(path, serde_json::to_vec(&policies).unwrap());
                    }
                }
                let policy = policies.get(&thread).cloned().unwrap_or_else(defaults);
                let policy = format!(
                    "{} / {}",
                    policy.0.as_str().unwrap_or("?"),
                    policy.1["type"].as_str().unwrap_or("?")
                );
                let model = models
                    .get(&thread)
                    .cloned()
                    .unwrap_or_else(|| "fake-default".into());
                let server = servers.get(&thread).cloned();
                let widgets = widget_threads.contains(&thread);
                // Unique per process, like real Codex IDs, so a restart never reuses one.
                let turn_id = format!(
                    "fake-turn-{}-{}",
                    std::process::id(),
                    next.fetch_add(1, Ordering::Relaxed)
                );
                reply(json!({"turn": {"id": turn_id, "status": "inProgress"}}));
                let turn = Arc::new(Turn {
                    interrupt: AtomicBool::new(false),
                    silent: AtomicBool::new(false),
                    end_on_steer: text.contains("end on steer"),
                    steer: Mutex::new(None),
                    answers: Mutex::new(None),
                });
                turns.lock().unwrap().insert(turn_id.clone(), turn.clone());
                let (out, next, turns) = (out.clone(), next.clone(), turns.clone());
                thread::spawn(move || {
                    run_turn(
                        &out,
                        &next,
                        &thread,
                        &turn_id,
                        &text,
                        &images,
                        &policy,
                        &model,
                        server.as_ref(),
                        widgets,
                        &turn,
                        delay,
                    );
                    turns.lock().unwrap().remove(&turn_id);
                });
            }
            "turn/steer" => {
                let turn_id = params["expectedTurnId"].as_str().unwrap_or_default();
                let live = turns.lock().unwrap();
                if live.get(turn_id).is_some_and(|turn| turn.end_on_steer) {
                    // After a pause, so the client can change its mind meanwhile.
                    let (out, turns) = (out.clone(), turns.clone());
                    let (thread, turn_id) = (params["threadId"].clone(), turn_id.to_owned());
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(300));
                        if let Some(turn) = turns.lock().unwrap().remove(&turn_id) {
                            turn.silent.store(true, Ordering::SeqCst);
                            turn.interrupt.store(true, Ordering::SeqCst);
                        }
                        finish(
                            &out,
                            thread.as_str().unwrap_or_default(),
                            &turn_id,
                            "completed",
                        );
                        emit(
                            &out,
                            json!({"id": id, "error": {"code": -32600, "message": "no active turn"}}),
                        );
                    });
                    continue;
                }
                match live.get(turn_id) {
                    Some(turn) => {
                        let input = params["input"].as_array().cloned().unwrap_or_default();
                        let text = input
                            .iter()
                            .find(|item| item["type"] == "text")
                            .and_then(|item| item["text"].as_str())
                            .unwrap_or_default();
                        images::record(&input.iter().filter_map(image_item).collect::<Vec<_>>());
                        *turn.steer.lock().unwrap() = Some(text.to_owned());
                        reply(json!({"turnId": turn_id}));
                    }
                    None => emit(
                        &out,
                        json!({"id": id, "error": {"code": -32600, "message": "no active turn"}}),
                    ),
                }
            }
            "turn/interrupt" => {
                if let Some(turn) = turns
                    .lock()
                    .unwrap()
                    .get(params["turnId"].as_str().unwrap_or_default())
                {
                    turn.interrupt.store(true, Ordering::Relaxed);
                }
                reply(json!({}));
            }
            _ => emit(
                &out,
                json!({"id": id, "error": {"code": -32601, "message": "not scripted"}}),
            ),
        }
    }
}

/// The model a thread runs on: the one `params` asks for, else the one it already has,
/// else the scripted default.
fn thread_model(
    models: &mut std::collections::HashMap<String, String>,
    thread: &str,
    params: &Value,
) -> String {
    if let Some(model) = params["model"].as_str() {
        models.insert(thread.to_owned(), model.to_owned());
    }
    models
        .entry(thread.to_owned())
        .or_insert_with(|| "fake-default".into())
        .clone()
}

fn thread_server(
    servers: &mut std::collections::HashMap<String, orchestrator::Server>,
    thread: &str,
    params: &Value,
) {
    if let Some(config) = params["config"]["mcp_servers"].get("keystone") {
        if let Some(server) = orchestrator::Server::from_config(config, "http_headers") {
            servers.insert(thread.to_owned(), server);
        } else {
            servers.remove(thread);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_turn(
    out: &Out,
    next: &AtomicU64,
    thread: &str,
    turn_id: &str,
    text: &str,
    images: &str,
    policy: &str,
    model: &str,
    server: Option<&orchestrator::Server>,
    widgets: bool,
    turn: &Turn,
    delay: Duration,
) {
    let base = json!({"threadId": thread, "turnId": turn_id});
    let with = |extra: Value| {
        let mut value = base.clone();
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        value
    };
    notify(
        out,
        "turn/started",
        json!({"threadId": thread, "turn": {"id": turn_id, "status": "inProgress"}}),
    );
    let user = json!({"type": "userMessage", "id": format!("user-{turn_id}"), "content": [{"type": "text", "text": text}]});
    notify(out, "item/started", with(json!({"item": user})));
    notify(out, "item/completed", with(json!({"item": user})));

    if let Some(reset) = support_limit::limited_until(support_limit::codex_home()) {
        return limit(out, thread, turn_id, reset);
    }
    if text.contains("run") {
        let item_id = format!("exec-{turn_id}");
        let command = "echo scripted";
        let item = |status: &str, output: Value, exit: Value| {
            json!({"type": "commandExecution", "id": item_id, "command": command, "status": status,
                   "aggregatedOutput": output, "exitCode": exit})
        };
        notify(
            out,
            "item/started",
            with(json!({"item": item("inProgress", Value::Null, Value::Null)})),
        );
        let (tx, rx): (Sender<Value>, Receiver<Value>) = mpsc::channel();
        *turn.answers.lock().unwrap() = Some(tx);
        let request = next.fetch_add(1, Ordering::Relaxed) + 1000;
        emit(
            out,
            json!({"id": request, "method": "item/commandExecution/requestApproval",
                         "params": with(json!({"itemId": item_id, "command": command, "cwd": "/tmp",
                                               "availableDecisions": ["accept", "decline", "cancel"]}))}),
        );
        let decision = rx
            .recv()
            .ok()
            .and_then(|answer| answer["result"]["decision"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "cancel".into());
        *turn.answers.lock().unwrap() = None;
        notify(
            out,
            "serverRequest/resolved",
            json!({"threadId": thread, "requestId": request}),
        );
        if decision == "accept" {
            notify(
                out,
                "item/commandExecution/outputDelta",
                with(json!({"itemId": item_id, "delta": "scripted\n"})),
            );
            notify(
                out,
                "item/completed",
                with(json!({"item": item("completed", json!("scripted\n"), json!(0))})),
            );
        } else {
            notify(
                out,
                "item/completed",
                with(json!({"item": item("declined", Value::Null, Value::Null)})),
            );
            if decision == "cancel" {
                return finish(out, thread, turn_id, "interrupted");
            }
        }
    }

    let calls = widgets::calls(text);
    let mut results = Vec::new();
    for (n, arguments) in calls.iter().enumerate().filter(|_| widgets) {
        let call_id = format!("call-ui-{turn_id}-{n}");
        let item = |status: &str, success: Value, content: Value| {
            json!({"type": "dynamicToolCall", "id": call_id, "tool": "show_ui",
                "arguments": arguments, "status": status, "success": success,
                "contentItems": content})
        };
        notify(
            out,
            "item/started",
            with(json!({"item": item("inProgress", Value::Null, Value::Null)})),
        );
        let (tx, rx): (Sender<Value>, Receiver<Value>) = mpsc::channel();
        *turn.answers.lock().unwrap() = Some(tx);
        let request = next.fetch_add(1, Ordering::Relaxed) + 1000;
        emit(
            out,
            json!({"id": request, "method": "item/tool/call", "params": with(json!({
                "callId": call_id, "tool": "show_ui", "arguments": arguments}))}),
        );
        let answer = loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(answer) => break Some(answer),
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(mpsc::RecvTimeoutError::Timeout) if turn.interrupt.load(Ordering::Relaxed) => {
                    break None;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        *turn.answers.lock().unwrap() = None;
        let Some(answer) = answer else {
            notify(
                out,
                "serverRequest/resolved",
                json!({"threadId": thread, "requestId": request}),
            );
            notify(
                out,
                "item/completed",
                with(json!({"item": item("failed", json!(false), json!([]))})),
            );
            return finish(out, thread, turn_id, "interrupted");
        };
        let result = &answer["result"];
        notify(
            out,
            "item/completed",
            with(json!({"item": item(
                if result["success"] == true { "completed" } else { "failed" },
                result["success"].clone(),
                result["contentItems"].clone(),
            )})),
        );
        results.push(
            result["contentItems"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        );
    }
    let widget_reply = (!calls.is_empty()).then(|| widgets::reply(&results));
    let words = widget_reply.or_else(|| orchestrator::reply(text, server, turn_id)).or_else(|| std::env::var("KEYSTONE_FAKE_CODEX_REPLY").ok()).unwrap_or_else(|| {
        format!(
            "Scripted Codex reply (test data, no model call). You wrote: {text}. \
             This reply streams one word at a time so streaming, steering, and stopping can be checked. \
             **Approvals:** `{policy}` **Model:** `{model}`{images}"
        )
    });
    let mut steer = None;
    let message_id = format!("msg-{turn_id}");
    let streamed = stream(out, &with, &message_id, &words, turn, delay, &mut steer);
    let scripted_child = orchestrator::is_child_message(text);
    if scripted_child && steer.is_none() {
        orchestrator::hold_child(|| {
            if let Some(text) = turn.steer.lock().unwrap().take() {
                steer = Some(text);
                return true;
            }
            turn.interrupt.load(Ordering::Relaxed)
        });
    }
    if turn.silent.load(Ordering::SeqCst) {
        return;
    }
    // Like real Codex, an interrupted message gets no completion, only the turn's end.
    if turn.interrupt.load(Ordering::Relaxed) {
        return finish(out, thread, turn_id, "interrupted");
    }
    let item = json!({"type": "agentMessage", "id": message_id, "text": streamed});
    notify(out, "item/completed", with(json!({"item": item})));
    if let Some(steer) = steer {
        let steered_id = format!("msg-steer-{turn_id}");
        let text = format!("Steered (test data): {steer}");
        let item = json!({"type": "agentMessage", "id": steered_id, "text": ""});
        notify(out, "item/started", with(json!({"item": item})));
        notify(
            out,
            "item/agentMessage/delta",
            with(json!({"itemId": steered_id, "delta": text})),
        );
        if scripted_child {
            orchestrator::hold_child(|| turn.interrupt.load(Ordering::Relaxed));
            if turn.interrupt.load(Ordering::Relaxed) {
                return finish(out, thread, turn_id, "interrupted");
            }
        }
        let item = json!({"type": "agentMessage", "id": steered_id, "text": text});
        notify(out, "item/completed", with(json!({"item": item})));
    }
    if let Some(percent) = usage_percent(text) {
        emit_usage(out, thread, turn_id, percent);
    }
    finish(out, thread, turn_id, "completed");
}

/// The percentage a prompt such as "usage 85" asks the fake to report.
fn usage_percent(text: &str) -> Option<u64> {
    let rest = &text[text.find("usage ")? + "usage ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Synthetic, labelled limits for the account: the 5-hour window at `percent`,
/// resetting in two hours, and the weekly one at 20%, resetting in four days. The
/// context is half of `percent` of a 200,000-token window.
fn emit_usage(out: &Out, thread: &str, turn_id: &str, percent: u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    emit(
        out,
        json!({"method": "account/rateLimits/updated", "params": {"keystoneFake": "synthetic test data", "rateLimits": {
            "limitId": "codex", "limitName": null, "normalModelSlug": null,
            "primary": {"usedPercent": percent, "windowDurationMins": 300, "resetsAt": now + 2 * 3600},
            "secondary": {"usedPercent": 20, "windowDurationMins": 10080, "resetsAt": now + 4 * 86_400},
            "credits": null, "individualLimit": null, "spendControlReached": null,
            "planType": null, "rateLimitReachedType": null}}}),
    );
    let tokens = |total: u64| {
        json!({"totalTokens": total, "inputTokens": total, "cachedInputTokens": 0,
        "cacheWriteInputTokens": 0, "outputTokens": 0, "reasoningOutputTokens": 0})
    };
    emit(
        out,
        json!({"method": "thread/tokenUsage/updated", "params": {"threadId": thread, "turnId": turn_id,
            "tokenUsage": {"total": tokens(percent * 2000), "last": tokens(percent * 1000), "modelContextWindow": 200_000}}}),
    );
}

/// Streams `text` word by word until done, steered, or interrupted. Returns what was sent.
fn stream(
    out: &Out,
    with: &impl Fn(Value) -> Value,
    item_id: &str,
    text: &str,
    turn: &Turn,
    delay: Duration,
    steer: &mut Option<String>,
) -> String {
    let item = json!({"type": "agentMessage", "id": item_id, "text": ""});
    notify(out, "item/started", with(json!({"item": item})));
    let mut sent = String::new();
    for (i, word) in text.split(' ').enumerate() {
        if turn.interrupt.load(Ordering::Relaxed) {
            break;
        }
        if let Some(text) = turn.steer.lock().unwrap().take() {
            *steer = Some(text);
            break;
        }
        let delta = if i == 0 {
            word.to_owned()
        } else {
            format!(" {word}")
        };
        notify(
            out,
            "item/agentMessage/delta",
            with(json!({"itemId": item_id, "delta": delta})),
        );
        sent.push_str(&delta);
        if i == 0 && text.contains("crash") {
            let item = json!({"type": "agentMessage", "id": item_id, "text": text});
            if std::env::var("KEYSTONE_FAKE_CODEX_STORE").is_ok_and(|v| v == "1") {
                record("item/completed", &with(json!({"item": item})));
            }
            std::process::exit(3);
        }
        thread::sleep(delay);
    }
    sent
}

/// The account's usage limit refuses the turn, as Codex 0.163.0's schema describes it:
/// the account's synthetic, labelled 5-hour window at 100% resetting at `reset`, then
/// a failed turn whose error kind is `usageLimitExceeded`.
fn limit(out: &Out, thread: &str, turn_id: &str, reset: u64) {
    emit(
        out,
        json!({"method": "account/rateLimits/updated", "params": {"keystoneFake": "synthetic test data", "rateLimits": {
            "limitId": "codex", "limitName": null,
            "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": reset},
            "secondary": null, "rateLimitReachedType": "rate_limit_reached"}}}),
    );
    notify(
        out,
        "turn/completed",
        json!({"threadId": thread, "turn": {"id": turn_id, "status": "failed", "error": {
            "message": "You've hit your usage limit. (synthetic test data)",
            "codexErrorInfo": "usageLimitExceeded", "additionalDetails": null}}}),
    );
}

fn finish(out: &Out, thread: &str, turn_id: &str, status: &str) {
    notify(
        out,
        "turn/completed",
        json!({"threadId": thread, "turn": {"id": turn_id, "status": status, "error": null}}),
    );
}

/// A Codex `localImage` input item: its path, and the hash of the file there.
fn image_item(item: &Value) -> Option<Value> {
    if item["type"] != "localImage" {
        return None;
    }
    let path = item["path"].as_str().unwrap_or_default();
    let data = std::fs::read(path).ok();
    Some(
        json!({"provider": "codex", "path": path, "exists": data.is_some(),
        "sha256": data.as_deref().map(images::sha256), "bytes": data.map(|d| d.len())}),
    )
}
