use super::*;
use crate::windows::open;
use crate::{AppState, close_window, install_app_state, open_windows, request_quit};
use gpui::{
    AnyWindowHandle, Focusable, Keystroke, Modifiers, TestAppContext, VisualTestContext, WeakEntity,
};
use keystone_core::{Provider, Store, WorkingDirectory};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

mod drawer;
mod input;
mod multi_window;
mod teardown;

/// A workspace with one Codex session whose folder is a fresh temporary directory.
fn seeded() -> (tempfile::TempDir, PathBuf, PathBuf) {
    seeded_with(1)
}

/// A workspace with `sessions` Codex sessions in one fresh temporary folder, the first
/// selected.
fn seeded_with(sessions: u64) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let project = root.join("project");
    std::fs::create_dir(&project).unwrap();
    let path = root.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    for now in 1..=sessions {
        store
            .create(
                Provider::Codex,
                &WorkingDirectory::new(&project).unwrap(),
                now,
            )
            .unwrap();
    }
    store.select(1).unwrap();
    (dir, path, project)
}

fn launch(path: &Path, cx: &mut TestAppContext) -> (Entity<Keystone>, AnyWindowHandle) {
    let store = Store::open(path).unwrap();
    cx.executor().allow_parking();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state: Entity<AppState> = install_app_state(store, cx);
        use_shell("/bin/sh", &[], cx);
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            open(state.clone(), id, placement, true, false, cx).unwrap();
        }
    });
    cx.run_until_parked();
    let view = cx.update(|cx| open_windows(cx))[0].clone();
    let handle = cx.read(|cx| view.read(cx).handle);
    (view, handle)
}

fn visual(handle: AnyWindowHandle, cx: &mut TestAppContext) -> &mut VisualTestContext {
    let cx = VisualTestContext::from_window(handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    cx.run_until_parked();
    cx
}

fn terminal(cx: &mut VisualTestContext) -> Option<Entity<SessionTerminal>> {
    cx.update(|_, cx| session_terminal(1, cx))
}

/// Every pane's output, scrollback included.
fn output(cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_, cx| {
        session_terminal(1, cx).map_or_else(Vec::new, |t| {
            t.read(cx).panes.iter().map(Pane::text).collect()
        })
    })
}

fn pids(cx: &mut VisualTestContext) -> Vec<u32> {
    cx.update(|_, cx| {
        session_terminal(1, cx).map_or_else(Vec::new, |t| {
            t.read(cx).panes.iter().map(|pane| pane.pid).collect()
        })
    })
}

/// Lets the shell's IO thread deliver until `done` holds, or fails after 10 s.
fn wait_until(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn has_line(lines: &[String], pane: usize, line: &str) -> bool {
    lines
        .get(pane)
        .is_some_and(|text| text.lines().any(|l| l.trim() == line))
}

fn run(cx: &mut VisualTestContext, command: &str) {
    cx.simulate_input(command);
    cx.simulate_keystrokes("enter");
}

/// Whether `pid` runs. A zombie has exited and only waits for its parent to reap it.
fn alive(pid: u32) -> bool {
    !matches!(keystone_agent::host::process_state(pid), None | Some('Z'))
}

fn pane_focused(cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| {
        session_terminal(1, cx).is_some_and(|t| t.read(cx).contains_focus(window))
    })
}

/// Opens the selected session in a second window and returns its handle.
fn second_window(
    first: &Entity<Keystone>,
    handle: AnyWindowHandle,
    cx: &mut TestAppContext,
) -> AnyWindowHandle {
    visual(handle, cx).simulate_keystrokes("secondary-alt-shift-n");
    cx.run_until_parked();
    let second = cx
        .update(|cx| open_windows(cx))
        .into_iter()
        .find(|view| view != first)
        .expect("a second window");
    let other = cx.read(|cx| second.read(cx).handle);
    visual(other, cx);
    other
}

fn copy_keys() -> &'static str {
    if cfg!(target_os = "macos") {
        "secondary-c"
    } else {
        "secondary-shift-c"
    }
}

fn paste_keys() -> &'static str {
    if cfg!(target_os = "macos") {
        "secondary-v"
    } else {
        "secondary-shift-v"
    }
}

/// The screen row showing exactly `line` in the first pane.
fn row_of(cx: &mut VisualTestContext, line: &str) -> Option<usize> {
    use alacritty_terminal::grid::Dimensions;
    cx.update(|_, cx| {
        let terminal = session_terminal(1, cx).unwrap();
        let term = terminal.read(cx).panes[0].term.lock();
        (0..term.screen_lines()).position(|row| {
            let at = |column| GridPoint::new(Line(row as i32), column);
            term.bounds_to_string(at(Column(0)), at(term.last_column()))
                .trim()
                == line
        })
    })
}

/// Drags across `columns` of `row` in the first pane, as this window lays it out.
fn drag_select(cx: &mut VisualTestContext, row: usize, columns: std::ops::RangeInclusive<usize>) {
    let (view, cell) = cx.update(|window, cx| {
        let terminal = session_terminal(1, cx).unwrap();
        let pane = &terminal.read(cx).panes[0];
        let view = pane.view(window.window_handle().window_id());
        (view.expect("this window paints the pane"), pane.size().cell)
    });
    let at = |column: f32| {
        view.bounds.origin + gpui::point(cell.width * column, cell.height * (row as f32 + 0.5))
    };
    let start = at(*columns.start() as f32 + 0.2);
    let end = at(*columns.end() as f32 + 0.8);
    cx.simulate_mouse_down(start, gpui::MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(end, gpui::MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(end, gpui::MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} was not written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits up to 3 s for `pid` to be gone, and says whether it is.
fn gone_within_3s(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

fn composer_focused(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| view.read(cx).composer.focus_handle(cx).is_focused(window))
}

fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} is shown"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}
