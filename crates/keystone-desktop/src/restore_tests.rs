// Included into main.rs's test module, whose helpers these tests use.

use crate::app_state::checkpoints::{State, UNAVAILABLE};

/// A window on one Codex session in `folder`, with checkpoints on.
fn open_in<'a>(
    dir: &Path,
    folder: &Path,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(
            Provider::Codex,
            &keystone_core::WorkingDirectory::new(folder).unwrap(),
            1,
        )
        .unwrap();
    drop(store);
    let home = dir.join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    let codex = wrapper(
        dir,
        "keystone-fake-codex",
        &format!(
            "KEYSTONE_FAKE_CODEX_DELAY_MS=1 CODEX_HOME='{}'",
            home.display()
        ),
    );
    let (view, cx) = open_window(&path, cx);
    view.update_in(cx, |this, window, cx| {
        this.focus.focus(window, cx);
        this.state.update(cx, |state, _| {
            state.test_checkpoints = true;
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
    (view, cx)
}

fn git_in(folder: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
}

/// A repository whose checkpoint holds `out/kept.txt`, where an ignored file now
/// stands, so a restore leaves that path and says so.
fn repository(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git_in(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "start\n").unwrap();
    std::fs::create_dir(repo.join("out")).unwrap();
    std::fs::write(repo.join("out/kept.txt"), "at the checkpoint\n").unwrap();
    git_in(&repo, &["add", "--all"]);
    git_in(
        &repo,
        &["-c", "user.name=T", "-c", "user.email=t@e", "commit", "-q", "-m", "start"],
    );
    repo
}

/// Turn one with its checkpoint saved, then later edits to the files.
fn turn_then_edits(view: &Entity<Keystone>, repo: &Path, cx: &mut gpui::VisualTestContext) {
    send_text(view, cx, "first");
    wait_until(cx, view, |v, cx| {
        let state = v.state.read(cx);
        !state.conversations()[&1].running
            && matches!(state.checkpoints(1), Some(State::Saved(saved)) if saved.turns.len() == 1)
    });
    std::fs::write(repo.join("a.txt"), "later\n").unwrap();
    std::fs::write(repo.join("b.txt"), "later\n").unwrap();
    std::fs::remove_dir_all(repo.join("out")).unwrap();
    std::fs::write(repo.join(".gitignore"), "out\n").unwrap();
    std::fs::write(repo.join("out"), "ignored now\n").unwrap();
}

fn files(repo: &Path) -> Vec<(String, Option<String>)> {
    ["a.txt", "b.txt", "out", ".gitignore"]
        .map(|name| (name.to_owned(), std::fs::read_to_string(repo.join(name)).ok()))
        .to_vec()
}

fn confirm_restore_of_turn_one(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) {
    let row = view.read_with(cx, |v, cx| {
        v.state.read(cx).conversations()[&1]
            .checkpoint_row(1)
            .unwrap()
    });
    view.update_in(cx, |v, window, cx| v.open_restore(MAIN_1, row, window, cx));
    cx.run_until_parked();
    let confirm = cx.debug_bounds("restore-dialog-confirm").unwrap();
    cx.simulate_click(confirm.center(), gpui::Modifiers::none());
}

fn sessions(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> usize {
    view.read_with(cx, |v, cx| {
        v.state.read(cx).store().workspace().sessions().len()
    })
}

#[gpui::test]
fn restore_to_here_keeps_the_files_once_the_fork_opens_and_says_what_it_left_there(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = repository(dir.path());
    let (view, cx) = open_in(dir.path(), &repo, cx);
    turn_then_edits(&view, &repo, cx);

    confirm_restore_of_turn_one(&view, cx);
    wait_until(cx, &view, |v, _| v.restore_dialog.is_none());

    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "start\n");
    assert!(!repo.join("b.txt").exists());
    assert_eq!(sessions(&view, cx), 2);
    view.read_with(cx, |v, cx| {
        let fork = v.selected(cx).unwrap();
        assert_ne!(fork, 1);
        let conversations = v.state.read(cx).conversations();
        assert_eq!(
            conversations[&fork].error.as_deref(),
            Some("Not restored: out/kept.txt.")
        );
        assert_eq!(conversations[&1].error, None);
    });
}

#[gpui::test]
fn a_restore_whose_fork_cannot_open_puts_every_file_back_and_says_why(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = repository(dir.path());
    let (view, cx) = open_in(dir.path(), &repo, cx);
    turn_then_edits(&view, &repo, cx);
    let before = files(&repo);
    view.update(cx, |v, cx| {
        v.state
            .update(cx, |state, _| state.phase = crate::app_state::Phase::Exiting)
    });

    confirm_restore_of_turn_one(&view, cx);
    wait_until(cx, &view, |v, _| {
        v.restore_dialog.as_ref().is_some_and(|d| !d.busy())
    });

    assert_eq!(files(&repo), before);
    assert!(cx.debug_bounds("restore-error").is_some());
    let refs = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["for-each-ref", "--format=%(refname)", "refs/keystone"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&refs.stdout).contains("/before-restore"),
        "the restore ran"
    );
    assert_eq!(sessions(&view, cx), 1, "no fork was made");
    view.read_with(cx, |v, cx| {
        assert_eq!(
            v.state.read(cx).conversations()[&1].error,
            None,
            "the source is not told what a fork would have been"
        );
    });
}

#[gpui::test]
fn cancel_stops_a_restore_stuck_in_a_filter_and_changes_nothing(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = repository(dir.path());
    let (view, cx) = open_in(dir.path(), &repo, cx);
    turn_then_edits(&view, &repo, cx);
    let pid = repo.join(".git/filter-pid");
    git_in(
        &repo,
        &[
            "config",
            "filter.slow.smudge",
            &format!("echo $$ > '{}'; sleep 30; cat", pid.display()),
        ],
    );
    std::fs::write(repo.join(".git/info/attributes"), "*.txt filter=slow\n").unwrap();
    let before = files(&repo);

    confirm_restore_of_turn_one(&view, cx);
    wait_until(cx, &view, |_, _| {
        std::fs::read_to_string(&pid).is_ok_and(|pid| pid.trim().parse::<u32>().is_ok())
    });
    assert!(cx.debug_bounds("restore-busy").is_some());
    cx.simulate_click(gpui::point(gpui::px(4.), gpui::px(4.)), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(
        view.read_with(cx, |v, _| v.restore_dialog.as_ref().is_some_and(|d| d.busy())),
        "a stray press outside the dialog does not stop the restore"
    );
    let cancel = cx.debug_bounds("restore-dialog-cancel").unwrap();
    cx.simulate_click(cancel.center(), gpui::Modifiers::none());
    wait_until(cx, &view, |v, _| v.restore_dialog.is_none());

    assert_eq!(files(&repo), before);
    assert_eq!(sessions(&view, cx), 1);
    view.read_with(cx, |v, cx| assert!(!v.state.read(cx).restoring(1)));
}

#[gpui::test]
fn outside_git_the_restore_control_says_why_only_in_its_tooltip(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("plain");
    std::fs::create_dir(&folder).unwrap();
    let (view, cx) = open_in(dir.path(), &folder, cx);
    send_text(&view, cx, "first");
    wait_until(cx, &view, |v, cx| {
        let state = v.state.read(cx);
        !state.conversations()[&1].running
            && state.checkpoints(1) == Some(&State::Unavailable)
            && state.conversations()[&1].latest_checkpoint().is_some()
    });
    let row = view.read_with(cx, |v, cx| {
        let conversation = &v.state.read(cx).conversations()[&1];
        conversation
            .checkpoint_row(conversation.latest_checkpoint().unwrap())
            .unwrap()
    });
    let control = cx
        .debug_bounds(Box::leak(format!("restore-{row}").into_boxed_str()))
        .expect("the control shows outside git");

    cx.simulate_click(control.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    view.read_with(cx, |v, cx| {
        assert!(v.restore_dialog.is_none());
        assert_eq!(v.state.read(cx).conversations()[&1].error, None);
        assert_eq!(v.error, None);
    });
    view.update_in(cx, |v, window, cx| v.open_restore(MAIN_1, row, window, cx));
    view.read_with(cx, |v, cx| {
        assert_eq!(v.error.as_deref(), Some(UNAVAILABLE), "the palette says why");
        assert_eq!(v.state.read(cx).conversations()[&1].error, None);
    });
}
