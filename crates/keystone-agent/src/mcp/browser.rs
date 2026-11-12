//! The browser preview's tools. Each acts on the page of the caller's project folder,
//! which the Browser surface shows beside the session.

use serde_json::{Value, json};

/// Every browser tool's name.
pub const TOOLS: [&str; 7] = [
    "browser_open",
    "browser_status",
    "browser_navigate",
    "browser_snapshot",
    "browser_act",
    "browser_screenshot",
    "browser_evaluate",
];

/// The browser tools Claude runs without asking, while the address they load is this
/// machine's (see [`auto_allowed`]). `browser_evaluate` runs any script, which can reach
/// the network, so it goes through the provider's own approval.
pub const AUTO_ALLOWED: [&str; 6] = [
    "browser_open",
    "browser_status",
    "browser_navigate",
    "browser_snapshot",
    "browser_act",
    "browser_screenshot",
];

/// Whether Claude runs `tool` with `input` without asking. A browser tool that loads an
/// address on another machine goes through the provider's approval, as Claude's own
/// web fetches do in Ask mode, so an agent can't take the page to a site, or send data
/// to one in an address, unasked. A page's own links and redirects aren't checked.
pub fn auto_allowed(tool: &str, input: &Value) -> bool {
    AUTO_ALLOWED.contains(&tool)
        && input
            .get("url")
            .and_then(Value::as_str)
            .is_none_or(is_local)
}

/// Whether `url`, as the Browser surface would load it, names this machine. Anything
/// unusual counts as remote, so it asks rather than slipping through.
pub fn is_local(url: &str) -> bool {
    let Ok(url) = crate::browser::normalize_url(url) else {
        return false;
    };
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    // Browsers read a backslash as a slash, so `evil.com\@localhost` is evil.com.
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']') {
            Some((inner, "")) => return inner == "::1",
            Some((inner, port)) if port.starts_with(':') => return inner == "::1",
            _ => return false,
        },
        None => host.rsplit_once(':').map_or(host, |(host, _)| host),
    }
    .to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "0.0.0.0"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
}

/// A browser tool may wait for Chrome to start and a page to load.
pub const BUDGET_SECS: u64 = 40;

pub(super) fn tools() -> Vec<Value> {
    let url = json!({"type":"string","minLength":1,"description":"An http or https address. A bare localhost:PORT opens over http."});
    let reference = json!({"type":"string","minLength":1,"description":"An element ref such as e12 from the latest browser_snapshot."});
    vec![
        super::protocol::tool(
            "browser_open",
            "Open the Browser surface beside the caller's session in Keystone, starting the project's browser page if needed, and load url when given. Returns status and the project's dev servers (loopback servers whose process works in the project folder).",
            json!({"url": url}),
            json!([]),
        ),
        super::protocol::tool(
            "browser_status",
            "The project's browser page: url, title, loading, viewport, and its dev servers.",
            json!({}),
            json!([]),
        ),
        super::protocol::tool(
            "browser_navigate",
            "Load url, or go back, forward or reload, then wait up to 15 seconds for the load event. Give url or history, not both.",
            json!({"url": url, "history":{"type":"string","enum":["back","forward","reload"]}}),
            json!([]),
        ),
        super::protocol::tool(
            "browser_snapshot",
            "The page's accessibility outline. Controls carry refs like [ref=e12] for browser_act; refs last until the next snapshot or navigation.",
            json!({}),
            json!([]),
        ),
        super::protocol::tool(
            "browser_act",
            "Act on the page with trusted input. click and hover take ref, or x and y in CSS pixels; type inserts text into ref or the focused element, clear replaces its contents; press sends key, such as Enter, Tab, ArrowDown, a or Meta+a, to ref or the focused element; scroll wheels by delta_x and delta_y pixels over ref or the page centre; select picks value (an option's value or label) in a select ref. Refused for read-only callers and second opinions.",
            json!({
                "action":{"type":"string","enum":["click","hover","type","press","scroll","select"]},
                "ref": reference,
                "x":{"type":"number","minimum":0},
                "y":{"type":"number","minimum":0},
                "click_count":{"type":"integer","minimum":1,"maximum":3},
                "text":{"type":"string"},
                "clear":{"type":"boolean"},
                "key":{"type":"string","minLength":1},
                "value":{"type":"string","minLength":1},
                "delta_x":{"type":"number"},
                "delta_y":{"type":"number"}
            }),
            json!(["action"]),
        ),
        super::protocol::tool(
            "browser_screenshot",
            "A JPEG of the page's viewport.",
            json!({}),
            json!([]),
        ),
        super::protocol::tool(
            "browser_evaluate",
            "Run a JavaScript expression in the page, awaiting a promise, and return its JSON value (at most 64 KiB). Refused for read-only callers and second opinions.",
            json!({"expression":{"type":"string","minLength":1,"maxLength":65536}}),
            json!(["expression"]),
        ),
    ]
}
