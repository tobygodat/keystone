//! Element geometry for native verification. In a debug build with
//! `KEYSTONE_AUTOMATION_PATH` set, every element named by a debug selector
//! reports its window-relative bounds each frame. Frames that change the set
//! rewrite that file as JSON, so a driver can click by name instead of guessing
//! coordinates from a screenshot. Unset, this adds nothing to the element tree.

use gpui::{Bounds, InteractiveElement, IntoElement, ParentElement, Pixels, Styled, canvas};
use std::{cell::RefCell, fmt::Write, path::PathBuf, sync::OnceLock};

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

#[derive(Default)]
struct Recorder {
    frame: Vec<Entry>,
    viewport: (f32, f32),
    scale: f32,
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
            move |_, (bounds, visible), _, _| {
                RECORDER.with_borrow_mut(|r| {
                    r.frame.push(Entry {
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
    canvas(
        |_, _, _| {},
        |_, _, window, cx| {
            let viewport = window.viewport_size();
            let scheduled = RECORDER.with_borrow_mut(|r| {
                r.viewport = (f32::from(viewport.width), f32::from(viewport.height));
                r.scale = window.scale_factor();
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

fn flush() {
    let Some(path) = output() else { return };
    RECORDER.with_borrow_mut(|r| {
        r.flush_pending = false;
        let frame = std::mem::take(&mut r.frame);
        let mut body = format!(
            "\"scale\":{},\"viewport\":{{\"w\":{},\"h\":{}}},\"elements\":[",
            r.scale, r.viewport.0, r.viewport.1,
        );
        for (i, entry) in frame.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            body.push_str("{\"id\":");
            push_string(&mut body, &entry.id);
            body.push_str(",\"bounds\":");
            push_bounds(&mut body, entry.bounds);
            body.push_str(",\"visible\":");
            match entry.visible {
                Some(visible) => push_bounds(&mut body, visible),
                None => body.push_str("null"),
            }
            body.push('}');
        }
        body.push(']');
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
}
