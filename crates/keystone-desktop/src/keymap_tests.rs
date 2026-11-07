use super::*;
use crate::commands;
use gpui::KeyBinding;
use serde_json::json;

fn keystroke(keys: &str) -> Keystroke {
    Keystroke::parse(keys).unwrap()
}

fn control(key: &str) -> Keystroke {
    Keystroke {
        modifiers: Modifiers::control(),
        key: key.into(),
        key_char: None,
    }
}

fn command_with(id: &str) -> &'static Command {
    command(id).unwrap()
}

fn refused(command: &Command, keystroke: &Keystroke) -> bool {
    refusal(command, keystroke).is_some()
}

#[test]
fn parse_keeps_good_entries_and_skips_each_bad_one_with_a_reason() {
    let keymap = Keymap::parse(&json!({
        "new-claude": "secondary-j",
        "toggle-sidebar": "secondary-b",
        "no-such-command": "secondary-y",
        "new-codex": 7,
        "focus-search": "secondary-x-y",
        "focus-composer": "a",
        "quit": "secondary-q secondary-q",
    }));
    let new_claude = command_with("new-claude");
    assert_eq!(keymap.keys(new_claude), "secondary-j");
    assert!(
        keymap.overridden("toggle-sidebar"),
        "an override equal to the default is kept"
    );
    assert_eq!(keymap.keys(command_with("new-codex")), "secondary-shift-n");
    let skipped = keymap.skipped().join("\n");
    assert_eq!(keymap.skipped().len(), 5, "{skipped}");
    for reason in [
        "“no-such-command” is not a command.",
        "New Codex session: 7 is not a shortcut.",
        "Focus search: “secondary-x-y” is not a shortcut.",
        "Focus composer: That key types in the composer",
        "Quit: “secondary-q secondary-q” is not a shortcut.",
    ] {
        assert!(skipped.contains(reason), "{reason} in {skipped}");
    }
}

#[test]
fn a_saved_key_with_no_key_name_is_skipped_and_keeps_its_default() {
    let keymap = Keymap::parse(&json!({
        "toggle-files": "secondary-",
        "new-claude": "",
        "new-codex": "secondary-shift-",
        "focus-search": "secondary-verylongkeyname",
        "toggle-sidebar": "secondary-\u{0}",
        "quit": "shift",
    }));
    assert!(!keymap.has_overrides(), "{:?}", keymap.overrides);
    assert_eq!(
        keymap.keys(command_with("toggle-files")),
        "secondary-shift-e"
    );
    assert_eq!(keymap.keys(command_with("new-claude")), "secondary-n");
    let skipped = keymap.skipped().join("\n");
    assert_eq!(keymap.skipped().len(), 6, "{skipped}");
    for reason in [
        "Toggle files: “secondary-” is not a shortcut.",
        "New Claude session: “” is not a shortcut.",
        "New Codex session: “secondary-shift-” is not a shortcut.",
        "Focus search: “secondary-verylongkeyname” is not a shortcut.",
        "Quit: “shift” is not a shortcut.",
    ] {
        assert!(skipped.contains(reason), "{reason} in {skipped}");
    }
    assert!(!skipped.contains("types in the composer"), "{skipped}");
}

#[test]
fn a_text_fields_editing_keys_are_refused_for_a_command_that_runs_in_one() {
    // Select all, clipboard, undo and redo, Send and new lines, cursor movement and
    // deletion, in both their Control and Command forms, so a binding saved on Linux
    // keeps them on macOS too.
    let app = command_with("new-claude");
    let quit = command_with("quit");
    for keys in [
        "secondary-a",
        "cmd-a",
        "secondary-v",
        "cmd-v",
        "secondary-c",
        "secondary-x",
        "secondary-z",
        "secondary-shift-z",
        "secondary-y",
        "secondary-enter",
        "cmd-enter",
        "shift-enter",
        "secondary-left",
        "alt-left",
        "alt-shift-right",
        "cmd-left",
        "cmd-shift-up",
        "secondary-home",
        "secondary-shift-end",
        "alt-backspace",
        "secondary-backspace",
        "cmd-delete",
    ] {
        let keystroke = keystroke(keys);
        let reason = refusal(app, &keystroke).unwrap_or_else(|| panic!("{keys} is refused"));
        assert!(
            reason.starts_with("Text fields use that key to "),
            "{keys}: {reason}"
        );
        assert!(
            refused(quit, &keystroke),
            "{keys} is refused for a command that runs in dialogs"
        );
    }
    assert_eq!(
        refusal(app, &keystroke("secondary-a")).unwrap(),
        "Text fields use that key to select all, so it can't run this command."
    );
    assert_eq!(
        refusal(app, &keystroke("secondary-v")).unwrap(),
        "Text fields use that key to paste, so it can't run this command."
    );
    for keys in [
        "secondary-j",
        "secondary-shift-a",
        "secondary-alt-left",
        "secondary-shift-enter",
    ] {
        assert!(!refused(app, &keystroke(keys)), "{keys} is allowed");
    }
    let copy = command_with("transcript-copy");
    assert!(
        !refused(copy, &keystroke("secondary-c")),
        "a field inside the transcript binds its editing keys deeper than the transcript"
    );
}

#[test]
fn no_command_takes_a_key_that_types_a_character() {
    // A widget's text field sits inside the transcript, where the transcript's
    // commands still run, and a typed character reaches it only after every binding.
    for id in [
        "transcript-next-row",
        "transcript-copy",
        "quit",
        "new-claude",
    ] {
        let command = command_with(id);
        for keys in [
            "j",
            "shift-j",
            "alt-j",
            "alt-shift-j",
            "1",
            "shift-1",
            ";",
            "/",
            "=",
            "space",
            "shift-space",
        ] {
            assert!(
                refused(command, &keystroke(keys)),
                "{id}: {keys} is refused"
            );
        }
    }
    let next_row = command_with("transcript-next-row");
    assert_eq!(
        refusal(next_row, &keystroke("j")).unwrap(),
        "Text fields type that key, so it can't run this command."
    );
    for keys in [
        "down",
        "up",
        "tab",
        "shift-tab",
        "escape",
        "alt-end",
        "f6",
        "secondary-j",
        "secondary-shift-j",
        "secondary-a",
    ] {
        assert!(!refused(next_row, &keystroke(keys)), "{keys} is allowed");
    }
    let keymap = Keymap::parse(&json!({"transcript-next-row": "j", "transcript-copy": "shift-c"}));
    assert!(!keymap.has_overrides());
    assert_eq!(keymap.keys(next_row), "down");
    let skipped = keymap.skipped().join("\n");
    assert!(
        skipped.contains("Transcript: next row: Text fields type that key"),
        "{skipped}"
    );
}

#[test]
fn a_saved_editing_key_is_skipped_with_its_reason() {
    let keymap = Keymap::parse(&json!({
        "new-claude": "secondary-a",
        "toggle-sidebar": "secondary-v",
        "new-codex": "secondary-enter",
    }));
    assert!(!keymap.has_overrides());
    assert_eq!(keymap.keys(command_with("new-claude")), "secondary-n");
    let skipped = keymap.skipped().join("\n");
    for reason in [
        "New Claude session: Text fields use that key to select all",
        "Toggle sidebar: Text fields use that key to paste",
        "New Codex session: Text fields use that key to send what you typed",
    ] {
        assert!(skipped.contains(reason), "{reason} in {skipped}");
    }
}

#[test]
fn saved_shortcuts_that_are_not_an_object_are_one_skipped_reason() {
    assert_eq!(Keymap::parse(&json!(["secondary-j"])).skipped().len(), 1);
    assert!(Keymap::parse(&Value::Null).skipped().is_empty());
    assert!(!Keymap::parse(&json!({})).has_overrides());
}

#[test]
fn text_keys_are_refused_only_where_the_composer_would_take_them() {
    let app = command_with("new-claude");
    for keys in ["a", "shift-a", "enter", "tab", "space", "alt-a"] {
        assert!(refused(app, &keystroke(keys)), "{keys} is refused");
    }
    for keys in ["f2", "shift-f6", "f35"] {
        assert!(!refused(app, &keystroke(keys)), "{keys} is allowed");
    }
    assert!(!refused(app, &keystroke("secondary-j")));
    assert!(!refused(app, &keystroke("secondary-shift-j")));
    assert!(!refused(app, &control("j")));
    assert!(
        !refused(command_with("transcript-next-row"), &keystroke("up")),
        "the transcript's keys never reach the composer"
    );
}

#[test]
fn every_default_shortcut_passes_the_text_key_rule() {
    for command in COMMANDS {
        for keys in command.keys {
            let parsed = parse(keys).unwrap_or_else(|| panic!("{keys} parses"));
            assert!(!refused(command, &parsed), "{} default {keys}", command.id);
        }
    }
}

#[test]
fn spelling_a_keystroke_round_trips_and_uses_secondary() {
    for keys in [
        "secondary-shift-j",
        "secondary-alt-shift-g",
        "f2",
        "shift-f6",
        "alt-end",
        "secondary-,",
        "secondary-\\",
    ] {
        let parsed = keystroke(keys);
        assert_eq!(spell(&parsed), keys);
        assert_eq!(keystroke(&spell(&parsed)), parsed);
    }
    let platform = Keystroke {
        modifiers: Modifiers::secondary_key(),
        key: "j".into(),
        key_char: None,
    };
    assert_eq!(spell(&platform), "secondary-j");
    assert_eq!(keystroke(&spell(&control("tab"))), control("tab"));
}

fn keymap_with(extra: Vec<KeyBinding>) -> gpui::Keymap {
    let mut bindings = commands::bindings(&Keymap::parse(&Value::Null));
    bindings.extend(extra);
    gpui::Keymap::new(bindings)
}

#[test]
fn only_a_clash_with_a_users_key_is_named() {
    let defaults = Keymap::parse(&Value::Null);
    let bindings = gpui::Keymap::new(commands::bindings(&defaults));
    let next_action = command_with("transcript-next-action");
    let mut with_focus = commands::bindings(&defaults);
    with_focus.push(KeyBinding::new(
        "tab",
        crate::NextFocus,
        Some("Keystone && !Dialog && !CommandPalette"),
    ));
    let with_focus = gpui::Keymap::new(with_focus);
    assert_eq!(
        conflicts(next_action, &keystroke("tab"), &with_focus),
        ["Next focus"]
    );
    assert!(defaults.clashes(next_action, &with_focus).is_empty());

    let keymap = Keymap::parse(&json!({"new-claude": "secondary-shift-n"}));
    let bindings_now = gpui::Keymap::new(commands::bindings(&keymap));
    assert_eq!(
        keymap.clashes(command_with("new-claude"), &bindings_now),
        ["New Codex session"]
    );
    assert_eq!(
        keymap.clashes(command_with("new-codex"), &bindings_now),
        ["New Claude session"],
        "the command whose default was taken names the user's binding"
    );
    assert!(
        defaults
            .clashes(command_with("new-codex"), &bindings)
            .is_empty()
    );
}

#[test]
fn a_panel_binding_conflicts_where_the_panel_has_focus() {
    let bindings = keymap_with(vec![
        KeyBinding::new(
            "secondary-shift-c",
            crate::files::CopyPath,
            Some("FileTree"),
        ),
        KeyBinding::new("space", crate::files::ViewFile, Some("FileTree")),
    ]);
    assert_eq!(
        conflicts(
            command_with("new-claude"),
            &keystroke("secondary-shift-c"),
            &bindings
        ),
        ["Copy path", "Transcript: copy focused content"],
        "the file tree is no named surface, but app commands run there too"
    );
    assert!(
        conflicts(
            command_with("transcript-copy"),
            &keystroke("secondary-shift-c"),
            &bindings
        )
        .is_empty()
    );
}

#[test]
fn a_command_bound_in_a_second_panel_is_checked_there_too() {
    let bindings = keymap_with(vec![
        KeyBinding::new(
            "secondary-shift-c",
            crate::changes::CopyChangePath,
            Some("ChangesList"),
        ),
        KeyBinding::new(
            "secondary-shift-d",
            crate::diff_view::ViewChanges,
            Some("ChangesList"),
        ),
    ]);
    assert_eq!(
        conflicts(
            command_with("view-changes"),
            &keystroke("secondary-shift-c"),
            &bindings
        ),
        ["Copy change path", "Transcript: copy focused content"],
        "View diff in the Changes list is checked against the list's own keys"
    );
}

#[test]
fn conflicts_name_other_bindings_that_share_a_key_and_a_surface() {
    let bindings = keymap_with(vec![
        KeyBinding::new(
            "secondary-v",
            keystone_ui::text_input::Paste,
            Some("TextInput"),
        ),
        KeyBinding::new("secondary-k", crate::OpenPalette, Some("!Dialog")),
        KeyBinding::new(
            "secondary-shift-c",
            crate::files::CopyPath,
            Some("FileTree"),
        ),
    ]);
    let new_claude = command_with("new-claude");
    assert_eq!(
        conflicts(new_claude, &keystroke("secondary-shift-n"), &bindings),
        ["New Codex session"]
    );
    assert_eq!(
        conflicts(new_claude, &keystroke("secondary-v"), &bindings),
        ["Paste"]
    );
    assert_eq!(
        conflicts(new_claude, &keystroke("secondary-k"), &bindings),
        ["Open palette"]
    );
    assert!(
        conflicts(new_claude, &keystroke("secondary-n"), &bindings).is_empty(),
        "a command does not conflict with itself"
    );
    assert!(
        conflicts(
            command_with("transcript-copy"),
            &keystroke("secondary-shift-c"),
            &bindings
        )
        .is_empty(),
        "the files tree and the transcript never hold focus together"
    );
}

#[cfg(feature = "ui-tests")]
mod surfaces {
    use super::super::{COMPOSER, DIALOG, SIDEBAR, TEXT_FIELDS, TRANSCRIPT, stack};
    use crate::{Keystone, bind_keys};
    use gpui::{AppContext, Entity, Focusable, TestAppContext, VisualTestContext};
    use keystone_core::{Provider, Store, WorkingDirectory};
    /// A window on one Codex session whose project is an empty temporary folder, so no
    /// panel reads the real checkout or runs `git` or `gh` on it.
    fn open<'a>(
        dir: &tempfile::TempDir,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Keystone>, &'a mut VisualTestContext) {
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        let folder = dir.path().join("project");
        std::fs::create_dir(&folder).unwrap();
        let project = WorkingDirectory::new(&folder).unwrap();
        store.create(Provider::Codex, &project, 1).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        cx.add_window_view(|window, cx| {
            let view = Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            );
            view.focus.focus(window, cx);
            view
        })
    }

    fn assert_stack(cx: &mut VisualTestContext, surface: &[&str], name: &str) {
        let actual = cx.update(|window, _| window.context_stack());
        assert_eq!(actual, stack(surface), "{name}: {actual:?}");
    }

    #[gpui::test]
    fn each_surface_dispatches_through_its_named_context_stack(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open(&dir, cx);
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            view.composer.focus_handle(cx).focus(window, cx)
        });
        cx.run_until_parked();
        assert_stack(cx, COMPOSER, "composer");

        cx.simulate_keystrokes("secondary-shift-t");
        cx.run_until_parked();
        assert_stack(cx, TRANSCRIPT, "transcript");

        cx.update(|_, cx| {
            let bindings = cx.key_bindings();
            let bindings = bindings.borrow();
            let keymap = super::super::current(cx);
            for command in crate::commands::COMMANDS {
                assert_eq!(
                    keymap.clashes(command, &bindings),
                    Vec::<String>::new(),
                    "{}'s default clashes with nothing",
                    command.id
                );
            }
        });
        view.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
        cx.run_until_parked();
        assert_stack(cx, SIDEBAR, "sidebar");

        view.update_in(cx, |view, window, cx| {
            view.search.focus_handle(cx).focus(window, cx)
        });
        cx.run_until_parked();
        assert_stack(cx, TEXT_FIELDS[0], "search");

        view.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("secondary-k");
        cx.run_until_parked();
        assert_stack(cx, TEXT_FIELDS[2], "palette query");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        view.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        assert!(cx.debug_bounds("rename-dialog").is_some());
        assert_stack(cx, TEXT_FIELDS[1], "rename field");
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        assert_stack(cx, DIALOG, "dialog");
    }
}
