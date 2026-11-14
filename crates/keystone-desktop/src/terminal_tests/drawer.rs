use super::*;

#[gpui::test]
fn the_shortcut_opens_a_shell_in_the_session_folder(cx: &mut TestAppContext) {
    let (_dir, path, project) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    assert!(cx.debug_bounds("terminal-drawer").is_none());

    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("terminal-drawer").is_some(),
        "the drawer opens"
    );
    assert!(pane_focused(cx), "and its pane takes focus");

    run(cx, "pwd");
    let folder = project.display().to_string();
    wait_until(cx, "pwd prints the session folder", |cx| {
        has_line(&output(cx), 0, &folder)
    });
    run(cx, "echo hello-$((40+2))");
    wait_until(cx, "typed echo output shows in the grid", |cx| {
        has_line(&output(cx), 0, "hello-42")
    });
}

#[gpui::test]
fn the_shortcut_hides_a_focused_terminal_and_refocuses_one_that_is_not(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let pid = pids(cx)[0];

    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(cx.debug_bounds("terminal-drawer").is_none(), "hidden");
    assert!(
        cx.update(|window, cx| view.read(cx).composer.focus_handle(cx).is_focused(window)),
        "focus returns to the composer"
    );
    assert!(alive(pid), "hiding keeps the shell");

    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(cx.debug_bounds("terminal-drawer").is_some());
    assert_eq!(pids(cx), [pid], "showing it again keeps the same shell");
    cx.simulate_keystrokes("f6");
    cx.run_until_parked();
    assert!(
        !pane_focused(cx),
        "F6 leaves the terminal on every platform"
    );
    cx.simulate_keystrokes("secondary-l");
    cx.run_until_parked();
    assert!(
        composer_focused(&view, cx),
        "and from there the composer's shortcut works, the drawer still open"
    );
    assert!(cx.debug_bounds("terminal-drawer").is_some());
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("terminal-drawer").is_some() && pane_focused(cx),
        "open but not focused, the shortcut focuses it"
    );
}

#[gpui::test]
fn the_top_bar_button_shows_and_hides_the_terminal(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    click(cx, "terminal-toggle");
    assert!(cx.debug_bounds("terminal-drawer").is_some());
    assert!(pane_focused(cx));
    click(cx, "terminal-toggle");
    assert!(cx.debug_bounds("terminal-drawer").is_none());
}

#[gpui::test]
fn split_adds_panes_up_to_four(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    cx.simulate_keystrokes("secondary-shift-d");
    cx.run_until_parked();
    click(cx, "terminal-split");
    cx.simulate_keystrokes("secondary-shift-d");
    cx.run_until_parked();
    assert_eq!(pids(cx).len(), MAX_PANES);
    for pane in [
        "terminal-pane-0",
        "terminal-pane-1",
        "terminal-pane-2",
        "terminal-pane-3",
    ] {
        assert!(cx.debug_bounds(pane).is_some(), "{pane}");
    }
    cx.simulate_keystrokes("secondary-shift-d");
    click(cx, "terminal-split");
    assert_eq!(pids(cx).len(), MAX_PANES, "a fifth split does nothing");
    assert!(cx.debug_bounds("terminal-pane-4").is_none());

    let shells = pids(cx);
    click(cx, "terminal-pane-1");
    click(cx, "terminal-pane-1-close");
    assert_eq!(pids(cx).len(), MAX_PANES - 1, "a pane's own close button");
    assert!(!pids(cx).contains(&shells[1]));
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(shells[1]) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(shells[1]), "closing a pane ends its shell");
}

#[gpui::test]
fn closing_the_last_pane_hides_the_drawer_and_the_next_toggle_starts_fresh(
    cx: &mut TestAppContext,
) {
    let (_dir, path, _) = seeded();
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let first = pids(cx)[0];
    click(cx, "terminal-close");
    assert!(cx.debug_bounds("terminal-drawer").is_none());
    assert!(terminal(cx).is_none(), "the session's terminal is gone");
    assert!(
        cx.update(|window, cx| view.read(cx).composer.focus_handle(cx).is_focused(window)),
        "focus returns to the composer"
    );

    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let second = pids(cx)[0];
    assert_ne!(first, second, "a fresh shell");
    run(cx, "exit");
    wait_until(cx, "a shell that exits closes its pane", |cx| {
        terminal(cx).is_none()
    });
    assert!(cx.debug_bounds("terminal-drawer").is_none());
}

#[gpui::test]
#[ignore = "benchmark; run through scripts/perf.py"]
fn perf_terminal_draw(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (view, handle) = launch(&path, cx);
    let cx = VisualTestContext::from_window(handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1960.), px(1500.)));
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let terminal = terminal(cx).unwrap();
    terminal.update(cx, |terminal, cx| {
        terminal.height = px(1000.);
        cx.notify();
    });
    let screen = dir.path().join("screen.txt");
    let row: String = (0..200)
        .map(|i| format!("\x1b[3{}m{}", i % 8, char::from(b'a' + (i % 26) as u8)))
        .collect();
    std::fs::write(&screen, format!("{row}\x1b[0m\n").repeat(60)).unwrap();
    run(cx, &format!("cat {}; echo done-$((1+1))", screen.display()));
    wait_until(cx, "the screen fills", |cx| {
        has_line(&output(cx), 0, "done-2")
    });
    let grid = cx.update(|_, cx| terminal.read(cx).panes[0].size());
    assert!(grid.columns >= 200 && grid.lines >= 50, "{grid:?}");
    // The test platform draws a notified window as the update's effects flush, so the
    // update is the frame. Output notifies only the terminal; a composer keystroke only
    // the window's root, which reuses the cached drawer.
    for (metric, output) in [("terminal_draw_ms", true), ("terminal_idle_ms", false)] {
        let frames = 30;
        let start = Instant::now();
        for _ in 0..frames {
            if output {
                terminal.update(cx, |_, cx| cx.notify());
            } else {
                view.update(cx, |_, cx| cx.notify());
            }
        }
        eprintln!(
            "PERF {metric}={:.2}",
            start.elapsed().as_secs_f64() * 1000. / frames as f64
        );
    }
}

#[gpui::test]
fn shortcuts_and_the_palette_move_focus_between_panes(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j secondary-shift-d secondary-shift-d");
    cx.run_until_parked();
    let focused = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            let terminal = session_terminal(1, cx).unwrap();
            terminal
                .read(cx)
                .panes
                .iter()
                .position(|pane| pane.focus.is_focused(window))
        })
    };
    assert_eq!(focused(cx), Some(2));
    cx.simulate_keystrokes("secondary-alt-right");
    assert_eq!(focused(cx), Some(0), "next wraps to the first pane");
    cx.simulate_keystrokes("secondary-alt-left");
    assert_eq!(focused(cx), Some(2), "previous wraps to the last");
    cx.simulate_keystrokes("secondary-alt-left");
    assert_eq!(focused(cx), Some(1));

    // Ctrl+K reaches the shell on Linux; Ctrl+Shift+P opens the palette on both.
    cx.simulate_keystrokes("secondary-shift-p");
    cx.simulate_input("Focus next terminal pane");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(focused(cx), Some(2), "the palette runs it too");
}

#[gpui::test]
fn the_shell_environment_leaves_out_keystone_and_multiplexer_variables(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    cx.update(|cx| {
        cx.global_mut::<Terminals>().shell.env = [
            ("KEYSTONE_STATE_PATH", "/tmp/leak"),
            ("TMUX", "/tmp/tmux-501/default,1,0"),
            ("TMUX_PANE", "%1"),
            ("STY", "1.pts-0"),
            ("ZELLIJ", "0"),
            ("ZELLIJ_SESSION_NAME", "main"),
            ("KEPT_BY_KEYSTONE", "yes"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .to_vec();
    });
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    let file = dir.path().join("env.txt");
    run(cx, &format!("env > {}", file.display()));
    let env = wait_for_file(&file);
    let names: Vec<&str> = env
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, _)| k)
        .collect();
    for withheld in [
        "KEYSTONE_STATE_PATH",
        "TMUX",
        "TMUX_PANE",
        "STY",
        "ZELLIJ",
        "ZELLIJ_SESSION_NAME",
        "ALACRITTY_WINDOW_ID",
        "WINDOWID",
    ] {
        assert!(!names.contains(&withheld), "{withheld} is left out: {env}");
    }
    assert!(env.lines().any(|l| l == "KEPT_BY_KEYSTONE=yes"), "{env}");
    assert!(env.lines().any(|l| l == "TERM=xterm-256color"), "{env}");
}

#[gpui::test]
fn the_drawer_leaves_room_for_the_main_area(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    terminal(cx).unwrap().update(cx, |terminal, cx| {
        terminal.height = px(2000.);
        cx.notify();
    });
    for (width, height) in [(1280., 800.), (860., 560.)] {
        cx.simulate_resize(gpui::size(px(width), px(height)));
        cx.run_until_parked();
        let drawer = cx.debug_bounds("terminal-drawer").unwrap();
        let send = cx.debug_bounds("send").expect("the send button shows");
        assert!(
            send.bottom() <= drawer.top() && send.top() >= px(0.),
            "at {width}×{height} the send button {send:?} is above the drawer {drawer:?}"
        );
        assert!(
            drawer.size.height > MIN_HEIGHT,
            "the drawer still takes the rest"
        );
    }
    cx.simulate_resize(gpui::size(px(1280.), px(300.)));
    cx.run_until_parked();
    assert_eq!(
        cx.debug_bounds("terminal-drawer").unwrap().size.height,
        MIN_HEIGHT,
        "a window too short for both keeps the drawer at its minimum"
    );
}

#[gpui::test]
fn switching_session_moves_focus_from_the_terminal_to_the_composer(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded_with(2);
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(pane_focused(cx));
    view.update(cx, |view, cx| view.select(2, cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("terminal-drawer").is_none());
    assert!(
        composer_focused(&view, cx),
        "typing goes to the composer on screen, not the hidden pane"
    );
}

#[gpui::test]
fn the_shortcut_in_settings_closes_them_and_shows_the_terminal(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-,");
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.settings.is_some()));
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.settings.is_none()));
    assert!(cx.debug_bounds("terminal-drawer").is_some());
    assert!(pane_focused(cx), "the terminal opens where it shows");
}

#[gpui::test]
fn closing_or_adding_a_pane_keeps_the_others_proportions(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j secondary-shift-d secondary-shift-d");
    cx.run_until_parked();
    let terminal = terminal(cx).unwrap();
    let shares = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| terminal.read(cx).split.read(cx).fractions().to_vec())
    };
    cx.update(|_, cx| {
        terminal.update(cx, |terminal, cx| {
            terminal
                .split
                .update(cx, |split, _| split.set_fractions(&[0.5, 0.25, 0.25]))
        })
    });
    click(cx, "terminal-pane-1");
    cx.simulate_keystrokes("secondary-alt-w");
    cx.run_until_parked();
    let close = |a: &[f32], b: &[f32]| {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-4)
    };
    assert!(close(&shares(cx), &[2. / 3., 1. / 3.]), "{:?}", shares(cx));
    cx.simulate_keystrokes("secondary-shift-d");
    cx.run_until_parked();
    assert!(
        close(&shares(cx), &[4. / 9., 2. / 9., 1. / 3.]),
        "{:?}",
        shares(cx)
    );
}

/// Down to the shortest window the layout supports with the drawer open, the
/// composer's Send button stays on screen above the drawer: a window too short for
/// the layout and the drawer's minimum folds into tabs.
#[gpui::test]
fn the_send_button_stays_above_the_drawer_in_short_windows(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let mut clipped = Vec::new();
    // 280 px holds the top bar, the fitted tabs, the composer and the drawer's 120 px
    // at Default density; nothing shorter can.
    for height in (280..=800).rev().step_by(10) {
        cx.simulate_resize(gpui::size(px(1100.), px(height as f32)));
        cx.run_until_parked();
        let drawer = cx.debug_bounds("terminal-drawer").unwrap();
        let fitted = cx.debug_bounds("fitted-tabs").is_some();
        match cx.debug_bounds("send") {
            Some(send) if send.bottom() <= drawer.top() && send.top() >= px(0.) => {}
            send => clipped.push((height, fitted, send.map(|s| s.bottom()), drawer.top())),
        }
    }
    assert!(clipped.is_empty(), "{clipped:?}");
}
