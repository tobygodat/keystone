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
