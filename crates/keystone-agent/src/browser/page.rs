//! One browser page: what the user watches in the Browser surface and what agents drive.

use super::cdp::Chrome;
use super::outline;
use crate::Wake;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

const CALL: Duration = Duration::from_secs(10);
/// How long a navigation waits for its page's load event.
pub const LOAD_WAIT: Duration = Duration::from_secs(15);
/// The most JSON an evaluation returns.
pub const MAX_VALUE: usize = 64 * 1024;

/// A screencast frame: the page's pixels as a JPEG.
pub struct Frame {
    pub seq: u64,
    pub jpeg: Vec<u8>,
}

/// The page's size in CSS pixels and its device scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

#[derive(Default)]
struct State {
    url: Mutex<String>,
    loading: AtomicBool,
    loads: Mutex<u64>,
    loaded: Condvar,
    frame: Mutex<Option<Arc<Frame>>>,
    frames: AtomicU64,
    refs: Mutex<Refs>,
    main_frame: Mutex<Option<String>>,
    /// What an agent last did here, shown in the surface until the user acts.
    note: Mutex<Option<String>>,
}

/// Snapshot refs. Each backend DOM node keeps its ref across snapshots, and a ref is
/// never given to another node, so a ref from an older snapshot names the same element
/// or fails. A navigation of the main frame forgets the nodes but not the count.
#[derive(Default)]
struct Refs {
    by_node: HashMap<i64, String>,
    by_ref: HashMap<String, i64>,
    next: u64,
}

impl Refs {
    fn name(&mut self, node: i64) -> String {
        if let Some(reference) = self.by_node.get(&node) {
            return reference.clone();
        }
        self.next += 1;
        let reference = format!("e{}", self.next);
        self.by_node.insert(node, reference.clone());
        self.by_ref.insert(reference.clone(), node);
        reference
    }

    fn forget(&mut self) {
        self.by_node.clear();
        self.by_ref.clear();
    }
}

static PAGES: AtomicU64 = AtomicU64::new(0);

pub struct Page {
    id: u64,
    /// Held for one whole agent or surface operation, so two never interleave, such as
    /// one's focus landing between another's focus and typing.
    operation: Mutex<()>,
    chrome: Arc<Chrome>,
    session: String,
    target: String,
    state: Arc<State>,
    viewport: Mutex<Viewport>,
    wake: Wake,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum History {
    Back,
    Forward,
    Reload,
}

/// One `browser_act` request.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Click {
        target: Target,
        count: u8,
    },
    Hover {
        target: Target,
    },
    Type {
        reference: Option<String>,
        text: String,
        clear: bool,
    },
    Press {
        reference: Option<String>,
        key: String,
    },
    Scroll {
        reference: Option<String>,
        dx: f64,
        dy: f64,
    },
    Select {
        reference: String,
        value: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Ref(String),
    Point(f64, f64),
}

impl Act {
    /// Reads `browser_act`'s arguments.
    pub fn parse(arguments: &Value) -> Result<Self, String> {
        let text = |key: &str| arguments[key].as_str().map(str::to_owned);
        let reference = text("ref");
        let need = |value: Option<String>, what: &str| {
            value.ok_or_else(|| format!("{} needs {what}", arguments["action"]))
        };
        let target = || match (&reference, arguments["x"].as_f64(), arguments["y"].as_f64()) {
            (Some(reference), None, None) => Ok(Target::Ref(reference.clone())),
            (None, Some(x), Some(y)) => Ok(Target::Point(x, y)),
            _ => Err(format!("{} needs ref, or x and y", arguments["action"])),
        };
        Ok(match arguments["action"].as_str().unwrap_or("") {
            "click" => Act::Click {
                target: target()?,
                count: arguments["click_count"].as_u64().unwrap_or(1).clamp(1, 3) as u8,
            },
            "hover" => Act::Hover { target: target()? },
            "type" => Act::Type {
                reference,
                text: text("text").unwrap_or_default(),
                clear: arguments["clear"].as_bool().unwrap_or(false),
            },
            "press" => Act::Press {
                reference,
                key: need(text("key"), "key")?,
            },
            "scroll" => Act::Scroll {
                reference,
                dx: arguments["delta_x"].as_f64().unwrap_or(0.),
                dy: arguments["delta_y"].as_f64().unwrap_or(0.),
            },
            "select" => Act::Select {
                reference: need(reference, "ref")?,
                value: need(text("value"), "value")?,
            },
            other => return Err(format!("Unknown action {other:?}")),
        })
    }
}

/// A typed address as a URL: `http` and `https` only, loopback hosts defaulting to http.
pub fn normalize_url(input: &str) -> Result<String, String> {
    const REFUSED: &str = "Only http and https addresses open here";
    let input = input.trim();
    if input.is_empty() || input.contains(char::is_whitespace) {
        return Err("Enter an http or https address".into());
    }
    if let Some((scheme, _)) = input.split_once("://") {
        return match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" => Ok(input.to_owned()),
            _ => Err(REFUSED.into()),
        };
    }
    if let Some(port) = input.strip_prefix(':') {
        return Ok(format!("http://localhost:{port}"));
    }
    let authority = input.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        Some(_) => return Err(REFUSED.into()),
        None => authority,
    };
    let local = matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "0.0.0.0")
        || host.ends_with(".localhost");
    Ok(format!(
        "{}://{input}",
        if local { "http" } else { "https" }
    ))
}

/// A key's DOM name, code and Windows key code, from a name such as `Enter`, `ArrowUp`,
/// GPUI's `up`, or a single character.
pub fn key_definition(key: &str) -> Option<(String, String, u32)> {
    let named =
        |name: &str, code: &str, key_code: u32| Some((name.to_owned(), code.to_owned(), key_code));
    match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => named("Enter", "Enter", 13),
        "tab" => named("Tab", "Tab", 9),
        "escape" | "esc" => named("Escape", "Escape", 27),
        "backspace" => named("Backspace", "Backspace", 8),
        "delete" => named("Delete", "Delete", 46),
        "arrowleft" | "left" => named("ArrowLeft", "ArrowLeft", 37),
        "arrowup" | "up" => named("ArrowUp", "ArrowUp", 38),
        "arrowright" | "right" => named("ArrowRight", "ArrowRight", 39),
        "arrowdown" | "down" => named("ArrowDown", "ArrowDown", 40),
        "home" => named("Home", "Home", 36),
        "end" => named("End", "End", 35),
        "pageup" => named("PageUp", "PageUp", 33),
        "pagedown" => named("PageDown", "PageDown", 34),
        "space" | " " => named(" ", "Space", 32),
        _ => {
            let mut chars = key.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            let (code, key_code) = if c.is_ascii_alphabetic() {
                (
                    format!("Key{}", c.to_ascii_uppercase()),
                    c.to_ascii_uppercase() as u32,
                )
            } else if c.is_ascii_digit() {
                (format!("Digit{c}"), c as u32)
            } else {
                (String::new(), 0)
            };
            Some((c.to_string(), code, key_code))
        }
    }
}

/// CDP modifier bits.
pub mod modifiers {
    pub const ALT: u8 = 1;
    pub const CONTROL: u8 = 2;
    pub const META: u8 = 4;
    pub const SHIFT: u8 = 8;
}

/// Splits `Control+Shift+a` into its modifier bits and key.
fn chord(spec: &str) -> Result<(u8, &str), String> {
    let (prefix, key) = match spec.rsplit_once('+') {
        Some((prefix, "")) => (prefix.trim_end_matches('+'), "+"),
        Some((prefix, key)) => (prefix, key),
        None => ("", spec),
    };
    let mut bits = 0;
    for part in prefix.split('+').filter(|p| !p.is_empty()) {
        bits |= match part.to_ascii_lowercase().as_str() {
            "alt" | "option" => modifiers::ALT,
            "control" | "ctrl" => modifiers::CONTROL,
            "meta" | "cmd" | "command" => modifiers::META,
            "shift" => modifiers::SHIFT,
            other => return Err(format!("Unknown modifier {other}")),
        };
    }
    Ok((bits, key))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mouse {
    Moved,
    Pressed,
    Released,
}

impl Page {
    /// Opens a blank page in `chrome` and starts its screencast. `wake` runs on each new
    /// frame and address.
    pub fn open(chrome: Arc<Chrome>, wake: Wake) -> Result<Self, String> {
        let target = chrome.call(
            None,
            "Target.createTarget",
            json!({"url": "about:blank"}),
            CALL,
        )?;
        let target = target["targetId"].as_str().ok_or("No target")?.to_owned();
        let session = chrome.call(
            None,
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
            CALL,
        )?;
        let session = session["sessionId"]
            .as_str()
            .ok_or("No session")?
            .to_owned();
        let state = Arc::new(State::default());
        *state.url.lock().unwrap() = "about:blank".into();
        let (listening, acking, ack_session, woken) = (
            state.clone(),
            Arc::downgrade(&chrome),
            session.clone(),
            wake.clone(),
        );
        chrome.listen(
            &session,
            Arc::new(move |method: &str, params: &Value| {
                match method {
                    "Page.screencastFrame" => {
                        if let Some(chrome) = acking.upgrade() {
                            chrome.send(
                                Some(&ack_session),
                                "Page.screencastFrameAck",
                                json!({"sessionId": params["sessionId"]}),
                            );
                        }
                        let Some(jpeg) = params["data"].as_str().and_then(|data| {
                            base64::engine::general_purpose::STANDARD.decode(data).ok()
                        }) else {
                            return;
                        };
                        let seq = listening.frames.fetch_add(1, Ordering::Relaxed) + 1;
                        *listening.frame.lock().unwrap() = Some(Arc::new(Frame { seq, jpeg }));
                    }
                    "Page.frameNavigated" if params["frame"].get("parentId").is_none() => {
                        *listening.main_frame.lock().unwrap() =
                            params["frame"]["id"].as_str().map(str::to_owned);
                        if let Some(url) = params["frame"]["url"].as_str() {
                            *listening.url.lock().unwrap() = url.to_owned();
                        }
                        listening.refs.lock().unwrap().forget();
                    }
                    "Page.navigatedWithinDocument" => {
                        if let Some(url) = params["url"].as_str() {
                            *listening.url.lock().unwrap() = url.to_owned();
                        }
                    }
                    "Page.frameStartedLoading" | "Page.frameStoppedLoading" => {
                        let main = listening.main_frame.lock().unwrap().clone();
                        if main.is_none() || params["frameId"].as_str() == main.as_deref() {
                            listening
                                .loading
                                .store(method == "Page.frameStartedLoading", Ordering::Release);
                        }
                    }
                    "Page.loadEventFired" => {
                        *listening.loads.lock().unwrap() += 1;
                        listening.loaded.notify_all();
                    }
                    // Headless Chrome leaves a dialog open until it is answered, and the
                    // page does nothing else meanwhile. Dismiss it, as Playwright does,
                    // but let `beforeunload` go so navigations finish.
                    "Page.javascriptDialogOpening" => {
                        let kind = params["type"].as_str().unwrap_or("dialog");
                        if let Some(chrome) = acking.upgrade() {
                            chrome.send(
                                Some(&ack_session),
                                "Page.handleJavaScriptDialog",
                                json!({"accept": kind == "beforeunload"}),
                            );
                        }
                        let message: String = params["message"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(120)
                            .collect();
                        *listening.note.lock().unwrap() =
                            Some(format!("Dismissed a {kind} dialog: “{message}”"));
                    }
                    _ => return,
                }
                woken();
            }),
        );
        let page = Self {
            id: PAGES.fetch_add(1, Ordering::Relaxed) + 1,
            operation: Mutex::new(()),
            chrome,
            session,
            target,
            state,
            viewport: Mutex::new(Viewport {
                width: 1280,
                height: 800,
                scale: 1.,
            }),
            wake,
        };
        page.call("Page.enable", json!({}))?;
        page.apply_viewport(*page.viewport.lock().unwrap())?;
        page.call("Page.startScreencast", screencast())?;
        Ok(page)
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.chrome.call(Some(&self.session), method, params, CALL)
    }

    fn send(&self, method: &str, params: Value) {
        self.chrome.send(Some(&self.session), method, params);
    }

    /// Unique to this page in this process, so frames of a replaced page are told apart.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Hold this for the whole of one navigation, snapshot, action or evaluation.
    pub fn operation(&self) -> MutexGuard<'_, ()> {
        self.operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn is_closed(&self) -> bool {
        self.chrome.is_closed()
    }

    pub fn url(&self) -> String {
        self.state.url.lock().unwrap().clone()
    }

    pub fn loading(&self) -> bool {
        self.state.loading.load(Ordering::Acquire)
    }

    pub fn frame(&self) -> Option<Arc<Frame>> {
        self.state.frame.lock().unwrap().clone()
    }

    pub fn viewport(&self) -> Viewport {
        *self.viewport.lock().unwrap()
    }

    pub fn note(&self) -> Option<String> {
        self.state.note.lock().unwrap().clone()
    }

    pub fn set_note(&self, note: Option<String>) {
        *self.state.note.lock().unwrap() = note;
    }

    /// The tab's target ID, which is also its main frame's ID.
    pub(super) fn target(&self) -> &str {
        &self.target
    }

    /// Shows a note the page's own doing earned, such as a refused download.
    pub(super) fn report(&self, note: String) {
        self.set_note(Some(note));
        (self.wake)();
    }

    /// A pop-up this page opened at `url` was closed, so nothing runs out of sight.
    /// An http or https one loads here instead, as one tab would.
    pub(super) fn popped_up(&self, url: Option<&str>) {
        match url.map(normalize_url) {
            Some(Ok(url)) => {
                self.send("Page.navigate", json!({"url": url}));
                self.report(format!("Opened a pop-up's page here: {url}"));
            }
            _ => self.report("Closed a pop-up window".into()),
        }
    }

    /// Sizes the page to the surface. Returns at once; the next frame has the new size.
    pub fn resize(&self, viewport: Viewport) {
        let mut current = self.viewport.lock().unwrap();
        if *current == viewport || viewport.width == 0 || viewport.height == 0 {
            return;
        }
        *current = viewport;
        self.send("Emulation.setDeviceMetricsOverride", metrics(viewport));
        // On Linux, Chrome sent no frame after a resize until the page next repainted,
        // so the surface kept showing the old size. Starting the screencast again sends
        // the resized page at once.
        self.send("Page.stopScreencast", json!({}));
        self.send("Page.startScreencast", screencast());
    }

    fn apply_viewport(&self, viewport: Viewport) -> Result<(), String> {
        self.call("Emulation.setDeviceMetricsOverride", metrics(viewport))
            .map(drop)
    }

    /// Loads `url` and waits for its load event, up to [`LOAD_WAIT`].
    pub fn navigate(&self, url: &str) -> Result<(), String> {
        let url = normalize_url(url)?;
        let before = *self.state.loads.lock().unwrap();
        let result = self.call("Page.navigate", json!({"url": url}))?;
        if let Some(error) = result["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(format!("{url} did not load: {error}"));
        }
        if result.get("loaderId").is_some() {
            self.wait_for_load(before, LOAD_WAIT);
        }
        Ok(())
    }

    pub fn go(&self, history: History) -> Result<(), String> {
        let before = *self.state.loads.lock().unwrap();
        match history {
            History::Reload => {
                self.call("Page.reload", json!({}))?;
            }
            History::Back | History::Forward => {
                let entries = self.call("Page.getNavigationHistory", json!({}))?;
                let index = entries["currentIndex"].as_i64().unwrap_or(0)
                    + if history == History::Back { -1 } else { 1 };
                let Some(entry) = entries["entries"]
                    .as_array()
                    .and_then(|entries| entries.get(usize::try_from(index).ok()?))
                else {
                    return Err(format!(
                        "There is no page to go {}",
                        if history == History::Back {
                            "back to"
                        } else {
                            "forward to"
                        }
                    ));
                };
                self.call(
                    "Page.navigateToHistoryEntry",
                    json!({"entryId": entry["id"]}),
                )?;
            }
        }
        self.wait_for_load(before, Duration::from_secs(5));
        Ok(())
    }

    fn wait_for_load(&self, before: u64, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let mut loads = self.state.loads.lock().unwrap();
        // Woken by each load; the slices notice a closed browser.
        while *loads == before && !self.is_closed() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            loads = self
                .state
                .loaded
                .wait_timeout(loads, left.min(Duration::from_millis(250)))
                .unwrap()
                .0;
        }
    }

    /// URL, title, loading state and viewport.
    pub fn status(&self) -> Result<Value, String> {
        let title = self.evaluate("document.title").unwrap_or(Value::Null);
        let viewport = self.viewport();
        Ok(json!({
            "url": self.url(),
            "title": title,
            "loading": self.loading(),
            "viewport": {"width": viewport.width, "height": viewport.height},
        }))
    }

    /// The accessibility outline with fresh refs.
    pub fn snapshot(&self) -> Result<Value, String> {
        let tree = self.call("Accessibility.getFullAXTree", json!({}))?;
        let outline = {
            let mut refs = self.state.refs.lock().unwrap();
            outline::outline(
                tree["nodes"].as_array().map_or(&[], Vec::as_slice),
                &mut |node| refs.name(node),
            )
        };
        let mut status = self.status()?;
        status["outline"] = json!(outline.text);
        if outline.truncated {
            status["truncated"] = json!(true);
        }
        Ok(status)
    }

    /// The viewport as a JPEG.
    pub fn screenshot(&self) -> Result<Vec<u8>, String> {
        let shot = self.call(
            "Page.captureScreenshot",
            json!({"format": "jpeg", "quality": 80}),
        )?;
        shot["data"]
            .as_str()
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
            .ok_or_else(|| "No screenshot".into())
    }

    /// Runs `expression` in the page and returns its JSON value.
    pub fn evaluate(&self, expression: &str) -> Result<Value, String> {
        let result = self.call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true,
                "timeout": 10_000,
            }),
        )?;
        if let Some(exception) = result.get("exceptionDetails") {
            let text = exception["exception"]["description"]
                .as_str()
                .or(exception["text"].as_str())
                .unwrap_or("The script threw");
            return Err(text.to_owned());
        }
        let value = &result["result"];
        let value = match value.get("value") {
            Some(value) => value.clone(),
            None => value["description"]
                .as_str()
                .map_or(Value::Null, |d| json!(d)),
        };
        if value.to_string().len() > MAX_VALUE {
            return Err(format!(
                "The value is larger than {MAX_VALUE} bytes of JSON"
            ));
        }
        Ok(value)
    }

    fn node(&self, reference: &str) -> Result<i64, String> {
        self.state
            .refs
            .lock()
            .unwrap()
            .by_ref
            .get(reference)
            .copied()
            .ok_or_else(|| format!("Unknown or stale ref {reference}; take a new browser_snapshot"))
    }

    /// The centre of `reference`'s element, scrolled into view, in CSS pixels.
    fn centre(&self, reference: &str) -> Result<(f64, f64), String> {
        let node = self.node(reference)?;
        let _ = self.call("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": node}));
        let model = self
            .call("DOM.getBoxModel", json!({"backendNodeId": node}))
            .map_err(|_| format!("{reference} has no box on screen; it may be hidden"))?;
        let quad: Vec<f64> = model["model"]["border"]
            .as_array()
            .map(|q| q.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        if quad.len() != 8 {
            return Err(format!(
                "{reference} has no box on screen; it may be hidden"
            ));
        }
        Ok((
            (quad[0] + quad[2] + quad[4] + quad[6]) / 4.,
            (quad[1] + quad[3] + quad[5] + quad[7]) / 4.,
        ))
    }

    fn point(&self, target: &Target) -> Result<(f64, f64), String> {
        match target {
            Target::Ref(reference) => self.centre(reference),
            Target::Point(x, y) => Ok((*x, *y)),
        }
    }

    fn focus(&self, reference: &str) -> Result<(), String> {
        let node = self.node(reference)?;
        self.call("DOM.focus", json!({"backendNodeId": node}))
            .map(drop)
            .map_err(|_| format!("{reference} cannot take focus"))
    }

    /// Performs an agent's action with trusted input. Returns what it did.
    pub fn act(&self, act: &Act) -> Result<Value, String> {
        match act {
            Act::Click { target, count } => {
                let (x, y) = self.point(target)?;
                self.mouse_call(Mouse::Moved, x, y, 0, 0)?;
                for click in 1..=*count {
                    self.mouse_call(Mouse::Pressed, x, y, click, 0)?;
                    self.mouse_call(Mouse::Released, x, y, click, 0)?;
                }
                Ok(json!({"x": x, "y": y}))
            }
            Act::Hover { target } => {
                let (x, y) = self.point(target)?;
                self.mouse_call(Mouse::Moved, x, y, 0, 0)?;
                Ok(json!({"x": x, "y": y}))
            }
            Act::Type {
                reference,
                text,
                clear,
            } => {
                if let Some(reference) = reference {
                    self.focus(reference)?;
                }
                if *clear {
                    self.evaluate(
                        "(() => { const e = document.activeElement; if (e && typeof e.select === 'function') e.select(); else document.execCommand('selectAll'); })()",
                    )?;
                    if text.is_empty() {
                        self.key_call("Backspace", None, 0)?;
                    }
                }
                if !text.is_empty() {
                    self.call("Input.insertText", json!({"text": text}))?;
                }
                Ok(json!({}))
            }
            Act::Press { reference, key } => {
                if let Some(reference) = reference {
                    self.focus(reference)?;
                }
                let (bits, name) = chord(key)?;
                let text = (bits & (modifiers::CONTROL | modifiers::META) == 0)
                    .then(|| key_definition(name).map(|(key, _, _)| key))
                    .flatten()
                    .map(|key| match key.as_str() {
                        "Enter" => "\r".to_owned(),
                        _ if key.chars().count() == 1 => key,
                        _ => String::new(),
                    })
                    .filter(|text| !text.is_empty());
                self.key_call(name, text.as_deref(), bits)?;
                Ok(json!({}))
            }
            Act::Scroll { reference, dx, dy } => {
                let (x, y) = match reference {
                    Some(reference) => self.centre(reference)?,
                    None => {
                        let viewport = self.viewport();
                        (
                            f64::from(viewport.width) / 2.,
                            f64::from(viewport.height) / 2.,
                        )
                    }
                };
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy}),
                )?;
                Ok(json!({}))
            }
            Act::Select { reference, value } => {
                let node = self.node(reference)?;
                let object = self.call("DOM.resolveNode", json!({"backendNodeId": node}))?;
                let result = self.call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object["object"]["objectId"],
                        "functionDeclaration": "function (v) { if (this.tagName !== 'SELECT') throw new Error('Not a select element'); const o = [...this.options].find(o => o.value === v || o.label === v || o.text.trim() === v); if (!o) throw new Error('No option ' + v); this.value = o.value; this.dispatchEvent(new Event('input', {bubbles: true})); this.dispatchEvent(new Event('change', {bubbles: true})); return o.value; }",
                        "arguments": [{"value": value}],
                        "returnByValue": true,
                    }),
                )?;
                if let Some(exception) = result.get("exceptionDetails") {
                    return Err(exception["exception"]["description"]
                        .as_str()
                        .unwrap_or("The option could not be chosen")
                        .to_owned());
                }
                Ok(json!({"selected": result["result"]["value"]}))
            }
        }
    }

    fn mouse_call(&self, kind: Mouse, x: f64, y: f64, count: u8, bits: u8) -> Result<(), String> {
        self.call(
            "Input.dispatchMouseEvent",
            mouse_params(kind, x, y, count, bits),
        )
        .map(drop)
    }

    fn key_call(&self, key: &str, text: Option<&str>, bits: u8) -> Result<(), String> {
        let (down, up) = key_params(key, text, bits).ok_or_else(|| format!("Unknown key {key}"))?;
        self.call("Input.dispatchKeyEvent", down)?;
        self.call("Input.dispatchKeyEvent", up).map(drop)
    }

    /// The user's pointer, from the surface. A move with `count` 1 drags. Returns at once.
    pub fn mouse(&self, kind: Mouse, x: f64, y: f64, count: u8, bits: u8) {
        self.send(
            "Input.dispatchMouseEvent",
            mouse_params(kind, x, y, count, bits),
        );
    }

    pub fn wheel(&self, x: f64, y: f64, dx: f64, dy: f64) {
        self.send(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy}),
        );
    }

    /// The user's key, from the surface: `key` as GPUI names it, with the text it types.
    pub fn key(&self, key: &str, text: Option<&str>, bits: u8) {
        if let Some((down, up)) = key_params(key, text, bits) {
            self.send("Input.dispatchKeyEvent", down);
            self.send("Input.dispatchKeyEvent", up);
        }
    }

    /// Types `text` as if pasted.
    pub fn insert_text(&self, text: &str) {
        self.send("Input.insertText", json!({"text": text}));
    }

    /// Runs an editing command such as `selectAll` on the focused element.
    pub fn command(&self, command: &str) {
        self.send(
            "Input.dispatchKeyEvent",
            json!({"type": "rawKeyDown", "key": "", "commands": [command]}),
        );
        self.send(
            "Input.dispatchKeyEvent",
            json!({"type": "keyUp", "key": ""}),
        );
    }

    /// Forgets this page's events and closes its tab.
    pub fn close(&self) {
        self.chrome.forget(&self.session);
        self.chrome
            .send(None, "Target.closeTarget", json!({"targetId": self.target}));
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.close();
    }
}

/// The screencast's settings: a JPEG frame of every repaint.
fn screencast() -> Value {
    json!({"format": "jpeg", "quality": 80, "everyNthFrame": 1})
}

fn metrics(viewport: Viewport) -> Value {
    json!({
        "width": viewport.width,
        "height": viewport.height,
        "deviceScaleFactor": viewport.scale,
        "mobile": false,
    })
}

fn mouse_params(kind: Mouse, x: f64, y: f64, count: u8, bits: u8) -> Value {
    let (kind, button, buttons) = match kind {
        Mouse::Moved if count == 0 => ("mouseMoved", "none", 0),
        Mouse::Moved => ("mouseMoved", "left", 1),
        Mouse::Pressed => ("mousePressed", "left", 1),
        Mouse::Released => ("mouseReleased", "left", 0),
    };
    json!({"type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": count, "modifiers": bits})
}

/// The key down and key up events for `key`.
fn key_params(key: &str, text: Option<&str>, bits: u8) -> Option<(Value, Value)> {
    let (name, code, key_code) = key_definition(key)?;
    let mut down = json!({
        "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
        "key": name,
        "code": code,
        "windowsVirtualKeyCode": key_code,
        "modifiers": bits,
    });
    if let Some(text) = text {
        down["text"] = json!(text);
        down["unmodifiedText"] = json!(text);
    }
    let up = json!({
        "type": "keyUp",
        "key": name,
        "code": code,
        "windowsVirtualKeyCode": key_code,
        "modifiers": bits,
    });
    Some((down, up))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_default_to_http_on_loopback_and_https_elsewhere() {
        assert_eq!(
            normalize_url("localhost:5173").unwrap(),
            "http://localhost:5173"
        );
        assert_eq!(
            normalize_url("127.0.0.1:3000/a?b").unwrap(),
            "http://127.0.0.1:3000/a?b"
        );
        assert_eq!(
            normalize_url("app.localhost").unwrap(),
            "http://app.localhost"
        );
        assert_eq!(normalize_url(":8080").unwrap(), "http://localhost:8080");
        assert_eq!(
            normalize_url("example.com/x").unwrap(),
            "https://example.com/x"
        );
        assert_eq!(
            normalize_url(" https://example.com ").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            normalize_url("HTTP://localhost").unwrap(),
            "HTTP://localhost"
        );
        for refused in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "chrome://settings",
            "",
            "a b",
        ] {
            assert!(normalize_url(refused).is_err(), "{refused} is refused");
        }
    }

    #[test]
    fn act_reads_each_action_and_refuses_missing_targets() {
        let parse = |v: Value| Act::parse(&v);
        assert_eq!(
            parse(json!({"action": "click", "ref": "e2"})).unwrap(),
            Act::Click {
                target: Target::Ref("e2".into()),
                count: 1
            }
        );
        assert_eq!(
            parse(json!({"action": "click", "x": 10.5, "y": 4, "click_count": 9})).unwrap(),
            Act::Click {
                target: Target::Point(10.5, 4.),
                count: 3
            }
        );
        assert_eq!(
            parse(json!({"action": "type", "ref": "e1", "text": "hi", "clear": true})).unwrap(),
            Act::Type {
                reference: Some("e1".into()),
                text: "hi".into(),
                clear: true
            }
        );
        assert_eq!(
            parse(json!({"action": "press", "key": "Enter"})).unwrap(),
            Act::Press {
                reference: None,
                key: "Enter".into()
            }
        );
        assert!(parse(json!({"action": "click"})).is_err());
        assert!(parse(json!({"action": "click", "ref": "e1", "x": 1, "y": 2})).is_err());
        assert!(parse(json!({"action": "select", "ref": "e1"})).is_err());
        assert!(parse(json!({"action": "drag", "ref": "e1"})).is_err());
    }

    #[test]
    fn keys_and_chords_map_to_dom_names() {
        assert_eq!(
            key_definition("enter").unwrap(),
            ("Enter".into(), "Enter".into(), 13)
        );
        assert_eq!(
            key_definition("up").unwrap(),
            ("ArrowUp".into(), "ArrowUp".into(), 38)
        );
        assert_eq!(
            key_definition("a").unwrap(),
            ("a".into(), "KeyA".into(), 65)
        );
        assert_eq!(
            key_definition("7").unwrap(),
            ("7".into(), "Digit7".into(), 55)
        );
        assert!(key_definition("f13x").is_none());
        assert_eq!(
            chord("Meta+Shift+z").unwrap(),
            (modifiers::META | modifiers::SHIFT, "z")
        );
        assert_eq!(chord("Control++").unwrap(), (modifiers::CONTROL, "+"));
        assert_eq!(chord("Enter").unwrap(), (0, "Enter"));
        assert!(chord("Hyper+a").is_err());
    }
}
