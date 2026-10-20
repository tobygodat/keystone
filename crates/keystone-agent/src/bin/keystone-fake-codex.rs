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
//! `KEYSTONE_FAKE_ENV_RECEIPT` names a file each process appends its environment to, and
//! a `CODEX_HOME` that is not a directory fails the start, as the real CLI does.

use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[path = "support/orchestrator.rs"]
mod orchestrator;

type Out = Arc<Mutex<io::Stdout>>;

fn emit(out: &Out, message: Value) {
    let mut out = out.lock().unwrap();
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

fn notify(out: &Out, method: &str, params: Value) {
    emit(out, json!({"method": method, "params": params}));
}

struct Turn {
    interrupt: AtomicBool,
    /// Ended by the main loop; the turn's thread stops without reporting anything.
    silent: AtomicBool,
    end_on_steer: bool,
    steer: Mutex<Option<String>>,
    answers: Mutex<Option<Sender<String>>>,
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

    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to one of our approval requests.
            let decision = message["result"]["decision"]
                .as_str()
                .unwrap_or("cancel")
                .to_owned();
            for turn in turns.lock().unwrap().values() {
                if let Some(tx) = turn.answers.lock().unwrap().as_ref() {
                    let _ = tx.send(decision.clone());
                }
            }
            continue;
        };
        let reply = |result: Value| emit(&out, json!({"id": id, "result": result}));
        match method {
            "initialize" => {
                if let Some(path) = std::env::var_os("KEYSTONE_FAKE_CODEX_INITIALIZE_LOG") {
                    let _ = std::fs::write(path, params.to_string());
                }
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
            "thread/start" => {
                let n = next.fetch_add(1, Ordering::Relaxed);
                let thread = format!("fake-thread-{n}");
                let (approval, sandbox) = defaults();
                let model = thread_model(&mut models, &thread, &params);
                thread_server(&mut servers, &thread, &params);
                reply(json!({"thread": {"id": thread},
                    "approvalPolicy": approval, "sandbox": sandbox, "model": model}));
            }
            "thread/resume" => {
                let thread = params["threadId"].as_str().unwrap_or_default().to_owned();
                let (approval, sandbox) = policies.get(&thread).cloned().unwrap_or_else(defaults);
                let model = thread_model(&mut models, &thread, &params);
                thread_server(&mut servers, &thread, &params);
                reply(json!({"thread": {"id": thread},
                    "approvalPolicy": approval, "sandbox": sandbox, "model": model}));
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
            "turn/start" => {
                let thread = params["threadId"].as_str().unwrap_or_default().to_owned();
                let text = params["input"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
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
                        &policy,
                        &model,
                        server.as_ref(),
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
                        let text = params["input"][0]["text"].as_str().unwrap_or_default();
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
    policy: &str,
    model: &str,
    server: Option<&orchestrator::Server>,
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
        let (tx, rx): (Sender<String>, Receiver<String>) = mpsc::channel();
        *turn.answers.lock().unwrap() = Some(tx);
        let request = next.fetch_add(1, Ordering::Relaxed) + 1000;
        emit(
            out,
            json!({"id": request, "method": "item/commandExecution/requestApproval",
                         "params": with(json!({"itemId": item_id, "command": command, "cwd": "/tmp",
                                               "availableDecisions": ["accept", "decline", "cancel"]}))}),
        );
        let decision = rx.recv().unwrap_or_else(|_| "cancel".into());
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

    let words = orchestrator::reply(text, server, turn_id).or_else(|| std::env::var("KEYSTONE_FAKE_CODEX_REPLY").ok()).unwrap_or_else(|| {
        format!(
            "Scripted Codex reply (test data, no model call). You wrote: {text}. \
             This reply streams one word at a time so streaming, steering, and stopping can be checked. \
             **Approvals:** `{policy}` **Model:** `{model}`"
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
    finish(out, thread, turn_id, "completed");
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
        thread::sleep(delay);
    }
    sent
}

fn finish(out: &Out, thread: &str, turn_id: &str, status: &str) {
    notify(
        out,
        "turn/completed",
        json!({"threadId": thread, "turn": {"id": turn_id, "status": status, "error": null}}),
    );
}
