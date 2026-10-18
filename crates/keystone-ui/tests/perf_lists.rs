use gpui::{
    Context, Entity, IntoElement, Modifiers, Render, SharedString, TestAppContext, Window, div,
    point, prelude::*, px, size,
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

const OPTIONS: usize = 500;
const OPENS: usize = 200;

struct Picker {
    options: Vec<SharedString>,
    chosen: usize,
}

impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.chosen * 7 % OPTIONS;
        div().size_full().p(px(16.)).child(
            Select::new("perf-select", self.options.clone(), Some(selected)).on_change(
                cx.listener(|this, _: &usize, _, cx| {
                    this.chosen += 1;
                    cx.notify();
                }),
            ),
        )
    }
}

/// Opens a 500-option select from its focused trigger and times the keystroke that
/// opens it, which includes drawing the frame that first shows the menu. Each open
/// starts on a different choice, so rows are shaped afresh, and Enter closes it.
#[gpui::test]
#[ignore = "benchmark"]
fn perf_select_open_500(cx: &mut TestAppContext) {
    cx.update(keystone_ui::init);
    let (view, cx) = cx.add_window_view(|_, _| Picker {
        options: (0..OPTIONS).map(|n| format!("Option {n}").into()).collect(),
        chosen: 0,
    });
    cx.simulate_resize(size(px(800.), px(600.)));
    cx.run_until_parked();
    // Debug selectors compile out of release builds, so press the trigger by position.
    // A press opens the menu and Escape closes it, leaving the trigger focused.
    cx.simulate_click(point(px(40.), px(30.)), Modifiers::none());
    cx.simulate_keystrokes("escape");
    let mut frames = Vec::with_capacity(OPENS);
    for _ in 0..OPENS {
        let start = std::time::Instant::now();
        cx.simulate_keystrokes("down");
        frames.push(start.elapsed().as_secs_f64() * 1000.);
        cx.simulate_keystrokes("enter");
    }
    assert_eq!(
        view.read_with(cx, |picker, _| picker.chosen),
        OPENS,
        "every open chose"
    );
    frames.sort_by(f64::total_cmp);
    let at = |q: f64| frames[((frames.len() - 1) as f64 * q).round() as usize];
    let (p50, p95, max) = (at(0.5), at(0.95), frames[frames.len() - 1]);
    eprintln!("PERF perf_select_open_500_p50={p50:.3}");
    eprintln!("PERF perf_select_open_500_p95={p95:.3}");
    eprintln!("PERF perf_select_open_500_max={max:.3}");
    assert!(p95 < 16.7, "p95 {p95:.3} ms exceeds the frame budget");
}

struct Files {
    state: Entity<TreeState>,
}

impl Render for Files {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().flex_col().child(
            Tree::new("perf-tree", &self.state, |entry, _, _, _| {
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::File).size(px(14.)))
                    .child(Label::new(entry.name.clone()).truncate())
                    .into_any_element()
            })
            .flex_1()
            .w(px(400.)),
        )
    }
}

/// Five folders of 10,000 files each, 50,005 nodes, with the first `open` expanded.
fn open_tree(
    cx: &mut TestAppContext,
    open: usize,
) -> (Entity<TreeState>, &mut gpui::VisualTestContext) {
    cx.update(keystone_ui::init);
    let (view, cx) = cx.add_window_view(|_, cx| {
        let mut model = TreeModel::new((0..5).map(|n| (format!("folder-{n}"), NodeKind::Folder)));
        for folder in 0..5 {
            model.set_children(
                folder,
                (0..10_000).map(|n| (format!("file-{folder}-{n}.rs"), NodeKind::File)),
            );
        }
        for folder in (0..open).rev() {
            model.expand(folder);
        }
        Files {
            state: cx.new(|cx| TreeState::new(model, cx)),
        }
    });
    cx.simulate_resize(size(px(800.), px(600.)));
    cx.simulate_mouse_move(point(px(700.), px(300.)), None, Modifiers::none());
    cx.run_until_parked();
    let state = view.read_with(cx, |files, _| files.state.clone());
    (state, cx)
}

fn report(name: &str, mut frames: Vec<f64>) -> (f64, f64) {
    frames.sort_by(f64::total_cmp);
    let at = |q: f64| frames[((frames.len() - 1) as f64 * q).round() as usize];
    let (p50, p95, max) = (at(0.5), at(0.95), frames[frames.len() - 1]);
    eprintln!("PERF {name}_p50={p50:.3}");
    eprintln!("PERF {name}_p95={p95:.3}");
    eprintln!("PERF {name}_max={max:.3}");
    (p95, max)
}

/// Sweeps the fully expanded 50,005-row tree the way `perf_virtual_list_50k` sweeps the list.
#[gpui::test]
#[ignore = "benchmark"]
fn perf_tree_50k(cx: &mut TestAppContext) {
    let (state, cx) = open_tree(cx, 5);
    let (list, rows) = state.read_with(cx, |state, _| {
        (state.list().clone(), state.model().visible().len())
    });
    assert_eq!(rows, 50_005);
    let mut frames = Vec::with_capacity(FRAMES);
    for frame in 0..FRAMES {
        let target = if frame % 2 == 0 {
            frame * 100
        } else {
            frame * 100 - 99
        };
        state.update(cx, |state, cx| state.highlight(target, cx));
        cx.update(|window, cx| {
            let start = std::time::Instant::now();
            window.draw(cx).clear(cx);
            frames.push(start.elapsed().as_secs_f64() * 1000.);
        });
        let top = list.read_with(cx, |list, _| {
            let scroll = list.scroll_handle().0.borrow();
            let size = scroll.last_item_size.expect("laid out");
            let row = size.contents.height / rows as f32;
            (-scroll.base_handle.offset().y / row).floor() as usize
        });
        assert!(
            top <= target && target < top + 30,
            "frame {frame}: row {target} on screen, top {top}"
        );
    }
    let (p95, _) = report("perf_tree_50k", frames);
    assert!(p95 < 8.0, "p95 {p95:.3} ms exceeds half the frame budget");
}

/// Expands a 10,000-child folder inside the 50,005-node tree and draws the next frame,
/// then collapses it, fifty times. Each sample is the expand plus its frame.
#[gpui::test]
#[ignore = "benchmark"]
fn perf_tree_expand_10k(cx: &mut TestAppContext) {
    let (state, cx) = open_tree(cx, 0);
    let mut samples = Vec::new();
    for _ in 0..50 {
        cx.update(|window, cx| {
            let start = std::time::Instant::now();
            state.update(cx, |state, cx| state.expand(2, cx));
            window.draw(cx).clear(cx);
            samples.push(start.elapsed().as_secs_f64() * 1000.);
        });
        assert_eq!(
            state.read_with(cx, |s, _| s.model().visible().len()),
            10_005
        );
        cx.update(|window, cx| {
            state.update(cx, |state, cx| state.collapse(2, cx));
            window.draw(cx).clear(cx);
        });
    }
    let (_, max) = report("perf_tree_expand_10k", samples);
    assert!(max < 16.7, "expand {max:.3} ms misses the frame");
}

const PALETTE_VERBS: [&str; 25] = [
    "Open",
    "Close",
    "New",
    "Rename",
    "Delete",
    "Duplicate",
    "Move",
    "Copy",
    "Pin",
    "Unpin",
    "Archive",
    "Restore",
    "Export",
    "Import",
    "Reload",
    "Focus",
    "Toggle",
    "Show",
    "Hide",
    "Search",
    "Filter",
    "Sort",
    "Reset",
    "Share",
    "Inspect",
];
const PALETTE_NOUNS: [&str; 40] = [
    "session",
    "project",
    "worktree",
    "branch",
    "commit",
    "terminal",
    "file",
    "folder",
    "tab",
    "pane",
    "sidebar",
    "composer",
    "transcript",
    "settings",
    "theme",
    "keymap",
    "model",
    "provider",
    "approval",
    "diff",
    "review",
    "comment",
    "issue",
    "pull request",
    "workspace",
    "layout",
    "split",
    "window",
    "notification",
    "log",
    "trace",
    "profile",
    "snippet",
    "bookmark",
    "history",
    "attachment",
    "screenshot",
    "agent",
    "task",
    "schedule",
];
const PALETTE_SCOPES: [&str; 10] = [
    "",
    " in new window",
    " for all sessions",
    ": current project",
    " to the right",
    " from history",
    " (Claude)",
    " (Codex)",
    " and keep focus",
    " in background",
];
const PALETTE_QUERIES: [&str; 10] = [
    "new session",
    "open settings",
    "toggle sidebar",
    "nwss",
    "rename worktree",
    "export logs",
    "focus composer",
    "set theme dark",
    "pull req",
    "shw ntf",
];

fn palette_items(count: usize) -> Vec<PaletteItem> {
    let mut items = Vec::with_capacity(count);
    'fill: for scope in PALETTE_SCOPES {
        for noun in PALETTE_NOUNS {
            for verb in PALETTE_VERBS {
                if items.len() == count {
                    break 'fill;
                }
                let n = items.len();
                let mut item =
                    PaletteItem::new(format!("cmd-{n}"), format!("{verb} {noun}{scope}"));
                if n % 3 == 0 {
                    item = item.detail(format!("{noun} commands"));
                }
                if n % 7 == 0 {
                    item = item.kbd(format!("Ctrl+{}", (b'A' + (n % 26) as u8) as char));
                }
                items.push(item);
            }
        }
    }
    assert_eq!(items.len(), count);
    items
}

fn palette_keystrokes() -> Vec<String> {
    let mut steps = Vec::new();
    for query in PALETTE_QUERIES.iter().chain(&PALETTE_QUERIES) {
        let chars: Vec<_> = query
            .char_indices()
            .map(|(at, c)| at + c.len_utf8())
            .collect();
        steps.extend(chars.iter().map(|&end| query[..end].to_owned()));
        steps.extend(
            chars
                .iter()
                .rev()
                .skip(1)
                .map(|&end| query[..end].to_owned()),
        );
        steps.push(String::new());
    }
    steps
}

/// Types and backspaces each query one keystroke at a time and times only the
/// rescore and sort the palette runs after each edit.
fn palette_filter(name: &str, count: usize, cx: &mut TestAppContext) {
    cx.update(TextInput::bind_keys);
    cx.update(keystone_ui::init);
    let state = cx.update(|cx| cx.new(|cx| CommandPaletteState::new(palette_items(count), cx)));
    let steps = palette_keystrokes();
    assert!(steps.len() >= 200, "{} steps", steps.len());
    let mut times = Vec::with_capacity(steps.len());
    for query in &steps {
        let shown = state.update(cx, |state, cx| {
            let start = std::time::Instant::now();
            state.set_query(query, cx);
            times.push(start.elapsed().as_secs_f64() * 1000.);
            state.matches().count()
        });
        if query.is_empty() {
            assert_eq!(shown, count, "an empty query shows every item");
        }
    }
    let first = state.update(cx, |state, cx| {
        state.set_query("new session", cx);
        state.matches().next().map(|item| item.label.to_string())
    });
    assert_eq!(first.as_deref(), Some("New session"));
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
    let (p50, p95, max) = (at(0.5), at(0.95), times[times.len() - 1]);
    eprintln!("PERF {name}_steps={}", times.len());
    eprintln!("PERF {name}_p50={p50:.3}");
    eprintln!("PERF {name}_p95={p95:.3}");
    eprintln!("PERF {name}_max={max:.3}");
    assert!(
        p95 < 4.0,
        "p95 {p95:.3} ms exceeds the 4 ms keystroke budget"
    );
}

#[gpui::test]
#[ignore = "benchmark"]
fn perf_palette_filter_10k(cx: &mut TestAppContext) {
    palette_filter("perf_palette_filter_10k", 10_000, cx);
}

#[gpui::test]
#[ignore = "benchmark"]
fn perf_palette_filter_2k(cx: &mut TestAppContext) {
    palette_filter("perf_palette_filter_2k", 2_000, cx);
}
