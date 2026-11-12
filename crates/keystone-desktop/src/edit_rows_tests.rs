// Included into main.rs's test module, whose helpers these tests use. The fakes replay
// the edits recorded in keystone-agent's tests/fixtures/edits.

/// A workspace with one session of `provider` in a fresh folder, and its path.
fn edit_workspace(dir: &Path, provider: Provider) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(
            provider,
            &keystone_core::WorkingDirectory::new(&repo).unwrap(),
            1,
        )
        .unwrap();
    path
}

/// Each file change row of session 1: its place and how many diff lines it holds.
fn edit_rows(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Vec<(usize, usize)> {
    view.read_with(cx, |v, cx| {
        let rows = &v.state.read(cx).conversations()[&1].rows;
        rows.iter()
            .enumerate()
            .filter_map(|(index, row)| match row {
                Row::Item(Item {
                    kind: ItemKind::FileChange { patch, .. },
                    ..
                }) => {
                    let lines = patch.iter().flat_map(|p| &p.files).flat_map(|f| &f.hunks);
                    Some((index, lines.map(|h| h.lines.len()).sum()))
                }
                _ => None,
            })
            .collect()
    })
}

fn leaked(text: String) -> &'static str {
    text.leak()
}

/// Opens and closes the edit row at `index` with the mouse, and checks it draws exactly
/// `lines` diff lines while open.
fn opens_to_its_diff(cx: &mut gpui::VisualTestContext, index: usize, lines: usize) {
    let collapsed = leaked(format!("activity-collapsed-{index}"));
    let body = leaked(format!("activity-body-{index}"));
    assert!(cx.debug_bounds(collapsed).is_some(), "row {index} starts collapsed");
    assert!(cx.debug_bounds(body).is_none());
    click(cx, collapsed);
    assert!(cx.debug_bounds(body).is_some(), "row {index} opens in place");
    let cut = leaked(format!("edit-cut-{index}"));
    assert!(cx.debug_bounds(cut).is_none(), "row {index} is whole");
    let line = |n: usize| leaked(format!("edit-{index}-line-{n}"));
    assert!(cx.debug_bounds(line(lines - 1)).is_some(), "row {index} draws its last line");
    assert!(cx.debug_bounds(line(lines)).is_none(), "row {index} draws only its own lines");
    click(cx, leaked(format!("activity-expanded-{index}")));
    assert!(cx.debug_bounds(body).is_none(), "row {index} closes again");
}

/// Quits by flushing the logs, then opens the workspace again in a new window.
fn relaunch<'a>(
    view: &Entity<Keystone>,
    path: &Path,
    cx: &'a mut gpui::VisualTestContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
    view.update(cx, |view, cx| view.flush_logs(cx));
    let store = Store::open(path).unwrap();
    let (restored, cx) = cx.add_window_view(|window, cx| {
        Keystone::new(
            cx.new(|cx| crate::AppState::new(store, cx)),
            false,
            window,
            cx,
        )
    });
    wait_until(cx, &restored, |view, cx| {
        view.state
            .read(cx)
            .conversations()
            .get(&1)
            .is_some_and(|c| !c.is_loading())
    });
    (restored, cx)
}

#[gpui::test]
fn claude_edit_rows_open_to_their_own_diffs_and_keep_them_after_a_restart(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = edit_workspace(dir.path(), Provider::Claude);
    let claude = wrapper(
        dir.path(),
        "keystone-fake-claude",
        &format!(
            "KEYSTONE_FAKE_CLAUDE_DELAY_MS=1 CLAUDE_CONFIG_DIR='{}'",
            dir.path().display()
        ),
    );
    let (view, cx) = open_window(&path, cx);
    view.update_in(cx, |this, window, cx| {
        this.focus.focus(window, cx);
        this.state.update(cx, |state, _| {
            state.test_set_claude(Some(ClaudeConnection::new(
                Some(claude),
                state.test_wake().clone(),
            )));
        });
    });
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    send_text(&view, cx, "replay edits");
    wait_until(cx, &view, |v, cx| {
        let c = &v.state.read(cx).conversations()[&1];
        !c.running && c.rows.len() > 2
    });

    // Edit a.txt, Edit b.txt, Write c.txt (new), Write d.txt, Edit b.txt again.
    let rows = edit_rows(&view, cx);
    let lines: Vec<usize> = rows.iter().map(|(_, lines)| *lines).collect();
    assert_eq!(lines, [6, 4, 2, 2, 4]);
    for (index, lines) in &rows {
        opens_to_its_diff(cx, *index, *lines);
    }

    let (first, _) = rows[0];
    activity_keys(cx, "secondary-shift-t home");
    for _ in 0..first {
        activity_keys(cx, "down");
    }
    activity_keys(cx, "tab enter");
    assert!(
        cx.debug_bounds(leaked(format!("activity-body-{first}"))).is_some(),
        "Enter opens the focused edit"
    );
    activity_keys(cx, "space");
    assert!(
        cx.debug_bounds(leaked(format!("activity-body-{first}"))).is_none(),
        "Space closes it"
    );
    activity_keys(cx, "space");

    let (restored, cx) = relaunch(&view, &path, cx);
    let rows = edit_rows(&restored, cx);
    let restored_lines: Vec<usize> = rows.iter().map(|(_, lines)| *lines).collect();
    assert_eq!(restored_lines, lines, "each edit's diff is restored");
    for (index, lines) in &rows {
        opens_to_its_diff(cx, *index, *lines);
    }
}

#[gpui::test]
fn a_codex_file_change_row_opens_to_each_files_diff_and_keeps_it_after_a_restart(
    cx: &mut gpui::TestAppContext,
) {
    let _env = codex_env();
    let dir = tempfile::tempdir().unwrap();
    let path = edit_workspace(dir.path(), Provider::Codex);
    let codex = wrapper(
        dir.path(),
        "keystone-fake-codex",
        "KEYSTONE_FAKE_CODEX_DELAY_MS=1",
    );
    let (view, cx) = open_window(&path, cx);
    view.update_in(cx, |this, window, cx| {
        this.focus.focus(window, cx);
        this.state.update(cx, |state, _| {
            state.test_set_codex(Some(CodexConnection::new(
                Some(codex),
                "test",
                state.test_wake().clone(),
            )));
        });
    });
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    send_text(&view, cx, "replay edits");
    wait_until(cx, &view, |v, cx| {
        let c = &v.state.read(cx).conversations()[&1];
        !c.running && c.rows.len() > 2
    });

    // a.txt's update (4 lines), c.txt added (2) and d.txt deleted (1), in one change.
    let rows = edit_rows(&view, cx);
    assert_eq!(rows.iter().map(|(_, l)| *l).collect::<Vec<_>>(), [7]);
    opens_to_its_diff(cx, rows[0].0, 7);

    let (restored, cx) = relaunch(&view, &path, cx);
    let rows = edit_rows(&restored, cx);
    assert_eq!(rows.iter().map(|(_, l)| *l).collect::<Vec<_>>(), [7]);
    opens_to_its_diff(cx, rows[0].0, 7);
}

#[gpui::test]
fn an_edit_over_the_cap_shows_its_start_and_says_it_was_cut(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, path, cx) = claude_window(dir.path(), cx);
    send_text(&view, cx, "long edit 1500");
    wait_until(cx, &view, |v, cx| {
        let c = &v.state.read(cx).conversations()[&1];
        !c.running && edit_count(c) == 1
    });
    says_it_was_cut(&view, cx, "on first launch");
    let (restored, cx) = relaunch(&view, &path, cx);
    says_it_was_cut(&restored, cx, "after a restart");
}

/// How many of an edit row's diff lines are built.
fn built_lines(cx: &mut gpui::VisualTestContext, index: usize) -> usize {
    (0..keystone_agent::diff::EDIT_LINES)
        .filter(|n| cx.debug_bounds(leaked(format!("edit-{index}-line-{n}"))).is_some())
        .count()
}

/// A window over one Claude session, run by the fake Claude, and the workspace's path.
fn claude_window<'a>(
    dir: &Path,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, PathBuf, &'a mut gpui::VisualTestContext) {
    let path = edit_workspace(dir, Provider::Claude);
    let claude = wrapper(
        dir,
        "keystone-fake-claude",
        &format!("KEYSTONE_FAKE_CLAUDE_DELAY_MS=1 CLAUDE_CONFIG_DIR='{}'", dir.display()),
    );
    let (view, cx) = open_window(&path, cx);
    view.update_in(cx, |this, window, cx| {
        this.focus.focus(window, cx);
        this.state.update(cx, |state, _| {
            state.test_set_claude(Some(ClaudeConnection::new(
                Some(claude),
                state.test_wake().clone(),
            )));
        });
    });
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    (view, path, cx)
}

/// Two long edits from one Claude process are two rows, each with its own lines.
#[gpui::test]
fn each_scripted_long_edit_is_its_own_row(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, _, cx) = claude_window(dir.path(), cx);
    for (n, prompt) in ["long edit 20", "long edit 30"].into_iter().enumerate() {
        send_text(&view, cx, prompt);
        wait_until(cx, &view, |v, cx| {
            let c = &v.state.read(cx).conversations()[&1];
            !c.running && edit_count(c) == n + 1
        });
    }
    let lines: Vec<usize> = edit_rows(&view, cx).into_iter().map(|(_, lines)| lines).collect();
    assert_eq!(lines, [20, 30]);
}

/// Session 1 holds one edit, kept to the cap. Its open row draws the preview and how
/// many lines are left; Open full edit, by mouse and by Ctrl+Shift+D, shows all of it in
/// the diff view, which says where the kept lines end.
fn says_it_was_cut(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, when: &str) {
    let rows = edit_rows(view, cx);
    assert_eq!(rows.len(), 1, "{when}");
    let (long, lines) = rows[0];
    assert_eq!(lines, keystone_agent::diff::EDIT_LINES, "{when}");
    click(cx, leaked(format!("activity-collapsed-{long}")));
    assert_eq!(built_lines(cx, long), crate::transcript::PREVIEW_LINES, "{when}");
    assert!(cx.debug_bounds(leaked(format!("edit-more-{long}"))).is_some(), "{when}");
    assert!(
        cx.debug_bounds(leaked(format!("edit-cut-{long}"))).is_none(),
        "{when}: the row does not show where the kept lines end"
    );
    for open in ["mouse", "tab and enter", "shortcut"] {
        match open {
            "mouse" => click(cx, leaked(format!("edit-open-{long}"))),
            "tab and enter" => {
                click(cx, leaked(format!("activity-expanded-{long}")));
                click(cx, leaked(format!("activity-collapsed-{long}")));
                activity_keys(cx, "tab");
                assert!(
                    view.read_with(cx, |v, _| v
                        .presentation
                        .current
                        .views
                        .keyboard_state(MAIN_1)
                        .and_then(|keyboard| keyboard.action)
                        == Some(crate::view_state::CopyTarget::OpenEdit)),
                    "{when}: Tab from the toggle reaches Open full edit"
                );
                activity_keys(cx, "enter");
            }
            _ => {
                click(cx, leaked(format!("activity-expanded-{long}")));
                click(cx, leaked(format!("activity-collapsed-{long}")));
                activity_keys(cx, "secondary-shift-d");
            }
        }
        assert!(cx.debug_bounds("diff-lines").is_some(), "{when}: {open} opens the full edit");
        assert!(cx.debug_bounds("diff-comment").is_none(), "{when}: the full edit takes no comments");
        // The list measures rows as they come into view, so it is wheeled to its end.
        for _ in 0..200 {
            if cx.debug_bounds("diff-truncated").is_some() {
                break;
            }
            let lines = cx.debug_bounds("diff-lines").unwrap();
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: lines.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-2_000.))),
                ..Default::default()
            });
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("diff-truncated").is_some(),
            "{when}: the full edit says where its kept lines end"
        );
        activity_keys(cx, "escape");
        assert!(cx.debug_bounds("diff-lines").is_none(), "{when}: Escape goes back");
        if open != "shortcut" {
            let back = view.read_with(cx, |v, _| {
                v.presentation
                    .current
                    .views
                    .keyboard_state(MAIN_1)
                    .map(|keyboard| (keyboard.row, keyboard.action))
            });
            assert_eq!(
                back,
                Some((Some(long), Some(crate::view_state::CopyTarget::OpenEdit))),
                "{when}: after {open}, Escape gives focus back to Open full edit"
            );
        }
    }
}

/// An open edit row builds no more line elements than the preview, however long the
/// edit, so its cost does not grow with the edit.
#[gpui::test]
fn an_open_edit_row_builds_only_its_preview_lines(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, index, cx) = long_edit_session(dir.path(), cx);
    click(cx, leaked(format!("activity-collapsed-{index}")));
    assert!(view.read_with(cx, |v, _| v.presentation.current.views.activity_expanded(MAIN_1, index, false)));
    assert_eq!(built_lines(cx, index), crate::transcript::PREVIEW_LINES);
    assert!(cx.debug_bounds(leaked(format!("edit-more-{index}"))).is_some());
}

/// What Ctrl+Shift+C on the disclosure of edit row `index` copies.
fn copy_edit(cx: &mut gpui::VisualTestContext, index: usize) -> String {
    let collapsed = leaked(format!("activity-collapsed-{index}"));
    let expanded = leaked(format!("activity-expanded-{index}"));
    let toggle = if cx.debug_bounds(collapsed).is_some() { collapsed } else { expanded };
    click(cx, toggle);
    cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into())));
    activity_keys(cx, "secondary-shift-c");
    cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).unwrap()
}

/// Each `---` and `+++` line names `/dev/null` or a path inside the session's folder,
/// after git's `a/` or `b/`, and no line names the folder itself.
fn paths_are_from_the_folder(copied: &str, folder: &Path) {
    let folder = folder.display().to_string();
    assert!(!copied.contains(&folder), "no path outside the folder: {copied}");
    let headers: Vec<&str> = copied
        .lines()
        .filter(|line| line.starts_with("--- ") || line.starts_with("+++ "))
        .collect();
    assert!(!headers.is_empty(), "{copied}");
    for header in headers {
        let path = &header[4..];
        let relative = path
            .strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .filter(|rest| !rest.is_empty() && !rest.starts_with('/'));
        assert!(path == "/dev/null" || relative.is_some(), "{header}");
    }
}

/// Runs `git apply` with `args` and `patch` in `repo`, and says whether git took it.
fn git_apply(repo: &Path, args: &[&str], patch: &str) -> bool {
    use std::io::Write;
    let mut git = std::process::Command::new("git")
        .arg("apply")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    git.stdin.take().unwrap().write_all(patch.as_bytes()).unwrap();
    let output = git.wait_with_output().unwrap();
    if !output.status.success() {
        eprintln!("git apply: {}", String::from_utf8_lossy(&output.stderr));
    }
    output.status.success()
}

/// A folder holding the files the recordings started from.
fn originals(dir: &Path) -> PathBuf {
    let repo = dir.join("originals");
    std::fs::create_dir(&repo).unwrap();
    for (file, text) in [
        ("a.txt", "one\ntwo\nthree\nfour\nfive\n"),
        ("b.txt", "red\ngreen\nblue\n"),
        ("d.txt", "old\n"),
    ] {
        std::fs::write(repo.join(file), text).unwrap();
    }
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(status.success());
    repo
}

/// A Claude edit copies with paths from the session's folder, though Claude names them
/// in full, and each copy applies, in order, to the files it changed.
#[gpui::test]
fn copying_a_claude_edit_gives_paths_from_the_folder_and_applies(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, _, cx) = claude_window(dir.path(), cx);
    send_text(&view, cx, "replay edits");
    wait_until(cx, &view, |v, cx| {
        let c = &v.state.read(cx).conversations()[&1];
        !c.running && edit_count(c) == 5
    });
    let repo = originals(dir.path());
    for (index, _) in edit_rows(&view, cx) {
        let copied = copy_edit(cx, index);
        paths_are_from_the_folder(&copied, &dir.path().join("repo"));
        assert!(git_apply(&repo, &["--check"], &copied), "{copied}");
        assert!(git_apply(&repo, &[], &copied), "{copied}");
    }
    let a = std::fs::read_to_string(repo.join("a.txt")).unwrap();
    assert_eq!(a, "one\nTWO\nthree\nfour\nfive\n");
}

/// A Codex change copies the same way, all three files in one patch.
#[gpui::test]
fn copying_a_codex_edit_gives_paths_from_the_folder_and_applies(cx: &mut gpui::TestAppContext) {
    let _env = codex_env();
    let dir = tempfile::tempdir().unwrap();
    let path = edit_workspace(dir.path(), Provider::Codex);
    let codex = wrapper(dir.path(), "keystone-fake-codex", "KEYSTONE_FAKE_CODEX_DELAY_MS=1");
    let (view, cx) = open_window(&path, cx);
    view.update_in(cx, |this, window, cx| {
        this.focus.focus(window, cx);
        this.state.update(cx, |state, _| {
            state.test_set_codex(Some(CodexConnection::new(
                Some(codex),
                "test",
                state.test_wake().clone(),
            )));
        });
    });
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    send_text(&view, cx, "replay edits");
    wait_until(cx, &view, |v, cx| {
        let c = &v.state.read(cx).conversations()[&1];
        !c.running && edit_count(c) == 1
    });
    let (index, _) = edit_rows(&view, cx)[0];
    let copied = copy_edit(cx, index);
    paths_are_from_the_folder(&copied, &dir.path().join("repo"));
    assert!(copied.starts_with("--- a/a.txt\n+++ b/a.txt\n"), "{copied}");
    let repo = originals(dir.path());
    assert!(git_apply(&repo, &["--check"], &copied), "{copied}");
}

/// An edit to a file with no final newline, as Claude Code's jsdiff writes it, copies
/// with the marker, and the copy applies to the file byte for byte.
#[gpui::test]
fn copying_an_edit_to_a_file_without_a_final_newline_applies(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, _, cx) = claude_window(dir.path(), cx);
    send_text(&view, cx, "edit without newline");
    wait_until(cx, &view, |v, cx| {
        let c = &v.state.read(cx).conversations()[&1];
        !c.running && edit_count(c) == 1
    });
    let (index, lines) = edit_rows(&view, cx)[0];
    assert_eq!(lines, 4, "the marker is not a line");
    let copied = copy_edit(cx, index);
    paths_are_from_the_folder(&copied, &dir.path().join("repo"));
    assert!(copied.ends_with(" three\n\\ No newline at end of file\n"), "{copied}");
    let repo = dir.path().join("apply");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "one\ntwo\nthree").unwrap();
    assert!(git_apply(&repo, &["--check"], &copied), "{copied}");
    assert!(git_apply(&repo, &[], &copied), "{copied}");
    assert_eq!(std::fs::read(repo.join("a.txt")).unwrap(), b"one\nTWO\nthree");
}

/// A capped edit copies with a first line saying only part of it was kept, and hunk
/// headers that count the lines it holds.
#[gpui::test]
fn copying_a_capped_edit_says_it_was_cut(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, index, cx) = long_edit_session(dir.path(), cx);
    let copied = copy_edit(cx, index);
    let mut lines = copied.lines();
    assert_eq!(
        lines.next(),
        Some("Keystone kept only the first 1000 lines of this edit, so this diff is partial and will not apply as it is.")
    );
    assert_eq!(lines.next(), Some("--- a/long.txt"));
    assert_eq!(lines.next(), Some("+++ b/long.txt"));
    assert_eq!(lines.next(), Some("@@ -1,333 +1,1000 @@"));
    assert_eq!(lines.count(), keystone_agent::diff::EDIT_LINES);
    drop(view);
}

fn edit_count(c: &crate::conversation::Conversation) -> usize {
    c.rows
        .iter()
        .filter(|row| matches!(row, Row::Item(Item { kind: ItemKind::FileChange { .. }, .. })))
        .count()
}

/// A session of 20 turns and, last, one edit of 1,500 lines (kept to the cap), with the
/// edit row's place.
fn long_edit_session<'a>(
    dir: &Path,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, usize, &'a mut gpui::VisualTestContext) {
    use keystone_agent::diff::{FileDiff, Hunk, Patch};
    use keystone_agent::log::{Entry, SessionLog};
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store.create(Provider::Codex, &working_directory(), 1).unwrap();
    seed_long_session(&dir.join("sessions"), 1, 20);
    {
        let mut log = SessionLog::open(&dir.join("sessions"), 1).unwrap();
        let lines: Vec<String> = (1..=1500)
            .map(|k| match k % 3 {
                0 => format!(" context line {k}"),
                _ => format!("+added line {k} {}", "x".repeat(40)),
            })
            .collect();
        let patch = Patch::edit(vec![FileDiff {
            path: "long.txt".into(),
            hunks: vec![Hunk::new(1, 1, lines).unwrap()],
            ..FileDiff::default()
        }]);
        let item = |id: &str, kind: ItemKind| Entry::Item {
            item: Item {
                id: id.into(),
                kind,
                text: String::new(),
                status: ItemStatus::Completed,
            },
            seq: None,
            run: None,
        };
        log.append(&Entry::TurnStarted).unwrap();
        let paths = vec!["long.txt".into()];
        log.append(&item("edit", ItemKind::FileChange { paths, patch: Some(patch) })).unwrap();
        log.append(&Entry::TurnEnded {
            status: TurnStatus::Completed,
            error: None,
            limit: None,
            seq: None,
            run: None,
        })
        .unwrap();
        log.sync().unwrap();
    }
    cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        Keystone::new(cx.new(|cx| crate::AppState::new(store, cx)), false, window, cx)
    });
    wait_until(cx, &view, |view, cx| {
        view.state.read(cx).conversations().get(&1).is_some_and(|c| !c.is_loading() && c.rows.len() > 60)
    });
    let index = edit_rows(&view, cx)[0].0;
    (view, index, cx)
}

/// Draw time of a transcript holding a 1,000-line edit, with the edit closed and open,
/// in view. A composer keystroke notifies the root view, which draws the same frame.
#[gpui::test]
#[ignore = "benchmark; run through scripts/perf.py"]
fn perf_edit_row_draw(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, index, cx) = long_edit_session(dir.path(), cx);
    let frames = 30;
    let draw = |cx: &mut gpui::VisualTestContext| {
        let mut total = std::time::Duration::ZERO;
        for _ in 0..frames {
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| {
                let start = std::time::Instant::now();
                window.draw(cx).clear(cx);
                total += start.elapsed();
            });
        }
        total.as_secs_f64() * 1000. / frames as f64
    };
    cx.run_until_parked();
    let closed = draw(cx);
    // Element bounds exist only in debug builds; a release run trusts the view state.
    let collapsed: &'static str = format!("activity-collapsed-{index}").leak();
    if cfg!(debug_assertions) {
        assert!(cx.debug_bounds(collapsed).is_some(), "the edit row is in view");
    }
    view.update_in(cx, |v, window, cx| v.toggle_activity(MAIN_1, index, window, cx));
    cx.run_until_parked();
    assert!(view.read_with(cx, |v, _| v.presentation.current.views.activity_expanded(MAIN_1, index, false)));
    let body: &'static str = format!("activity-body-{index}").leak();
    if cfg!(debug_assertions) {
        assert!(cx.debug_bounds(body).is_some(), "the edit is open and drawn");
    }
    let open = draw(cx);
    eprintln!("PERF edit_row_closed_ms={closed:.2}");
    eprintln!("PERF edit_row_open_ms={open:.2}");
}
