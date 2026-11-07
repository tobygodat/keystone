use crate::keymap;
use crate::{Keystone, bind_keys};
use gpui::{
    AppContext, ClipboardItem, Entity, Focusable, Modifiers, TestAppContext, VisualTestContext,
};
use keystone_core::Store;
use serde_json::{Value, json};
use std::path::Path;

fn seeded(path: &Path, shortcuts: Value) {
    let workspace = json!({
        "version": 1,
        "next_id": 2,
        "workspace": {
            "sessions": [{"id": 1, "title": "One", "provider": "Codex", "settled": false, "draft": ""}],
            "selected": 1,
            "preferences": {"shortcuts": shortcuts},
        },
    });
    std::fs::write(path, workspace.to_string()).unwrap();
}

/// Opens a window as `main` does: the saved shortcuts bind before it shows. The window
/// shows an empty folder beside `path`, never the checkout the tests run in, so no panel
/// runs `git` or `gh` on a real repository.
fn open_window<'a>(
    path: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<Keystone>, &'a mut VisualTestContext) {
    let mut store = Store::open(path).unwrap();
    let folder = path.parent().unwrap().join("project");
    std::fs::create_dir_all(&folder).unwrap();
    let project = keystone_core::ProjectId::new(&folder).unwrap();
    store
        .open_project(keystone_core::WindowId::FIRST, &project, None, None, 1)
        .unwrap();
    let shortcuts = store.workspace().preferences().shortcuts().clone();
    cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
        keymap::apply(&shortcuts, cx);
    });
    cx.executor().allow_parking();
    cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| crate::AppState::new(store, cx));
        state.update(cx, |state, _| {
            let wake = state.test_wake().clone();
            state.test_set_claude(Some(keystone_agent::ClaudeConnection::new(
                Some(fake("keystone-fake-claude")),
                wake.clone(),
            )));
            state.test_set_codex(Some(keystone_agent::CodexConnection::new(
                Some(fake("keystone-fake-codex")),
                "test",
                wake,
            )));
        });
        let view = Keystone::new(state, false, window, cx);
        view.focus.focus(window, cx);
        view
    })
}

/// A scripted provider built beside this test binary, run as it is, so a session the
/// recorded keys start never reaches the real `claude` or `codex`.
fn fake(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().unwrap().parent().unwrap().join(name);
    assert!(
        path.exists(),
        "build it with `cargo build -p keystone-agent --bins`"
    );
    path
}

fn shown(cx: &mut VisualTestContext, selector: &'static str) -> bool {
    cx.debug_bounds(selector).is_some()
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} rendered"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

fn keys(cx: &mut VisualTestContext, keys: &str) {
    cx.simulate_keystrokes(keys);
    cx.run_until_parked();
}

fn saved(path: &Path) -> Value {
    Store::open(path)
        .unwrap()
        .workspace()
        .preferences()
        .shortcuts()
        .clone()
}

fn sessions(path: &Path) -> usize {
    Store::open(path).unwrap().workspace().sessions().len()
}

fn open_shortcuts(cx: &mut VisualTestContext) {
    keys(cx, "secondary-,");
    click(cx, "settings-section-shortcuts");
    assert!(shown(cx, "settings-shortcuts"));
}

fn recording(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |view, _| {
        view.settings
            .as_ref()
            .is_some_and(|page| page.shortcuts.recording.is_some())
    })
}

#[gpui::test]
fn a_recorded_shortcut_saves_and_runs_at_once(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (view, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    assert!(!shown(cx, "settings-shortcut-new-claude-reset"));
    click(cx, "settings-shortcut-new-claude");
    assert!(recording(&view, cx));
    keys(cx, "secondary-j");
    assert!(!recording(&view, cx));
    assert_eq!(saved(&path), json!({"new-claude": "secondary-j"}));
    assert_eq!(sessions(&path), 1, "the recorded keys ran nothing");
    assert!(shown(cx, "settings-shortcut-new-claude-reset"));

    keys(cx, "escape");
    assert!(!shown(cx, "settings-page"));
    keys(cx, "secondary-n");
    assert_eq!(sessions(&path), 1, "the old keys no longer run it");
    keys(cx, "secondary-j");
    assert_eq!(sessions(&path), 2, "the new keys run it");

    keys(cx, "secondary-k");
    let hint = view.read_with(cx, |view, cx| {
        let palette = view.palette.as_ref().unwrap().state.read(cx);
        let row = palette.matches().find(|item| item.id == "new-claude");
        row.and_then(|item| item.kbd.clone())
    });
    assert_eq!(
        hint.as_deref(),
        Some(keystone_ui::shortcut_text("secondary-j").as_str())
    );
}

#[gpui::test]
fn a_bad_saved_shortcut_is_skipped_and_the_rest_bind(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(
        &path,
        json!({
            "no-such-command": "secondary-y",
            "rename-session": 7,
            "new-claude": "a",
            "new-codex": "secondary-j",
        }),
    );
    let (_, cx) = open_window(&path, cx);
    keys(cx, "secondary-j");
    assert_eq!(sessions(&path), 2, "the good entry binds");
    keys(cx, "secondary-n");
    assert_eq!(sessions(&path), 3, "a skipped entry keeps the default");
    open_shortcuts(cx);
    assert!(shown(cx, "settings-shortcuts-skipped"));
    let skipped = cx.update(|_, cx| keymap::current(cx).skipped().len());
    assert_eq!(skipped, 3);
}

#[gpui::test]
fn a_key_the_composer_types_is_refused_and_saves_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (view, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    click(cx, "settings-shortcut-new-claude");
    keys(cx, "a");
    assert!(!recording(&view, cx));
    assert!(shown(cx, "settings-shortcut-new-claude-warning"));
    assert_eq!(saved(&path), Value::Null);
    assert!(shown(cx, "settings-page"));
}

#[gpui::test]
fn escape_cancels_recording_and_keeps_the_page(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (view, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    click(cx, "settings-shortcut-new-claude");
    keys(cx, "escape");
    assert!(!recording(&view, cx));
    assert!(shown(cx, "settings-page"), "Escape ends only the recording");
    assert_eq!(saved(&path), Value::Null);
    cx.simulate_keystrokes("enter");
    cx.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse("enter").unwrap(),
    });
    cx.run_until_parked();
    assert!(
        recording(&view, cx),
        "Enter on the focused shortcut records"
    );
    keys(cx, "escape");
    keys(cx, "escape");
    assert!(!shown(cx, "settings-page"));
}

#[gpui::test]
fn a_clash_warns_and_still_saves(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (_, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    click(cx, "settings-shortcut-new-claude");
    keys(cx, "secondary-shift-n");
    assert_eq!(saved(&path), json!({"new-claude": "secondary-shift-n"}));
    assert!(shown(cx, "settings-shortcut-new-claude-warning"));
    assert!(
        shown(cx, "settings-shortcut-new-codex-warning"),
        "the command whose keys were taken says so too"
    );
    let clashes = cx.update(|_, cx| {
        let bindings = cx.key_bindings();
        let command = keymap::command("new-claude").unwrap();
        keymap::current(cx).clashes(command, &bindings.borrow())
    });
    assert_eq!(clashes, ["New Codex session"]);
}

#[gpui::test]
fn reset_and_reset_all_restore_the_defaults_live(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(
        &path,
        json!({"new-claude": "secondary-j", "new-codex": "secondary-alt-j"}),
    );
    let (_, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    click(cx, "settings-shortcut-new-claude-reset");
    assert_eq!(saved(&path), json!({"new-codex": "secondary-alt-j"}));
    assert!(!shown(cx, "settings-shortcut-new-claude-reset"));
    click(cx, "settings-shortcuts-reset-all");
    assert_eq!(saved(&path), Value::Null);
    assert!(!shown(cx, "settings-shortcut-new-codex-reset"));

    keys(cx, "escape");
    assert!(
        !shown(cx, "settings-page"),
        "the keys still reach the window"
    );
    keys(cx, "secondary-n");
    assert_eq!(sessions(&path), 2);
    keys(cx, "secondary-shift-n");
    assert_eq!(sessions(&path), 3);
    keys(cx, "secondary-j secondary-alt-j");
    assert_eq!(sessions(&path), 3, "the overrides are gone");
}

#[gpui::test]
fn the_components_keep_their_keys_after_shortcuts_apply(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, json!({"new-claude": "secondary-j"}));
    let (_, cx) = open_window(&path, cx);
    keys(cx, "secondary-k");
    assert!(shown(cx, "palette"));
    keys(cx, "escape");
    assert!(!shown(cx, "palette"), "the palette's own Escape is bound");
}

#[gpui::test]
fn leaving_the_section_or_the_shortcut_ends_recording(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (view, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    click(cx, "settings-shortcut-new-claude");
    assert!(recording(&view, cx));
    click(cx, "settings-section-fonts");
    assert!(!recording(&view, cx));
    keys(cx, "secondary-j");
    assert_eq!(saved(&path), Value::Null);

    click(cx, "settings-section-shortcuts");
    click(cx, "settings-shortcut-new-claude");
    assert!(recording(&view, cx));
    view.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
    keys(cx, "secondary-j");
    assert!(!recording(&view, cx));
    assert_eq!(saved(&path), Value::Null, "the key went where focus went");
}

#[gpui::test]
fn applying_shortcuts_keeps_every_other_binding(cx: &mut TestAppContext) {
    fn bound(cx: &gpui::App) -> std::collections::BTreeSet<String> {
        cx.key_bindings()
            .borrow()
            .bindings()
            .map(|binding| {
                let keys: Vec<String> = binding
                    .keystrokes()
                    .iter()
                    .map(|k| k.inner().unparse())
                    .collect();
                let context = binding.predicate().map(|p| p.to_string());
                format!("{keys:?} {} {context:?}", binding.action().name())
            })
            .collect()
    }
    let before = cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
        bound(cx)
    });
    let after = cx.update(|cx| {
        keymap::apply(&Value::Null, cx);
        bound(cx)
    });
    let lost: Vec<_> = before.difference(&after).collect();
    assert!(lost.is_empty(), "lost on rebinding: {lost:#?}");
}

#[gpui::test]
fn reset_all_clears_entries_that_were_only_skipped(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, json!({"later-command": "f9"}));
    let (_, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    assert!(shown(cx, "settings-shortcuts-skipped"));
    click(cx, "settings-shortcuts-reset-all");
    assert_eq!(saved(&path), Value::Null);
    assert!(!shown(cx, "settings-shortcuts-skipped"));
}

#[gpui::test]
fn a_command_main_added_rebinds_and_its_hints_follow(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (view, cx) = open_window(&path, cx);
    let hint = |cx: &mut VisualTestContext, id: &'static str| {
        cx.update(|_, cx| crate::commands::hint(id, cx))
    };
    assert_eq!(
        hint(cx, "view-uncommitted"),
        keystone_ui::shortcut_text("secondary-alt-u")
    );
    open_shortcuts(cx);
    click(cx, "settings-shortcut-view-uncommitted");
    keys(cx, "secondary-alt-y");
    assert_eq!(saved(&path), json!({"view-uncommitted": "secondary-alt-y"}));
    assert_eq!(
        hint(cx, "view-uncommitted"),
        keystone_ui::shortcut_text("secondary-alt-y")
    );
    keys(cx, "escape");
    keys(cx, "secondary-k");
    let row = view.read_with(cx, |view, cx| {
        let palette = view.palette.as_ref().unwrap().state.read(cx);
        palette
            .matches()
            .find(|item| item.id == "view-uncommitted")
            .and_then(|item| item.kbd.clone())
    });
    assert_eq!(
        row.as_deref(),
        Some(keystone_ui::shortcut_text("secondary-alt-y").as_str())
    );
}

#[gpui::test]
fn a_palette_only_command_can_be_given_keys(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (view, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    assert!(!shown(cx, "settings-shortcut-next-model-reset"));
    click(cx, "settings-shortcut-next-model");
    keys(cx, "secondary-alt-shift-m");
    assert_eq!(saved(&path), json!({"next-model": "secondary-alt-shift-m"}));
    assert!(shown(cx, "settings-shortcut-next-model-reset"));
    let bound = cx.update(|_, cx| {
        let name = (keymap::command("next-model").unwrap().action)().name();
        cx.key_bindings()
            .borrow()
            .bindings()
            .filter(|binding| binding.action().name() == name)
            .map(|binding| binding.keystrokes()[0].inner().unparse())
            .collect::<Vec<_>>()
    });
    assert_eq!(bound.len(), 1, "{bound:?}");
    click(cx, "settings-shortcut-next-model-reset");
    assert_eq!(saved(&path), Value::Null);
    keys(cx, "escape");
    keys(cx, "secondary-k");
    let row = view.read_with(cx, |view, cx| {
        let palette = view.palette.as_ref().unwrap().state.read(cx);
        palette
            .matches()
            .find(|item| item.id == "next-model")
            .map(|item| item.kbd.clone())
    });
    assert_eq!(row, Some(None), "an unbound command shows no hint");
}

/// View diff in the Changes list runs the same action as the transcript's command, so
/// it takes the user's keys too.
#[gpui::test]
fn the_changes_list_follows_the_view_changes_binding(cx: &mut TestAppContext) {
    let keys_of = |cx: &gpui::App| {
        let name = (keymap::command("view-changes").unwrap().action)().name();
        let mut keys: Vec<(String, Option<String>)> = cx
            .key_bindings()
            .borrow()
            .bindings()
            .filter(|binding| binding.action().name() == name)
            .map(|binding| {
                (
                    binding.keystrokes()[0].inner().unparse(),
                    binding.predicate().map(|p| p.to_string()),
                )
            })
            .collect();
        keys.sort();
        keys
    };
    let (before, after) = cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
        let before = keys_of(cx);
        keymap::apply(&json!({"view-changes": "secondary-alt-d"}), cx);
        (before, keys_of(cx))
    });
    assert_eq!(before.len(), 2, "{before:?}");
    assert!(
        before
            .iter()
            .any(|(_, context)| context.as_deref() == Some("ChangesList"))
    );
    assert_eq!(after.len(), 2, "{after:?}");
    let alt_d = gpui::Keystroke::parse("secondary-alt-d").unwrap().unparse();
    assert!(after.iter().all(|(keys, _)| *keys == alt_d), "{after:?}");
}

/// Which text field a test types in.
#[derive(Clone, Copy)]
enum Field {
    Composer,
    Search,
    Rename,
}

fn field(
    view: &Entity<Keystone>,
    which: Field,
    cx: &mut VisualTestContext,
) -> Entity<keystone_ui::prelude::TextInput> {
    view.read_with(cx, |view, _| match which {
        Field::Composer => view.composer.clone(),
        Field::Search => view.search.clone(),
        Field::Rename => view.rename_input.clone(),
    })
}

fn value(view: &Entity<Keystone>, which: Field, cx: &mut VisualTestContext) -> String {
    let input = field(view, which, cx);
    input.read_with(cx, |input, _| input.value().to_owned())
}

/// Puts `text` in the field and focuses it, as clicking into it with a draft does.
fn type_in(view: &Entity<Keystone>, which: Field, text: &str, cx: &mut VisualTestContext) {
    if let Field::Rename = which {
        keys(cx, "f2");
        assert!(shown(cx, "rename-dialog"));
    }
    let input = field(view, which, cx);
    input.update(cx, |input, cx| input.set_value(text.into(), cx));
    cx.update(|window, cx| input.focus_handle(cx).focus(window, cx));
    cx.run_until_parked();
}

fn field_focused(view: &Entity<Keystone>, which: Field, cx: &mut VisualTestContext) -> bool {
    let input = field(view, which, cx);
    cx.update(|window, cx| input.focus_handle(cx).is_focused(window))
}

fn sidebar(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> (bool, bool) {
    view.read_with(cx, |view, _| {
        let current = &view.presentation.current;
        (current.sidebar, current.arrangement.sidebar_overlay)
    })
}

/// Select all, then a typed letter, replaces the field's text, and Paste inserts the
/// clipboard: neither key runs the command saved on it.
fn select_all_and_paste_edit(
    view: &Entity<Keystone>,
    which: Field,
    path: &Path,
    cx: &mut VisualTestContext,
) {
    // A session in this window's project, started with its default keys, so the
    // composer and Rename have one to work on.
    let started = sessions(path);
    keys(cx, "secondary-shift-n");
    assert_eq!(sessions(path), started + 1, "a Codex session started");
    let before = (sessions(path), sidebar(view, cx));
    type_in(view, which, "A draft to replace", cx);
    keys(cx, "secondary-a");
    assert_eq!(
        (sessions(path), sidebar(view, cx)),
        before,
        "Select all ran no command"
    );
    assert!(field_focused(view, which, cx), "the field kept focus");
    cx.simulate_input("X");
    cx.run_until_parked();
    assert_eq!(value(view, which, cx), "X", "Select all selected the draft");
    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string(" pasted".into())));
    keys(cx, "secondary-v");
    assert_eq!(
        (sessions(path), sidebar(view, cx)),
        before,
        "Paste ran no command"
    );
    assert_eq!(value(view, which, cx), "X pasted", "Paste pasted");
    assert_eq!(
        (sessions(path), sidebar(view, cx)),
        before,
        "no command ran"
    );
}

#[gpui::test]
fn a_saved_rebind_never_takes_select_all_or_paste_from_the_composer(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(
        &path,
        json!({"new-claude": "secondary-a", "toggle-sidebar": "secondary-v"}),
    );
    let (view, cx) = open_window(&path, cx);
    select_all_and_paste_edit(&view, Field::Composer, &path, cx);
    let skipped = cx.update(|_, cx| keymap::current(cx).skipped().join("\n"));
    assert!(
        skipped.contains("New Claude session: Text fields use that key to select all")
            && skipped.contains("Toggle sidebar: Text fields use that key to paste"),
        "{skipped}"
    );
    keys(cx, "secondary-n");
    assert_eq!(sessions(&path), 3, "New Claude keeps its default");
}

#[gpui::test]
fn a_saved_rebind_never_takes_select_all_or_paste_from_search(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(
        &path,
        json!({"new-claude": "secondary-a", "toggle-sidebar": "secondary-v"}),
    );
    let (view, cx) = open_window(&path, cx);
    select_all_and_paste_edit(&view, Field::Search, &path, cx);
}

#[gpui::test]
fn a_command_that_runs_in_dialogs_never_takes_their_editing_keys(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(
        &path,
        json!({"quit": "secondary-a", "toggle-sidebar": "secondary-v"}),
    );
    let (view, cx) = open_window(&path, cx);
    select_all_and_paste_edit(&view, Field::Rename, &path, cx);
    let skipped = cx.update(|_, cx| keymap::current(cx).skipped().len());
    assert_eq!(skipped, 2);
}

#[gpui::test]
fn recording_an_editing_key_is_refused_and_says_which(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (view, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    for (keystroke, what) in [
        ("secondary-a", "select all"),
        ("secondary-v", "paste"),
        ("secondary-enter", "send what you typed"),
        ("alt-left", "move the cursor"),
        ("secondary-backspace", "delete text"),
    ] {
        click(cx, "settings-shortcut-new-claude");
        keys(cx, keystroke);
        assert!(!recording(&view, cx));
        let warning = view.read_with(cx, |view, _| {
            view.settings.as_ref().unwrap().shortcuts.refused.clone()
        });
        assert_eq!(
            warning,
            Some((
                "new-claude",
                format!("Text fields use that key to {what}, so it can't run this command.")
            )),
            "{keystroke}"
        );
        assert!(shown(cx, "settings-shortcut-new-claude-warning"));
        assert_eq!(saved(&path), Value::Null, "{keystroke} saved nothing");
    }
    keys(cx, "escape");
    select_all_and_paste_edit(&view, Field::Composer, &path, cx);
}

#[gpui::test]
fn a_shortcut_whose_save_fails_says_not_saved(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (view, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    let writer = crate::tests::lock_database(&path);
    click(cx, "settings-shortcut-new-claude");
    keys(cx, "secondary-j");
    let state = view.read_with(cx, |view, _| view.state.clone());
    crate::tests::settle(&state, cx);
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        let status = view.status_line(cx).map(|(text, _)| text);
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.starts_with("Not saved.")),
            "{status:?}"
        );
    });
    assert!(shown(cx, "settings-error"), "the page shows the save error");
    let error = cx.debug_bounds("settings-error").unwrap();
    let window = cx.update(|window, _| window.viewport_size());
    assert!(
        error.bottom() <= window.height,
        "the error shows without scrolling past {} shortcuts: {error:?} in {window:?}",
        crate::commands::COMMANDS.len()
    );
    drop(writer);
    click(cx, "settings-shortcut-new-codex");
    keys(cx, "secondary-alt-j");
    crate::tests::settle(&state, cx);
    cx.run_until_parked();
    assert_eq!(
        saved(&path),
        json!({"new-claude": "secondary-j", "new-codex": "secondary-alt-j"}),
        "the next save writes both"
    );
    assert!(!shown(cx, "settings-error"), "a good save clears the error");
}

/// The agent widget's form in the selected session, once it waits for an answer: its
/// row and the call it answers.
fn waiting_widget(view: &Keystone, cx: &gpui::App) -> Option<(u64, usize, String)> {
    let session = view.selected(cx)?;
    let conversation = view.state.read(cx).conversations().get(&session)?;
    conversation
        .rows
        .iter()
        .enumerate()
        .find_map(|(index, row)| match row {
            crate::conversation::Row::Surface(surface) if surface.waiting() => {
                Some((session, index, surface.call.clone()))
            }
            _ => None,
        })
}

fn summary(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Option<Value> {
    view.read_with(cx, |view, cx| {
        let session = view.selected(cx)?;
        view.state.read(cx).conversations()[&session]
            .rows
            .iter()
            .find_map(|row| match row {
                crate::conversation::Row::Surface(surface) => surface.value("/summary").cloned(),
                _ => None,
            })
    })
}

/// Whether the widget's first control, its Summary field, has focus.
fn summary_focused(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> bool {
    view.update_in(cx, |view, window, cx| {
        let (session, _, call) = waiting_widget(view, cx).unwrap();
        let key = crate::view_state::ViewKey {
            view: keystone_core::ViewId::MAIN,
            session,
        };
        view.presentation
            .current
            .views
            .widget_controls(key, &call)
            .first()
            .is_some_and(|focus| focus.is_focused(window))
    })
}

/// A widget's text field sits inside the transcript, where the transcript's commands
/// still run. A letter saved on one of them is skipped, so J types in the field instead
/// of moving to the next row, and a field key such as Select all stays the field's.
#[gpui::test]
fn a_letter_saved_on_a_transcript_command_still_types_in_a_widget_field(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(
        &path,
        json!({"transcript-next-row": "j", "transcript-copy": "secondary-a"}),
    );
    let (view, cx) = open_window(&path, cx);
    let skipped = cx.update(|_, cx| keymap::current(cx).skipped().join("\n"));
    assert_eq!(
        skipped,
        "Transcript: next row: Text fields type that key, so it can't run this command."
    );

    keys(cx, "secondary-n");
    cx.simulate_input("show a widget form");
    keys(cx, "secondary-enter");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while view.read_with(cx, |view, cx| waiting_widget(view, cx).is_none()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the widget form shows"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        cx.run_until_parked();
    }
    let index = view.read_with(cx, |view, cx| waiting_widget(view, cx).unwrap().1);
    click(cx, format!("widget-{index}-summary-field").leak());
    assert!(summary_focused(&view, cx), "the click focused Summary");
    cx.simulate_input("ab");
    cx.run_until_parked();
    keys(cx, "end j x");
    assert!(summary_focused(&view, cx), "J kept the field's focus");
    assert_eq!(summary(&view, cx), Some(json!("abjx")), "J typed");

    keys(cx, "secondary-a");
    cx.simulate_input("Z");
    cx.run_until_parked();
    assert!(
        summary_focused(&view, cx),
        "Select all kept the field's focus"
    );
    assert_eq!(summary(&view, cx), Some(json!("Z")), "Select all selected");
}

/// Recording a letter on a transcript command is refused with the reason, and nothing
/// is saved.
#[gpui::test]
fn recording_a_letter_on_a_transcript_command_is_refused(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    seeded(&path, Value::Null);
    let (view, cx) = open_window(&path, cx);
    open_shortcuts(cx);
    // The transcript's rows are far down the page: scroll to them as a user does.
    let height = cx.update(|window, _| window.viewport_size().height);
    for _ in 0..20 {
        let row = cx
            .debug_bounds("settings-shortcut-transcript-next-row")
            .unwrap();
        if row.bottom() < height {
            break;
        }
        let page = cx.debug_bounds("settings-shortcuts").unwrap();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: gpui::point(page.center().x, height / 2.),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-300.))),
            ..Default::default()
        });
        cx.run_until_parked();
    }
    for keystroke in ["j", "shift-j", "1", "space"] {
        click(cx, "settings-shortcut-transcript-next-row");
        assert!(recording(&view, cx), "recording before {keystroke}");
        keys(cx, keystroke);
        assert!(!recording(&view, cx), "{keystroke} ended recording");
        let warning = view.read_with(cx, |view, _| {
            view.settings.as_ref().unwrap().shortcuts.refused.clone()
        });
        assert_eq!(
            warning,
            Some((
                "transcript-next-row",
                "Text fields type that key, so it can't run this command.".to_owned()
            )),
            "{keystroke}"
        );
        assert!(shown(cx, "settings-shortcut-transcript-next-row-warning"));
        assert_eq!(saved(&path), Value::Null, "{keystroke} saved nothing");
    }
}

/// Every key a text field binds, wherever it is bound, is one the keymap refuses for a
/// command that ties with the field, so no rebinding can shadow it.
#[gpui::test]
fn every_key_bound_in_a_text_field_is_an_editing_key(cx: &mut TestAppContext) {
    cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
        let bindings = cx.key_bindings();
        let bindings = bindings.borrow();
        let mut outside = Vec::new();
        for binding in bindings.bindings() {
            let Some(context) = binding.predicate() else {
                continue;
            };
            let context = context.to_string();
            if !context.contains("TextInput") {
                continue;
            }
            if context != "TextInput" {
                outside.push(context.clone());
            }
            for keystroke in binding.keystrokes() {
                assert!(
                    keystone_ui::text_input::editing_key(keystroke.inner()).is_some(),
                    "{} under {context} is in editing_keys",
                    keystroke.inner().unparse()
                );
            }
        }
        assert!(
            outside
                .iter()
                .any(|context| context.contains("QuickComposer")),
            "the quick composer's own field key is checked too: {outside:?}"
        );
    });
}
