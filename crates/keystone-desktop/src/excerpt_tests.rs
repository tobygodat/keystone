//! Send to the agent from a Files view and from a reply, by shortcut, menu, palette and
//! button, with the scripted Claude logging anything it is sent.

use super::*;
use crate::windows::open;
use crate::{install_app_state, open_windows};
use gpui::{
    Entity, Focusable, Modifiers, MouseButton, TestAppContext, VisualTestContext, point, px,
};
use keystone_agent::{Event, Item, ItemKind, ItemStatus};
use keystone_core::{ProjectId, Provider, Store, WindowId, WorkingDirectory};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const LIB: &str = "one\ntwo\n\tthree ```\nfour\n";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The scripted Claude, logging its messages in the session folder.
fn claude(dir: &Path) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("keystone-fake-claude");
    assert!(
        binary.exists(),
        "build keystone-fake-claude first: cargo build -p keystone-agent --bins"
    );
    let script = dir.join("claude-excerpt-wrapper");
    std::fs::write(
        &script,
        format!(
            "#!/usr/bin/python3\nimport os, sys\nos.environ['KEYSTONE_FAKE_CLAUDE_DELAY_MS'] = '2'\nbinary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

struct Run {
    _dir: tempfile::TempDir,
    folder: PathBuf,
}

impl Run {
    /// What the scripted Claude was sent.
    fn received(&self) -> String {
        std::fs::read_to_string(self.folder.join(".fake-claude-messages")).unwrap()
    }
}

/// A window on a project whose `src/lib.rs` holds `lib`, showing its Claude session
/// when `selected`, with the composer focused.
fn launch<'a>(
    lib: &str,
    selected: bool,
    cx: &'a mut TestAppContext,
) -> (Run, Entity<Keystone>, &'a mut VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap().join("project");
    write(&folder.join("src/lib.rs"), lib);
    write(&folder.join(".fake-claude-messages"), "");
    let project = ProjectId::new(&folder).unwrap();
    let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
    let directory = WorkingDirectory::new(&folder).unwrap();
    let id = store.create(Provider::Claude, &directory, 1).unwrap().value;
    store
        .open_project(WindowId::FIRST, &project, None, selected.then_some(id), 10)
        .unwrap();
    let claude = claude(dir.path());
    cx.executor().allow_parking();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state = install_app_state(store, cx);
        state.update(cx, |state, _| {
            state.test_programs = Some((claude.clone(), claude))
        });
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            open(state.clone(), id, placement, true, false, cx).unwrap();
        }
    });
    cx.run_until_parked();
    let view = cx.update(|cx| open_windows(cx))[0].clone();
    let handle = cx.read(|cx| view.read(cx).handle);
    let cx = VisualTestContext::from_window(handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    view.update_in(cx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
    (Run { _dir: dir, folder }, view, cx)
}

fn composer(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer.read(cx).value().to_owned())
}

fn composer_focused(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> bool {
    let handle = view.read_with(cx, |view, cx| view.composer.focus_handle(cx));
    cx.update(|window, _| handle.is_focused(window))
}

fn click(cx: &mut VisualTestContext, id: &'static str, button: MouseButton, modifiers: Modifiers) {
    let at = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} is shown"))
        .center();
    cx.simulate_mouse_down(at, button, modifiers);
    cx.simulate_mouse_up(at, button, modifiers);
    cx.run_until_parked();
}

/// Opens Files and shows `src/lib.rs` read-only.
fn view_lib(cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("secondary-shift-e");
    cx.run_until_parked();
    // Rows: the project folder, then src; Right expands src and steps into lib.rs.
    cx.simulate_keystrokes("down right");
    cx.run_until_parked();
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    cx.simulate_keystrokes("space");
    cx.run_until_parked();
    assert!(cx.debug_bounds("file-line-0").is_some(), "the file shows");
}

fn fenced(lines: std::ops::RangeInclusive<usize>, text: &str) -> String {
    Excerpt::file("src/lib.rs".into(), lines, text).prompt()
}

/// Lines picked in a Files view go into the composer with their path and numbers by
/// the shortcut, the menu, the palette and the button, and nothing is sent.
#[gpui::test]
fn files_lines_go_into_the_composer_by_every_entry_point(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    cx.simulate_input("Look at");
    view_lib(cx);
    assert!(
        cx.debug_bounds("file-view-send").is_some(),
        "the view offers Send"
    );

    click(cx, "file-line-1", MouseButton::Left, Modifiers::none());
    click(cx, "file-line-2", MouseButton::Left, Modifiers::shift());
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    let two = fenced(2..=3, "two\n\tthree ```");
    assert_eq!(composer(&view, cx), format!("Look at\n{two}"));
    assert!(composer_focused(&view, cx), "the composer has focus");

    click(cx, "file-line-3", MouseButton::Right, Modifiers::none());
    assert!(
        cx.debug_bounds("menu-send-selection").is_some(),
        "the menu opens"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("menu-send-selection").is_none(),
        "Escape closes the menu"
    );
    assert!(
        cx.debug_bounds("file-line-3").is_some(),
        "and keeps the file shown"
    );
    click(cx, "file-line-3", MouseButton::Right, Modifiers::none());
    click(
        cx,
        "menu-send-selection",
        MouseButton::Left,
        Modifiers::none(),
    );
    let four = fenced(4..=4, "four");
    assert_eq!(composer(&view, cx), format!("Look at\n{two}\n{four}"));
    assert!(
        cx.debug_bounds("menu-send-selection").is_none(),
        "the menu closes"
    );

    click(cx, "file-line-0", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("Send selection");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let one = fenced(1..=1, "one");
    assert_eq!(
        composer(&view, cx),
        format!("Look at\n{two}\n{four}\n{one}")
    );

    click(cx, "file-line-3", MouseButton::Left, Modifiers::none());
    click(cx, "file-view-send", MouseButton::Left, Modifiers::none());
    assert_eq!(
        composer(&view, cx),
        format!("Look at\n{two}\n{four}\n{one}\n{four}")
    );
    assert_eq!(run.received(), "", "nothing is sent");

    let before = composer(&view, cx);
    view.update_in(cx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(
        composer(&view, cx),
        before,
        "the view's lines go only while it has focus"
    );

    click(cx, "file-line-0", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("file-line-0").is_none(),
        "Escape shows the tree"
    );
    cx.simulate_keystrokes("space");
    cx.run_until_parked();
    click(cx, "file-view-back", MouseButton::Left, Modifiers::none());
    assert!(
        cx.debug_bounds("file-line-0").is_none(),
        "Back shows the tree"
    );
    assert!(cx.debug_bounds("files-tree-row-1").is_some());
}

/// Dragging over lines selects them all, and Ctrl+A every line.
#[gpui::test]
fn dragging_and_select_all_pick_whole_lines(cx: &mut TestAppContext) {
    let (_run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    let from = cx.debug_bounds("file-line-0").unwrap().center();
    let to = cx.debug_bounds("file-line-2").unwrap().center();
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), fenced(1..=3, "one\ntwo\n\tthree ```"));
    click(cx, "file-line-0", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("secondary-a secondary-shift-a");
    cx.run_until_parked();
    assert!(composer(&view, cx).ends_with(&fenced(1..=4, LIB.trim_end())));
}

/// A selection over 64 KiB keeps its first and last 32 KiB, as tool output does.
#[gpui::test]
fn a_large_selection_is_capped_at_both_ends(cx: &mut TestAppContext) {
    let line = "0123456789".repeat(5);
    let lib = format!("{line}\n").repeat(2048);
    assert!(lib.len() >= 100 * 1024);
    let (_run, view, cx) = launch(&lib, true, cx);
    view_lib(cx);
    click(cx, "file-line-0", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("secondary-a secondary-shift-a");
    cx.run_until_parked();
    let text = lib.trim_end();
    let expected = fenced(1..=2048, text);
    assert_eq!(composer(&view, cx), expected);
    assert!(expected.contains(&format!("… {} bytes omitted …", text.len() - 64 * 1024)));
}

/// A window showing no session has nowhere to send: the welcome composer stays empty,
/// no menu opens, and the palette row does nothing.
#[gpui::test]
fn with_no_session_nothing_is_sent(cx: &mut TestAppContext) {
    let (_run, view, cx) = launch(LIB, false, cx);
    let taken = (
        1,
        std::rc::Rc::new(Excerpt::file("src/lib.rs".into(), 2..=2, "two")),
    );
    view.update_in(cx, |view, window, cx| {
        assert_eq!(view.selected(cx), None);
        assert!(view.take_selection(window, cx).is_none());
        view.send_excerpt(&taken, window, cx);
        view.open_selection_menu(Some(taken.clone()), point(px(10.), px(10.)), cx);
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("menu-send-selection").is_none(),
        "for a session not shown"
    );
    cx.simulate_keystrokes("secondary-shift-a secondary-k");
    cx.simulate_input("Send selection");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), "");
}

/// Shows a completed reply holding the text in the window's session, once it has loaded.
fn show_reply(view: &Entity<Keystone>, text: &str, cx: &mut VisualTestContext) {
    let session = view.read_with(cx, |view, cx| view.selected(cx).unwrap());
    let loaded = std::time::Instant::now();
    while view.read_with(cx, |view, cx| {
        view.state.read(cx).conversations()[&session].is_loading()
    }) {
        assert!(loaded.elapsed().as_secs() < 15, "the conversation loads");
        cx.run_until_parked();
    }
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, _| {
            state
                .test_conversation(session)
                .apply(Event::ItemCompleted(Item {
                    id: "reply".into(),
                    kind: ItemKind::AgentMessage,
                    text: text.into(),
                    status: ItemStatus::Completed,
                }));
        });
        view.commit(session, cx);
        cx.notify();
    });
    cx.run_until_parked();
}

/// Text selected in a reply goes into the composer quoted, by shortcut, menu and
/// palette, and nothing is sent.
#[gpui::test]
fn a_reply_selection_goes_into_the_composer_quoted(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    show_reply(&view, "Plain **bold** words here.\n\nSecond paragraph.", cx);
    let select = |cx: &mut VisualTestContext| {
        let bounds = cx.debug_bounds("reply-0-selection-surface").unwrap();
        let from = bounds.origin + point(px(1.), px(8.));
        let to = from + point(px(300.), px(0.));
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        from
    };
    let quote = "> Plain bold words here.";

    select(cx);
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), quote);
    assert!(composer_focused(&view, cx));

    let at = select(cx);
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    click(
        cx,
        "menu-send-selection",
        MouseButton::Left,
        Modifiers::none(),
    );
    assert_eq!(composer(&view, cx), format!("{quote}\n{quote}"));

    select(cx);
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("Send selection");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), format!("{quote}\n{quote}\n{quote}"));

    let at = select(cx);
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("menu-send-selection").is_none(),
        "Escape closes the menu"
    );
    assert_eq!(
        cx.update(|window, cx| keystone_ui::markdown::focused_selection(window, cx))
            .as_deref(),
        Some("Plain bold words here."),
        "and keeps the selection"
    );
    assert_eq!(run.received(), "", "nothing is sent");
}

/// A right click on a reply offers only that reply's own selection. Here the Files view
/// has focus with a line selected, which the shortcut would send, and the reply was
/// never selected; the reply's menu must not offer the line.
#[gpui::test]
fn a_reply_menu_never_offers_another_views_selection(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    show_reply(&view, "Plain **bold** words here.", cx);
    view_lib(cx);
    click(cx, "file-line-1", MouseButton::Left, Modifiers::none());
    let taken = view.update_in(cx, |view, window, cx| view.take_selection(window, cx));
    assert_eq!(
        taken.map(|(_, excerpt)| excerpt.prompt()),
        Some(fenced(2..=2, "two")),
        "the Files view has focus and its line is selected"
    );
    let at = cx
        .debug_bounds("reply-0-selection-surface")
        .expect("the reply shows beside the Files view")
        .origin
        + point(px(1.), px(8.));
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("menu-send-selection").is_none(),
        "the reply's menu does not offer the Files view's line"
    );
    assert_eq!(run.received(), "", "nothing is sent");
}

/// Lines selected before the file changed on disk are never sent: the next send reads
/// the file again and clears the selection, by the shortcut and by the palette.
#[gpui::test]
fn a_file_changed_since_it_was_shown_is_read_again_before_sending(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    let lib = run.folder.join("src/lib.rs");
    click(cx, "file-line-1", MouseButton::Left, Modifiers::none());
    std::fs::write(&lib, "zero\none\ntwo, longer now\n").unwrap();
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), "", "the old line 2 is not sent");
    click(cx, "file-line-1", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), fenced(2..=2, "one"));

    click(cx, "file-line-2", MouseButton::Left, Modifiers::none());
    std::fs::write(&lib, "changed again\n").unwrap();
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("Send selection");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        composer(&view, cx),
        fenced(2..=2, "one"),
        "the palette sends nothing stale"
    );
}

/// A selection menu closes once the window shows another session, here through a
/// notification, which presses no key.
#[gpui::test]
fn a_selection_menu_closes_when_the_session_changes(cx: &mut TestAppContext) {
    let (_run, view, cx) = launch(LIB, true, cx);
    cx.simulate_keystrokes("secondary-n");
    cx.run_until_parked();
    click(cx, "session-1", MouseButton::Left, Modifiers::none());
    view_lib(cx);
    click(cx, "file-line-1", MouseButton::Right, Modifiers::none());
    assert!(cx.debug_bounds("menu-send-selection").is_some());
    cx.update(|_, cx| cx.defer(|cx| crate::notifications::show(2, cx)));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, cx| view.selected(cx)), Some(2));
    assert!(cx.debug_bounds("menu-send-selection").is_none());
    assert!(view.read_with(cx, |view, _| view.context_menu.is_none()));
}

/// The palette's Send, opened over a selection, puts it only in the session it was taken
/// for: nothing while a notification has the window on another session, and the
/// selection once the window is back on its own.
#[gpui::test]
fn the_palette_sends_only_into_the_session_it_was_opened_on(cx: &mut TestAppContext) {
    let (_run, view, cx) = launch(LIB, true, cx);
    cx.simulate_keystrokes("secondary-n");
    cx.run_until_parked();
    click(cx, "session-1", MouseButton::Left, Modifiers::none());
    view_lib(cx);
    let palette_over_line_2 = |cx: &mut VisualTestContext| {
        click(cx, "file-line-1", MouseButton::Left, Modifiers::none());
        cx.simulate_keystrokes("secondary-k");
        cx.simulate_input("Send selection");
        cx.run_until_parked();
    };
    let show = |session: u64, cx: &mut VisualTestContext| {
        cx.update(|_, cx| cx.defer(move |cx| crate::notifications::show(session, cx)));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| view.selected(cx)),
            Some(session)
        );
    };
    let pick = |cx: &mut VisualTestContext| {
        click(cx, "palette-input", MouseButton::Left, Modifiers::none());
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(cx.debug_bounds("palette").is_none(), "Enter picked the row");
    };

    palette_over_line_2(cx);
    show(2, cx);
    pick(cx);
    assert_eq!(composer(&view, cx), "");
    click(cx, "session-1", MouseButton::Left, Modifiers::none());
    assert_eq!(composer(&view, cx), "");

    palette_over_line_2(cx);
    show(2, cx);
    show(1, cx);
    pick(cx);
    assert_eq!(composer(&view, cx), fenced(2..=2, "two"));
}

/// A blank line still has a path and a number to send.
#[gpui::test]
fn a_blank_line_is_sent(cx: &mut TestAppContext) {
    let (_run, view, cx) = launch("one\n\nthree\n", true, cx);
    view_lib(cx);
    click(cx, "file-line-1", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), fenced(2..=2, ""));
}
