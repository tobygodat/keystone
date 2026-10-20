use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};

use futures_channel::oneshot;
use rquickjs::{Context, Function, Object, Runtime};

const MAX_SOURCE: usize = 32_768;
const MAX_SVG: usize = 1_048_576;
const MAX_CACHE: usize = 8 * 1_048_576;
const MAX_ENTRIES: usize = 32;
const RENDER_TIME: Duration = Duration::from_millis(750);
const WAIT_TIME: Duration = Duration::from_secs(4);
const BUNDLE: &str = include_str!("../../vendor/pele/pele.global.js");

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Palette {
    pub variables: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RenderRequest {
    pub source: String,
    pub width: u32,
    pub palette: Palette,
    pub font_family: String,
    pub font_family_mono: String,
}

#[derive(Clone, Debug)]
pub struct RenderedDiagram {
    pub svg: Arc<[u8]>,
    pub width: f32,
    pub height: f32,
}

pub type MeasureText = dyn Fn(&str, f32, u8) -> f32 + Send + Sync;

#[derive(Clone)]
pub struct TextMeasurer {
    pub cache_key: u64,
    pub measure: Arc<MeasureText>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagramError {
    pub message: String,
    pub line: usize,
    pub column: usize,
    kind: ErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorKind {
    Permanent,
    Transient,
    Runtime,
    RenderTimeout,
}

impl DiagramError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            line: 1,
            column: 1,
            kind: ErrorKind::Permanent,
        }
    }

    fn transient(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Transient,
            ..Self::new(message)
        }
    }

    pub fn is_transient(&self) -> bool {
        matches!(self.kind, ErrorKind::Transient | ErrorKind::RenderTimeout)
    }

    pub fn request_timeout() -> Self {
        Self::transient(
            "Diagram request timed out while waiting for the queue or renderer; retry after other diagrams finish",
        )
    }

    fn resets_runtime(&self) -> bool {
        matches!(self.kind, ErrorKind::Runtime | ErrorKind::RenderTimeout)
    }
}

impl fmt::Display for DiagramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (line {}, column {})",
            self.message, self.line, self.column
        )
    }
}

impl std::error::Error for DiagramError {}

struct Job {
    request: RenderRequest,
    measurer: Option<TextMeasurer>,
    cancel: Arc<AtomicBool>,
    expires: Instant,
    reply: oneshot::Sender<Result<RenderedDiagram, DiagramError>>,
}

#[derive(Debug)]
pub struct RenderTicket {
    response: oneshot::Receiver<Result<RenderedDiagram, DiagramError>>,
}

impl Future for RenderTicket {
    type Output = Result<RenderedDiagram, DiagramError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.response).poll(cx).map(|result| {
            result.unwrap_or_else(|_| {
                Err(DiagramError::new(
                    "Diagram worker stopped; restart the application",
                ))
            })
        })
    }
}

/// Enqueues bounded work without waiting for the worker. The caller owns its timeout.
pub fn submit_async(
    request: RenderRequest,
    cancel: Arc<AtomicBool>,
    measurer: TextMeasurer,
) -> Result<RenderTicket, DiagramError> {
    enqueue(request, cancel, Some(measurer))
}

/// Blocks the calling thread; never call this on the UI thread.
pub fn render(
    request: RenderRequest,
    cancel: Arc<AtomicBool>,
) -> Result<RenderedDiagram, DiagramError> {
    wait_for_ticket(enqueue(request, cancel.clone(), None)?, cancel)
}

/// The trusted host callback runs on the worker. It must not block on the UI thread.
/// Style bits are bold=1, italic=2, mono=4. Change cache_key when metrics change.
pub fn render_with_measurer(
    request: RenderRequest,
    cancel: Arc<AtomicBool>,
    measurer: TextMeasurer,
) -> Result<RenderedDiagram, DiagramError> {
    wait_for_ticket(submit_async(request, cancel.clone(), measurer)?, cancel)
}

fn enqueue(
    mut request: RenderRequest,
    cancel: Arc<AtomicBool>,
    measurer: Option<TextMeasurer>,
) -> Result<RenderTicket, DiagramError> {
    validate_request(&mut request)?;
    check_cancel(&cancel)?;
    static WORKER: OnceLock<Result<SyncSender<Job>, DiagramError>> = OnceLock::new();
    let sender = WORKER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::sync_channel::<Job>(8);
            std::thread::Builder::new()
                .name("diagram-quickjs".into())
                .spawn(move || {
                    let mut engine: Option<Engine> = None;
                    let mut layouts = Cache::default();
                    let mut images = Cache::default();
                    for job in receiver {
                        let result = process(&job, &mut engine, &mut layouts, &mut images);
                        let _ = job.reply.send(result);
                    }
                })
                .map_err(|e| DiagramError::new(format!("Cannot start diagram worker: {e}")))?;
            Ok(sender)
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let (reply, response) = oneshot::channel();
    let job = Job {
        request,
        measurer,
        cancel: cancel.clone(),
        expires: Instant::now() + WAIT_TIME,
        reply,
    };
    match sender.try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            return Err(DiagramError::transient(
                "Diagram queue is full; try again after other diagrams finish",
            ));
        }
        Err(TrySendError::Disconnected(_)) => {
            return Err(DiagramError::new(
                "Diagram worker stopped; restart the application",
            ));
        }
    }
    Ok(RenderTicket { response })
}

fn wait_for_ticket(
    mut ticket: RenderTicket,
    cancel: Arc<AtomicBool>,
) -> Result<RenderedDiagram, DiagramError> {
    let deadline = Instant::now() + WAIT_TIME;
    loop {
        check_cancel(&cancel)?;
        match ticket.response.try_recv() {
            Ok(Some(result)) => return result,
            Err(_) => {
                return Err(DiagramError::new(
                    "Diagram worker stopped; restart the application",
                ));
            }
            Ok(None) if Instant::now() >= deadline => {
                cancel.store(true, Ordering::Relaxed);
                return Err(DiagramError::request_timeout());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), DiagramError> {
    if cancel.load(Ordering::Relaxed) {
        Err(DiagramError::new("Diagram rendering cancelled"))
    } else {
        Ok(())
    }
}

fn validate_request(request: &mut RenderRequest) -> Result<(), DiagramError> {
    if request.source.len() > MAX_SOURCE {
        return Err(DiagramError::new(
            "Diagram exceeds 32768 UTF-8 bytes; split it into smaller diagrams",
        ));
    }
    if !(64..=4096).contains(&request.width) {
        return Err(DiagramError::new(
            "Diagram width must be between 64 and 4096 pixels",
        ));
    }
    for font in [&request.font_family, &request.font_family_mono] {
        if font.is_empty()
            || font.len() > 128
            || !font
                .chars()
                .all(|c| c.is_alphanumeric() || " -_,".contains(c))
        {
            return Err(DiagramError::new(
                "Diagram UI and monospace fonts must be plain font family names (at most 128 bytes)",
            ));
        }
    }
    if request.palette.variables.len() > 32 {
        return Err(DiagramError::new("Diagram palette has more than 32 colors"));
    }
    for (name, value) in &request.palette.variables {
        if !name.starts_with("--pele-")
            || name.len() > 48
            || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || !matches!(value.len(), 4 | 7 | 9)
            || !value.starts_with('#')
            || !value[1..].bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(DiagramError::new(
                "Diagram palette requires --pele-* names and CSS hex colors",
            ));
        }
    }
    request.palette.variables.sort();
    if request
        .palette
        .variables
        .windows(2)
        .any(|p| p[0].0 == p[1].0)
    {
        return Err(DiagramError::new(
            "Diagram palette has duplicate color names",
        ));
    }
    for (line, text) in request.source.lines().enumerate() {
        let text = text.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
        if text.starts_with("---") || text.contains("%%{") {
            return Err(DiagramError { message: "Diagram frontmatter and directives are disabled; remove configuration from the source".into(), line: line + 1, column: 1, kind: ErrorKind::Permanent });
        }
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
struct LayoutKey {
    source: String,
    width: u32,
    font: String,
    font_mono: String,
    metrics: Option<u64>,
}

#[derive(Clone, Debug)]
struct Layout {
    svg: Arc<str>,
    width: f32,
    height: f32,
}

struct Cache<K, V> {
    entries: VecDeque<(K, V, usize)>,
    bytes: usize,
}

impl<K, V> Default for Cache<K, V> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
        }
    }
}

impl<K: PartialEq, V: Clone> Cache<K, V> {
    fn get(&mut self, key: &K) -> Option<V> {
        let index = self.entries.iter().position(|(k, _, _)| k == key)?;
        let entry = self.entries.remove(index)?;
        let value = entry.1.clone();
        self.entries.push_back(entry);
        Some(value)
    }

    fn insert(&mut self, key: K, value: V, bytes: usize) {
        if bytes > MAX_CACHE {
            return;
        }
        while self.bytes + bytes > MAX_CACHE || self.entries.len() >= MAX_ENTRIES {
            if let Some((_, _, size)) = self.entries.pop_front() {
                self.bytes -= size;
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.entries.push_back((key, value, bytes));
    }
}

fn process(
    job: &Job,
    engine: &mut Option<Engine>,
    layouts: &mut Cache<LayoutKey, Layout>,
    images: &mut Cache<(LayoutKey, Palette), RenderedDiagram>,
) -> Result<RenderedDiagram, DiagramError> {
    check_cancel(&job.cancel)?;
    if Instant::now() >= job.expires {
        return Err(DiagramError::transient(
            "Diagram expired in the queue; retry",
        ));
    }
    let key = LayoutKey {
        source: job.request.source.clone(),
        width: job.request.width,
        font: job.request.font_family.clone(),
        font_mono: job.request.font_family_mono.clone(),
        metrics: job.measurer.as_ref().map(|m| m.cache_key),
    };
    let image_key = (key.clone(), job.request.palette.clone());
    if let Some(image) = images.get(&image_key) {
        return Ok(image);
    }
    let (layout, fresh) = if let Some(layout) = layouts.get(&key) {
        (layout, false)
    } else {
        if engine.is_none() {
            *engine = Some(Engine::new()?);
        }
        let result = engine.as_ref().unwrap().layout(job);
        if engine.as_ref().unwrap().interrupted.load(Ordering::Relaxed)
            || result.as_ref().is_err_and(|error| error.resets_runtime())
        {
            *engine = None;
        }
        let layout = result?;
        (layout, true)
    };
    let svg = sanitize_svg(
        &layout.svg,
        &job.request.palette,
        &job.request.font_family,
        &job.request.font_family_mono,
    )?;
    if fresh {
        layouts.insert(
            key.clone(),
            layout.clone(),
            key.source.len() + key.font.len() + key.font_mono.len() + layout.svg.len(),
        );
    }
    check_cancel(&job.cancel)?;
    let image = RenderedDiagram {
        svg: Arc::from(svg.into_bytes()),
        width: layout.width,
        height: layout.height,
    };
    let bytes = key.source.len() + key.font.len() + key.font_mono.len() + image.svg.len() + 4096;
    images.insert(image_key, image.clone(), bytes);
    Ok(image)
}

struct Budget {
    deadline: Instant,
    cancel: Arc<AtomicBool>,
}

struct Engine {
    context: Context,
    runtime: Runtime,
    budget: Arc<Mutex<Option<Budget>>>,
    interrupted: Arc<AtomicBool>,
}

impl Engine {
    fn new() -> Result<Self, DiagramError> {
        let runtime = Runtime::new().map_err(js_error)?;
        runtime.set_memory_limit(64 * 1_048_576);
        runtime.set_max_stack_size(512 * 1024);
        let budget: Arc<Mutex<Option<Budget>>> = Arc::new(Mutex::new(Some(Budget {
            deadline: Instant::now() + RENDER_TIME,
            cancel: Arc::new(AtomicBool::new(false)),
        })));
        let interrupt = budget.clone();
        let interrupted = Arc::new(AtomicBool::new(false));
        let interrupt_flag = interrupted.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let Ok(guard) = interrupt.lock() else {
                interrupt_flag.store(true, Ordering::Relaxed);
                return true;
            };
            let abort = guard
                .as_ref()
                .is_some_and(|b| b.cancel.load(Ordering::Relaxed) || Instant::now() >= b.deadline);
            if abort {
                interrupt_flag.store(true, Ordering::Relaxed);
            }
            abort
        })));
        let context = Context::full(&runtime).map_err(js_error)?;
        let initialized = context.with(|ctx| {
            ctx.eval::<(), _>(BUNDLE)?;
            ctx.eval::<(), _>(WRAPPER)
        });
        if let Err(error) = initialized {
            return Err(if interrupted.load(Ordering::Relaxed) {
                DiagramError {
                    kind: ErrorKind::RenderTimeout,
                    ..DiagramError::new(
                        "Diagram runtime initialization exceeded 750 ms; retry when the application is less busy",
                    )
                }
            } else {
                js_error(error)
            });
        }
        *budget.lock().unwrap() = None;
        Ok(Self {
            context,
            runtime,
            budget,
            interrupted,
        })
    }

    fn layout(&self, job: &Job) -> Result<Layout, DiagramError> {
        self.interrupted.store(false, Ordering::Relaxed);
        *self.budget.lock().unwrap() = Some(Budget {
            deadline: Instant::now() + RENDER_TIME,
            cancel: job.cancel.clone(),
        });
        let result = self.context.with(|ctx| -> Result<Layout, DiagramError> {
            let calls = Arc::new(AtomicUsize::new(0));
            let bytes = Arc::new(AtomicUsize::new(0));
            let measurer = job.measurer.clone();
            let cancel = job.cancel.clone();
            let callback = Function::new(
                ctx.clone(),
                move |text: String, size: f32, style: u8| -> f32 {
                    if calls.fetch_add(1, Ordering::Relaxed) >= 4096
                        || bytes.fetch_add(text.len(), Ordering::Relaxed) + text.len() > 262_144
                        || !size.is_finite()
                        || !(1.0..=256.0).contains(&size)
                        || style > 7
                        || cancel.load(Ordering::Relaxed)
                    {
                        return f32::NAN;
                    }
                    let Some(measurer) = &measurer else {
                        return f32::NAN;
                    };
                    let width = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        (measurer.measure)(&text, size, style)
                    }))
                    .unwrap_or(f32::NAN);
                    if width.is_finite() && (0.0..=1_000_000.0).contains(&width) {
                        width
                    } else {
                        f32::NAN
                    }
                },
            )
            .map_err(js_error)?;
            let render: Function = ctx.globals().get("__keystoneRender").map_err(js_error)?;
            let result: Object = render
                .call((
                    job.request.source.as_str(),
                    job.request.width,
                    job.request.font_family.as_str(),
                    job.request.font_family_mono.as_str(),
                    job.measurer.is_some(),
                    callback,
                ))
                .map_err(js_error)?;
            if !result.get::<_, bool>("ok").map_err(js_error)? {
                let message: String = result.get("message").map_err(js_error)?;
                return Err(DiagramError {
                    message: message.chars().take(512).collect(),
                    line: result.get::<_, usize>("line").unwrap_or(1).max(1),
                    column: result.get::<_, usize>("column").unwrap_or(1).max(1),
                    kind: if result.get::<_, bool>("recoverable").unwrap_or(false) {
                        ErrorKind::Permanent
                    } else {
                        ErrorKind::Runtime
                    },
                });
            }
            let svg: String = result.get("svg").map_err(js_error)?;
            let width: f32 = result.get("width").map_err(js_error)?;
            let height: f32 = result.get("height").map_err(js_error)?;
            if svg.len() > MAX_SVG {
                return Err(DiagramError::new("Diagram SVG exceeds 1 MiB; simplify it"));
            }
            if !width.is_finite()
                || !height.is_finite()
                || width <= 0.0
                || height <= 0.0
                || width > 16384.0
                || height > 16384.0
                || width * height > 16_777_216.0
            {
                return Err(DiagramError::new(
                    "Diagram dimensions exceed the safe raster size; split the diagram",
                ));
            }
            Ok(Layout {
                svg: Arc::from(svg),
                width,
                height,
            })
        });
        let timed_out = self
            .budget
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|b| Instant::now() >= b.deadline);
        *self.budget.lock().unwrap() = None;
        self.runtime.run_gc();
        if result.is_err() {
            check_cancel(&job.cancel)?;
        }
        if timed_out {
            return Err(DiagramError {
                kind: ErrorKind::RenderTimeout,
                ..DiagramError::new("Diagram exceeded the 750 ms render limit; simplify it")
            });
        }
        result
    }
}

fn js_error(error: rquickjs::Error) -> DiagramError {
    DiagramError {
        kind: ErrorKind::Runtime,
        ..DiagramError::new(format!(
            "Diagram runtime failed ({error}); simplify the diagram and retry"
        ))
    }
}

const WRAPPER: &str = r#"
globalThis.__keystoneRender = function(source, width, font, mono, measured, measure) {
  try {
    pele.forgetTextWidths();
    const options = {maxWidth:width, fontFamily:font, fontFamilyMono:mono, images:false, links:false,
      limit:32768, outputLimit:1048576, maxEdges:512, now:0};
    if (measured) options.measurer = {width(text,size,style) {
      const value = measure(text,size,style);
      if (!Number.isFinite(value)) throw new Error('Text measurement limit exceeded or host returned an invalid width');
      return value;
    }};
    const result = pele.render(source, options);
    return {ok:true, svg:result.svg, width:result.width, height:result.height};
  } catch(e) {
    return {ok:false, message:String(e.message || e), line:e.line || 1, column:e.column || 1,
      recoverable:e instanceof pele.PeleError};
  }
};
"#;

fn sanitize_svg(
    svg: &str,
    palette: &Palette,
    font: &str,
    mono: &str,
) -> Result<String, DiagramError> {
    let unsafe_svg = || {
        DiagramError::new(
            "Diagram contains unsupported or unsafe SVG; remove custom styles, links, and resources",
        )
    };
    if svg.len() > MAX_SVG || svg.contains("<!") || svg.contains("<?") {
        return Err(unsafe_svg());
    }
    let document = roxmltree::Document::parse_with_options(
        svg,
        roxmltree::ParsingOptions {
            nodes_limit: 30_000,
            ..Default::default()
        },
    )
    .map_err(|_| unsafe_svg())?;
    let root = document.root_element();
    if root.tag_name().name() != "svg"
        || root.tag_name().namespace() != Some("http://www.w3.org/2000/svg")
    {
        return Err(unsafe_svg());
    }
    let mut variables: BTreeMap<String, String> = palette.variables.iter().cloned().collect();
    for declaration in root.attribute("style").unwrap_or("").split(';') {
        if let Some((name, value)) = declaration.split_once(':') {
            if name.trim().starts_with("--_") {
                variables.insert(name.trim().into(), value.trim().into());
            }
        }
    }
    let mut output = String::with_capacity(svg.len());
    let mut count = 0;
    let mut nodes = vec![(root, false)];
    while let Some((node, closing)) = nodes.pop() {
        match (node, closing) {
            (node, false) if node.is_element() => {
                nodes.push((node, true));
                nodes.extend(node.children().rev().map(|child| (child, false)));
                count += 1;
                if count > 20_000 {
                    return Err(unsafe_svg());
                }
                let tag = node.tag_name();
                if tag.namespace() != Some("http://www.w3.org/2000/svg")
                    || !matches!(
                        tag.name(),
                        "svg"
                            | "g"
                            | "path"
                            | "rect"
                            | "circle"
                            | "ellipse"
                            | "line"
                            | "polyline"
                            | "polygon"
                            | "text"
                            | "tspan"
                            | "defs"
                            | "marker"
                            | "clipPath"
                            | "linearGradient"
                            | "radialGradient"
                            | "stop"
                            | "pattern"
                            | "title"
                            | "desc"
                    )
                {
                    return Err(unsafe_svg());
                }
                if node != root && tag.name() == "svg" {
                    return Err(unsafe_svg());
                }
                output.push('<');
                output.push_str(tag.name());
                if node == root {
                    output.push_str(" xmlns=\"http://www.w3.org/2000/svg\" font-family=\"");
                    escape_xml(font, &mut output);
                    output.push('"');
                }
                for attr in node.attributes() {
                    let name = attr.name();
                    if attr.namespace() == Some("http://www.w3.org/XML/1998/namespace")
                        && name == "space"
                        && matches!(attr.value(), "preserve" | "default")
                    {
                        output.push_str(" xml:space=\"");
                        output.push_str(attr.value());
                        output.push('"');
                        continue;
                    }
                    if attr.namespace().is_some() || !allowed_attribute(name) {
                        return Err(unsafe_svg());
                    }
                    if node == root && name == "font-family" {
                        continue;
                    }
                    let value = resolve_vars(attr.value(), &variables, 0)?;
                    if name == "style" {
                        let mut style = String::new();
                        for declaration in value.split(';').filter(|d| !d.trim().is_empty()) {
                            let Some((property, value)) = declaration.split_once(':') else {
                                return Err(unsafe_svg());
                            };
                            let property = property.trim();
                            if property.starts_with("--") {
                                continue;
                            }
                            if !allowed_style(property) || !safe_value(value) {
                                return Err(unsafe_svg());
                            }
                            if property == "font" {
                                if node == root {
                                    let (size, _) =
                                        value.trim().split_once("px ").ok_or_else(unsafe_svg)?;
                                    let size: f32 = size.parse().map_err(|_| unsafe_svg())?;
                                    if !size.is_finite() || !(1.0..=256.0).contains(&size) {
                                        return Err(unsafe_svg());
                                    }
                                    style.push_str(&format!("font-size:{size}px;"));
                                }
                                continue;
                            }
                            let value = if property == "font-family" {
                                if value.trim().trim_matches(['\'', '"']) == mono {
                                    mono
                                } else {
                                    font
                                }
                            } else {
                                value
                            };
                            style.push_str(property);
                            style.push(':');
                            style.push_str(value);
                            style.push(';');
                        }
                        output.push_str(" style=\"");
                        escape_xml(&style, &mut output);
                        output.push('"');
                    } else {
                        if !safe_value(&value) {
                            return Err(unsafe_svg());
                        }
                        let value = if name == "font-family" {
                            if value.trim().trim_matches(['\'', '"']) == mono {
                                mono
                            } else {
                                font
                            }
                        } else {
                            &value
                        };
                        output.push(' ');
                        output.push_str(name);
                        output.push_str("=\"");
                        escape_xml(value, &mut output);
                        output.push('"');
                    }
                }
                output.push('>');
            }
            (node, false) if node.is_text() => escape_xml(node.text().unwrap_or(""), &mut output),
            (node, true) if node.is_element() => {
                output.push_str("</");
                output.push_str(node.tag_name().name());
                output.push('>');
            }
            _ => {}
        }
        if output.len() > MAX_SVG {
            return Err(unsafe_svg());
        }
    }
    Ok(output)
}

fn escape_xml(text: &str, output: &mut String) {
    for c in text.chars() {
        match c {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            _ => output.push(c),
        }
    }
}

fn resolve_vars(
    text: &str,
    variables: &BTreeMap<String, String>,
    depth: usize,
) -> Result<String, DiagramError> {
    if depth > 8 || text.len() > MAX_SVG {
        return Err(DiagramError::new("Diagram has recursive CSS variables"));
    }
    let mut output = String::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("var(") {
        output.push_str(&remaining[..start]);
        let content = &remaining[start + 4..];
        let mut nesting = 1;
        let mut end = None;
        for (index, c) in content.char_indices() {
            if c == '(' {
                nesting += 1;
            }
            if c == ')' {
                nesting -= 1;
            }
            if nesting == 0 {
                end = Some(index);
                break;
            }
        }
        let end = end.ok_or_else(|| DiagramError::new("Diagram has an invalid CSS variable"))?;
        let (name, fallback) = content[..end]
            .split_once(',')
            .unwrap_or((&content[..end], ""));
        let value = variables
            .get(name.trim())
            .map(String::as_str)
            .unwrap_or(fallback.trim());
        if value.is_empty() {
            return Err(DiagramError::new("Diagram has an unresolved CSS variable"));
        }
        output.push_str(&resolve_vars(value, variables, depth + 1)?);
        if output.len() > MAX_SVG {
            return Err(DiagramError::new("Diagram CSS output is too large"));
        }
        remaining = &content[end + 1..];
    }
    output.push_str(remaining);
    Ok(output)
}

fn safe_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains(['\\', '@', '<', '>', '\0'])
        || lower.contains("var(")
        || lower.contains("javascript:")
        || lower.contains("data:")
        || lower.contains("http:")
        || lower.contains("https:")
        || lower.contains("file:")
        || lower.contains("expression(")
    {
        return false;
    }
    let mut remaining = lower.as_str();
    while let Some(start) = remaining.find("url(") {
        let content = &remaining[start + 4..];
        let Some(end) = content.find(')') else {
            return false;
        };
        let id = content[..end].trim().trim_matches(['\'', '"']);
        if !id.starts_with('#')
            || id.len() < 2
            || !id[1..]
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return false;
        }
        remaining = &content[end + 1..];
    }
    true
}

fn allowed_style(name: &str) -> bool {
    matches!(
        name,
        "fill"
            | "stroke"
            | "color"
            | "opacity"
            | "fill-opacity"
            | "stroke-opacity"
            | "stroke-width"
            | "stroke-dasharray"
            | "stroke-dashoffset"
            | "stroke-linecap"
            | "stroke-linejoin"
            | "font"
            | "font-family"
            | "font-size"
            | "font-style"
            | "font-weight"
            | "text-anchor"
            | "text-decoration"
            | "dominant-baseline"
            | "letter-spacing"
            | "word-spacing"
            | "text-transform"
            | "direction"
            | "rx"
            | "ry"
            | "max-width"
            | "height"
    )
}

fn allowed_attribute(name: &str) -> bool {
    matches!(
        name,
        "id" | "class"
            | "role"
            | "viewBox"
            | "width"
            | "height"
            | "style"
            | "transform"
            | "x"
            | "y"
            | "x1"
            | "y1"
            | "x2"
            | "y2"
            | "cx"
            | "cy"
            | "r"
            | "rx"
            | "ry"
            | "dx"
            | "dy"
            | "d"
            | "points"
            | "fill"
            | "stroke"
            | "color"
            | "opacity"
            | "fill-opacity"
            | "stroke-opacity"
            | "fill-rule"
            | "clip-rule"
            | "stroke-width"
            | "stroke-dasharray"
            | "stroke-dashoffset"
            | "stroke-linecap"
            | "stroke-linejoin"
            | "stroke-miterlimit"
            | "font-family"
            | "font-size"
            | "font-weight"
            | "font-style"
            | "text-anchor"
            | "dominant-baseline"
            | "alignment-baseline"
            | "text-decoration"
            | "marker-start"
            | "marker-mid"
            | "marker-end"
            | "markerWidth"
            | "markerHeight"
            | "markerUnits"
            | "refX"
            | "refY"
            | "orient"
            | "clip-path"
            | "clipPathUnits"
            | "offset"
            | "stop-color"
            | "stop-opacity"
            | "gradientUnits"
            | "gradientTransform"
            | "spreadMethod"
            | "patternUnits"
            | "patternContentUnits"
            | "patternTransform"
            | "textLength"
            | "lengthAdjust"
    ) || name.starts_with("data-")
        || name.starts_with("aria-")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: &str = include_str!("../../tests/fixtures/diagrams/flowchart.mmd");
    const SEQUENCE: &str = include_str!("../../tests/fixtures/diagrams/sequence.mmd");
    const PIE: &str = include_str!("../../tests/fixtures/diagrams/pie.mmd");

    fn request(source: &str, width: u32, dark: bool) -> RenderRequest {
        let colors = if dark {
            ["#17191f", "#eeeeee", "#252a34", "#8f9aaa"]
        } else {
            ["#ffffff", "#202530", "#edf0f5", "#586579"]
        };
        RenderRequest {
            source: source.into(),
            width,
            font_family: "DejaVu Sans".into(),
            font_family_mono: "DejaVu Sans Mono".into(),
            palette: Palette {
                variables: [
                    "--pele-bg",
                    "--pele-fg",
                    "--pele-surface",
                    "--pele-border",
                    "--pele-line",
                    "--pele-muted",
                ]
                .into_iter()
                .zip([
                    colors[0], colors[1], colors[2], colors[3], colors[1], colors[3],
                ])
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            },
        }
    }

    fn job(mut request: RenderRequest) -> Job {
        validate_request(&mut request).unwrap();
        Job {
            request,
            measurer: None,
            cancel: Arc::new(AtomicBool::new(false)),
            expires: Instant::now() + WAIT_TIME,
            reply: oneshot::channel().0,
        }
    }

    #[test]
    fn fixtures_render_and_rasterize_in_both_themes_and_widths() {
        let mut engine = Some(Engine::new().unwrap());
        let mut layouts = Cache::default();
        let mut images = Cache::default();
        let mut fonts = resvg::usvg::fontdb::Database::new();
        fonts.load_system_fonts();
        let fonts = Arc::new(fonts);
        let mut differences = 0;
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/diagram-runtime-visuals");
        std::fs::create_dir_all(&directory).unwrap();
        for (name, source) in [("flowchart", FLOW), ("sequence", SEQUENCE), ("pie", PIE)] {
            for width in [240, 800] {
                let mut light = Vec::new();
                for dark in [false, true] {
                    let result = process(
                        &job(request(source, width, dark)),
                        &mut engine,
                        &mut layouts,
                        &mut images,
                    )
                    .unwrap();
                    let svg = std::str::from_utf8(&result.svg).unwrap();
                    assert!(!svg.contains("var("));
                    assert!(svg.contains("font-family=\"DejaVu Sans\""));
                    let options = resvg::usvg::Options {
                        fontdb: fonts.clone(),
                        ..Default::default()
                    };
                    let tree = resvg::usvg::Tree::from_data(&result.svg, &options).unwrap();
                    assert!(!tree.root().children().is_empty());
                    let size = tree.size().to_int_size();
                    let mut pixmap =
                        resvg::tiny_skia::Pixmap::new(size.width(), size.height()).unwrap();
                    resvg::render(
                        &tree,
                        resvg::tiny_skia::Transform::identity(),
                        &mut pixmap.as_mut(),
                    );
                    assert!(pixmap.pixels().iter().filter(|p| p.alpha() > 0).count() > 100);
                    pixmap
                        .save_png(directory.join(format!(
                            "{name}-{width}-{}.png",
                            if dark { "dark" } else { "light" }
                        )))
                        .unwrap();
                    if dark {
                        assert_ne!(light, pixmap.data());
                        differences += 1;
                    } else {
                        light = pixmap.data().to_vec();
                    }
                    assert!(result.width.is_finite() && result.height.is_finite());
                }
            }
        }
        assert_eq!(differences, 6);
        assert_eq!(layouts.entries.len(), 6);
        assert_eq!(images.entries.len(), 12);
    }

    #[test]
    fn errors_have_source_locations_and_config_is_not_accepted() {
        let engine = Engine::new().unwrap();
        let malformed = include_str!("../../tests/fixtures/diagrams/malformed.mmd");
        let error = engine
            .layout(&job(request(malformed, 400, false)))
            .unwrap_err();
        assert!(error.line >= 2, "{error}");
        assert!(error.column >= 1);
        assert!(error.to_string().contains("line"));
        for source in [
            "---\nconfig:\n  images: true\n---\nflowchart TD\nA-->B",
            "flowchart TD\n%%{init: {images: true}}%%\nA-->B",
            "\u{feff}---\nflowchart TD\nA-->B",
        ] {
            let error = validate_request(&mut request(source, 400, false)).unwrap_err();
            assert!(error.message.contains("disabled"));
        }
    }

    #[test]
    fn mono_label_raster_font_matches_the_selected_metrics_font() {
        let mut engine = Some(Engine::new().unwrap());
        let mut layouts = Cache::default();
        let mut images = Cache::default();
        let source = "railroad-beta\nexpr = terminal(\"iiiiiiiiiiiiiiii\");";
        let image = process(
            &job(request(source, 640, false)),
            &mut engine,
            &mut layouts,
            &mut images,
        )
        .unwrap();
        let svg = std::str::from_utf8(&image.svg).unwrap();
        assert!(svg.contains("font-family=\"DejaVu Sans Mono\""), "{svg}");
        let mut changed = request(source, 640, false);
        changed.font_family_mono = "Liberation Mono".into();
        let changed = process(&job(changed), &mut engine, &mut layouts, &mut images).unwrap();
        assert!(
            std::str::from_utf8(&changed.svg)
                .unwrap()
                .contains("font-family=\"Liberation Mono\"")
        );
        assert_eq!(layouts.entries.len(), 2);
        assert_ne!(image.svg, changed.svg);
    }

    #[test]
    fn input_palette_font_and_edge_limits_are_enforced() {
        for width in [0, 63, 4097, u32::MAX] {
            assert!(validate_request(&mut request(FLOW, width, false)).is_err());
        }
        assert!(validate_request(&mut request(&"x".repeat(MAX_SOURCE + 1), 400, false)).is_err());
        assert!(
            sanitize_svg(
                &"x".repeat(MAX_SVG + 1),
                &Palette::default(),
                "sans-serif",
                "monospace"
            )
            .is_err()
        );
        for font in ["", "bad\" onload=\"alert(1)", "url(https://example.com)"] {
            let mut req = request(FLOW, 400, false);
            req.font_family = font.into();
            assert!(validate_request(&mut req).is_err());
            let mut req = request(FLOW, 400, false);
            req.font_family_mono = font.into();
            assert!(validate_request(&mut req).is_err());
        }
        for (name, color) in [
            ("--pele-fg", "url(https://example.com)"),
            ("onload", "#fff"),
            ("--pele-fg", "#gggggg"),
        ] {
            let mut req = request(FLOW, 400, false);
            req.palette.variables = vec![(name.into(), color.into())];
            assert!(validate_request(&mut req).is_err());
        }
        let edges = format!(
            "flowchart TD\n{}",
            (0..520)
                .map(|i| format!("A{i}-->A{}\n", i + 1))
                .collect::<String>()
        );
        let error = Engine::new()
            .unwrap()
            .layout(&job(request(&edges, 400, false)))
            .unwrap_err();
        assert!(
            error.message.to_lowercase().contains("limit") || error.message.contains("edges"),
            "{error}"
        );
        let tall = format!(
            "flowchart TD\n{}",
            (0..300)
                .map(|i| format!("A{i}-->A{}\n", i + 1))
                .collect::<String>()
        );
        let error = Engine::new()
            .unwrap()
            .layout(&job(request(&tall, 400, false)))
            .unwrap_err();
        assert!(
            error.message.contains("dimensions") || error.message.contains("render limit"),
            "{error}"
        );
    }

    #[test]
    fn strict_svg_gate_blocks_active_content_and_external_resources() {
        for content in [
            "<script>alert(1)</script>",
            "<foreignObject><p>HTML</p></foreignObject>",
            "<image href=\"https://example.com/x.png\"/>",
            "<a href=\"https://example.com\"><text>x</text></a>",
            "<rect onload=\"alert(1)\"/>",
            "<use href=\"#x\"/>",
            "<style>@import 'https://example.com/x.css';</style>",
            "<rect style=\"fill:url(//example.com/x)\"/>",
            "<rect fill=\"url(data:image/svg+xml,bad)\"/>",
            "<rect style=\"filter:url(#x)\"/>",
            "<animate attributeName=\"href\" to=\"https://example.com\"/>",
            "<rect style=\"fill:u\\72l(https://example.com)\"/>",
            "<g xmlns=\"https://example.com\"><text>x</text></g>",
        ] {
            let svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\">{content}</svg>");
            assert!(
                sanitize_svg(&svg, &Palette::default(), "sans-serif", "monospace").is_err(),
                "{content}"
            );
        }
        for svg in [
            "<!DOCTYPE svg [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><svg/>",
            "<?xml-stylesheet href='https://example.com'?><svg/>",
        ] {
            assert!(sanitize_svg(svg, &Palette::default(), "sans-serif", "monospace").is_err());
        }
        let safe = "<svg xmlns=\"http://www.w3.org/2000/svg\"><defs><clipPath id=\"clip\"><rect width=\"1\" height=\"1\"/></clipPath></defs><g clip-path=\"url(#clip)\"><text>&lt;script&gt;</text></g></svg>";
        assert!(
            sanitize_svg(safe, &Palette::default(), "sans-serif", "monospace")
                .unwrap()
                .contains("&lt;script&gt;")
        );
        let recursive = "<svg xmlns=\"http://www.w3.org/2000/svg\" style=\"--_a:var(--_a)\" fill=\"var(--_a)\"/>";
        assert!(sanitize_svg(recursive, &Palette::default(), "sans-serif", "monospace").is_err());
    }

    #[test]
    fn source_font_overrides_cannot_reintroduce_unresolved_families() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" style="font:14px sans-serif;font-family:Inter"><text font-family="Symbol" style="font:bold 20px Inter;font-family:Inter">Label</text><text font-family="Resolved Mono">Mono</text></svg>"#;
        let safe =
            sanitize_svg(svg, &Palette::default(), "Resolved Sans", "Resolved Mono").unwrap();
        assert!(!safe.contains("Inter") && !safe.contains("Symbol") && !safe.contains("font:"));
        assert!(safe.contains("font-size:14px;") && safe.contains("font-family:Resolved Sans;"));
        assert!(safe.contains("font-family=\"Resolved Mono\""));
    }

    #[test]
    fn source_is_data_and_the_runtime_has_no_io() {
        let engine = Engine::new().unwrap();
        engine.context.with(|ctx| {
            let safe: bool = ctx.eval("typeof fetch === 'undefined' && typeof XMLHttpRequest === 'undefined' && typeof require === 'undefined' && typeof process === 'undefined' && typeof std === 'undefined' && typeof os === 'undefined'").unwrap();
            assert!(safe);
        });
        let source = "flowchart TD\n A[\"'); globalThis.__injected = true; //\"] --> B[Safe]";
        let _ = engine.layout(&job(request(source, 400, false)));
        engine.context.with(|ctx| {
            assert!(
                ctx.eval::<bool, _>("typeof __injected === 'undefined'")
                    .unwrap()
            )
        });
        let linked = "flowchart TD\n A[Click]-->B[Safe]\n click A \"https://example.com\"";
        let layout = engine.layout(&job(request(linked, 400, false))).unwrap();
        assert!(!layout.svg.contains("href="));
        assert!(!layout.svg.contains("<image"));
    }

    #[test]
    fn theme_reuses_layout_but_width_font_and_metrics_invalidate_it() {
        let mut engine = Some(Engine::new().unwrap());
        let mut layouts = Cache::default();
        let mut images = Cache::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let make_measurer = |key| {
            let calls = calls.clone();
            TextMeasurer {
                cache_key: key,
                measure: Arc::new(move |text, size, _| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    text.chars().count() as f32 * size * 0.6
                }),
            }
        };
        let mut first = job(request(FLOW, 400, false));
        first.measurer = Some(make_measurer(1));
        let result = process(&first, &mut engine, &mut layouts, &mut images).unwrap();
        let measured_calls = calls.load(Ordering::Relaxed);
        assert!(measured_calls > 0);
        let cached = process(&first, &mut engine, &mut layouts, &mut images).unwrap();
        assert!(Arc::ptr_eq(&result.svg, &cached.svg));
        let mut dark = job(request(FLOW, 400, true));
        dark.measurer = Some(make_measurer(1));
        let dark = process(&dark, &mut engine, &mut layouts, &mut images).unwrap();
        assert_ne!(result.svg, dark.svg);
        assert_eq!(calls.load(Ordering::Relaxed), measured_calls);
        assert_eq!(layouts.entries.len(), 1);
        for (width, font, mono, metrics) in [
            (800, "DejaVu Sans", "DejaVu Sans Mono", 1),
            (400, "DejaVu Serif", "DejaVu Sans Mono", 1),
            (400, "DejaVu Sans", "DejaVu Sans Mono", 2),
            (400, "DejaVu Sans", "Liberation Mono", 1),
        ] {
            let mut req = request(FLOW, width, false);
            req.font_family = font.into();
            req.font_family_mono = mono.into();
            let mut next = job(req);
            next.measurer = Some(make_measurer(metrics));
            process(&next, &mut engine, &mut layouts, &mut images).unwrap();
        }
        assert_eq!(layouts.entries.len(), 5);
        let mut cache = Cache::default();
        for i in 0..100 {
            cache.insert(i, i, MAX_CACHE / 3);
        }
        assert!(cache.bytes <= MAX_CACHE && cache.entries.len() <= 3);
        for i in 100..200 {
            cache.insert(i, i, 1);
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn interrupt_cancellation_memory_and_measurement_limits_are_enforced() {
        let engine = Engine::new().unwrap();
        *engine.budget.lock().unwrap() = Some(Budget {
            deadline: Instant::now() + Duration::from_millis(20),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        let start = Instant::now();
        assert!(
            engine
                .context
                .with(|ctx| ctx.eval::<(), _>("while(true) {}"))
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        *engine.budget.lock().unwrap() = None;
        engine.context.with(|ctx| {
            ctx.catch();
        });
        assert!(
            engine
                .context
                .with(|ctx| ctx.eval::<(), _>("new ArrayBuffer(128 * 1024 * 1024)"))
                .is_err()
        );
        engine.context.with(|ctx| {
            ctx.catch();
        });
        let mut cancelled = job(request(FLOW, 400, false));
        cancelled.cancel.store(true, Ordering::Relaxed);
        assert!(
            engine
                .layout(&cancelled)
                .unwrap_err()
                .message
                .contains("cancelled")
        );
        cancelled.cancel.store(false, Ordering::Relaxed);
        let cancel = cancelled.cancel.clone();
        cancelled.measurer = Some(TextMeasurer {
            cache_key: 1,
            measure: Arc::new(move |_, _, _| {
                cancel.store(true, Ordering::Relaxed);
                10.0
            }),
        });
        assert!(
            engine
                .layout(&cancelled)
                .unwrap_err()
                .message
                .contains("cancelled")
        );
        let mut invalid = job(request(FLOW, 400, false));
        invalid.measurer = Some(TextMeasurer {
            cache_key: 1,
            measure: Arc::new(|_, _, _| f32::INFINITY),
        });
        assert!(
            engine
                .layout(&invalid)
                .unwrap_err()
                .message
                .contains("measurement")
        );
        let expired = Job {
            expires: Instant::now() - Duration::from_secs(1),
            ..job(request(FLOW, 400, false))
        };
        assert!(
            process(
                &expired,
                &mut None,
                &mut Cache::default(),
                &mut Cache::default()
            )
            .unwrap_err()
            .message
            .contains("expired")
        );
    }

    #[test]
    fn public_worker_is_lazy_bounded_and_recovers_after_cancellation() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let released = Arc::new(AtomicBool::new(false));
        let release = released.clone();
        let first_call = Arc::new(AtomicBool::new(true));
        let first = std::thread::spawn(move || {
            render_with_measurer(
                request(FLOW, 401, false),
                Arc::new(AtomicBool::new(false)),
                TextMeasurer {
                    cache_key: 99,
                    measure: Arc::new(move |text, size, _| {
                        if first_call.swap(false, Ordering::Relaxed) {
                            started_tx.send(()).unwrap();
                            let until = Instant::now() + Duration::from_secs(2);
                            while !release.load(Ordering::Relaxed) && Instant::now() < until {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                        }
                        text.len() as f32 * size * 0.6
                    }),
                },
            )
        });
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut queued = Vec::new();
        let mut errors = Vec::new();
        let start = Instant::now();
        for i in 0..12 {
            match submit_async(
                request(PIE, 500 + i, false),
                Arc::new(AtomicBool::new(false)),
                TextMeasurer {
                    cache_key: 100,
                    measure: Arc::new(|text, size, _| text.len() as f32 * size * 0.6),
                },
            ) {
                Ok(ticket) => queued.push(ticket),
                Err(error) => errors.push(error),
            }
        }
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(queued.len(), 8);
        assert_eq!(errors.len(), 4);
        assert!(
            errors
                .iter()
                .all(|error| error.is_transient() && error.message.contains("queue is full"))
        );
        struct WakeCounter(AtomicUsize);
        impl futures_util::task::ArcWake for WakeCounter {
            fn wake_by_ref(this: &Arc<Self>) {
                this.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let wakes = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = futures_util::task::waker(wakes.clone());
        let mut context = TaskContext::from_waker(&waker);
        let start = Instant::now();
        assert!(Pin::new(&mut queued[0]).poll(&mut context).is_pending());
        assert!(start.elapsed() < Duration::from_millis(100));
        released.store(true, Ordering::Relaxed);
        first.join().unwrap().unwrap();
        for ticket in queued {
            wait_for_ticket(ticket, Arc::new(AtomicBool::new(false))).unwrap();
        }
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        let cancelled = Arc::new(AtomicBool::new(true));
        let error = render(request(FLOW, 400, false), cancelled).unwrap_err();
        assert!(error.message.contains("cancelled") && !error.is_transient());
        assert!(
            render(
                request(SEQUENCE, 400, false),
                Arc::new(AtomicBool::new(false))
            )
            .is_ok()
        );
    }

    #[test]
    fn syntax_preserves_engine_but_runtime_failures_and_timeouts_reset_it() {
        let mut engine = Some(Engine::new().unwrap());
        let mut layouts = Cache::default();
        let mut images = Cache::default();
        engine
            .as_ref()
            .unwrap()
            .context
            .with(|ctx| ctx.eval::<(), _>("globalThis.__engineMarker = 42").unwrap());
        let error = process(
            &job(request("flowchart TD\nA[Unclosed", 400, false)),
            &mut engine,
            &mut layouts,
            &mut images,
        )
        .unwrap_err();
        assert!(!error.is_transient());
        assert!(engine.is_some());
        engine
            .as_ref()
            .unwrap()
            .context
            .with(|ctx| assert_eq!(ctx.eval::<u32, _>("__engineMarker").unwrap(), 42));
        engine.as_ref().unwrap().context.with(|ctx| {
            ctx.eval::<(), _>(
                "globalThis.__keystoneRender = function() { throw new Error('runtime failure'); }",
            )
            .unwrap()
        });
        let error = process(
            &job(request(FLOW, 400, false)),
            &mut engine,
            &mut layouts,
            &mut images,
        )
        .unwrap_err();
        assert!(error.resets_runtime());
        assert!(engine.is_none());
        engine = Some(Engine::new().unwrap());
        engine.as_ref().unwrap().context.with(|ctx| {
            ctx.eval::<(), _>("globalThis.__keystoneRender = function() { return new ArrayBuffer(128 * 1024 * 1024); }").unwrap()
        });
        let error = process(
            &job(request(FLOW, 400, false)),
            &mut engine,
            &mut layouts,
            &mut images,
        )
        .unwrap_err();
        assert!(error.resets_runtime());
        assert!(engine.is_none());
        engine = Some(Engine::new().unwrap());
        engine.as_ref().unwrap().context.with(|ctx| {
            ctx.eval::<(), _>("globalThis.__keystoneRender = function() { while (true) {} }")
                .unwrap()
        });
        let error = process(
            &job(request(FLOW, 400, false)),
            &mut engine,
            &mut layouts,
            &mut images,
        )
        .unwrap_err();
        assert!(error.message.contains("750 ms") && error.is_transient());
        assert!(engine.is_none());
        engine = Some(Engine::new().unwrap());
        engine.as_ref().unwrap().context.with(|ctx| {
            ctx.eval::<(), _>("globalThis.__keystoneRender = function() { while (true) {} }")
                .unwrap()
        });
        let cancelled = job(request(FLOW, 400, false));
        let cancel = cancelled.cancel.clone();
        let cancelling = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            cancel.store(true, Ordering::Relaxed);
        });
        let error = process(&cancelled, &mut engine, &mut layouts, &mut images).unwrap_err();
        cancelling.join().unwrap();
        assert!(error.message.contains("cancelled") && !error.is_transient());
        assert!(engine.is_none());
        process(
            &job(request(FLOW, 400, false)),
            &mut engine,
            &mut layouts,
            &mut images,
        )
        .unwrap();
        assert!(engine.is_some());
    }

    #[test]
    fn queue_expiry_is_transient_and_does_not_shorten_the_render_budget() {
        let engine = Engine::new().unwrap();
        let mut delayed = job(request(FLOW, 400, false));
        delayed.expires = Instant::now() - Duration::from_millis(1);
        assert!(engine.layout(&delayed).is_ok());
        let error = process(
            &delayed,
            &mut None,
            &mut Cache::default(),
            &mut Cache::default(),
        )
        .unwrap_err();
        assert!(error.is_transient() && error.message.contains("queue"));
        assert!(DiagramError::request_timeout().is_transient());
        let mut invalid = request(FLOW, 1, false);
        assert!(!validate_request(&mut invalid).unwrap_err().is_transient());
    }

    #[test]
    fn completed_safe_layout_is_cached_when_cancelled_before_delivery() {
        let mut engine = Some(Engine::new().unwrap());
        let mut layouts = Cache::default();
        let mut images = Cache::default();
        let cancelled = job(request(FLOW, 400, false));
        let cancel = cancelled.cancel.clone();
        engine.as_ref().unwrap().context.with(|ctx| {
            let callback = Function::new(ctx.clone(), move || { cancel.store(true, Ordering::Relaxed); }).unwrap();
            ctx.globals().set("__cancelAfterRender", callback).unwrap();
            ctx.eval::<(), _>("const original = __keystoneRender; __keystoneRender = function(...args) { const result = original(...args); __cancelAfterRender(); return result; }").unwrap();
        });
        assert!(
            process(&cancelled, &mut engine, &mut layouts, &mut images)
                .unwrap_err()
                .message
                .contains("cancelled")
        );
        assert_eq!(layouts.entries.len(), 1);
        assert_eq!(images.entries.len(), 0);
        assert!(engine.is_some());
        process(
            &job(request(FLOW, 400, true)),
            &mut engine,
            &mut layouts,
            &mut images,
        )
        .unwrap();
        assert_eq!(layouts.entries.len(), 1);
    }
}
