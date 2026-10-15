//! A scripted stand-in for `codex app-server`, for tests and UI verification.
//!
//! It speaks the same JSON-RPC subset Keystone uses and replies with labeled test
//! text. A prompt containing "run" asks approval for a command first. Set
//! `KEYSTONE_FAKE_CODEX_DELAY_MS` to change the pause between streamed words.

use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    steer: Mutex<Option<String>>,
    answers: Mutex<Option<Sender<String>>>,
}

fn main() {
    let delay = Duration::from_millis(
        std::env::var("KEYSTONE_FAKE_CODEX_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    );
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let next = Arc::new(AtomicU64::new(1));
    let turns: Arc<Mutex<std::collections::HashMap<String, Arc<Turn>>>> = Arc::default();

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
            "initialize" => reply(json!({"userAgent": "keystone-fake-codex"})),
            "initialized" => {}
            "thread/start" => {
                let n = next.fetch_add(1, Ordering::Relaxed);
                reply(json!({"thread": {"id": format!("fake-thread-{n}")}}));
            }
            "thread/resume" => reply(json!({"thread": {"id": params["threadId"]}})),
            "turn/start" => {
                let thread = params["threadId"].as_str().unwrap_or_default().to_owned();
                let text = params["input"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                // Unique per process, like real Codex IDs, so a restart never reuses one.
                let turn_id = format!(
                    "fake-turn-{}-{}",
                    std::process::id(),
                    next.fetch_add(1, Ordering::Relaxed)
                );
                reply(json!({"turn": {"id": turn_id, "status": "inProgress"}}));
                let turn = Arc::new(Turn {
                    interrupt: AtomicBool::new(false),
                    steer: Mutex::new(None),
                    answers: Mutex::new(None),
                });
                turns.lock().unwrap().insert(turn_id.clone(), turn.clone());
                let (out, next, turns) = (out.clone(), next.clone(), turns.clone());
                thread::spawn(move || {
                    run_turn(&out, &next, &thread, &turn_id, &text, &turn, delay);
                    turns.lock().unwrap().remove(&turn_id);
                });
            }
            "turn/steer" => {
                let turn_id = params["expectedTurnId"].as_str().unwrap_or_default();
                match turns.lock().unwrap().get(turn_id) {
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

fn run_turn(
    out: &Out,
    next: &AtomicU64,
    thread: &str,
    turn_id: &str,
    text: &str,
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

    let words = format!(
        "Scripted Codex reply (test data, no model call). You wrote: {text}. \
         This reply streams one word at a time so streaming, steering, and stopping can be checked."
    );
    let mut steer = None;
    let message_id = format!("msg-{turn_id}");
    let streamed = stream(out, &with, &message_id, &words, turn, delay, &mut steer);
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
