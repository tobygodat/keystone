use super::*;
use crate::windows::open;
use crate::{install_app_state, open_windows};
use gpui::{Entity, TestAppContext, VisualTestContext};
use keystone_core::{ProjectId, Store, WindowId, WorkingDirectory};
use std::path::Path;

fn git(directory: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

fn launch(
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, Entity<Keystone>, &mut VisualTestContext) {
    launch_in(".", cx)
}

/// A repository with one commit whose folder `within` is opened as the first window's
/// project, with one ordinary session there.
fn launch_in<'a>(
    within: &str,
    cx: &'a mut TestAppContext,
) -> (
    tempfile::TempDir,
    Entity<Keystone>,
    &'a mut VisualTestContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--initial-branch=main"]);
    let config = repo.join(".git/config");
    let mut settings = std::fs::read_to_string(&config).unwrap();
    settings.push_str("\n[user]\n\tname = Worktree test\n\temail = worktree@example.invalid\n");
    std::fs::write(config, settings).unwrap();
    std::fs::write(repo.join("tracked"), "base\n").unwrap();
    git(&repo, &["add", "tracked"]);
    git(&repo, &["commit", "-m", "base"]);
    let repo = repo.join(within);
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
    store
        .create(Provider::Codex, &WorkingDirectory::new(&repo).unwrap(), 1)
        .unwrap();
    store
        .open_project(
            WindowId::FIRST,
            &ProjectId::new(&repo).unwrap(),
            None,
            Some(1),
            10,
        )
        .unwrap();
    cx.executor().allow_parking();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state = install_app_state(store, cx);
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            open(state.clone(), id, placement, true, false, cx).unwrap();
        }
    });
    cx.run_until_parked();
    let view = cx.update(|cx| open_windows(cx))[0].clone();
    let handle = cx.read(|cx| view.read(cx).handle);
    let vcx = VisualTestContext::from_window(handle, cx).into_mut();
    vcx.simulate_resize(gpui::size(px(1280.), px(800.)));
    view.update_in(vcx, |this, window, cx| this.focus.focus(window, cx));
    vcx.run_until_parked();
    (dir, view, vcx)
}

/// Runs git's background work to completion: the dialog's task finishes off the UI
/// thread.
fn settle(view: &Entity<Keystone>, cx: &mut VisualTestContext) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if !view.read_with(cx, |this, _| {
            this.worktree_dialog
                .as_ref()
                .is_some_and(WorktreeDialog::busy)
        }) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "git did not finish");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn dialog_error(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |this, _| match &this.worktree_dialog {
        Some(WorktreeDialog::Create { error, .. } | WorktreeDialog::Remove { error, .. }) => {
            error.clone()
        }
        None => None,
    })
}

fn worktree_of(
    view: &Entity<Keystone>,
    id: u64,
    cx: &mut VisualTestContext,
) -> Option<WorktreeContext> {
    view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .session(id)
            .and_then(|s| s.worktree().cloned())
    })
}

#[gpui::test]
fn a_worktree_session_is_created_shown_refused_while_dirty_and_removed_when_clean(
    cx: &mut TestAppContext,
) {
    let (_dir, view, cx) = launch(cx);
    let repo = view.read_with(cx, |this, _| this.presentation.project.clone().unwrap());
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("session in worktree");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("worktree-dialog").is_some(),
        "the palette opens the dialog"
    );
    assert_eq!(
        view.read_with(cx, |this, cx| this
            .worktree_input
            .read(cx)
            .value()
            .to_owned()),
        "session-1"
    );
    cx.simulate_input("feature");
    cx.simulate_keystrokes("enter");
    settle(&view, cx);

    assert!(
        cx.debug_bounds("worktree-dialog").is_none(),
        "{:?}",
        dialog_error(&view, cx)
    );
    let worktree = worktree_of(&view, 2, cx).expect("session 2 works in the worktree");
    assert_eq!(view.read_with(cx, |this, cx| this.selected(cx)), Some(2));
    assert_eq!(worktree.branch, "keystone/feature");
    assert_eq!(
        worktree.directory.path(),
        repo.path().join(".keystone/worktrees/feature")
    );
    assert!(worktree.directory.path().join("tracked").is_file());
    let exclude = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
    assert!(exclude.lines().any(|line| line == "/.keystone/worktrees/"));
    assert!(!repo.path().join(".gitignore").exists());
    assert!(
        cx.debug_bounds("session-branch-2").is_some(),
        "the row shows the branch"
    );
    assert!(cx.debug_bounds("session-branch-1").is_none());
    assert!(
        cx.debug_bounds("worktree-branch").is_some(),
        "the top bar shows it"
    );
    assert!(cx.debug_bounds("worktree-missing").is_none());

    std::fs::write(
        worktree.directory.path().join("notes"),
        "work in progress\n",
    )
    .unwrap();
    cx.simulate_keystrokes("secondary-alt-shift-g");
    cx.run_until_parked();
    let confirm = cx.debug_bounds("remove-worktree-dialog-confirm").unwrap();
    cx.simulate_click(confirm.center(), gpui::Modifiers::none());
    settle(&view, cx);
    let refused = dialog_error(&view, cx).expect("a dirty worktree is refused");
    assert!(refused.contains("Uncommitted"), "{refused}");
    assert!(worktree.directory.path().join("notes").is_file());

    std::fs::remove_file(worktree.directory.path().join("notes")).unwrap();
    let confirm = cx.debug_bounds("remove-worktree-dialog-confirm").unwrap();
    cx.simulate_click(confirm.center(), gpui::Modifiers::none());
    settle(&view, cx);
    assert!(
        view.read_with(cx, |this, _| this.worktree_dialog.is_none()),
        "{:?}",
        dialog_error(&view, cx)
    );
    assert!(!worktree.directory.path().exists());
    git(
        repo.path(),
        &["rev-parse", "--verify", "refs/heads/keystone/feature"],
    );
    assert_eq!(
        worktree_of(&view, 2, cx),
        Some(worktree.clone()),
        "the session keeps its record"
    );
    assert!(
        cx.debug_bounds("worktree-missing").is_some(),
        "the top bar says it is gone"
    );

    let sent = view.update(cx, |this, cx| {
        this.state
            .update(cx, |state, cx| state.dispatch_directory(2, cx))
    });
    assert!(sent.is_none());
    let error = view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .conversations()
            .get(&2)
            .and_then(|c| c.error.clone())
    });
    assert!(error.unwrap().contains("worktree folder is missing"));
}

#[gpui::test]
fn a_taken_name_keeps_the_dialog_open_with_the_reason(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(cx);
    for _ in 0..2 {
        view.update_in(cx, |this, window, cx| this.open_new_worktree(window, cx));
        cx.simulate_input("same");
        cx.simulate_keystrokes("enter");
        settle(&view, cx);
    }
    let refused = dialog_error(&view, cx).expect("the second create is refused");
    assert!(refused.contains("already exists"), "{refused}");
    assert!(cx.debug_bounds("worktree-dialog").is_some());
    assert_eq!(
        view.read_with(cx, |this, cx| this
            .state
            .read(cx)
            .store()
            .workspace()
            .sessions()
            .len()),
        2
    );
}

#[gpui::test]
fn the_sidebar_menu_offers_a_worktree_session_on_either_provider(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(cx);
    let bounds = cx.debug_bounds("new-session").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let bounds = cx.debug_bounds("new-worktree").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(view.read_with(cx, |this, _| matches!(
        this.worktree_dialog,
        Some(WorktreeDialog::Create { .. })
    )));
    let bounds = cx.debug_bounds("worktree-codex").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let confirm = cx.debug_bounds("worktree-dialog-confirm").unwrap();
    cx.simulate_click(confirm.center(), gpui::Modifiers::none());
    settle(&view, cx);
    let provider = view.read_with(cx, |this, cx| this.state.read(cx).provider(2));
    assert_eq!(provider, Some(Provider::Codex));
    assert!(worktree_of(&view, 2, cx).is_some());
}

#[gpui::test]
fn the_dialog_stays_while_git_runs(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(cx);
    view.update_in(cx, |this, window, cx| {
        this.open_new_worktree(window, cx);
        this.confirm_worktree(window, cx);
        assert!(this.worktree_dialog.as_ref().unwrap().busy());
        this.close_worktree_dialog(window, cx);
        assert!(
            this.worktree_dialog.is_some(),
            "Cancel cannot drop a create that git is running"
        );
    });
    settle(&view, cx);
    assert!(
        worktree_of(&view, 2, cx).is_some(),
        "the create still lands"
    );
}

#[gpui::test]
fn a_worktree_made_from_a_subfolder_project_opens_the_repository(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch_in("crates/x", cx);
    view.update_in(cx, |this, window, cx| this.open_new_worktree(window, cx));
    cx.simulate_input("from-sub");
    cx.simulate_keystrokes("enter");
    settle(&view, cx);
    let worktree = worktree_of(&view, 2, cx).expect("session 2 works in the worktree");
    let project = view.read_with(cx, |this, _| this.presentation.project.clone().unwrap());
    assert_eq!(project.path(), worktree.repository.path());
    assert_eq!(view.read_with(cx, |this, cx| this.selected(cx)), Some(2));
    assert!(
        cx.debug_bounds("session-branch-2").is_some(),
        "its row is listed"
    );
}

#[gpui::test]
fn a_removed_worktrees_name_is_not_suggested_again(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(cx);
    view.update_in(cx, |this, window, cx| this.open_new_worktree(window, cx));
    cx.simulate_keystrokes("enter");
    settle(&view, cx);
    let worktree = worktree_of(&view, 2, cx).unwrap();
    assert_eq!(worktree.branch, "keystone/session-1");
    view.update_in(cx, |this, window, cx| {
        this.open_remove_worktree(2, window, cx);
        this.confirm_worktree(window, cx);
    });
    settle(&view, cx);
    assert!(!worktree.directory.path().exists());
    view.update_in(cx, |this, window, cx| this.open_new_worktree(window, cx));
    assert_eq!(
        view.read_with(cx, |this, cx| this
            .worktree_input
            .read(cx)
            .value()
            .to_owned()),
        "session-2"
    );
}

#[cfg(unix)]
#[gpui::test]
fn a_worktree_folder_swapped_for_another_shows_missing_and_does_not_send(cx: &mut TestAppContext) {
    let (dir, view, cx) = launch(cx);
    view.update_in(cx, |this, window, cx| this.open_new_worktree(window, cx));
    cx.simulate_input("swapped");
    cx.simulate_keystrokes("enter");
    settle(&view, cx);
    let worktree = worktree_of(&view, 2, cx).unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    git(&elsewhere, &["init"]);
    git(
        worktree.repository.path(),
        &[
            "worktree",
            "remove",
            worktree.directory.path().to_str().unwrap(),
        ],
    );
    std::os::unix::fs::symlink(&elsewhere, worktree.directory.path()).unwrap();
    view.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert!(cx.debug_bounds("worktree-missing").is_some());
    let sent = view.update(cx, |this, cx| {
        this.state
            .update(cx, |state, cx| state.dispatch_directory(2, cx))
    });
    assert!(sent.is_none(), "nothing starts in the other repository");
}

#[gpui::test]
fn only_one_removal_holds_a_worktree_at_a_time(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(cx);
    view.update_in(cx, |this, window, cx| this.open_new_worktree(window, cx));
    cx.simulate_keystrokes("enter");
    settle(&view, cx);
    let worktree = worktree_of(&view, 2, cx).unwrap();
    view.update(cx, |this, cx| {
        let state = this.state.read(cx);
        let first = state
            .hold_worktree(&worktree)
            .expect("the first removal holds it");
        assert!(
            state.hold_worktree(&worktree).is_none(),
            "a second must wait"
        );
        drop(first);
        assert!(state.hold_worktree(&worktree).is_some(), "released on drop");
    });
    let inside = worktree.directory.path().join("inside");
    std::fs::create_dir(&inside).unwrap();
    let inside = WorkingDirectory::new(&inside).unwrap();
    let sent = view.update(cx, |this, cx| {
        this.state.update(cx, |state, cx| {
            let session = state
                .create(Provider::Claude, &inside, 1, cx)
                .unwrap()
                .value;
            let _hold = state.hold_worktree(&worktree).unwrap();
            state.dispatch_directory(session, cx)
        })
    });
    assert!(
        sent.is_none(),
        "a subfolder session waits for the removal too"
    );
}
