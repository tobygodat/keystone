use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{Handler, State, auth, protocol};

pub(super) const MAX_HEADER: usize = 16 * 1024;
pub(super) const MAX_BODY: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

struct Request {
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

pub(super) fn serve(mut stream: TcpStream, authority: &str, state: &State, handler: &Handler) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let request = match read_request(&mut stream, authority, state) {
        Ok(request) => request,
        Err((status, message)) => {
            respond(
                &mut stream,
                status,
                protocol::error(Value::Null, -32600, message),
            );
            return;
        }
    };
    let caller = request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| auth::authenticate(state, token));
    let Some(caller) = caller else {
        respond(
            &mut stream,
            401,
            protocol::error(Value::Null, -32001, "Unauthorized"),
        );
        return;
    };
    let value: Value = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(_) => {
            respond(
                &mut stream,
                400,
                protocol::error(Value::Null, -32700, "Parse error"),
            );
            return;
        }
    };
    if protocol::is_notification(&value) {
        let _ = stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n");
        return;
    }
    let clock = protocol::SystemClock::new(state);
    let response = protocol::dispatch(value, caller, handler, state, &clock);
    respond(&mut stream, 200, response);
}

type Failure = (u16, &'static str);

fn read_request(
    stream: &mut TcpStream,
    authority: &str,
    state: &State,
) -> Result<Request, Failure> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut bytes = Vec::new();
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let end = end + 4;
            if end > MAX_HEADER {
                return Err((431, "Headers too large"));
            }
            break end;
        }
        if bytes.len() >= MAX_HEADER {
            return Err((431, "Headers too large"));
        }
        read_chunk(stream, &mut bytes, deadline, state, 1024)?;
    };
    let head = &bytes[..end - 4];
    if !head.is_ascii() {
        return Err((400, "Invalid headers"));
    }
    let head = std::str::from_utf8(head).map_err(|_| (400, "Invalid headers"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or((400, "Missing request line"))?;
    let parts: Vec<_> = request_line.split(' ').collect();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" {
        return Err((400, "Invalid request line"));
    }
    if parts[1] != "/mcp" {
        return Err((404, "Not found"));
    }
    if parts[0] != "POST" {
        return Err((405, "Only POST is supported"));
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or((400, "Invalid header"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            || value
                .bytes()
                .any(|byte| byte < 0x20 && byte != b'\t' || byte == 0x7f)
        {
            return Err((400, "Invalid header"));
        }
        if headers
            .insert(
                name.to_ascii_lowercase(),
                value.trim_matches([' ', '\t']).to_owned(),
            )
            .is_some()
        {
            return Err((400, "Duplicate header"));
        }
    }
    if headers.get("host").map(String::as_str) != Some(authority) {
        return Err((403, "Invalid Host"));
    }
    if headers.contains_key("origin") {
        return Err((403, "Browser origins are not permitted"));
    }
    if headers.contains_key("transfer-encoding")
        || headers.contains_key("trailer")
        || headers.contains_key("expect")
    {
        return Err((400, "Unsupported framing"));
    }
    if !headers.get("content-type").is_some_and(|value| {
        value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("application/json")
    }) {
        return Err((415, "Expected application/json"));
    }
    let length = headers
        .get("content-length")
        .ok_or((411, "Content-Length required"))?;
    if length.is_empty() || !length.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err((400, "Invalid Content-Length"));
    }
    let length: usize = length.parse().map_err(|_| (413, "Body too large"))?;
    if length > MAX_BODY {
        return Err((413, "Body too large"));
    }
    let mut body = bytes.split_off(end);
    if body.len() > length {
        return Err((400, "Unexpected trailing bytes"));
    }
    let deadline = Instant::now() + READ_TIMEOUT;
    while body.len() < length {
        let remaining = length - body.len();
        read_chunk(stream, &mut body, deadline, state, remaining.min(8192))?;
    }
    Ok(Request { headers, body })
}

fn read_chunk(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    deadline: Instant,
    state: &State,
    max: usize,
) -> Result<(), Failure> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || state.stopped() {
            return Err((408, "Request timed out"));
        }
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_millis(200))))
            .map_err(|_| (400, "Read failed"))?;
        let mut chunk = [0; 8192];
        match stream.read(&mut chunk[..max]) {
            Ok(0) => return Err((400, "Truncated request")),
            Ok(count) => {
                bytes.extend_from_slice(&chunk[..count]);
                return Ok(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return Err((400, "Read failed")),
        }
    }
}

pub(super) fn respond(stream: &mut TcpStream, mut status: u16, value: Value) {
    let mut body = serde_json::to_vec(&value).unwrap();
    if body.len() > MAX_RESPONSE {
        status = 500;
        body = serde_json::to_vec(&protocol::error(Value::Null, -32603, "Response too large"))
            .unwrap();
    }
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let auth = if status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n{auth}\r\n",
        body.len()
    );
    let _ = stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(&body));
}
