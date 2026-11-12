use super::*;

#[gpui::test]
fn every_window_showing_the_session_shows_its_drawer(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (first, handle) = launch(&path, cx);
    visual(handle, cx).simulate_keystrokes("secondary-alt-shift-n");
    cx.run_until_parked();
    let second = cx
        .update(|cx| open_windows(cx))
        .into_iter()
        .find(|view| view != &first)
        .expect("a second window");
    let other = cx.read(|cx| second.read(cx).handle);
    visual(other, cx);
    let shown = |handle: AnyWindowHandle, cx: &mut TestAppContext| {
        VisualTestContext::from_window(handle, cx)
            .debug_bounds("terminal-drawer")
            .is_some()
    };
    assert!(!shown(handle, cx) && !shown(other, cx));

    visual(handle, cx).simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(
        shown(handle, cx),
        "the window that toggled shows the drawer"
    );
    assert!(shown(other, cx), "so does the other window on the session");
    visual(handle, cx).simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(
        !shown(handle, cx),
        "the shortcut hides it again from the first window"
    );
    click(visual(handle, cx), "terminal-toggle");
    assert!(shown(handle, cx), "the top bar button shows it again");
    assert!(shown(other, cx), "in both windows");

    click(visual(other, cx), "terminal-toggle");
    assert!(!shown(other, cx), "hiding it in the other window");
    assert!(!shown(handle, cx), "hides it in the first too");

    visual(other, cx).simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    click(visual(other, cx), "terminal-close");
    assert!(terminal(visual(handle, cx)).is_none());
    assert!(
        !shown(handle, cx) && !shown(other, cx),
        "closing the last pane hides the drawer everywhere"
    );
}

#[gpui::test]
fn closing_a_window_keeps_the_shell_and_its_scrollback(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (first, handle) = launch(&path, cx);
    {
        let cx = visual(handle, cx);
        cx.simulate_keystrokes("secondary-j");
        run(cx, "seq 1 300; echo marker-$((6*7))");
        wait_until(cx, "output lands", |cx| {
            has_line(&output(cx), 0, "marker-42")
        });
        cx.simulate_keystrokes("secondary-alt-shift-n");
        cx.run_until_parked();
    }
    let pid = cx.update(|cx| session_terminal(1, cx).unwrap().read(cx).panes[0].pid);
    let root: WeakEntity<Keystone> = first.downgrade();
    let closed = handle
        .update(cx, |_, window, cx| {
            let closed = close_window(&root, window, cx);
            if closed {
                window.remove_window();
            }
            closed
        })
        .unwrap();
    assert!(closed, "the first window closes while the second stays");
    cx.run_until_parked();
    assert!(alive(pid), "the shell outlives the window");

    let second = cx.update(|cx| open_windows(cx))[0].clone();
    let other = cx.read(|cx| second.read(cx).handle);
    let cx = visual(other, cx);
    assert!(
        cx.debug_bounds("terminal-drawer").is_some(),
        "the other window shows the session's terminal"
    );
    let text = &output(cx)[0];
    assert!(has_line(&output(cx), 0, "1"), "scrollback kept: {text}");
    assert!(has_line(&output(cx), 0, "marker-42"));
    assert_eq!(pids(cx), [pid]);
}

/// The PTY has one size. With the session in a large and a small window, it takes the
/// small one's and keeps it frame after frame, and a drag in either window selects
/// through that window's own layout.
#[gpui::test]
fn two_windows_of_different_sizes_settle_on_one_grid(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (first, handle) = launch(&path, cx);
    let other = second_window(&first, handle, cx);
    VisualTestContext::from_window(other, cx)
        .into_mut()
        .simulate_resize(gpui::size(px(900.), px(420.)));
    visual(handle, cx).simulate_keystrokes("secondary-j");
    cx.run_until_parked();

    let size = cx.update(|cx| session_terminal(1, cx).unwrap().read(cx).panes[0].size());
    let fits = |handle: AnyWindowHandle, cx: &mut TestAppContext| {
        cx.update(|cx| {
            let terminal = session_terminal(1, cx).unwrap();
            terminal.read(cx).panes[0]
                .view(handle.window_id())
                .expect("both windows paint the pane")
                .fits
        })
    };
    let (large, small) = (fits(handle, cx), fits(other, cx));
    assert!(small.columns < large.columns && small.lines < large.lines);
    assert_eq!((size.columns, size.lines), (small.columns, small.lines));

    let winch = dir.path().join("winch.txt");
    let stop = dir.path().join("stop");
    let row = {
        let cx = visual(handle, cx);
        // Ended by a file rather than Ctrl+C, whose interrupt can flush the line typed
        // after it.
        run(
            cx,
            &format!(
                "sh -c 'trap \"echo w >> {}\" WINCH; while [ ! -e {} ]; do sleep 0.05; done'",
                winch.display(),
                stop.display()
            ),
        );
        std::thread::sleep(Duration::from_millis(300));
        let terminal = terminal(cx).unwrap();
        for _ in 0..6 {
            terminal.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !winch.exists(),
            "redrawing both windows never resized the PTY: {:?}",
            std::fs::read_to_string(&winch)
        );
        std::fs::write(&stop, "").unwrap();
        let stty = dir.path().join("stty.txt");
        run(cx, &format!("stty size > {}", stty.display()));
        assert_eq!(
            wait_for_file(&stty).trim(),
            format!("{} {}", small.lines, small.columns),
            "the shell sees the small window's grid"
        );
        run(cx, "printf 'alpha beta\\n'");
        wait_until(cx, "the text shows", |cx| {
            row_of(cx, "alpha beta").is_some()
        });
        row_of(cx, "alpha beta").unwrap()
    };
    {
        let cx = VisualTestContext::from_window(other, cx).into_mut();
        drag_select(cx, row, 0..=4);
        cx.simulate_keystrokes(copy_keys());
        assert_eq!(
            clipboard(cx).as_deref(),
            Some("alpha"),
            "in the small window"
        );
    }
    let cx = visual(handle, cx);
    drag_select(cx, row, 6..=9);
    cx.simulate_keystrokes(copy_keys());
    assert_eq!(
        clipboard(cx).as_deref(),
        Some("beta"),
        "and in the large one"
    );
}

/// Each window acts on the pane focused in it, not the one another window focused last.
#[gpui::test]
fn close_follows_the_pane_focused_in_this_window(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (first, handle) = launch(&path, cx);
    let other = second_window(&first, handle, cx);
    {
        let cx = visual(handle, cx);
        cx.simulate_keystrokes("secondary-j secondary-shift-d");
        cx.run_until_parked();
    }
    let shells = pids(visual(handle, cx));
    assert_eq!(shells.len(), 2);
    click(visual(handle, cx), "terminal-pane-0");
    click(visual(other, cx), "terminal-pane-1");

    visual(handle, cx).simulate_keystrokes("secondary-alt-w");
    cx.run_until_parked();
    assert_eq!(
        pids(visual(handle, cx)),
        [shells[1]],
        "Close in the first window closes its focused pane"
    );
    assert!(
        cx.update(|cx| {
            let terminal = session_terminal(1, cx).unwrap();
            other
                .update(cx, |_, window, cx| terminal.read(cx).contains_focus(window))
                .unwrap()
        }),
        "the other window keeps its pane focused"
    );
}

/// A small window that stops showing the session gives up its claim on the PTY's
/// size, and the large window's drawer repaints at its own size with no output from
/// the shell to prompt it.
#[gpui::test]
fn a_window_that_stops_showing_the_session_gives_up_its_size(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded_with(2);
    let (first, handle) = launch(&path, cx);
    let other = second_window(&first, handle, cx);
    VisualTestContext::from_window(other, cx)
        .into_mut()
        .simulate_resize(gpui::size(px(900.), px(420.)));
    // A shell that writes nothing, even when the PTY resizes, so only Keystone can
    // repaint the drawer.
    cx.update(|cx| use_shell("/bin/sleep", &["30"], cx));
    visual(handle, cx).simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let second = cx
        .update(|cx| open_windows(cx))
        .into_iter()
        .find(|view| view != &first)
        .unwrap();
    let grid = |cx: &mut TestAppContext| {
        let size = cx.update(|cx| session_terminal(1, cx).unwrap().read(cx).panes[0].size());
        (size.columns, size.lines)
    };
    let fits = |handle: AnyWindowHandle, cx: &mut TestAppContext| {
        let fits = cx.update(|cx| {
            let terminal = session_terminal(1, cx).unwrap();
            terminal.read(cx).panes[0]
                .view(handle.window_id())
                .unwrap()
                .fits
        });
        (fits.columns, fits.lines)
    };
    let (large, small) = (fits(handle, cx), fits(other, cx));
    assert!(small < large);
    assert_eq!(grid(cx), small);

    second.update(cx, |view, cx| view.select(2, cx));
    cx.run_until_parked();
    assert_eq!(grid(cx), large, "switching session releases the small size");

    second.update(cx, |view, cx| view.select(1, cx));
    cx.run_until_parked();
    assert_eq!(grid(cx), small, "showing it again claims it back");

    let settings = |cx: &mut TestAppContext| {
        VisualTestContext::from_window(other, cx)
            .into_mut()
            .simulate_keystrokes("secondary-,");
        cx.run_until_parked();
    };
    settings(cx);
    assert_eq!(grid(cx), large, "Settings covering the drawer releases it");
    settings(cx);
    assert_eq!(grid(cx), small);

    cx.simulate_window_visibility_change(other, gpui::WindowVisibility::Hidden);
    cx.run_until_parked();
    assert_eq!(grid(cx), large, "a hidden window, such as a minimized one");
    cx.simulate_window_visibility_change(other, gpui::WindowVisibility::Visible);
    cx.run_until_parked();
    assert_eq!(grid(cx), small, "and shown again, it claims it back");

    let root = second.downgrade();
    other
        .update(cx, |_, window, cx| {
            assert!(close_window(&root, window, cx));
            window.remove_window();
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(grid(cx), large, "closing the small window releases it");
}
