use super::*;

/// Output throughput while the drawer redraws after every batch, as it does under
/// heavy output. The test platform draws a notified window as the update's effects
/// flush, so every pump turn is a frame, and a frame that holds the screen's lock
/// while it shapes text holds up the IO thread for that long.
#[gpui::test]
#[ignore = "benchmark; run in release: cargo test --release ... perf_terminal_flood"]
fn perf_terminal_flood(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = VisualTestContext::from_window(handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1960.), px(1500.)));
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    let terminal = terminal(cx).unwrap();
    terminal.update(cx, |terminal, cx| {
        terminal.height = px(1000.);
        cx.notify();
    });
    cx.run_until_parked();
    let file = dir.path().join("flood.txt");
    let row: String = (0..200)
        .map(|i| format!("\x1b[3{}m{}", i % 8, char::from(b'a' + (i % 26) as u8)))
        .collect();
    let row = format!("{row}\x1b[0m\n");
    let megabytes = 40;
    let rows = megabytes * 1024 * 1024 / row.len();
    std::fs::write(&file, row.repeat(rows)).unwrap();
    super::element::LOCK_HELD.lock().unwrap().clear();
    let started = Instant::now();
    run(cx, &format!("cat {}; echo flood-$((1+1))", file.display()));
    let deadline = started + Duration::from_secs(300);
    loop {
        // A frame, as the display asks for one while output arrives.
        terminal.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        let screen =
            cx.update(|_, cx| session_terminal(1, cx).unwrap().read(cx).panes[0].screen_text());
        if screen.lines().any(|line| line.trim() == "flood-2") {
            break;
        }
        assert!(Instant::now() < deadline, "the flood did not end");
        std::thread::sleep(Duration::from_millis(2));
    }
    let seconds = started.elapsed().as_secs_f64();
    let mut held = std::mem::take(&mut *super::element::LOCK_HELD.lock().unwrap());
    held.sort();
    let ms = |at: f64| held[((held.len() - 1) as f64 * at) as usize].as_secs_f64() * 1000.;
    eprintln!("PERF terminal_flood_mb_s={:.1}", megabytes as f64 / seconds);
    eprintln!("PERF terminal_flood_frames={}", held.len());
    eprintln!("PERF terminal_flood_lock_p50_ms={:.3}", ms(0.5));
    eprintln!("PERF terminal_flood_lock_max_ms={:.3}", ms(1.));
}

/// Output faster than the screen draws loses nothing the scrollback holds.
#[gpui::test]
fn a_flood_keeps_every_line_the_scrollback_holds(cx: &mut TestAppContext) {
    let (_dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    run(cx, "seq 1 30000; echo end-$((1+1))");
    wait_until(cx, "the flood ends", |cx| has_line(&output(cx), 0, "end-2"));
    let text = &output(cx)[0];
    let numbers: Vec<u32> = text
        .lines()
        .skip_while(|line| line.trim().parse::<u32>().is_err())
        .map_while(|line| line.trim().parse().ok())
        .collect();
    assert!(numbers.len() >= 10_000, "{} lines kept", numbers.len());
    assert_eq!(numbers.last(), Some(&30_000));
    assert!(
        numbers.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "no line is missing"
    );
}

/// Replies a program asks for are never dropped, however many it asks for at once:
/// more than one turn of the pump applies.
#[gpui::test]
fn every_reply_reaches_the_program(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-j");
    let queries = EVENTS_PER_TURN * 3;
    let replies = dir.path().join("replies");
    let script = dir.path().join("ask.sh");
    // Each status query (DSR 5) is answered with the four bytes ESC [ 0 n.
    std::fs::write(
        &script,
        format!(
            "stty raw -echo\nprintf '\\033[5n%.0s' $(seq 1 {queries})\n\
             dd bs=1 count={} of={} 2>/dev/null\nstty sane\n",
            queries * 4,
            replies.display()
        ),
    )
    .unwrap();
    run(cx, &format!("sh {}", script.display()));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let got = std::fs::read(&replies).unwrap_or_default();
        if got.len() == queries * 4 {
            assert_eq!(got, b"\x1b[0n".repeat(queries));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{} of {} reply bytes arrived",
            got.len(),
            queries * 4
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// OSC 52 from the pane being typed in sets the clipboard. From a pane in the
/// background it does not, so output there cannot replace what the user copied.
#[gpui::test]
fn only_a_focused_pane_sets_the_clipboard(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.update(|window, _| window.activate_window());
    cx.simulate_keystrokes("secondary-j");
    // OSC 52 with "focused" and "hidden" in base64.
    run(cx, "printf '\\033]52;c;Zm9jdXNlZA==\\007'");
    let deadline = Instant::now() + Duration::from_secs(10);
    while clipboard(cx).as_deref() != Some("focused") {
        assert!(
            Instant::now() < deadline,
            "the focused pane set the clipboard"
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    let marker = dir.path().join("written");
    run(
        cx,
        &format!(
            "(sleep 0.5; printf '\\033]52;c;aGlkZGVu\\007'; touch {}) &",
            marker.display()
        ),
    );
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(composer_focused(&view, cx));
    wait_for_file_to_exist(&marker);
    std::thread::sleep(Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(clipboard(cx).as_deref(), Some("focused"));
}

/// While another app is in front, no Keystone window is active, and even the focused
/// pane cannot set the clipboard.
#[gpui::test]
fn a_pane_in_an_inactive_window_cannot_set_the_clipboard(cx: &mut TestAppContext) {
    let (dir, path, _) = seeded();
    let (_view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.update(|window, _| window.activate_window());
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(pane_focused(cx));
    // OSC 52 with "copied" in base64, from the focused pane of the active window.
    run(cx, "printf '\\033]52;c;Y29waWVk\\007'");
    let deadline = Instant::now() + Duration::from_secs(10);
    while clipboard(cx).as_deref() != Some("copied") {
        assert!(Instant::now() < deadline, "the active window's pane set it");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    let marker = dir.path().join("written");
    // OSC 52 with "hidden" in base64, once the window is no longer active.
    run(
        cx,
        &format!(
            "(sleep 0.5; printf '\\033]52;c;aGlkZGVu\\007'; touch {}) &",
            marker.display()
        ),
    );
    cx.deactivate_window();
    assert!(pane_focused(cx), "the pane keeps focus in its window");
    wait_for_file_to_exist(&marker);
    std::thread::sleep(Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(clipboard(cx).as_deref(), Some("copied"));
}

fn wait_for_file_to_exist(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{} was not made", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}
