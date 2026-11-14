//! Send to the agent from a Files editor and from a reply, by shortcut, menu, palette and
//! button, with the scripted Claude logging anything it is sent.

use super::*;
use crate::windows::open;
use crate::{install_app_state, open_windows};
use gpui::{
    Entity, Focusable, Modifiers, MouseButton, TestAppContext, VisualTestContext, point, px,
};
use keystone_agent::{Event, Item, ItemKind, ItemStatus};
use keystone_core::{ProjectId, Provider, Store, WindowId, WorkingDirectory};
use std::path::{Path, PathBuf};

const LIB: &str = "one\ntwo\n\tthree ```\nfour\n";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The scripted Claude, logging its messages in the session folder `folder`, where a
/// settings file shortens its pause between words. It runs as built, with no wrapper
/// script for the test to write and then execute.
fn claude(folder: &Path) -> PathBuf {
    let binary = keystone_core::test_support::built_binary("keystone-fake-claude");
    write(&folder.join(".fake-claude-delay-ms"), "2");
    binary
}

pub(crate) struct Run {
    _dir: tempfile::TempDir,
    pub(crate) folder: PathBuf,
}

impl Run {
    /// What the scripted Claude was sent.
    fn received(&self) -> String {
        std::fs::read_to_string(self.folder.join(".fake-claude-messages")).unwrap()
    }
}

/// A window on a project whose `src/lib.rs` holds `lib`, showing its Claude session
/// when `selected`, with the composer focused.
pub(crate) fn launch<'a>(
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
    let claude = claude(&folder);
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

pub(crate) fn composer(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer.read(cx).value().to_owned())
}

fn composer_focused(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> bool {
    let handle = view.read_with(cx, |view, cx| view.composer.focus_handle(cx));
    cx.update(|window, _| handle.is_focused(window))
}

pub(crate) fn click(
    cx: &mut VisualTestContext,
    id: &'static str,
    button: MouseButton,
    modifiers: Modifiers,
) {
    let at = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} is shown"))
        .center();
    cx.simulate_mouse_down(at, button, modifiers);
    cx.simulate_mouse_up(at, button, modifiers);
    cx.run_until_parked();
}

/// Opens Files and opens `src/lib.rs` in the editor, which takes focus.
pub(crate) fn view_lib(cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("secondary-shift-e");
    cx.run_until_parked();
    // Rows: the project folder, then src; Right expands src and steps into lib.rs.
    cx.simulate_keystrokes("down right");
    cx.run_until_parked();
    cx.simulate_keystrokes("right");
    cx.run_until_parked();
    cx.simulate_keystrokes("space");
    cx.run_until_parked();
    assert!(cx.debug_bounds("file-editor").is_some(), "the file shows");
}

/// Focuses the editor and selects lines `first` to `last`, 0-based, from the start of
/// the first to the start of the line after the last.
pub(crate) fn select_lines(cx: &mut VisualTestContext, first: usize, last: usize) {
    click(cx, "file-editor", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("secondary-home");
    for _ in 0..first {
        cx.simulate_keystrokes("down");
    }
    for _ in first..=last {
        cx.simulate_keystrokes("shift-down");
    }
    cx.run_until_parked();
}

pub(crate) fn fenced(lines: std::ops::RangeInclusive<usize>, text: &str) -> String {
    Excerpt::file("src/lib.rs".into(), lines, text).prompt()
}

/// Lines selected in a Files editor go into the composer with their path and numbers
/// by the shortcut, the menu, the palette and the button, and nothing is sent.
#[gpui::test]
fn files_lines_go_into_the_composer_by_every_entry_point(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    cx.simulate_input("Look at");
    view_lib(cx);
    assert!(
        cx.debug_bounds("file-view-send").is_some(),
        "the view offers Send"
    );

    select_lines(cx, 1, 2);
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    let two = fenced(2..=3, "two\n\tthree ```");
    assert_eq!(composer(&view, cx), format!("Look at\n{two}"));
    assert!(composer_focused(&view, cx), "the composer has focus");

    select_lines(cx, 3, 3);
    click(cx, "file-editor", MouseButton::Right, Modifiers::none());
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
        cx.debug_bounds("file-editor").is_some(),
        "and keeps the file shown"
    );
    click(cx, "file-editor", MouseButton::Right, Modifiers::none());
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

    select_lines(cx, 0, 0);
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("Send selection");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let one = fenced(1..=1, "one");
    assert_eq!(
        composer(&view, cx),
        format!("Look at\n{two}\n{four}\n{one}")
    );

    select_lines(cx, 3, 3);
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
        "the editor's lines go only while it has focus"
    );

    click(cx, "file-editor", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("file-editor").is_none(),
        "Escape shows the tree"
    );
    cx.simulate_keystrokes("space");
    cx.run_until_parked();
    click(cx, "file-view-back", MouseButton::Left, Modifiers::none());
    assert!(
        cx.debug_bounds("file-editor").is_none(),
        "Back shows the tree"
    );
    assert!(cx.debug_bounds("files-tree-row-1").is_some());
}

/// A selection sends every line it touches whole, and a selection that ends at the
/// start of a line leaves that line out.
#[gpui::test]
fn a_selection_sends_the_whole_lines_it_covers(cx: &mut TestAppContext) {
    let (_run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    cx.simulate_keystrokes("down right shift-right");
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    let two = fenced(2..=2, "two");
    assert_eq!(composer(&view, cx), two, "part of a line sends the line");

    click(cx, "file-editor", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("secondary-home down end shift-down shift-down shift-right");
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    let both = fenced(2..=4, "two\n\tthree ```\nfour");
    assert_eq!(composer(&view, cx), format!("{two}\n{both}"));

    click(cx, "file-editor", MouseButton::Left, Modifiers::none());
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

/// A right click on a reply offers only that reply's own selection. Here the Files editor
/// has focus with a line selected, which the shortcut would send, and the reply was
/// never selected; the reply's menu must not offer the line.
#[gpui::test]
fn a_reply_menu_never_offers_another_views_selection(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    show_reply(&view, "Plain **bold** words here.", cx);
    view_lib(cx);
    select_lines(cx, 1, 1);
    let taken = view.update_in(cx, |view, window, cx| view.take_selection(window, cx));
    assert_eq!(
        taken.map(|(_, excerpt)| excerpt.prompt()),
        Some(fenced(2..=2, "two")),
        "the Files editor has focus and its line is selected"
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

/// Lines selected before the file changed on disk are never sent: their numbers may
/// no longer match, by the shortcut and by the palette. Once the editor has taken the
/// new text, lines send again.
#[gpui::test]
fn a_file_changed_since_it_was_shown_is_not_sent_until_reloaded(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    let lib = run.folder.join("src/lib.rs");
    select_lines(cx, 1, 1);
    std::fs::write(&lib, "zero\none\ntwo, longer now\n").unwrap();
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), "", "the old line 2 is not sent");
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    select_lines(cx, 1, 1);
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), fenced(2..=2, "one"));

    select_lines(cx, 2, 2);
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
    select_lines(cx, 1, 1);
    click(cx, "file-editor", MouseButton::Right, Modifiers::none());
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
        select_lines(cx, 1, 1);
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
    select_lines(cx, 1, 1);
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), fenced(2..=2, ""));
}
