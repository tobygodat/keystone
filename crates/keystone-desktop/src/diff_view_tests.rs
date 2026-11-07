// Included into main.rs's test module, whose helpers these tests use.

/// Runs the app until `selector` is drawn.
fn wait_for_element(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    for _ in 0..500 {
        cx.run_until_parked();
        if cx.debug_bounds(selector).is_some() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("{selector} was never drawn");
}

/// A wrapper script for the fake `name`, written by a child process. A file this
/// process writes and then runs can be held open by a fork from a sibling test's thread,
/// and exec then fails with "Text file busy".
fn script_wrapper(dir: &Path, name: &str, env: &str) -> PathBuf {
    use std::io::Write;
    let script = dir.join(format!("{name}-wrapper"));
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
        .arg(&script)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let body = format!("#!/bin/sh\n{env} exec '{}' \"$@\"\n", fake(name).display());
    child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    script
}

#[gpui::test]
fn a_turns_diff_sends_line_comments_keeps_the_draft_and_keeps_unsent_comments(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=T", "-c", "user.email=t@e"])
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    // The app's own git commands in this repository run without the flags above.
    git(&["config", "maintenance.auto", "false"]);
    git(&["config", "gc.auto", "0"]);
    std::fs::write(repo.join("notes.md"), "Draft\n").unwrap();
    git(&["add", "--all"]);
    git(&["commit", "-q", "-m", "start"]);

    let path = dir.path().join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(
            Provider::Claude,
            &keystone_core::WorkingDirectory::new(&repo).unwrap(),
            1,
        )
        .unwrap();
    drop(store);
    let home = dir.path().join("claude-home");
    std::fs::create_dir_all(&home).unwrap();
    let claude = script_wrapper(
        dir.path(),
        "keystone-fake-claude",
        &format!(
            "KEYSTONE_FAKE_CLAUDE_DELAY_MS=1 CLAUDE_CONFIG_DIR='{}'",
            home.display()
        ),
    );
    let (view, cx) = open_window(&path, cx);
    view.update_in(cx, |this, window, cx| {
        this.focus.focus(window, cx);
        this.state.update(cx, |state, _| {
            state.test_checkpoints = true;
            state.test_set_claude(Some(ClaudeConnection::new(
                Some(claude),
                state.test_wake().clone(),
            )));
        });
    });
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });

    send_text(&view, cx, "write notes.md: Shipped");
    wait_until(cx, &view, |v, cx| {
        let state = v.state.read(cx);
        !state.conversations()[&1].running
            && matches!(
                state.checkpoints(1),
                Some(crate::app_state::checkpoints::State::Saved(saved))
                    if saved.turns.get(&1) == Some(&Some(1))
            )
    });
    view.update(cx, |v, cx| {
        v.composer
            .update(cx, |input, cx| input.set_value("my own draft".into(), cx));
        assert!(v.save_draft(cx));
    });
    let row = view.read_with(cx, |v, cx| {
        v.state.read(cx).conversations()[&1]
            .checkpoint_row(1)
            .unwrap()
    });
    let button: &'static str = format!("view-changes-{row}").leak();
    wait_for_element(cx, button);
    let bounds = cx.debug_bounds(button).unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    wait_for_element(cx, "diff-lines");

    cx.simulate_keystrokes("down down c");
    wait_for_element(cx, "diff-comment-input");
    cx.simulate_input("Why ship it?");
    cx.simulate_keystrokes("enter");
    wait_for_element(cx, "diff-comment-0");
    cx.simulate_keystrokes("secondary-enter");
    wait_until(cx, &view, |v, cx| {
        v.state.read(cx).conversations()[&1].rows.iter().any(|row| {
            matches!(row, Row::Item(item)
                if item.kind == ItemKind::UserMessage
                    && item.text == "Comments on your changes:\n\nnotes.md line 1:\n```diff\n+Shipped\n```\nWhy ship it?")
        })
    });
    view.read_with(cx, |v, cx| {
        assert_eq!(v.composer.read(cx).value(), "my own draft");
        let state = v.state.read(cx);
        assert_eq!(state.store().workspace().sessions()[0].draft(), "my own draft");
    });
    assert!(cx.debug_bounds("diff-comment-0").is_none(), "sent comments clear");

    cx.simulate_keystrokes("c");
    wait_for_element(cx, "diff-comment-input");
    cx.simulate_input("Not sent yet");
    cx.simulate_keystrokes("enter escape");
    cx.run_until_parked();
    assert!(view.read_with(cx, |v, _| v.diff_in(MAIN_1).is_none()));
    let bounds = cx.debug_bounds(button).unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    wait_for_element(cx, "diff-comment-0");
}

/// In a window too narrow for both, Changes shows as a tab in front of the
/// conversation. Its diff button brings the conversation forward with the diff in it.
#[gpui::test]
fn the_changes_diff_button_opens_behind_a_narrow_windows_changes_tab(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=T", "-c", "user.email=t@e"])
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "maintenance.auto", "false"]);
    git(&["config", "gc.auto", "0"]);
    std::fs::write(repo.join("notes.md"), "Draft\n").unwrap();
    git(&["add", "--all"]);
    git(&["commit", "-q", "-m", "start"]);
    std::fs::write(repo.join("notes.md"), "Shipped\n").unwrap();

    let path = dir.path().join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(
            Provider::Claude,
            &keystone_core::WorkingDirectory::new(&repo).unwrap(),
            1,
        )
        .unwrap();
    drop(store);
    let (view, cx) = open_window(&path, cx);
    view.update_in(cx, |this, window, cx| this.focus.focus(window, cx));
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    cx.simulate_resize(gpui::size(px(400.), px(900.)));
    cx.run_until_parked();
    cx.simulate_keystrokes("secondary-shift-g");
    wait_for_element(cx, "changes-diff");
    assert!(
        cx.debug_bounds("conversation").is_none(),
        "Changes covers the conversation"
    );

    let bounds = cx.debug_bounds("changes-diff").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    wait_for_element(cx, "diff-lines");
    assert!(view.read_with(cx, |v, _| v.diff_in(MAIN_1).is_some()));
}
