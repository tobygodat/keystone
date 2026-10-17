use gpui::{
    Context, Entity, IntoElement, Modifiers, Render, TestAppContext, Window, div, point,
    prelude::*, px, size,
};
use keystone_ui::prelude::*;

const ROWS: usize = 50_000;
const FRAMES: usize = 500;

struct Sweep {
    state: Entity<VirtualListState>,
}

impl Render for Sweep {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().flex_col().child(
            VirtualList::new("perf-list", &self.state, |ix, highlighted, _, _| {
                MenuItem::new(("row", ix), format!("Row {ix}"))
                    .highlighted(highlighted)
                    .into_any_element()
            })
            .flex_1()
            .w(px(400.)),
        )
    }
}

fn top_row(scroll: &gpui::UniformListScrollHandle) -> usize {
    let scroll = scroll.0.borrow();
    let size = scroll.last_item_size.expect("laid out");
    let row = size.contents.height / ROWS as f32;
    (-scroll.base_handle.offset().y / row).floor() as usize
}

/// Jumps across the whole list on even frames and steps one row on odd frames, so
/// every frame scrolls and lays out a fresh window of rows.
#[gpui::test]
#[ignore = "benchmark"]
fn perf_virtual_list_50k(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let (view, cx) = cx.add_window_view(|_, cx| Sweep {
        state: cx.new(|cx| VirtualListState::new(ROWS, cx)),
    });
    cx.simulate_resize(size(px(800.), px(600.)));
    // A pointer over the list would re-highlight whichever row scrolls under it.
    cx.simulate_mouse_move(point(px(700.), px(300.)), None, Modifiers::none());
    cx.run_until_parked();
    let state = view.read_with(cx, |sweep, _| sweep.state.clone());
    let mut frames = Vec::with_capacity(FRAMES);
    for frame in 0..FRAMES {
        let target = state.update(cx, |state, cx| {
            if frame % 2 == 0 {
                state.move_to(frame * 100);
            } else {
                state.move_by(1);
            }
            cx.notify();
            state.highlighted().unwrap()
        });
        cx.update(|window, cx| {
            let start = std::time::Instant::now();
            window.draw(cx).clear(cx);
            frames.push(start.elapsed().as_secs_f64() * 1000.);
        });
        // Debug selectors compile out of release builds, so check the scroll instead.
        let top = state.read_with(cx, |state, _| top_row(state.scroll_handle()));
        assert!(
            top <= target && target < top + 30,
            "frame {frame}: row {target} on screen, top {top}"
        );
    }
    assert_eq!(
        state.read_with(cx, |state, _| state.highlighted()),
        Some(49_801)
    );
    frames.sort_by(f64::total_cmp);
    let at = |q: f64| frames[((frames.len() - 1) as f64 * q).round() as usize];
    let (p50, p95, max) = (at(0.5), at(0.95), frames[frames.len() - 1]);
    eprintln!("PERF perf_virtual_list_50k_p50={p50:.3}");
    eprintln!("PERF perf_virtual_list_50k_p95={p95:.3}");
    eprintln!("PERF perf_virtual_list_50k_max={max:.3}");
    assert!(p95 < 8.0, "p95 {p95:.3} ms exceeds half the frame budget");
}
