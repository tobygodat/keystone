use super::*;
use serde_json::json;
use std::io::{Read, Write};

fn handler() -> Handler {
    Arc::new(|caller, name, arguments, _| {
        Ok(json!({"caller":caller.session,"tool":name,"arguments":arguments,"running":false}))
    })
}

fn exchange(credentials: &Credentials, request: &str) -> (u16, Vec<u8>) {
    let authority = credentials
        .url()
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix("/mcp")
        .unwrap();
    let mut stream = TcpStream::connect(authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    // macOS fails this once the server has closed, which a busy server may do first.
    let _ = stream.shutdown(Shutdown::Write);
    let mut bytes = Vec::new();
    if let Err(error) = stream.read_to_end(&mut bytes) {
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    }
    let end = bytes
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    let head = std::str::from_utf8(&bytes[..end]).unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(bytes.len(), end + length, "incomplete HTTP response");
    (status, bytes[end..].to_vec())
}

fn request(credentials: &Credentials, body: &str, extra: &str) -> String {
    let authority = credentials
        .url()
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix("/mcp")
        .unwrap();
    format!(
        "POST /mcp HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}",
        credentials.token(),
        body.len()
    )
}

fn call(credentials: &Credentials, name: &str, arguments: Value) -> Value {
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}).to_string();
    let (status, body) = exchange(credentials, &request(credentials, &body, ""));
    assert_eq!(status, 200);
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn tokens_identify_their_own_session_and_rotation_revokes_only_old_generation() {
    let server = Server::start(handler()).unwrap();
    let issuer = server.issuer();
    let first = issuer.issue(1).unwrap();
    let second = issuer.issue(2).unwrap();
    assert_eq!(first.token().len(), 64);
    assert_ne!(first.token(), second.token());
    assert_eq!(
        call(&first, "session_read", json!({"session":2}))["result"]["structuredContent"]["caller"],
        1
    );
    assert_eq!(
        call(&second, "session_read", json!({"session":1}))["result"]["structuredContent"]["caller"],
        2
    );
    let old_caller = first.caller();
    let old_request = request(&first, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#, "");
    let replacement = issuer.issue(1).unwrap();
    assert!(!issuer.is_live(old_caller));
    assert_eq!(exchange(&replacement, &old_request).0, 401);
    drop(first);
    assert!(issuer.is_live(replacement.caller()));
    assert!(issuer.is_live(second.caller()));
    let revoked = second.caller();
    drop(second);
    assert!(!issuer.is_live(revoked));
    drop(server);
    assert!(!issuer.is_live(replacement.caller()));
    assert!(issuer.issue(3).is_err());
}

#[test]
fn current_live_turn_and_mode_are_required_at_authorization() {
    let server = Server::start(handler()).unwrap();
    let issuer = server.issuer();
    let credentials = issuer.issue(1).unwrap();
    let caller = credentials.caller();
    assert!(!issuer.authorize(caller, ApprovalMode::ReadOnly));
    issuer.set_running(1, true);
    for ceiling in ApprovalMode::ALL {
        issuer.set_mode(1, ceiling);
        for target in ApprovalMode::ALL {
            let allowed = target == ApprovalMode::Provider
                || ceiling == ApprovalMode::Provider
                || ApprovalMode::ALL.iter().position(|m| *m == target).unwrap()
                    <= ApprovalMode::ALL
                        .iter()
                        .position(|m| *m == ceiling)
                        .unwrap();
            assert_eq!(
                mode_allowed(target, ceiling),
                allowed,
                "{target:?} from {ceiling:?}"
            );
            assert_eq!(issuer.authorize(caller, target), allowed);
        }
    }
    issuer.set_mode(1, ApprovalMode::ReadOnly);
    assert!(!issuer.authorize(caller, ApprovalMode::FullAccess));
    issuer.set_running(1, false);
    assert!(!issuer.authorize(caller, ApprovalMode::Provider));
}

#[test]
fn notifications_are_empty_202_but_requests_have_json_responses() {
    let server = Server::start(handler()).unwrap();
    let credentials = server.issuer().issue(1).unwrap();
    let notice = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let (status, body) = exchange(&credentials, &request(&credentials, notice, ""));
    assert_eq!(status, 202);
    assert!(body.is_empty());
    let init = r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
    let (status, body) = exchange(&credentials, &request(&credentials, init, ""));
    assert_eq!(status, 200);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["id"], "init");
    assert_eq!(value["result"]["protocolVersion"], "2025-06-18");
    let idless = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"session_launch","arguments":{"provider":"codex","message":"never","client_request_id":"x"}}}"#;
    let (_, body) = exchange(&credentials, &request(&credentials, idless, ""));
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], -32600);
    assert!(value.get("result").is_none());
}

#[test]
fn transport_rejects_ambiguous_framing_browser_origins_and_wrong_authorities() {
    let server = Server::start(handler()).unwrap();
    let credentials = server.issuer().issue(1).unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for (extra, status) in [
        ("Origin: http://localhost\r\n", 403),
        ("Content-Length: 0\r\n", 400),
        ("Transfer-Encoding: chunked\r\n", 400),
        ("Expect: 100-continue\r\n", 400),
    ] {
        assert_eq!(
            exchange(&credentials, &request(&credentials, body, extra)).0,
            status
        );
    }
    let normal = request(&credentials, body, "");
    let authority = credentials
        .url()
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix("/mcp")
        .unwrap();
    assert_eq!(
        exchange(
            &credentials,
            &normal.replace(&format!("Host: {authority}"), "Host: attacker.invalid")
        )
        .0,
        403
    );
    assert_eq!(
        exchange(&credentials, &normal.replace("POST /mcp", "GET /mcp")).0,
        405
    );
    assert_eq!(
        exchange(&credentials, &normal.replace("POST /mcp", "POST /other")).0,
        404
    );
    assert_eq!(
        exchange(
            &credentials,
            &normal.replace("application/json", "text/plain")
        )
        .0,
        415
    );
    assert_eq!(
        exchange(
            &credentials,
            &normal.replace(&format!("Bearer {}", credentials.token()), "Bearer invalid")
        )
        .0,
        401
    );
    assert_eq!(
        exchange(&credentials, &request(&credentials, "not json", "")).0,
        400
    );
    let oversized = normal.replace(
        &format!("Content-Length: {}", body.len()),
        "Content-Length: 1048577",
    );
    assert_eq!(exchange(&credentials, &oversized).0, 413);
    let truncated = normal.replace(
        &format!("Content-Length: {}", body.len()),
        &format!("Content-Length: {}", body.len() + 1),
    );
    assert_eq!(exchange(&credentials, &truncated).0, 400);
}

struct Clock(Arc<AtomicU64>);

impl protocol::Clock for Clock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::Relaxed))
    }
    fn wait(&self, duration: Duration) {
        self.0
            .fetch_add(duration.as_millis() as u64, Ordering::Relaxed);
    }
}

#[test]
fn write_deadline_tells_the_caller_to_retry_with_the_same_request_id() {
    let server = Server::start(handler()).unwrap();
    let credentials = server.issuer().issue(1).unwrap();
    let expired: Handler = Arc::new(|_, _, _, _| Err(HandlerError::Deadline));
    let response = protocol::dispatch(
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"session_launch","arguments":{"provider":"codex","message":"child","client_request_id":"launch-1"}}}),
        credentials.caller(),
        &expired,
        &server.issuer.state,
        &Clock(Arc::new(AtomicU64::new(0))),
    );
    assert_eq!(response["result"]["isError"], true);
    let error = response["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap();
    assert!(error.contains("may still have taken effect"), "{error}");
    assert!(error.contains("same client_request_id"), "{error}");
}

#[test]
fn wait_returns_authorized_snapshot_despite_short_deadline() {
    let server = Server::start(handler()).unwrap();
    let credentials = server.issuer().issue(1).unwrap();
    for timeout in [0.005, 0.01, 0.25, 0.5, 50.0] {
        let clock = Clock(Arc::new(AtomicU64::new(0)));
        let time = clock.0.clone();
        let delayed: Handler = Arc::new(move |_, _, _, budget| {
            let latency = Duration::from_millis(20);
            time.fetch_add(budget.min(latency).as_millis() as u64, Ordering::Relaxed);
            if budget < latency {
                return Err("dispatch deadline".into());
            }
            Ok(json!({"running":true,"session":2,"status":"running"}))
        });
        let response = protocol::dispatch(
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"session_wait","arguments":{"session":2,"timeout":timeout}}}),
            credentials.caller(),
            &delayed,
            &server.issuer.state,
            &clock,
        );
        assert_eq!(
            response["result"]["isError"], false,
            "{timeout}: {response}"
        );
        assert_eq!(response["result"]["structuredContent"]["timed_out"], true);
        assert_eq!(response["result"]["structuredContent"]["session"], 2);
        assert!(clock.0.load(Ordering::Relaxed) <= 50_000);
    }
}

#[test]
fn wait_only_recovers_dispatch_expiry_after_an_authorized_snapshot() {
    let server = Server::start(handler()).unwrap();
    let credentials = server.issuer().issue(1).unwrap();
    for error in ["deadline", "unknown target", "no first snapshot"] {
        let clock = Clock(Arc::new(AtomicU64::new(0)));
        let time = clock.0.clone();
        let delayed: Handler = Arc::new(move |_, _, _, budget| {
            if time.load(Ordering::Relaxed) == 0 && error != "no first snapshot" {
                time.fetch_add(20, Ordering::Relaxed);
                return Ok(json!({"running":true,"session":2,"status":"running"}));
            }
            time.fetch_add(budget.as_millis() as u64, Ordering::Relaxed);
            if error == "unknown target" {
                Err(error.into())
            } else {
                Err(HandlerError::Deadline)
            }
        });
        let response = protocol::dispatch(
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"session_wait","arguments":{"session":2,"timeout":0.5}}}),
            credentials.caller(),
            &delayed,
            &server.issuer.state,
            &clock,
        );
        assert_eq!(
            response["result"]["isError"],
            error != "deadline",
            "{error}: {response}"
        );
        if error == "deadline" {
            assert_eq!(response["result"]["structuredContent"]["session"], 2);
            assert_eq!(response["result"]["structuredContent"]["timed_out"], true);
        } else {
            assert!(
                response["result"]["structuredContent"]
                    .get("running")
                    .is_none()
            );
        }
        assert!(clock.0.load(Ordering::Relaxed) <= 50_000);
    }
}

#[test]
fn wait_caps_at_fifty_seconds_including_dispatch_budget_and_zero_polls_once() {
    let server = Server::start(handler()).unwrap();
    let credentials = server.issuer().issue(1).unwrap();
    let clock = Clock(Arc::new(AtomicU64::new(0)));
    let time = clock.0.clone();
    let calls = Arc::new(AtomicU64::new(0));
    let called = calls.clone();
    let handler: Handler = Arc::new(move |_, _, _, budget| {
        called.fetch_add(1, Ordering::Relaxed);
        assert!(budget <= Duration::from_secs(5));
        time.fetch_add(
            budget.min(Duration::from_millis(30)).as_millis() as u64,
            Ordering::Relaxed,
        );
        Ok(json!({"running":true}))
    });
    let request = |timeout| json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"session_wait","arguments":{"session":2,"timeout":timeout}}});
    let response = protocol::dispatch(
        request(500),
        credentials.caller(),
        &handler,
        &server.issuer.state,
        &clock,
    );
    assert_eq!(response["result"]["structuredContent"]["timed_out"], true);
    assert_eq!(clock.0.load(Ordering::Relaxed), 50_000);
    let before = calls.load(Ordering::Relaxed);
    let response = protocol::dispatch(
        request(0),
        credentials.caller(),
        &handler,
        &server.issuer.state,
        &clock,
    );
    assert_eq!(response["result"]["structuredContent"]["timed_out"], true);
    assert_eq!(calls.load(Ordering::Relaxed), before + 1);
    let invalid = protocol::dispatch(
        request(-1),
        credentials.caller(),
        &handler,
        &server.issuer.state,
        &clock,
    );
    assert_eq!(invalid["error"]["code"], -32602);
}

#[test]
fn wait_returns_completed_or_revoked_instead_of_starting_any_work() {
    let server = Server::start(handler()).unwrap();
    let credentials = server.issuer().issue(1).unwrap();
    let caller = credentials.caller();
    let clock = Clock(Arc::new(AtomicU64::new(0)));
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"session_wait","arguments":{"session":2,"timeout":50}}});
    let done = protocol::dispatch(
        request.clone(),
        caller,
        &handler(),
        &server.issuer.state,
        &clock,
    );
    assert_eq!(done["result"]["structuredContent"]["timed_out"], false);
    let held = Arc::new(Mutex::new(Some(credentials)));
    let handler: Handler = Arc::new(move |_, _, _, _| {
        held.lock().unwrap().take();
        Ok(json!({"running":true}))
    });
    let revoked = protocol::dispatch(request, caller, &handler, &server.issuer.state, &clock);
    assert_eq!(revoked["result"]["isError"], true);
    assert!(
        revoked["result"]["structuredContent"]["error"]
            .as_str()
            .unwrap()
            .contains("revoked")
    );
}

#[test]
fn bounded_connections_and_shutdown_close_partial_requests() {
    let server = Server::start(handler()).unwrap();
    let issuer = server.issuer();
    let credentials = issuer.issue(1).unwrap();
    let authority = credentials
        .url()
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix("/mcp")
        .unwrap();
    let mut clients = Vec::new();
    for _ in 0..MAX_CLIENTS {
        clients.push(TcpStream::connect(authority).unwrap());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while issuer.state.clients.lock().unwrap().len() < MAX_CLIENTS {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        exchange(&credentials, &request(&credentials, "{}", "")).0,
        503
    );
    let start = std::time::Instant::now();
    drop(server);
    assert!(start.elapsed() < Duration::from_secs(1));
    for mut client in clients {
        // macOS refuses socket options on a connection the server has already reset.
        let _ = client.set_read_timeout(Some(Duration::from_secs(1)));
        let mut bytes = Vec::new();
        let _ = client.read_to_end(&mut bytes);
    }
    assert!(!issuer.is_live(credentials.caller()));
}

#[test]
fn browser_tools_get_a_longer_budget_and_an_image_becomes_an_image_block() {
    let server = Server::start(Arc::new(|_, name, arguments, budget| {
        Ok(match name {
            "browser_screenshot" => json!({"url":"http://localhost:5173/","image":{"mime_type":"image/jpeg","data":"/9j/AA=="}}),
            _ => json!({"tool":name,"arguments":arguments,"budget":budget.as_secs()}),
        })
    }))
    .unwrap();
    let caller = server.issuer().issue(1).unwrap();
    let listed = {
        let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string();
        let (_, body) = exchange(&caller, &request(&caller, &body, ""));
        serde_json::from_slice::<Value>(&body).unwrap()
    };
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        browser::TOOLS.iter().all(|tool| names.contains(tool)),
        "{names:?}"
    );

    let shot = &call(&caller, "browser_screenshot", json!({}))["result"];
    assert_eq!(
        shot["structuredContent"],
        json!({"url":"http://localhost:5173/"})
    );
    assert_eq!(
        shot["content"][1],
        json!({"type":"image","data":"/9j/AA==","mimeType":"image/jpeg"})
    );
    let act = &call(
        &caller,
        "browser_act",
        json!({"action":"type","ref":"e2","text":"","clear":true}),
    )["result"]["structuredContent"];
    assert_eq!(act["budget"], browser::BUDGET_SECS);
    assert_eq!(act["arguments"]["clear"], true);
    assert_eq!(
        call(&caller, "session_read", json!({"session":2}))["result"]["structuredContent"]["budget"],
        5
    );
    for refused in [
        json!({"action":"type","clear":"yes"}),
        json!({"action":"drag"}),
        json!({"action":"click","ref":""}),
    ] {
        assert_eq!(
            call(&caller, "browser_act", refused)["error"]["message"],
            "Invalid tool arguments"
        );
    }
}

#[test]
fn only_this_machines_addresses_load_without_asking() {
    for local in [
        "localhost:5173",
        ":3000",
        "http://127.0.0.1:8080/app?x=1",
        "http://127.8.0.2/",
        "http://[::1]:4000/",
        "http://app.localhost/",
        "HTTP://LOCALHOST/",
        "0.0.0.0:8000",
    ] {
        assert!(browser::is_local(local), "{local}");
    }
    for remote in [
        "example.com",
        "https://example.com/?leak=1",
        "http://localhost@example.com/",
        "http://example.com\\@localhost/",
        "http://localhost.example.com/",
        "http://[::ffff:7f00:1]/",
        "http://192.168.1.4:3000/",
        "file:///etc/hosts",
        "",
    ] {
        assert!(!browser::is_local(remote), "{remote}");
    }
    assert!(browser::auto_allowed("browser_snapshot", &json!({})));
    assert!(browser::auto_allowed("browser_open", &json!({})));
    assert!(browser::auto_allowed(
        "browser_navigate",
        &json!({"url": "localhost:5173"})
    ));
    assert!(!browser::auto_allowed(
        "browser_navigate",
        &json!({"url": "https://example.com"})
    ));
    assert!(!browser::auto_allowed(
        "browser_open",
        &json!({"url": "https://example.com"})
    ));
    assert!(!browser::auto_allowed(
        "browser_evaluate",
        &json!({"expression": "1"})
    ));
}
