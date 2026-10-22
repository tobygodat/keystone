use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub const CHILD_MESSAGE: &str = "long Scripted child reply";
pub const CHILD_STEER: &str = "long Scripted child reply steered";

pub fn is_child_message(text: &str) -> bool {
    matches!(text, CHILD_MESSAGE | CHILD_STEER)
}

pub fn hold_child(mut finished: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Clone)]
pub struct Server {
    address: SocketAddr,
    authority: String,
    url: String,
    authorization: String,
}

impl Server {
    pub fn from_config(config: &Value, header_key: &str) -> Option<Self> {
        let url = config["url"].as_str()?;
        let (authority, path) = url.strip_prefix("http://")?.split_once('/')?;
        let address: SocketAddr = authority.parse().ok()?;
        let authorization = config[header_key]["Authorization"].as_str()?;
        if !address.ip().is_loopback()
            || address.port() == 0
            || path != "mcp"
            || !authorization.starts_with("Bearer ")
            || authorization.len() <= 7
            || !authorization
                .bytes()
                .all(|byte| (0x20..0x7f).contains(&byte))
        {
            return None;
        }
        Some(Self {
            address,
            authority: authority.into(),
            url: url.into(),
            authorization: authorization.into(),
        })
    }

    fn call(&self, name: &str, arguments: Value) -> Result<Value, &'static str> {
        let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}})
        .to_string();
        let mut stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(2))
            .map_err(|_| "connection failed")?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|_| "connection setup failed")?;
        write!(stream, "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.authority, self.authorization, body.len(), body)
            .map_err(|_| "request failed")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bytes = Vec::new();
        let end = loop {
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                if end > 16 * 1024 {
                    return Err("response headers too large");
                }
                break end + 4;
            }
            if bytes.len() > 16 * 1024 {
                return Err("response headers too large");
            }
            read_chunk(&mut stream, &mut bytes, deadline, 1024)?;
        };
        let header = std::str::from_utf8(&bytes[..end]).map_err(|_| "invalid response headers")?;
        let mut lines = header.split("\r\n");
        if !lines
            .next()
            .is_some_and(|line| line.starts_with("HTTP/1.1 200 "))
        {
            return Err("HTTP request rejected");
        }
        let mut length = None;
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').ok_or("invalid response headers")?;
            if name.eq_ignore_ascii_case("transfer-encoding") {
                return Err("unsupported response framing");
            }
            if name.eq_ignore_ascii_case("content-length") {
                if length.is_some() {
                    return Err("duplicate response length");
                }
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| "invalid response length")?,
                );
            }
        }
        let length = length
            .filter(|length| *length <= 1024 * 1024)
            .ok_or("invalid response length")?;
        let mut body = bytes.split_off(end);
        while body.len() < length {
            let remaining = length - body.len();
            read_chunk(&mut stream, &mut body, deadline, remaining.min(8192))?;
        }
        if body.len() != length {
            return Err("invalid response body");
        }
        let response: Value = serde_json::from_slice(&body).map_err(|_| "invalid response JSON")?;
        if response["id"] != 1 || response.get("error").is_some() {
            return Err("MCP request rejected");
        }
        let result = response.get("result").ok_or("missing MCP result")?;
        if result["isError"] == true {
            let error = result["structuredContent"]["error"]
                .as_str()
                .unwrap_or_default();
            return Err(if error.contains("ceiling") {
                "ceiling refused"
            } else if error.contains("busy") {
                "busy"
            } else {
                "tool failed"
            });
        }
        result
            .get("structuredContent")
            .cloned()
            .ok_or("missing structured result")
    }

    fn redact(&self, text: &str) -> String {
        text.replace(&self.url, "[redacted]")
            .replace(&self.authorization, "[redacted]")
            .replace(
                self.authorization.strip_prefix("Bearer ").unwrap(),
                "[redacted]",
            )
    }
}

fn read_chunk(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    deadline: Instant,
    max: usize,
) -> Result<(), &'static str> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("response timed out");
    }
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|_| "connection setup failed")?;
    let mut chunk = [0; 8192];
    let count = stream
        .read(&mut chunk[..max])
        .map_err(|_| "response read failed")?;
    if count == 0 {
        return Err("truncated response");
    }
    bytes.extend_from_slice(&chunk[..count]);
    Ok(())
}

pub fn reply(text: &str, server: Option<&Server>, request: &str) -> Option<String> {
    let target = match (
        text.strip_prefix("orchestrator test: launch in "),
        text.strip_prefix("orchestrator test: launch in new worktree "),
    ) {
        (_, Some(name)) => Some(json!({"worktree": name})),
        (Some(cwd), None) => Some(json!({"cwd": cwd})),
        (None, None) => None,
    };
    if let Some(mut launch) = target {
        launch["provider"] = json!("claude");
        launch["mode"] = json!("read-only");
        launch["message"] = json!("Scripted worktree child");
        launch["client_request_id"] = json!(format!("scripted-worktree-{request}"));
        let result = server
            .ok_or("missing loopback MCP configuration")
            .and_then(|server| server.call("session_launch", launch));
        return Some(match result {
            Ok(child) => format!(
                "Scripted orchestrator success: launched session {} (test data, no model call).",
                child["session"]
            ),
            Err(error) => {
                format!("Scripted orchestrator failure: {error} (test data, no model call).")
            }
        });
    }
    let provider = match text {
        "orchestrator test: launch codex" => "codex",
        "orchestrator test: launch claude" => "claude",
        _ => return None,
    };
    let result = server
        .ok_or("missing loopback MCP configuration")
        .and_then(|server| {
            scenario(provider, request, |name, arguments| {
                server.call(name, arguments)
            })
            .map(|text| server.redact(&text))
        });
    Some(match result {
        Ok(text) => format!("Scripted orchestrator success: {text}"),
        Err(error) => format!("Scripted orchestrator failure: {error} (test data, no model call)."),
    })
}

fn scenario(
    provider: &str,
    request: &str,
    mut call: impl FnMut(&str, Value) -> Result<Value, &'static str>,
) -> Result<String, &'static str> {
    let capabilities = call("capabilities", json!({}))?;
    let launch = json!({"provider":provider,"account":"default","mode":"read-only",
        "message":CHILD_MESSAGE,"client_request_id":format!("scripted-child-{request}")});
    let ceiling = match capabilities["mode_ceiling"].as_str() {
        Some("read-only" | "ask" | "auto") => {
            let mut denied = launch.clone();
            denied["mode"] = json!("full-access");
            denied["client_request_id"] = json!(format!("scripted-denied-{request}"));
            if call("session_launch", denied) != Err("ceiling refused") {
                return Err("broader launch was not refused");
            }
            "refused"
        }
        Some("provider" | "full-access") => "uncapped",
        _ => return Err("invalid caller ceiling"),
    };
    let child = call("session_launch", launch.clone())?;
    let session = child["session"]
        .as_u64()
        .filter(|session| *session > 0)
        .ok_or("missing child session")?;
    if child["sent"] != true {
        return Err("child message was not sent");
    }
    let duplicate = call("session_launch", launch)?;
    if duplicate["session"] != session || duplicate["sent"] != true {
        return Err("duplicate child session differs");
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut attempt = 0;
    loop {
        let sent = call(
            "session_send",
            json!({"session":session,"message":CHILD_STEER,
            "client_request_id":format!("scripted-steer-{request}-{attempt}")}),
        );
        match sent {
            Ok(sent) if sent["sent"] == true => break,
            Err("busy") if Instant::now() < deadline => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err("busy") => return Err("child did not become steer-ready"),
            Err(error) => return Err(error),
            _ => return Err("active child send was not acknowledged"),
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let read = call("session_read", json!({"session":session,"limit":100}))?;
        if child_text(&read)?.contains("Scripted child reply steered") {
            break;
        }
        if Instant::now() >= deadline {
            return Err("child did not receive the active steer");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let active = call("session_wait", json!({"session":session,"timeout":0}))?;
    if active["running"] != true || active["timed_out"] != true {
        return Err("steered child was not active before interrupt");
    }
    let interrupted = call("session_interrupt", json!({"session":session}))?;
    if interrupted["interrupted"] != true {
        return Err("active interrupt was not acknowledged");
    }
    let waited = call("session_wait", json!({"session":session,"timeout":5}))?;
    if waited["running"] != false || waited["timed_out"] != false {
        return Err("child did not become idle");
    }
    let read = call("session_read", json!({"session":session,"limit":100}))?;
    let child_text = child_text(&read)?;
    if !child_text.contains("Scripted child reply steered") {
        return Err("missing scripted child reply");
    }
    if call(
        "session_send",
        json!({"session":session,"message":"Scripted idle send must not run",
        "client_request_id":format!("scripted-send-{request}")}),
    ) != Err("busy")
    {
        return Err("idle child send was not busy");
    }
    let after_send = call("session_wait", json!({"session":session,"timeout":0}))?;
    if after_send["running"] != false || after_send["timed_out"] != false {
        return Err("idle send started work");
    }
    let interrupted = call("session_interrupt", json!({"session":session}))?;
    if interrupted["interrupted"] != true {
        return Err("idle interrupt was not acknowledged");
    }
    let after_interrupt = call("session_wait", json!({"session":session,"timeout":0}))?;
    if after_interrupt["running"] != false || after_interrupt["timed_out"] != false {
        return Err("idle interrupt started work");
    }
    let unchanged = call("session_read", json!({"session":session,"limit":100}))?;
    if unchanged["rows"] != read["rows"] || unchanged["running"] != false {
        return Err("idle operations changed the child transcript");
    }
    Ok(format!(
        "child session={session}; duplicate_same_id=true; active_send=ok; active_interrupt=ok; idle_send=busy; idle_interrupt=ok; ceiling={ceiling}; child text: {child_text} (test data, no model call)."
    ))
}

fn child_text(read: &Value) -> Result<String, &'static str> {
    Ok(read["rows"]
        .as_array()
        .ok_or("missing child transcript")?
        .iter()
        .filter(|row| row["item"]["kind"]["type"] == "agent_message")
        .filter_map(|row| row["item"]["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    fn config(url: &str) -> Value {
        json!({"url":url,"headers":{"Authorization":"Bearer private-test-token"}})
    }

    #[test]
    fn only_exact_triggers_activate_the_scenario() {
        for text in [
            "hello",
            "orchestrator test: launch codex\n",
            "Orchestrator test: launch claude",
            "orchestrator test: launch codex extra",
        ] {
            assert!(reply(text, None, "parent-1").is_none());
        }
        for provider in ["codex", "claude"] {
            assert!(
                reply(
                    &format!("orchestrator test: launch {provider}"),
                    None,
                    "parent-1"
                )
                .unwrap()
                .starts_with("Scripted orchestrator failure:")
            );
        }
    }

    #[test]
    fn configuration_rejects_remote_addresses_and_header_injection() {
        for url in [
            "https://127.0.0.1:1/mcp",
            "http://example.com:1/mcp",
            "http://192.0.2.1:1/mcp",
            "http://127.0.0.1:0/mcp",
            "http://127.0.0.1:1/mcp\r\nInjected: yes",
        ] {
            assert!(Server::from_config(&config(url), "headers").is_none());
        }
        let mut injected = config("http://127.0.0.1:1/mcp");
        injected["headers"]["Authorization"] = json!("Bearer private\r\nInjected: yes");
        assert!(Server::from_config(&injected, "headers").is_none());
        assert!(Server::from_config(&config("http://[::1]:1/mcp"), "headers").is_some());
    }

    #[test]
    fn scenario_checks_both_directions_and_each_permission_ceiling() {
        for provider in ["codex", "claude"] {
            for ceiling in ["provider", "full-access", "read-only", "ask", "auto"] {
                let mut launches = Vec::new();
                let mut operations = Vec::new();
                let mut active_sends = Vec::new();
                let mut interrupts = 0;
                let mut reads = 0;
                let mut waits = 0;
                let text = scenario(provider, "parent-123", |name, args| {
                    operations.push(name.to_owned());
                    match name {
                        "capabilities" => Ok(json!({"mode_ceiling":ceiling})),
                        "session_launch" => {
                            assert_eq!(args["provider"], provider);
                            assert_eq!(args["account"], "default");
                            assert_eq!(args["message"], CHILD_MESSAGE);
                            if args["mode"] == "full-access" {
                                assert!(!["provider", "full-access"].contains(&ceiling));
                                return Err("ceiling refused");
                            }
                            assert_eq!(args["mode"], "read-only");
                            assert_eq!(args["client_request_id"], "scripted-child-parent-123");
                            launches.push(args);
                            Ok(json!({"session":42,"sent":true}))
                        }
                        "session_wait" => {
                            assert_eq!(args["session"], 42);
                            waits += 1;
                            let running = interrupts == 0;
                            assert_eq!(args["timeout"], if waits == 2 { 5 } else { 0 });
                            Ok(json!({"running":running,"timed_out":running}))
                        }
                        "session_read" => {
                            reads += 1;
                            if reads == 1 {
                                return Ok(json!({"running":true,"rows":[]}));
                            }
                            Ok(json!({"running":interrupts == 0,"rows":[
                                {"item":{"kind":{"type":"agent_message"},"text":"Claude test reply: Scripted child reply steered"}}
                            ]}))
                        }
                        "session_send" if args["message"] == CHILD_STEER => {
                            active_sends.push(args["client_request_id"].clone());
                            if active_sends.len() < 3 { return Err("busy"); }
                            Ok(json!({"session":42,"sent":true,"delivery":"steer-requested"}))
                        }
                        "session_send" => Err("busy"),
                        "session_interrupt" => {
                            assert_eq!(args, json!({"session":42}));
                            interrupts += 1;
                            Ok(json!({"interrupted":true}))
                        }
                        _ => panic!("unexpected tool"),
                    }
                }).unwrap();
                assert_eq!(launches.len(), 2);
                assert_eq!(launches[0], launches[1]);
                assert_eq!(active_sends.len(), 3);
                assert_eq!(
                    active_sends
                        .iter()
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    3
                );
                assert_eq!(interrupts, 2);
                assert!(text.contains(
                    "child session=42; duplicate_same_id=true; active_send=ok; active_interrupt=ok; idle_send=busy; idle_interrupt=ok"
                ));
                assert!(text.contains("Scripted child reply"));
                assert_eq!(
                    &operations[operations.len() - 5..],
                    &[
                        "session_send",
                        "session_wait",
                        "session_interrupt",
                        "session_wait",
                        "session_read"
                    ]
                );
            }
        }
    }

    #[test]
    fn duplicate_launch_must_return_the_same_child() {
        let mut launches = 0;
        let result = scenario("codex", "parent-1", |name, _| match name {
            "capabilities" => Ok(json!({"mode_ceiling":"provider"})),
            "session_launch" => {
                launches += 1;
                Ok(json!({"session":launches,"sent":true}))
            }
            _ => panic!("scenario continued after mismatched duplicate"),
        });
        assert_eq!(result, Err("duplicate child session differs"));
    }

    #[test]
    fn http_client_sends_authenticated_json_and_reads_a_fragmented_keepalive_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (done, received) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut header = String::new();
            loop {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                header.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(header.starts_with("POST /mcp HTTP/1.1\r\n"));
            assert!(header.contains(&format!("Host: {address}\r\n")));
            assert!(header.contains("Authorization: Bearer private-test-token\r\n"));
            assert!(header.contains("Content-Type: application/json\r\n"));
            let length: usize = header
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                request["params"],
                json!({"name":"capabilities","arguments":{}})
            );
            let body = json!({"jsonrpc":"2.0","id":1,"result":{"isError":false,"structuredContent":{"mode_ceiling":"provider"}}}).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                body.len()
            );
            for chunk in response.as_bytes().chunks(7) {
                stream.write_all(chunk).unwrap();
            }
            received.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        let server =
            Server::from_config(&config(&format!("http://{address}/mcp")), "headers").unwrap();
        assert_eq!(
            server.call("capabilities", json!({})).unwrap(),
            json!({"mode_ceiling":"provider"})
        );
        done.send(()).unwrap();
        worker.join().unwrap();
        let redacted = server.redact(&format!(
            "{} {} private-test-token",
            server.url, server.authorization
        ));
        assert_eq!(redacted, "[redacted] [redacted] [redacted]");
    }
}
