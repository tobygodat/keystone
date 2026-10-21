//! Element geometry for native verification. In a debug build with
//! `KEYSTONE_AUTOMATION_PATH` set, every element named by a debug selector
//! reports its window-relative bounds each frame. Frames that change the set
//! rewrite that file as JSON, so a driver can click by name instead of guessing
//! coordinates from a screenshot. Unset, this adds nothing to the element tree.

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
