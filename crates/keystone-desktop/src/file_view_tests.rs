//! The Files editor as a user drives it: typing, saving, changes made outside Keystone,
//! leaving with edits, sending lines, and Quit, against a file in a temporary project.

use super::*;
use crate::app_state::Phase;
use crate::excerpt::ui_tests::{Run, click, composer, fenced, launch, select_lines, view_lib};
use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext};
use std::path::PathBuf;

const LIB: &str = "one\ntwo\nthree\n";

fn lib(run: &Run) -> PathBuf {
    run.folder.join("src/lib.rs")
}

fn on_disk(run: &Run) -> String {
    std::fs::read_to_string(lib(run)).unwrap()
}

fn editor_text(keystone: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    keystone.read_with(cx, |keystone, cx| {
        let view = keystone.file_view(cx).expect("the editor is open");
        view.read(cx)
            .open()
            .expect("the file is open")
            .editor
            .read(cx)
            .text()
    })
}

fn shown(cx: &mut VisualTestContext, id: &'static str) -> bool {
    cx.debug_bounds(id).is_some()
}

/// Lets the editor's once-a-second check for outside changes run.
fn tick(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(WATCH_INTERVAL);
    cx.run_until_parked();
}

#[gpui::test]
fn typing_marks_the_file_edited_and_saving_writes_it(cx: &mut TestAppContext) {
    let (run, _view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    assert!(!shown(cx, "file-editor-edited"));
    cx.simulate_input("// ");
    cx.run_until_parked();
    assert!(shown(cx, "file-editor-edited"), "typing marks it edited");
    assert_eq!(on_disk(&run), LIB, "nothing is written before a save");

    cx.simulate_keystrokes("secondary-s");
    cx.run_until_parked();
    assert_eq!(on_disk(&run), format!("// {LIB}"));
    assert!(!shown(cx, "file-editor-edited"), "the shortcut saved it");

    cx.simulate_input("x");
    cx.run_until_parked();
    click(cx, "file-editor-save", MouseButton::Left, Modifiers::none());
    assert_eq!(on_disk(&run), format!("// x{LIB}"));
    assert!(!shown(cx, "file-editor-edited"), "the button saved it");

    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert!(shown(cx, "file-editor-edited"));
    cx.simulate_keystrokes("secondary-z");
    cx.run_until_parked();
    assert!(
        !shown(cx, "file-editor-edited"),
        "undo back to the save is clean"
    );
}

#[gpui::test]
fn an_outside_change_to_an_unedited_file_just_shows_up(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    std::fs::write(lib(&run), "from the agent\n").unwrap();
    tick(cx);
    assert_eq!(editor_text(&view, cx), "from the agent\n");
    assert!(!shown(cx, "file-editor-conflict"), "without asking");
    assert!(!shown(cx, "file-editor-edited"));

    cx.simulate_input("// ");
    cx.simulate_keystrokes("secondary-s");
    cx.run_until_parked();
    assert_eq!(
        on_disk(&run),
        "// from the agent\n",
        "a save after it writes"
    );
}

#[gpui::test]
fn an_outside_change_to_an_edited_file_asks_before_either_side_is_lost(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    cx.simulate_input("mine ");
    std::fs::write(lib(&run), "theirs\n").unwrap();
    tick(cx);
    assert!(shown(cx, "file-editor-conflict"), "the conflict bar shows");
    assert_eq!(
        editor_text(&view, cx),
        format!("mine {LIB}"),
        "edits are kept"
    );

    cx.simulate_keystrokes("secondary-s");
    cx.run_until_parked();
    assert_eq!(on_disk(&run), "theirs\n", "Save writes nothing over it");
    assert!(shown(cx, "file-editor-conflict"));

    click(
        cx,
        "file-editor-reload",
        MouseButton::Left,
        Modifiers::none(),
    );
    assert_eq!(
        editor_text(&view, cx),
        "theirs\n",
        "Reload takes the disk's text"
    );
    assert!(!shown(cx, "file-editor-conflict"));
    assert!(!shown(cx, "file-editor-edited"));

    cx.simulate_keystrokes("secondary-home");
    cx.simulate_input("mine ");
    std::fs::write(lib(&run), "theirs again\n").unwrap();
    tick(cx);
    assert!(shown(cx, "file-editor-conflict"));
    click(
        cx,
        "file-editor-overwrite",
        MouseButton::Left,
        Modifiers::none(),
    );
    assert_eq!(
        on_disk(&run),
        "mine theirs\n",
        "Overwrite writes the editor's"
    );
    assert!(!shown(cx, "file-editor-conflict"));
    assert!(!shown(cx, "file-editor-edited"));
}

/// An edit that keeps the size and modification time slips past the check, and the
/// save still refuses to write over it.
#[gpui::test]
fn an_outside_change_that_keeps_size_and_time_is_caught_at_save(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    let modified = std::fs::metadata(lib(&run)).unwrap().modified().unwrap();
    std::fs::write(lib(&run), "ONE\nTWO\nTHREE\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(lib(&run))
        .unwrap()
        .set_modified(modified)
        .unwrap();
    tick(cx);
    assert!(
        !shown(cx, "file-editor-conflict"),
        "the check cannot see it"
    );
    assert_eq!(editor_text(&view, cx), LIB);

    cx.simulate_input("x");
    cx.simulate_keystrokes("secondary-s");
    cx.run_until_parked();
    assert_eq!(on_disk(&run), "ONE\nTWO\nTHREE\n", "nothing is written");
    assert!(shown(cx, "file-editor-conflict"));
    assert!(shown(cx, "file-editor-reload"));
}

#[gpui::test]
fn a_deleted_file_says_so_and_overwrite_recreates_it(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    std::fs::remove_file(lib(&run)).unwrap();
    tick(cx);
    assert!(shown(cx, "file-editor-conflict"));
    assert!(
        !shown(cx, "file-editor-reload"),
        "there is nothing to reload"
    );
    assert_eq!(editor_text(&view, cx), LIB, "the text stays");
    click(
        cx,
        "file-editor-overwrite",
        MouseButton::Left,
        Modifiers::none(),
    );
    assert_eq!(on_disk(&run), LIB);
    assert!(!shown(cx, "file-editor-conflict"));
}

#[gpui::test]
fn back_by_mouse_returns_to_the_tree_for_the_keyboard(cx: &mut TestAppContext) {
    let (_run, _view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    click(cx, "file-editor", MouseButton::Left, Modifiers::none());
    click(cx, "file-editor-save", MouseButton::Left, Modifiers::none());
    click(cx, "file-view-back", MouseButton::Left, Modifiers::none());
    assert!(!shown(cx, "file-editor"));
    // The pointer over the tree highlights the row under it, so the file is picked again.
    cx.simulate_keystrokes("end space");
    cx.run_until_parked();
    assert!(shown(cx, "file-editor"), "the tree has the keyboard again");
}

#[gpui::test]
fn leaving_with_edits_asks_first(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    cx.simulate_input("x");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(shown(cx, "file-editor-discard"), "Escape asks");
    click(
        cx,
        "file-editor-discard-cancel",
        MouseButton::Left,
        Modifiers::none(),
    );
    assert!(!shown(cx, "file-editor-discard"));
    assert!(shown(cx, "file-editor-edited"), "Cancel keeps the edits");
    cx.simulate_input("y");
    cx.run_until_parked();
    assert_eq!(
        editor_text(&view, cx),
        format!("xy{LIB}"),
        "and gives the editor focus back"
    );

    click(cx, "file-view-back", MouseButton::Left, Modifiers::none());
    assert!(shown(cx, "file-editor-discard"), "Back asks");
    click(
        cx,
        "file-editor-discard-confirm",
        MouseButton::Left,
        Modifiers::none(),
    );
    assert!(!shown(cx, "file-editor"), "Discard closes the editor");
    assert!(shown(cx, "files-tree-row-1"));
    assert_eq!(on_disk(&run), LIB);
}

/// Presses keys without letting background work, such as a save's write, run between
/// or after them.
fn press(cx: &mut VisualTestContext, keys: &str) {
    for key in keys.split_whitespace() {
        let keystroke = gpui::Keystroke::parse(key).unwrap();
        cx.update(|window, cx| window.dispatch_keystroke(keystroke, cx));
    }
}

#[gpui::test]
fn leaving_during_a_save_waits_for_it(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    cx.simulate_input("x");
    press(cx, "secondary-s escape");
    assert!(shown(cx, "file-editor"), "Back waits while the save writes");
    assert!(
        !shown(cx, "file-editor-discard"),
        "and doesn't ask about it"
    );
    cx.run_until_parked();
    assert!(
        !shown(cx, "file-editor"),
        "the view closes once the save lands"
    );
    assert_eq!(on_disk(&run), format!("x{LIB}"));

    cx.simulate_keystrokes("space");
    cx.run_until_parked();
    cx.simulate_input("y");
    press(cx, "secondary-s");
    cx.simulate_input("z");
    press(cx, "escape");
    cx.run_until_parked();
    assert!(
        shown(cx, "file-editor-discard"),
        "edits typed during the save are asked about once it lands"
    );
    assert_eq!(on_disk(&run), format!("yx{LIB}"));
    click(
        cx,
        "file-editor-discard-cancel",
        MouseButton::Left,
        Modifiers::none(),
    );

    std::fs::write(lib(&run), "theirs\n").unwrap();
    press(cx, "secondary-s escape");
    cx.run_until_parked();
    assert!(!shown(cx, "file-editor-discard"));
    assert!(
        shown(cx, "file-editor-conflict"),
        "a save that finds a conflict keeps the view open"
    );
    assert_eq!(
        editor_text(&view, cx),
        format!("yzx{LIB}"),
        "with the edits"
    );
    assert_eq!(on_disk(&run), "theirs\n");

    press(cx, "escape");
    cx.run_until_parked();
    assert!(shown(cx, "file-editor-discard"));
    cx.simulate_keystrokes("secondary-s");
    cx.run_until_parked();
    assert_eq!(
        on_disk(&run),
        "theirs\n",
        "Save does nothing behind the question"
    );
}

#[gpui::test]
fn lines_send_only_while_they_match_the_file(cx: &mut TestAppContext) {
    let (_run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    cx.simulate_input("x");
    select_lines(cx, 1, 1);
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    click(cx, "file-view-send", MouseButton::Left, Modifiers::none());
    assert_eq!(composer(&view, cx), "", "edited lines are not sent");

    click(cx, "file-editor", MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("secondary-s");
    cx.run_until_parked();
    select_lines(cx, 1, 2);
    cx.simulate_keystrokes("secondary-shift-a");
    cx.run_until_parked();
    assert_eq!(composer(&view, cx), fenced(2..=3, "two\nthree"));
}

#[gpui::test]
fn quit_and_close_keep_a_file_with_unsaved_edits(cx: &mut TestAppContext) {
    let (run, view, cx) = launch(LIB, true, cx);
    view_lib(cx);
    cx.simulate_input("x");
    cx.run_until_parked();
    let message = "Unsaved changes in src/lib.rs. Save or discard them first.";
    let refused = format!("Not quit. {message}");

    cx.simulate_keystrokes("secondary-q");
    cx.run_until_parked();
    let state = view.read_with(cx, |view, _| view.state.clone());
    assert_eq!(state.read_with(cx, |state, _| state.phase), Phase::Running);
    assert_eq!(
        state
            .read_with(cx, |state, _| state.shutdown_error.clone())
            .as_deref(),
        Some(refused.as_str())
    );
    assert!(shown(cx, "shutdown-error"), "Quit says why it stopped");

    let weak = view.downgrade();
    assert!(!cx.update(|window, cx| crate::close_window(&weak, window, cx)));
    assert_eq!(
        view.read_with(cx, |view, _| view.error.clone()).as_deref(),
        Some(message),
        "closing the window is refused the same way"
    );
    assert_eq!(on_disk(&run), LIB);

    cx.simulate_keystrokes("secondary-s");
    cx.run_until_parked();
    cx.simulate_keystrokes("secondary-q");
    cx.run_until_parked();
    assert_ne!(
        state.read_with(cx, |state, _| state.phase),
        Phase::Running,
        "once saved, Quit goes ahead"
    );
    assert_eq!(on_disk(&run), format!("x{LIB}"));
}

/// The user's shortcuts move Save, and none of them can take a key the editor types or
/// edits with: a saved binding on one is skipped, and the editor keeps the key.
#[gpui::test]
fn a_rebound_save_follows_the_keymap_and_no_command_takes_the_editors_keys(
    cx: &mut TestAppContext,
) {
    let (run, view, cx) = launch(LIB, true, cx);
    let sessions = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        view.read_with(cx, |view, cx| {
            view.state.read(cx).store().workspace().sessions().len()
        })
    };
    let before = sessions(&view, cx);
    cx.update(|_, cx| {
        crate::keymap::apply(
            &serde_json::json!({
                "save-file": "secondary-alt-h",
                "new-claude": "tab",
                "focus-composer": "secondary-a",
            }),
            cx,
        )
    });
    cx.run_until_parked();
    let skipped = cx.update(|_, cx| crate::keymap::current(cx).skipped().join(" "));
    assert!(skipped.contains("New Claude session"), "{skipped}");
    assert!(skipped.contains("Focus composer"), "{skipped}");
    assert_eq!(
        cx.update(|_, cx| crate::commands::hint(SAVE_FILE, cx)),
        keystone_ui::shortcut_text("secondary-alt-h"),
        "Save's hint follows its binding"
    );

    view_lib(cx);
    cx.simulate_keystrokes("tab");
    cx.simulate_input("x");
    cx.simulate_keystrokes("secondary-a");
    cx.run_until_parked();
    assert_eq!(sessions(&view, cx), before, "Tab indented instead");
    let text = editor_text(&view, cx);
    assert!(
        text.ends_with(&format!("x{LIB}")) && text.len() > LIB.len() + 1,
        "{text:?}"
    );
    assert!(
        view.read_with(cx, |view, cx| {
            let editor = view
                .file_view(cx)
                .unwrap()
                .read(cx)
                .open()
                .unwrap()
                .editor
                .clone();
            let selection = editor.read(cx).selection();
            selection.start.line == 0 && selection.start.column == 0 && selection.end.line == 3
        }),
        "Ctrl+A selected all in the editor"
    );

    cx.simulate_keystrokes("right secondary-s");
    cx.run_until_parked();
    assert_eq!(on_disk(&run), LIB, "the old key no longer saves");
    cx.simulate_keystrokes("secondary-alt-h");
    cx.run_until_parked();
    assert_eq!(on_disk(&run), text, "the new key saves");
    assert!(!shown(cx, "file-editor-edited"));
}
