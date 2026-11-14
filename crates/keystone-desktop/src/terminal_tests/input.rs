use super::*;

/// Control keys reach the shell. On Linux, where the app's shortcuts are Ctrl, the
/// terminal's context turns them off; on macOS they are ⌘ and keep working.
#[gpui::test]
fn control_keys_reach_the_shell_and_app_shortcuts_resolve_per_platform(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    run(cx, "echo ready");
    wait_until(cx, "the shell runs", |cx| has_line(&output(cx), 0, "ready"));

    cx.simulate_input("echo one");
    cx.simulate_keystrokes("ctrl-c");
    run(cx, "echo two");
    wait_until(cx, "the line after Ctrl+C runs", |cx| {
        has_line(&output(cx), 0, "two")
    });
    assert!(
        !has_line(&output(cx), 0, "one"),
        "Ctrl+C discarded the typed line: {}",
        output(cx)[0]
    );
    cx.simulate_keystrokes("ctrl-l");
    cx.run_until_parked();
    assert!(pane_focused(cx), "Ctrl+L stays in the terminal");

    let resolved = |cx: &mut VisualTestContext, keys: &str| {
        cx.update(|window, cx| {
            let stack = window.context_stack();
            assert!(stack.iter().any(|c| c.contains(CONTEXT)));
            let keystroke = Keystroke::parse(keys).unwrap();
            cx.key_bindings()
                .borrow()
                .bindings_for_input(&[keystroke], &stack)
                .0
                .iter()
                .map(|binding| binding.action().name().to_owned())
                .collect::<Vec<_>>()
        })
    };
    let new_claude = resolved(cx, "secondary-n");
    if cfg!(target_os = "macos") {
        assert_eq!(new_claude, ["keystone::NewClaude"], "⌘N still works");
        assert!(resolved(cx, "ctrl-c").is_empty());
    } else {
        for keys in ["c", "d", "k", "n", "r", "l", "backslash"] {
            assert!(
                resolved(cx, &format!("secondary-{keys}")).is_empty(),
                "Ctrl+{keys} goes to the shell"
            );
        }
    }
    assert_eq!(
        resolved(cx, "secondary-q"),
        ["keystone::Quit"],
        "Quit works from inside the terminal"
    );
    assert_eq!(
        resolved(cx, "secondary-j"),
        ["terminal::ToggleTerminal"],
        "the toggle works from inside the terminal"
    );
    assert!(resolved(cx, "tab").is_empty(), "Tab completes in the shell");
    assert!(resolved(cx, "f2").is_empty(), "F2 reaches the shell");
    assert_eq!(
        resolved(cx, "secondary-alt-right"),
        ["terminal::FocusNextPane"]
    );
    assert_eq!(
        resolved(cx, "secondary-alt-left"),
        ["terminal::FocusPreviousPane"]
    );
    assert_eq!(
        resolved(cx, "ctrl-tab"),
        ["keystone::NextTab"],
        "an app key"
    );
}

/// A rebound toggle still leaves the terminal, and the key it no longer uses reaches
/// the shell again.
#[gpui::test]
fn a_rebound_toggle_works_from_inside_the_terminal(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.update(|_, cx| {
        crate::keymap::apply(&serde_json::json!({"toggle-terminal": "secondary-g"}), cx)
    });
    cx.simulate_keystrokes("secondary-g");
    run(cx, "echo ready");
    wait_until(cx, "the shell runs", |cx| has_line(&output(cx), 0, "ready"));
    let resolved = |cx: &mut VisualTestContext, keys: &str| {
        cx.update(|window, cx| {
            let stack = window.context_stack();
            let keystroke = Keystroke::parse(keys).unwrap();
            cx.key_bindings()
                .borrow()
                .bindings_for_input(&[keystroke], &stack)
                .0
                .iter()
                .map(|binding| binding.action().name().to_owned())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        resolved(cx, "secondary-g"),
        ["terminal::ToggleTerminal"],
        "the rebound toggle works from inside the terminal"
    );
    if !cfg!(target_os = "macos") {
        assert!(
            resolved(cx, "secondary-j").is_empty(),
            "Ctrl+J reaches the shell once the toggle moved"
        );
    }
    cx.simulate_keystrokes("secondary-g");
    cx.run_until_parked();
    assert!(!pane_focused(cx), "the rebound toggle left the terminal");
}

/// Every command marked to work inside the terminal keeps the key the user gave it:
/// a rebound Split splits from inside a pane.
#[gpui::test]
fn a_rebound_split_works_from_inside_the_terminal(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.update(|_, cx| {
        crate::keymap::apply(&serde_json::json!({"split-terminal": "secondary-g"}), cx)
    });
    cx.simulate_keystrokes("secondary-j");
    run(cx, "echo ready");
    wait_until(cx, "the shell runs", |cx| has_line(&output(cx), 0, "ready"));
    assert!(pane_focused(cx));
    cx.simulate_keystrokes("secondary-g");
    cx.run_until_parked();
    let panes = cx.update(|_, cx| session_terminal(1, cx).unwrap().read(cx).panes.len());
    assert_eq!(panes, 2, "the rebound Split split from inside the terminal");
}

#[gpui::test]
fn the_wheel_scrolls_back_and_typing_returns_to_the_bottom(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    run(cx, "seq 1 300");
    wait_until(cx, "seq finishes", |cx| has_line(&output(cx), 0, "300"));
    let screen = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| session_terminal(1, cx).unwrap().read(cx).panes[0].screen_text())
    };
    assert!(!screen(cx).lines().any(|l| l.trim() == "1"));
    let at = cx.debug_bounds("terminal-pane-0").unwrap().center();
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: at,
        delta: gpui::ScrollDelta::Lines(gpui::point(0., 400.)),
        ..Default::default()
    });
    cx.run_until_parked();
    assert!(
        screen(cx).lines().any(|l| l.trim() == "1"),
        "the top of the scrollback shows: {}",
        screen(cx)
    );
    cx.simulate_input("x");
    cx.run_until_parked();
    assert!(
        !screen(cx).lines().any(|l| l.trim() == "1"),
        "typing scrolls down"
    );
}

#[gpui::test]
fn copy_takes_the_selection_and_falls_through_without_one(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let fell_through = Rc::new(Cell::new(false));
    let flag = fell_through.clone();
    cx.update(|cx| {
        cx.on_action(move |_: &CopySelection, _| flag.set(true));
    });
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    run(cx, "printf 'one two\\n'");
    wait_until(cx, "the text shows", |cx| row_of(cx, "one two").is_some());
    let row = row_of(cx, "one two").unwrap();
    drag_select(cx, row, 4..=6);
    cx.simulate_keystrokes(copy_keys());
    assert_eq!(clipboard(cx).as_deref(), Some("two"));
    assert!(!fell_through.get(), "the pane handled it");

    cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
    cx.simulate_input("x");
    cx.simulate_keystrokes(copy_keys());
    assert_eq!(clipboard(cx).as_deref(), Some("before"), "nothing selected");
    assert!(fell_through.get(), "so the app gets the shortcut");
}

#[gpui::test]
fn paste_sends_the_clipboard_bracketed_when_the_program_asks(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    let mode = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            *session_terminal(1, cx).unwrap().read(cx).panes[0]
                .term
                .lock()
                .mode()
        })
    };
    for (bracketed, name) in [(false, "plain.txt"), (true, "bracketed.txt")] {
        let file = dir.path().join(name);
        let on = if bracketed {
            "printf '\\033[?2004h'; "
        } else {
            ""
        };
        run(cx, &format!("{on}cat > {}", file.display()));
        wait_until(cx, "the mode is set", |cx| {
            mode(cx).contains(TermMode::BRACKETED_PASTE) == bracketed
        });
        std::thread::sleep(Duration::from_millis(100));
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("pasted".into()));
        cx.simulate_keystrokes(paste_keys());
        cx.simulate_keystrokes("enter ctrl-d");
        let expected = if bracketed {
            "\x1b[200~pasted\x1b[201~\n"
        } else {
            "pasted\n"
        };
        // Readline, as in a /bin/sh that is bash 5.1 or later, turns the mode on at
        // each prompt and off before it runs a command, so only `cat`'s run is checked.
        assert_eq!(wait_for_file(&file), expected);
    }
}

/// Presses `key` as a platform reports it, with the text the layout gives it.
fn press(cx: &mut VisualTestContext, key: &str, alt: bool, key_char: Option<&str>) {
    let keystroke = Keystroke {
        key: key.into(),
        key_char: key_char.map(Into::into),
        modifiers: Modifiers {
            alt,
            ..Modifiers::default()
        },
    };
    cx.update(|window, cx| window.dispatch_keystroke(keystroke, cx));
    cx.run_until_parked();
}

/// What the shell receives while `keys` are typed, as `cat -v` shows it: `^[` for
/// Esc, so `^[l` is Meta+L.
fn received(
    cx: &mut VisualTestContext,
    dir: &Path,
    name: &str,
    keys: impl FnOnce(&mut VisualTestContext),
) -> String {
    let file = dir.join(name);
    run(cx, &format!("cat -v > {}", file.display()));
    keys(cx);
    cx.simulate_keystrokes("enter");
    let text = wait_for_file(&file);
    cx.simulate_keystrokes("ctrl-d");
    cx.run_until_parked();
    text.trim_end().to_owned()
}

fn mac_keyboard(mac: bool) {
    crate::terminal::input::MAC_KEYBOARD.set(Some(mac));
}

fn option_as_meta(cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| {
        cx.global::<crate::DesktopApp>()
            .state
            .read(cx)
            .store()
            .workspace()
            .preferences()
            .option_as_meta()
    })
}

/// On macOS, Option types the character the keyboard layout gives it, as on a German
/// layout where Option+L is @ and Option+8 is {. Settings → Shortcuts turns Option
/// into Meta, Esc and the key, for the shell's shortcuts.
#[gpui::test]
fn option_types_the_layout_character_on_macos_unless_it_is_meta(cx: &mut TestAppContext) {
    mac_keyboard(true);
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    let typed = received(cx, dir.path(), "option.txt", |cx| {
        press(cx, "l", true, Some("@"));
        press(cx, "8", true, Some("{"));
    });
    assert_eq!(typed, "@{", "Option types the layout's characters");
    assert!(!option_as_meta(cx), "Option as Meta is off unless chosen");

    cx.simulate_keystrokes("secondary-,");
    click(cx, "settings-section-shortcuts");
    click(cx, "settings-option-as-meta");
    assert!(option_as_meta(cx), "the switch saves the choice");
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(pane_focused(cx));
    let typed = received(cx, dir.path(), "meta.txt", |cx| {
        press(cx, "l", true, Some("@"));
    });
    assert_eq!(typed, "^[l", "as Meta, Option sends Esc and the key");
    mac_keyboard(false);
}

/// Linux keeps Alt as Meta, and Settings offers no choice there.
#[gpui::test]
fn alt_stays_meta_on_linux(cx: &mut TestAppContext) {
    mac_keyboard(false);
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-,");
    click(cx, "settings-section-shortcuts");
    assert!(
        cx.debug_bounds("settings-option-as-meta").is_none(),
        "no Option as Meta switch"
    );
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let typed = received(cx, dir.path(), "alt.txt", |cx| {
        press(cx, "b", true, None);
        press(cx, "f", true, Some("f"));
    });
    assert_eq!(typed, "^[b^[f");
}

/// A dead key composes with the next key, and an input method's composition shows at
/// the cursor without reaching the shell until the input method commits it. The
/// platform calls the pane's input handler as these steps do: on Linux a dead key
/// marks its accent, then the composed key types the character.
#[gpui::test]
fn dead_keys_and_input_methods_compose_in_a_pane(cx: &mut TestAppContext) {
    use gpui::InputHandler;
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let terminal = terminal(cx).unwrap();
    let id = cx.update(|_, cx| terminal.read(cx).panes[0].id);
    let mut input = crate::terminal::text::PaneInput {
        terminal: terminal.downgrade(),
        pane: id,
    };
    let marked =
        |cx: &mut VisualTestContext| cx.update(|_, cx| terminal.read(cx).panes[0].marked.clone());
    let file = dir.path().join("composed.txt");
    run(cx, &format!("cat > {}", file.display()));

    // As a key event only: the test platform's simulated input method would type the
    // key's name, where a real one gives a dead key no text.
    cx.update(|window, cx| {
        window.dispatch_event(
            gpui::PlatformInput::KeyDown(gpui::KeyDownEvent {
                keystroke: Keystroke::parse("dead_acute").unwrap(),
                is_held: false,
                prefer_character_input: false,
            }),
            cx,
        )
    });
    cx.update(|window, cx| input.replace_and_mark_text_in_range(None, "´", None, window, cx));
    assert_eq!(
        marked(cx).as_deref(),
        Some("´"),
        "the accent waits at the cursor"
    );
    press(cx, "eacute", false, Some("é"));
    assert_eq!(marked(cx), None, "the composed key ends the composition");

    for step in ["に", "日本"] {
        cx.update(|window, cx| input.replace_and_mark_text_in_range(None, step, None, window, cx));
    }
    let (range, bounds, view) = cx.update(|window, cx| {
        let view = terminal.read(cx).panes[0]
            .view(window.window_handle().window_id())
            .unwrap();
        (
            input.marked_text_range(window, cx),
            input.bounds_for_range(0..1, window, cx).unwrap(),
            view.bounds,
        )
    });
    assert_eq!(
        range,
        Some(0..2),
        "the composition is the input method's text"
    );
    assert!(
        view.contains(&bounds.origin),
        "its candidates open at the cursor, {bounds:?} in {view:?}"
    );
    cx.update(|window, cx| input.replace_text_in_range(None, "日本", window, cx));
    cx.update(|window, cx| input.replace_and_mark_text_in_range(None, "한", None, window, cx));
    cx.update(|window, cx| input.unmark_text(window, cx));
    assert_eq!(marked(cx), None);
    cx.simulate_keystrokes("enter");
    assert_eq!(wait_for_file(&file).trim_end(), "é日本한");
}

/// A composition the pane loses focus in is dropped, so its underline cannot stay
/// behind if the platform never ends it.
#[gpui::test]
fn a_composition_ends_when_the_pane_loses_focus(cx: &mut TestAppContext) {
    use gpui::InputHandler;
    let (_dir, path, _) = seeded();
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    // Focus moves within an active window; in an inactive one GPUI reports none.
    cx.update(|window, _| window.activate_window());
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let terminal = terminal(cx).unwrap();
    let id = cx.update(|_, cx| terminal.read(cx).panes[0].id);
    let mut input = crate::terminal::text::PaneInput {
        terminal: terminal.downgrade(),
        pane: id,
    };
    cx.update(|window, cx| input.replace_and_mark_text_in_range(None, "に", None, window, cx));
    let marked =
        |cx: &mut VisualTestContext| cx.update(|_, cx| terminal.read(cx).panes[0].marked.clone());
    assert_eq!(marked(cx).as_deref(), Some("に"));
    cx.simulate_keystrokes("f6");
    cx.simulate_keystrokes("secondary-l");
    cx.run_until_parked();
    assert!(composer_focused(&view, cx));
    assert_eq!(
        marked(cx),
        None,
        "the composition ended with the pane's focus"
    );
}
