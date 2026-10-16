//! A diagnostic benchmark for Markdown layout on nested lists. Run with `--nocapture` to read the
//! timings. The view parses its source on every render, so the timings include parsing.
//!
//! Each case draws once in a fresh window, then once more after a resize to a narrower width.
//! The resized height must match a fresh layout at that width, which catches layout reused from a
//! cache under the old width. The nested levels wrap at the narrow width, so their height changes.
use gpui::{
    Context, IntoElement, Pixels, Render, Size, TestAppContext, VisualTestContext, Window, div,
    prelude::*, px, size,
};
use keystone_ui::prelude::*;
use std::time::Instant;

struct Doc(String);

impl Render for Doc {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            div()
                .debug_selector(|| "timed-doc".into())
                .child(Markdown::from_source("timed", &self.0)),
        )
    }
}

fn nested_levels(levels: usize) -> String {
    (0..levels)
        .map(|level| {
            format!(
                "{}- level {level} with a few words of text\n",
                "  ".repeat(level)
            )
        })
        .collect()
}

fn item_chain(items: usize) -> String {
    (0..items)
        .map(|i| format!("{}- item {i}\n", "  ".repeat(i)))
        .collect()
}

fn fresh_height(cx: &mut TestAppContext, source: String, window_size: Size<Pixels>) -> Pixels {
    let window = cx.open_window(window_size, |_, _| Doc(source));
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    cx.debug_bounds("timed-doc")
        .expect("the document was drawn")
        .size
        .height
}

fn time(cx: &mut TestAppContext, label: &str, source: String) -> (Pixels, Pixels) {
    let narrow = size(px(320.), px(700.));
    let expected = fresh_height(cx, source.clone(), narrow);

    let started = Instant::now();
    let (_view, cx) = cx.add_window_view(|_, _| Doc(source));
    cx.run_until_parked();
    let first = started.elapsed();
    let wide = cx
        .debug_bounds("timed-doc")
        .expect("the document was drawn");
    let started = Instant::now();
    cx.simulate_resize(narrow);
    cx.run_until_parked();
    let resized = started.elapsed();
    let bounds = cx
        .debug_bounds("timed-doc")
        .expect("the document was drawn");
    assert_eq!(
        bounds.size.height, expected,
        "{label}: the resized layout matches a fresh layout at the same width"
    );
    eprintln!("layout_timing {label}: first draw {first:?}, resized draw {resized:?}");
    (wide.size.height, bounds.size.height)
}

#[gpui::test]
fn markdown_nested_list_layout_diagnostic(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let levels: usize = std::env::var("LEVELS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let items: usize = std::env::var("ITEMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let (wide, narrow) = time(
        cx,
        &format!("{levels} nested levels"),
        nested_levels(levels),
    );
    assert!(
        narrow > wide,
        "text wraps at the narrow width, so the resize changed the layout: {wide:?} then {narrow:?}"
    );
    time(cx, &format!("{items} item chain"), item_chain(items));
}
