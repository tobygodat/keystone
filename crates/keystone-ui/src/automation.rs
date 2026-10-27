//! Element geometry for native verification. In a debug build with
//! `KEYSTONE_AUTOMATION_PATH` set, every element named by a debug selector
//! reports its window-relative bounds each frame. Frames that change the set
//! rewrite that file as JSON, so a driver can click by name instead of guessing
//! coordinates from a screenshot. Unset, this adds nothing to the element tree.
//!
//! Where no private desktop can inject input, as on macOS, `KEYSTONE_AUTOMATION_INBOX`
//! names a folder of commands the app runs on itself; see [`serve_inbox`].

use gpui::{Bounds, InteractiveElement, IntoElement, ParentElement, Pixels, Styled, canvas};
use std::{cell::RefCell, collections::BTreeMap, fmt::Write, path::PathBuf, sync::OnceLock};

fn output() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        if cfg!(debug_assertions) {
            std::env::var_os("KEYSTONE_AUTOMATION_PATH").map(PathBuf::from)
        } else {
            None
        }
    })
    .as_ref()
}

fn inbox() -> Option<PathBuf> {
    cfg!(debug_assertions)
        .then(|| std::env::var_os("KEYSTONE_AUTOMATION_INBOX").map(PathBuf::from))
        .flatten()
}

/// Whether this process reports element geometry. Fixed for the process lifetime.
pub fn enabled() -> bool {
    output().is_some()
}

struct Entry {
    id: String,
    bounds: Bounds<Pixels>,
    visible: Option<Bounds<Pixels>>,
}

/// One window's latest painted frame.
#[derive(Default)]
struct WindowFrame {
    title: String,
    viewport: (f32, f32),
    scale: f32,
    active: bool,
    elements: Vec<Entry>,
}

#[derive(Default)]
struct Recorder {
    /// Elements painted since the last flush, by window.
    painting: BTreeMap<u64, Vec<Entry>>,
    /// Windows whose frame finished drawing since the last flush.
    drawn: BTreeMap<u64, WindowFrame>,
    /// Each open window's latest frame, by window.
    windows: BTreeMap<u64, WindowFrame>,
    flush_pending: bool,
    written: String,
    revision: u64,
}

thread_local! {
    static RECORDER: RefCell<Recorder> = RefCell::default();
}

/// Names an element for GPUI's simulated tests and, when enabled, native verification.
pub trait AutomationExt: InteractiveElement + ParentElement + Sized {
    fn automation_id(self, id: impl Into<String>) -> Self {
        tag(self, id.into())
    }
}

impl<E: InteractiveElement + ParentElement> AutomationExt for E {}

pub(crate) fn tag<E: InteractiveElement + ParentElement>(element: E, id: String) -> E {
    if !enabled() {
        return element.debug_selector(move || id);
    }
    // An absolute, full-size child takes no part in flex layout, and its prepaint
    // sees the parent's final bounds and clip. Recording waits for paint, which
    // GPUI skips for hidden elements and for prepaints it rolls back.
    let probe = canvas(
        |bounds, window, _| {
            let clipped = bounds.intersect(&window.content_mask().bounds);
            (bounds, (!clipped.is_empty()).then_some(clipped))
        },
        {
            let id = id.clone();
            move |_, (bounds, visible), window, _| {
                let window = window.window_handle().window_id().as_u64();
                RECORDER.with_borrow_mut(|r| {
                    r.painting.entry(window).or_default().push(Entry {
                        id,
                        bounds,
                        visible,
                    })
                })
            }
        },
    )
    .absolute()
    .inset_0();
    element.debug_selector(move || id).child(probe)
}

/// Writes each frame's painted elements, deferred menus included, once the frame
/// finishes drawing. Place it once, as a child of the window's root element.
pub fn frame_writer() -> impl IntoElement {
    titled_frame_writer("")
}

/// A [`frame_writer`] that names its window, so a driver can tell windows of one
/// process apart. The title matches the one the window shows the compositor.
pub fn titled_frame_writer(title: impl Into<String>) -> impl IntoElement {
    let title = title.into();
    canvas(
        |_, _, _| {},
        move |_, _, window, cx| {
            let viewport = window.viewport_size();
            let frame = WindowFrame {
                title: title.clone(),
                viewport: (f32::from(viewport.width), f32::from(viewport.height)),
                scale: window.scale_factor(),
                active: window.is_window_active(),
                elements: Vec::new(),
            };
            let id = window.window_handle().window_id().as_u64();
            let scheduled = RECORDER.with_borrow_mut(|r| {
                r.drawn.insert(id, frame);
                std::mem::replace(&mut r.flush_pending, true)
            });
            // Deferred work runs after this draw, so it sees every probe painted
            // after the writer, and nothing from the next frame.
            if !scheduled {
                cx.defer(|_| flush());
            }
        },
    )
    .absolute()
    .size_0()
}

/// Drops a closed window from the frame file.
pub fn forget_window(window: u64) {
    if !enabled() {
        return;
    }
    RECORDER.with_borrow_mut(|r| {
        r.painting.remove(&window);
        r.drawn.remove(&window);
        r.windows.remove(&window);
    });
    flush();
}

fn flush() {
    let Some(path) = output() else { return };
    RECORDER.with_borrow_mut(|r| {
        r.flush_pending = false;
        for (id, mut frame) in std::mem::take(&mut r.drawn) {
            frame.elements = r.painting.remove(&id).unwrap_or_default();
            r.windows.insert(id, frame);
        }
        let body = frame_body(&r.windows);
        if body == r.written {
            return;
        }
        r.revision += 1;
        let json = format!(
            "{{\"pid\":{},\"revision\":{},{body}}}\n",
            std::process::id(),
            r.revision
        );
        // Rename keeps readers from seeing a partial frame.
        let staging = path.with_extension("tmp");
        if std::fs::write(&staging, json).is_ok() && std::fs::rename(&staging, path).is_ok() {
            r.written = body;
        }
    });
}

/// The frame file's body. `windows` lists every window's latest frame. The top-level
/// `scale`, `viewport` and `elements` repeat the active window's, else the first
/// one's, for drivers that know only one window.
fn frame_body(windows: &BTreeMap<u64, WindowFrame>) -> String {
    let mut body = String::new();
    if let Some(main) = windows
        .values()
        .find(|w| w.active)
        .or_else(|| windows.values().next())
    {
        push_frame(&mut body, main);
        body.push(',');
    }
    body.push_str("\"windows\":[");
    for (i, (id, frame)) in windows.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        let _ = write!(body, "{{\"id\":{id},\"title\":");
        push_string(&mut body, &frame.title);
        let _ = write!(body, ",\"active\":{},", frame.active);
        push_frame(&mut body, frame);
        body.push('}');
    }
    body.push(']');
    body
}

fn push_frame(body: &mut String, frame: &WindowFrame) {
    let _ = write!(
        body,
        "\"scale\":{},\"viewport\":{{\"w\":{},\"h\":{}}},\"elements\":[",
        frame.scale, frame.viewport.0, frame.viewport.1,
    );
    for (i, entry) in frame.elements.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str("{\"id\":");
        push_string(body, &entry.id);
        body.push_str(",\"bounds\":");
        push_bounds(body, entry.bounds);
        body.push_str(",\"visible\":");
        match entry.visible {
            Some(visible) => push_bounds(body, visible),
            None => body.push_str("null"),
        }
        body.push('}');
    }
    body.push(']');
}

fn push_bounds(out: &mut String, b: Bounds<Pixels>) {
    let _ = write!(
        out,
        "{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height)
    );
}

fn push_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Runs each command file a driver writes to the inbox folder, in name order, on the
/// active window (else the first), and answers in `<name>.done`: `ok`, or `error:` and
/// why. A command file is one of
///
/// - `key secondary-n escape`: keystrokes, as GPUI parses them;
/// - `type Some text`: everything after `type `, one key per character;
/// - `click 120 48`: a left click at window coordinates, in logical pixels;
/// - `capture /path/shot.png`: the window's latest frame, rendered by GPUI's own
///   renderer, in a build with the `capture` feature.
///
/// Input enters through [`gpui::Window::dispatch_event`] and
/// [`gpui::Window::dispatch_keystroke`], the calls the platform makes for real input,
/// so key bindings, focus and hit-testing run as they do for a person. What it skips
/// is the OS: its event translation, input methods and window focus.
pub fn serve_inbox(cx: &mut gpui::App) {
    let Some(dir) = inbox() else { return };
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(50))
                .await;
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut commands: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|e| e == "cmd"))
                .collect();
            commands.sort();
            for path in commands {
                let Ok(command) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let answer = match cx.update(|cx| run(&command, cx)) {
                    Ok(()) => "ok".to_owned(),
                    Err(error) => format!("error: {error}"),
                };
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::write(path.with_extension("done"), answer);
            }
        }
    })
    .detach();
}

fn run(command: &str, cx: &mut gpui::App) -> Result<(), String> {
    use gpui::{
        Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
        PlatformInput, point, px,
    };
    let (verb, rest) = command.split_once(' ').unwrap_or((command, ""));
    let window = cx
        .active_window()
        .or_else(|| cx.windows().first().copied())
        .ok_or("no window is open")?;
    let rest = rest.to_owned();
    let verb = verb.trim().to_owned();
    window
        .update(cx, move |_, window, cx| match verb.as_str() {
            "key" => {
                for stroke in rest.split_whitespace() {
                    let stroke = Keystroke::parse(stroke).map_err(|e| e.to_string())?;
                    window.dispatch_keystroke(stroke, cx);
                }
                Ok(())
            }
            "type" => {
                for c in rest.chars() {
                    let key = match c {
                        ' ' => "space".to_owned(),
                        '\n' => "enter".to_owned(),
                        c => c.to_lowercase().collect(),
                    };
                    let modifiers = Modifiers {
                        shift: c.is_uppercase(),
                        ..Modifiers::default()
                    };
                    let key_char = Some(c.to_string());
                    window.dispatch_keystroke(
                        Keystroke {
                            modifiers,
                            key,
                            key_char,
                        },
                        cx,
                    );
                }
                Ok(())
            }
            "click" => {
                let mut numbers = rest.split_whitespace().map(str::parse::<f32>);
                let (Some(Ok(x)), Some(Ok(y))) = (numbers.next(), numbers.next()) else {
                    return Err(format!("click needs x and y, not {rest:?}"));
                };
                let position = point(px(x), px(y));
                let modifiers = Modifiers::default();
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: None,
                        modifiers,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count: 1,
                    }),
                    cx,
                );
                Ok(())
            }
            "capture" => capture(window, rest.trim()),
            other => Err(format!("unknown command {other:?}")),
        })
        .map_err(|e| e.to_string())?
}

#[cfg(feature = "capture")]
fn capture(window: &mut gpui::Window, path: &str) -> Result<(), String> {
    let image = window.render_to_image().map_err(|e| e.to_string())?;
    image.save(path).map_err(|e| e.to_string())
}

#[cfg(not(feature = "capture"))]
fn capture(_: &mut gpui::Window, _: &str) -> Result<(), String> {
    Err("this build has no capture feature".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_json_escaped() {
        let mut out = String::new();
        push_string(&mut out, "a\"b\\c\n");
        assert_eq!(out, r#""a\"b\\c\u000a""#);
    }

    fn frame(title: &str, active: bool, ids: &[&str]) -> WindowFrame {
        let bounds = Bounds::new(
            gpui::point(gpui::px(1.), gpui::px(2.)),
            gpui::size(gpui::px(3.), gpui::px(4.)),
        );
        WindowFrame {
            title: title.into(),
            viewport: (800., 600.),
            scale: 1.,
            active,
            elements: ids
                .iter()
                .map(|id| Entry {
                    id: (*id).into(),
                    bounds,
                    visible: Some(bounds),
                })
                .collect(),
        }
    }

    #[test]
    fn frame_file_lists_each_window() {
        let windows = BTreeMap::from([
            (4, frame("Keystone", false, &["composer"])),
            (9, frame("Keystone 2", true, &["composer", "session-1"])),
        ]);
        let element = |id: &str| {
            format!(
                r#"{{"id":"{id}","bounds":{{"x":1,"y":2,"w":3,"h":4}},"visible":{{"x":1,"y":2,"w":3,"h":4}}}}"#
            )
        };
        let second = format!(
            r#""scale":1,"viewport":{{"w":800,"h":600}},"elements":[{},{}]"#,
            element("composer"),
            element("session-1")
        );
        let first = format!(
            r#""scale":1,"viewport":{{"w":800,"h":600}},"elements":[{}]"#,
            element("composer")
        );
        assert_eq!(
            frame_body(&windows),
            format!(
                r#"{second},"windows":[{{"id":4,"title":"Keystone","active":false,{first}}},{{"id":9,"title":"Keystone 2","active":true,{second}}}]"#
            ),
            "each window is listed, and the top level repeats the active one"
        );
        assert_eq!(frame_body(&BTreeMap::new()), r#""windows":[]"#);
    }
}
