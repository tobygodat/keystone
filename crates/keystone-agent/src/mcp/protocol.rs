use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Caller, Handler, HandlerError, State, auth};

pub(super) trait Clock {
    fn now(&self) -> Duration;
    fn wait(&self, duration: Duration);
}

pub(super) struct SystemClock<'a> {
    start: Instant,
    state: &'a State,
}

impl<'a> SystemClock<'a> {
    pub(super) fn new(state: &'a State) -> Self {
        Self {
            start: Instant::now(),
            state,
        }
    }
}

impl Clock for SystemClock<'_> {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn wait(&self, duration: Duration) {
        self.state.sleep(duration);
    }
}

pub(super) fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message}})
}

fn result(id: Value, value: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "result":value})
}

pub(super) fn is_notification(request: &Value) -> bool {
    request.is_object()
        && request.get("id").is_none()
        && request["jsonrpc"] == "2.0"
        && request["method"]
            .as_str()
            .is_some_and(|method| method.starts_with("notifications/"))
        && request.get("params").is_none_or(Value::is_object)
}

pub(super) fn dispatch(
    request: Value,
    caller: Caller,
    handler: &Handler,
    state: &State,
    clock: &dyn Clock,
) -> Value {
    let Some(object) = request.as_object() else {
        return error(Value::Null, -32600, "Invalid request");
    };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    if !object.contains_key("id")
        || !(id.is_null() || id.is_string() || id.as_i64().is_some() || id.as_u64().is_some())
        || object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !object.get("method").is_some_and(Value::is_string)
        || object
            .get("params")
            .is_some_and(|params| !params.is_object())
    {
        return error(Value::Null, -32600, "Invalid request");
    }
    if !auth::is_live(state, caller) {
        return error(id, -32001, "Caller revoked");
    }
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    match object["method"].as_str().unwrap() {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|version| {
                    matches!(
                        *version,
                        "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
                    )
                })
                .unwrap_or("2025-03-26");
            result(
                id,
                json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"keystone","version":env!("CARGO_PKG_VERSION")}}),
            )
        }
        "notifications/initialized" | "notifications/cancelled" => result(id, json!({})),
        "ping" => result(id, json!({})),
        "tools/list" => result(id, json!({"tools":tools()})),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return error(id, -32602, "Tool name required");
            };
            let Some(tool) = tools().into_iter().find(|tool| tool["name"] == name) else {
                return error(id, -32602, "Unknown tool");
            };
            let mut arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if name == "session_wait"
                && let Some(timeout) = arguments.get("timeout").and_then(Value::as_f64)
                && timeout > 50.0
            {
                arguments["timeout"] = json!(50);
            }
            if !valid_arguments(&arguments, &tool["inputSchema"]) {
                return error(id, -32602, "Invalid tool arguments");
            }
            let outcome = if name == "session_wait" {
                wait(caller, arguments, handler, state, clock)
            } else if auth::is_live(state, caller) {
                handler(caller, name, arguments, Duration::from_secs(5))
            } else {
                Err("Caller revoked".into())
            };
            let output = match outcome {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                Err(message) => {
                    let value = json!({"error":message.to_string()});
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":true})
                }
            };
            result(id, output)
        }
        _ => error(id, -32601, "Method not found"),
    }
}

fn wait(
    caller: Caller,
    arguments: Value,
    handler: &Handler,
    state: &State,
    clock: &dyn Clock,
) -> Result<Value, HandlerError> {
    let timeout = arguments
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(50.0)
        .clamp(0.0, 50.0);
    let start = clock.now();
    let deadline = start + Duration::from_secs_f64(timeout);
    let hard_deadline = start + Duration::from_secs(50);
    let mut last: Option<Value> = None;
    loop {
        if !auth::is_live(state, caller) {
            return Err("Caller revoked".into());
        }
        let remaining = deadline.saturating_sub(clock.now());
        if remaining < Duration::from_millis(100)
            && let Some(mut snapshot) = last.take()
        {
            clock.wait(remaining);
            if !auth::is_live(state, caller) {
                return Err("Caller revoked".into());
            }
            snapshot["timed_out"] = json!(true);
            return Ok(snapshot);
        }
        let budget = if last.is_none() {
            hard_deadline
                .saturating_sub(clock.now())
                .min(Duration::from_secs(5))
        } else {
            remaining.min(Duration::from_secs(5))
        };
        let outcome = handler(caller, "session_wait", arguments.clone(), budget);
        if !auth::is_live(state, caller) {
            return Err("Caller revoked".into());
        }
        let mut snapshot = match outcome {
            Ok(snapshot) => snapshot,
            Err(HandlerError::Deadline) if clock.now() >= deadline => {
                let Some(mut snapshot) = last else {
                    return Err(HandlerError::Deadline);
                };
                snapshot["timed_out"] = json!(true);
                return Ok(snapshot);
            }
            Err(error) => return Err(error),
        };
        let running = snapshot
            .get("running")
            .and_then(Value::as_bool)
            .ok_or("Wait snapshot must contain a running boolean")?;
        if !running {
            snapshot["timed_out"] = json!(false);
            return Ok(snapshot);
        }
        let remaining = deadline.saturating_sub(clock.now());
        if remaining.is_zero() {
            snapshot["timed_out"] = json!(true);
            return Ok(snapshot);
        }
        clock.wait(remaining.min(Duration::from_millis(100)));
        if !auth::is_live(state, caller) {
            return Err("Caller revoked".into());
        }
        if clock.now() >= deadline {
            snapshot["timed_out"] = json!(true);
            return Ok(snapshot);
        }
        last = Some(snapshot);
    }
}

fn valid_arguments(arguments: &Value, schema: &Value) -> bool {
    let Some(arguments) = arguments.as_object() else {
        return false;
    };
    if schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .any(|key| !arguments.contains_key(key.as_str().unwrap()))
    {
        return false;
    }
    let properties = schema["properties"].as_object().unwrap();
    for (key, value) in arguments {
        let Some(property) = properties.get(key) else {
            return false;
        };
        let valid_type = match property["type"].as_str() {
            Some("string") => value.as_str().is_some_and(|value| !value.trim().is_empty()),
            Some("integer") => value.as_u64().is_some(),
            Some("number") => value.as_f64().is_some(),
            _ => false,
        };
        if !valid_type
            || property
                .get("enum")
                .is_some_and(|values| !values.as_array().unwrap().contains(value))
            || property
                .get("minimum")
                .is_some_and(|min| value.as_f64().unwrap() < min.as_f64().unwrap())
            || property
                .get("maximum")
                .is_some_and(|max| value.as_f64().unwrap() > max.as_f64().unwrap())
        {
            return false;
        }
    }
    true
}

fn tool(name: &str, description: &str, properties: Value, required: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}

pub(super) fn tools() -> Vec<Value> {
    let session = json!({"type":"integer","minimum":1,"description":"Keystone session ID in the caller's project."});
    let message = json!({"type":"string","minLength":1});
    let request_id = json!({"type":"string","minLength":1,"description":"Stable unique ID for this write. Reuse only to retry the same request."});
    vec![
        tool(
            "capabilities",
            "List available providers, accounts, models and orchestration permissions.",
            json!({}),
            json!([]),
        ),
        tool(
            "session_launch",
            "Accept a new session in the caller's project; later startup failure is visible through read/wait. Startup survives the parent's turn ending, but rechecks its live generation and current approval ceiling. Account defaults to default; cwd must equal capabilities.project when supplied. Omitted mode inherits caller. Provider-settings callers are uncapped; Provider-settings targets inherit native config.",
            json!({
                "provider":{"type":"string","enum":["claude","codex"]},
                "account":{"type":"string","minLength":1},
                "model":{"type":"string","minLength":1},
                "mode":{"type":"string","enum":["provider","read-only","ask","auto","full-access"]},
                "cwd":{"type":"string","minLength":1},
                "message":message,
                "client_request_id":request_id
            }),
            json!(["provider", "message", "client_request_id"]),
        ),
        tool(
            "session_send",
            "Steer a running session in the caller's project immediately. Idle or unsteerable targets return busy; messages never queue or start a new turn.",
            json!({"session":session,"message":message,"client_request_id":request_id}),
            json!(["session", "message", "client_request_id"]),
        ),
        tool(
            "session_read",
            "Read transcript rows after an exclusive position. Limit defaults to 50, maximum 200; next does not advance past streaming rows, so poll it again for completion. Large text is marked truncated.",
            json!({"session":session,"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":200,"default":50}}),
            json!(["session"]),
        ),
        tool(
            "session_wait",
            "Wait for the session to stop running, for up to 50 seconds (default 50, zero polls once). The first authorized snapshot has a five-second dispatch budget even for shorter timeouts. Returns the latest snapshot with timed_out; loop when needed.",
            json!({"session":session,"timeout":{"type":"number","minimum":0,"maximum":50,"default":50}}),
            json!(["session"]),
        ),
        tool(
            "session_interrupt",
            "Interrupt the session's current turn in the caller's project, including Codex input waiting for thread startup. Work still queued on a connection worker returns busy. An idle target is unchanged.",
            json!({"session":session}),
            json!(["session"]),
        ),
    ]
}
