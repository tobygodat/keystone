//! A new worktree's setup script through the window: its card, the send it holds,
//! and Cancel, Retry and Skip, with a temporary repository and the scripted Claude.

use super::*;
use crate::windows::open;
use crate::{install_app_state, open_windows};
use gpui::{Entity, TestAppContext, VisualTestContext};
use keystone_agent::host;
use keystone_core::{Provider, Store, WindowId, WorkingDirectory};
use std::path::{Path, PathBuf};

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

fn fake_claude(dir: &Path) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("keystone-fake-claude");
    assert!(
        binary.exists(),
        "build keystone-fake-claude first: cargo build -p keystone-agent --bins"
    );
    let script = dir.join("claude-setup-wrapper");
    keystone_core::test_support::write_executable(
        &script,
        &format!(
            "#!/usr/bin/python3\nimport os, sys\nos.environ['KEYSTONE_FAKE_CLAUDE_DELAY_MS'] = '2'\nbinary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    );
    script
}

/// A repository with one commit, opened as the first window's project with `script`
/// as its setup script. An empty repository, `other`, is listed as a second project.
fn launch<'a>(
    script: Option<&str>,
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
    git(&repo, &["config", "user.name", "Setup test"]);
    git(&repo, &["config", "user.email", "setup@example.invalid"]);
    std::fs::write(repo.join("tracked"), "base\n").unwrap();
    git(&repo, &["add", "tracked"]);
    git(&repo, &["commit", "-m", "base"]);
    let project = ProjectId::new(&repo).unwrap();
    let other = repo.with_file_name("other");
    std::fs::create_dir(&other).unwrap();
    git(&other, &["init", "--initial-branch=main"]);
    let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
    store
        .create(Provider::Claude, &WorkingDirectory::new(&repo).unwrap(), 1)
        .unwrap();
    store
        .open_project(
            WindowId::FIRST,
            &ProjectId::new(&other).unwrap(),
            None,
            None,
            5,
        )
        .unwrap();
    store
        .open_project(WindowId::FIRST, &project, None, Some(1), 10)
        .unwrap();
    store
        .set_project_setup_script(&project, script.map(str::to_owned))
        .unwrap();
    let claude = fake_claude(dir.path());
    cx.executor().allow_parking();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state = install_app_state(store, cx);
        state.update(cx, |state, _| {
            state.test_programs = Some((PathBuf::from("/nonexistent-codex"), claude));
            state.setup_shell = "/bin/sh".into();
        });
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

/// Creates a worktree session named `name` from the "In worktree" pill's dialog and
/// returns its ID.
fn create_worktree(view: &Entity<Keystone>, name: &str, cx: &mut VisualTestContext) -> u64 {
    view.update_in(cx, |this, window, cx| this.open_new_worktree(window, cx));
    cx.simulate_input(name);
    cx.simulate_keystrokes("enter");
    wait(view, cx, "the worktree is created", |this, cx| {
        this.worktree_dialog.is_none() && this.selected(cx) != Some(1)
    });
    view.read_with(cx, |this, cx| this.selected(cx)).unwrap()
}

fn wait(
    view: &Entity<Keystone>,
    cx: &mut VisualTestContext,
    what: &str,
    done: impl Fn(&Keystone, &App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |this, cx| done(this, cx)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn state_is(
    view: &Entity<Keystone>,
    session: u64,
    cx: &mut VisualTestContext,
    what: &str,
    matches: impl Fn(&SetupState) -> bool,
) {
    wait(view, cx, what, |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .is_some_and(|setup| matches(&setup.state))
    });
}

fn output(view: &Entity<Keystone>, session: u64, cx: &mut VisualTestContext) -> Vec<String> {
    view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .map(|setup| setup.output.iter().cloned().collect())
            .unwrap_or_default()
    })
}

fn composer(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |this, cx| this.composer.read(cx).value().to_owned())
}

fn notice(view: &Entity<Keystone>, session: u64, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .conversations()
            .get(&session)
            .and_then(|c| c.error.clone())
    })
}

fn sent(view: &Entity<Keystone>, session: u64, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .conversations()
            .get(&session)
            .is_some_and(|c| {
                c.rows.iter().any(|row| {
                    matches!(row, crate::conversation::Row::Item(item)
                        if item.kind == keystone_agent::ItemKind::UserMessage)
                })
            })
    })
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is shown"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

fn type_and_send(view: &Entity<Keystone>, text: &str, cx: &mut VisualTestContext) {
    view.update_in(cx, |this, window, cx| this.focus_composer(window, cx));
    cx.simulate_input(text);
    cx.simulate_keystrokes("secondary-enter");
    cx.run_until_parked();
}

fn session_setup<T>(
    view: &Entity<Keystone>,
    session: u64,
    cx: &mut VisualTestContext,
    read: impl Fn(&Setup) -> T,
) -> T {
    view.read_with(cx, |this, cx| {
        read(this.state.read(cx).setup(session).unwrap())
    })
}

fn retry(view: &Entity<Keystone>, session: u64, cx: &mut VisualTestContext) {
    view.update(cx, |this, cx| {
        this.state
            .update(cx, |state, cx| state.retry_setup(session, cx))
    });
    cx.run_until_parked();
}

/// Whether a process of the group `pid` leads has not exited yet. A killed child the
/// shell left is reparented, and stays in the group as a zombie until its new parent
/// reaps it, which Keystone cannot hurry.
fn group_alive(pid: i32) -> bool {
    let leader_exited = matches!(host::process_state(pid as u32), None | Some('Z' | 'X'));
    !leader_exited || host::group_has_others(pid as u32)
}

#[gpui::test]
fn a_failing_script_holds_the_draft_until_skip(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(Some("echo nope >&2; exit 3"), cx);
    view.update_in(cx, |this, window, cx| this.open_new_worktree(window, cx));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("worktree-setup-script").is_some(),
        "the dialog says the script will run"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let session = create_worktree(&view, "failing", cx);
    state_is(&view, session, cx, "the script fails", |state| {
        matches!(state, SetupState::Failed(SetupExit::Code(3)))
    });
    assert_eq!(output(&view, session, cx), ["nope"]);
    assert!(cx.debug_bounds("setup").is_some(), "the card is shown");
    assert!(cx.debug_bounds("setup-output").is_some());
    assert!(cx.debug_bounds("setup-retry").is_some());
    assert!(cx.debug_bounds("setup-open").is_some());

    type_and_send(&view, "first message", cx);
    assert_eq!(composer(&view, cx), "first message", "the draft stays");
    assert_eq!(notice(&view, session, cx).as_deref(), Some(NOT_DONE));
    assert!(!sent(&view, session, cx));

    view.update_in(cx, |this, window, cx| this.focus_composer(window, cx));
    cx.simulate_keystrokes("shift-tab");
    cx.update(|window, cx| {
        assert!(
            view.read(cx).setup_focus.open.is_focused(window),
            "Shift+Tab from the composer reaches Open folder"
        )
    });
    cx.simulate_keystrokes("shift-tab");
    cx.update(|window, cx| {
        assert!(
            view.read(cx).setup_focus.skip.is_focused(window),
            "then Skip"
        )
    });
    cx.simulate_keystrokes("shift-tab");
    cx.update(|window, cx| {
        assert!(
            view.read(cx).setup_focus.retry.is_focused(window),
            "then Retry"
        )
    });

    click(cx, "setup-skip");
    assert!(cx.debug_bounds("setup").is_none(), "Skip clears the card");
    assert_eq!(notice(&view, session, cx), None);
    view.update_in(cx, |this, window, cx| this.focus_composer(window, cx));
    cx.simulate_keystrokes("secondary-enter");
    cx.run_until_parked();
    assert!(sent(&view, session, cx), "{:?}", notice(&view, session, cx));
    assert_eq!(composer(&view, cx), "");
}

#[gpui::test]
fn a_passing_script_reports_its_time_and_lets_send_go(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(
        Some(r#"echo installing; test "$PWD" = "$KEYSTONE_WORKTREE_PATH" && echo done"#),
        cx,
    );
    let session = create_worktree(&view, "passing", cx);
    state_is(&view, session, cx, "the script succeeds", |state| {
        matches!(state, SetupState::Succeeded { .. })
    });
    assert_eq!(output(&view, session, cx), ["installing", "done"]);
    assert!(
        cx.debug_bounds("setup").is_some(),
        "the finished line shows"
    );
    assert!(cx.debug_bounds("setup-retry").is_none());

    type_and_send(&view, "go", cx);
    assert!(sent(&view, session, cx), "{:?}", notice(&view, session, cx));
    assert!(
        view.read_with(cx, |this, cx| this.state.read(cx).setup(session).is_none()),
        "the first message forgets the setup"
    );
    assert!(cx.debug_bounds("setup").is_none());
}

#[gpui::test]
fn cancel_ends_a_running_script_and_retry_runs_it_again(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(Some("echo waiting; sleep 30"), cx);
    let session = create_worktree(&view, "slow", cx);
    wait(&view, cx, "the script prints", |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .is_some_and(|s| s.running() && !s.output.is_empty())
    });
    assert!(cx.debug_bounds("setup-cancel").is_some());
    type_and_send(&view, "too soon", cx);
    assert_eq!(notice(&view, session, cx).as_deref(), Some(STILL_RUNNING));
    assert_eq!(composer(&view, cx), "too soon");

    let cancelled = Instant::now();
    click(cx, "setup-cancel");
    state_is(&view, session, cx, "the script is cancelled", |state| {
        matches!(state, SetupState::Cancelled)
    });
    assert!(cancelled.elapsed() < Duration::from_secs(5));
    assert_eq!(notice(&view, session, cx), None, "the hold's notice clears");

    click(cx, "setup-retry");
    wait(&view, cx, "the retry prints", |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .is_some_and(|s| s.running() && s.output.len() == 1)
    });
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("cancel setup script");
    cx.simulate_keystrokes("enter");
    state_is(&view, session, cx, "the palette cancels it", |state| {
        matches!(state, SetupState::Cancelled)
    });
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("skip setup script");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(view.read_with(cx, |this, cx| this.state.read(cx).setup(session).is_none()));
}

#[gpui::test]
fn quitting_stops_a_running_script_and_its_children(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(Some("echo started; sleep 30 & sleep 30; wait"), cx);
    let session = create_worktree(&view, "quit", cx);
    wait(&view, cx, "the script prints", |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .is_some_and(|s| !s.output.is_empty())
    });
    let pid = view
        .read_with(cx, |this, cx| {
            this.state
                .read(cx)
                .setup(session)
                .and_then(Setup::run)
                .map(SetupRun::pid)
        })
        .unwrap();
    assert!(group_alive(pid));
    let stopped = Instant::now();
    view.update(cx, |this, cx| {
        this.state.update(cx, |state, cx| state.teardown(cx))
    });
    assert!(stopped.elapsed() < Duration::from_secs(3));
    let deadline = Instant::now() + Duration::from_secs(3);
    while group_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!group_alive(pid), "no process of the script is left");
}

#[gpui::test]
fn without_a_script_a_worktree_session_sends_at_once(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(None, cx);
    view.update_in(cx, |this, window, cx| this.open_new_worktree(window, cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("worktree-setup-script").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let session = create_worktree(&view, "plain", cx);
    assert!(view.read_with(cx, |this, cx| this.state.read(cx).setup(session).is_none()));
    assert!(cx.debug_bounds("setup").is_none());
    type_and_send(&view, "hello", cx);
    assert!(sent(&view, session, cx), "{:?}", notice(&view, session, cx));
}

#[gpui::test]
fn the_project_setting_saves_from_settings_and_the_palette_opens_it(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(None, cx);
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("edit worktree setup script");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(cx.debug_bounds("settings-project").is_some());
    let save = cx.debug_bounds("settings-setup-script-save").unwrap();
    cx.simulate_input("make setup");
    cx.simulate_click(save.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("settings-setup-script-error").is_none());
    let project = view.read_with(cx, |this, _| this.presentation.project.clone().unwrap());
    assert_eq!(
        view.read_with(cx, |this, cx| this
            .state
            .read(cx)
            .store()
            .workspace()
            .setup_script(&project)
            .map(str::to_owned)),
        Some("make setup".into())
    );
}

#[gpui::test]
fn quitting_kills_a_script_that_ignores_term_before_it_returns(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(Some("echo started; trap '' TERM; sleep 30"), cx);
    let session = create_worktree(&view, "stubborn", cx);
    wait(&view, cx, "the script prints", |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .is_some_and(|s| !s.output.is_empty())
    });
    let pid = session_setup(&view, session, cx, |s| s.run().map(SetupRun::pid)).unwrap();
    assert!(group_alive(pid));
    let stopped = Instant::now();
    view.update(cx, |this, cx| {
        this.state.update(cx, |state, cx| state.teardown(cx))
    });
    assert!(
        !group_alive(pid),
        "no process of the script is left once quitting returns"
    );
    assert!(
        stopped.elapsed() < Duration::from_secs(3),
        "{:?}",
        stopped.elapsed()
    );
}

#[gpui::test]
fn a_cancelled_script_that_exits_zero_is_still_cancelled(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(
        Some("trap 'exit 0' TERM; echo waiting; while :; do sleep 1; done"),
        cx,
    );
    let session = create_worktree(&view, "trapping", cx);
    wait(&view, cx, "the script prints", |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .is_some_and(|s| s.running() && !s.output.is_empty())
    });
    click(cx, "setup-cancel");
    state_is(&view, session, cx, "the script ends", |state| {
        !matches!(state, SetupState::Running(_) | SetupState::Stopping(_))
    });
    assert!(session_setup(&view, session, cx, |s| matches!(
        s.state,
        SetupState::Cancelled
    )));
    type_and_send(&view, "held", cx);
    assert_eq!(notice(&view, session, cx).as_deref(), Some(NOT_DONE));
    assert!(!sent(&view, session, cx));
}

#[gpui::test]
fn the_hold_notice_stays_while_the_script_prints_and_ticks(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(Some("echo waiting; sleep 1.5; echo more; sleep 30"), cx);
    let session = create_worktree(&view, "chatty", cx);
    wait(&view, cx, "the script prints", |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .is_some_and(|s| s.running() && !s.output.is_empty())
    });
    type_and_send(&view, "too soon", cx);
    assert_eq!(notice(&view, session, cx).as_deref(), Some(STILL_RUNNING));
    wait(&view, cx, "the script prints again", |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .is_some_and(|s| s.output.len() == 2)
    });
    assert_eq!(notice(&view, session, cx).as_deref(), Some(STILL_RUNNING));
}

#[gpui::test]
fn retry_refuses_a_worktree_being_removed_or_gone(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(Some("exit 3"), cx);
    let session = create_worktree(&view, "doomed", cx);
    state_is(&view, session, cx, "the script fails", |state| {
        matches!(state, SetupState::Failed(_))
    });
    let worktree = session_setup(&view, session, cx, |s| s.worktree.clone());
    let hold = view.read_with(cx, |this, cx| this.state.read(cx).hold_worktree(&worktree));
    click(cx, "setup-retry");
    assert!(session_setup(&view, session, cx, |s| matches!(
        &s.state,
        SetupState::CouldNotStart(reason) if reason == BEING_REMOVED
    )));
    drop(hold);
    std::fs::remove_dir_all(worktree.directory.path()).unwrap();
    retry(&view, session, cx);
    assert!(session_setup(&view, session, cx, |s| matches!(
        &s.state,
        SetupState::CouldNotStart(reason) if reason == NOT_INTACT
    )));
}

#[gpui::test]
fn retry_leaves_a_script_that_succeeded(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(Some("echo once"), cx);
    let session = create_worktree(&view, "done", cx);
    state_is(&view, session, cx, "the script succeeds", |state| {
        matches!(state, SetupState::Succeeded { .. })
    });
    let started = session_setup(&view, session, cx, |s| s.started);
    retry(&view, session, cx);
    assert!(session_setup(&view, session, cx, |s| s.started == started
        && matches!(s.state, SetupState::Succeeded { .. })));
}

#[gpui::test]
fn typing_after_a_click_on_the_card_reaches_the_composer(cx: &mut TestAppContext) {
    let (_dir, view, cx) = launch(Some("sleep 300"), cx);
    let session = create_worktree(&view, "typing", cx);
    let cancelled = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        state_is(view, session, cx, "the script is cancelled", |state| {
            matches!(state, SetupState::Cancelled)
        });
    };
    click(cx, "setup-cancel");
    cx.simulate_input("a");
    assert_eq!(composer(&view, cx), "a", "after Cancel");
    cancelled(&view, cx);
    click(cx, "setup-retry");
    cx.simulate_input("b");
    assert_eq!(composer(&view, cx), "ab", "after Retry");
    click(cx, "setup-cancel");
    cancelled(&view, cx);
    click(cx, "setup-skip");
    cx.simulate_input("c");
    assert_eq!(composer(&view, cx), "abc", "after Skip");
}

#[gpui::test]
fn a_script_whose_save_fails_stays_shown_and_saves_on_the_next_try(cx: &mut TestAppContext) {
    let (dir, view, cx) = launch(None, cx);
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("edit worktree setup script");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.simulate_input("make setup");
    cx.run_until_parked();
    let database = dir.path().join("workspace.sqlite3");
    let writer = crate::tests::lock_database(&database);
    let save = cx.debug_bounds("settings-setup-script-save").unwrap();
    cx.simulate_click(save.center(), gpui::Modifiers::none());
    let state = view.read_with(cx, |this, _| this.state.clone());
    crate::tests::settle(&state, cx);
    cx.run_until_parked();
    let status = view.read_with(cx, |this, cx| this.status_line(cx).map(|(line, _)| line));
    assert!(
        status
            .as_deref()
            .is_some_and(|s| s.starts_with("Not saved.")),
        "{status:?}"
    );
    assert!(
        cx.debug_bounds("settings-setup-script-error").is_none(),
        "the store has the script, and the status line says it is not on disk"
    );
    let project = view.read_with(cx, |this, _| this.presentation.project.clone().unwrap());
    let shown = view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .store()
            .workspace()
            .setup_script(&project)
            .map(str::to_owned)
    });
    assert_eq!(shown.as_deref(), Some("make setup"));
    drop(writer);
    crate::tests::settle(&state, cx);
    cx.run_until_parked();
    assert!(view.read_with(cx, |this, cx| this.status_line(cx).is_none()));
    let saved = Store::open(dir.path().join("workspace.json")).unwrap();
    assert_eq!(saved.workspace().setup_script(&project), Some("make setup"));
}

#[gpui::test]
fn a_quit_refused_by_a_failed_save_leaves_the_script_running_and_the_next_stops_it(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = launch(Some("sleep 300 & wait"), cx);
    let session = create_worktree(&view, "quitting", cx);
    let pid = view
        .read_with(cx, |this, cx| {
            this.state
                .read(cx)
                .setup(session)
                .and_then(Setup::run)
                .map(SetupRun::pid)
        })
        .unwrap();
    let writer = crate::tests::lock_database(&dir.path().join("workspace.sqlite3"));
    view.update(cx, |this, cx| {
        this.state.update(cx, |state, cx| {
            state.test_conversation(session).push_local("unsaved");
            state.commit(session);
            state.publish(cx);
        })
    });
    cx.update(|_, cx| crate::request_quit(cx, true));
    cx.run_until_parked();
    let phase = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        view.read_with(cx, |this, cx| this.state.read(cx).phase)
    };
    assert_eq!(phase(&view, cx), Phase::Running, "the save failed");
    assert!(group_alive(pid), "a refused quit stops nothing");
    assert!(view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .setup(session)
            .is_some_and(Setup::running)
    }));
    drop(writer);
    cx.update(|_, cx| crate::request_quit(cx, true));
    cx.run_until_parked();
    assert_eq!(phase(&view, cx), Phase::Exiting);
    assert!(
        !group_alive(pid),
        "Quit returns with no process of the script left"
    );
}

#[gpui::test]
fn saving_after_the_window_changed_project_reloads_instead(cx: &mut TestAppContext) {
    let (dir, view, cx) = launch(None, cx);
    let other = ProjectId::new(&dir.path().canonicalize().unwrap().join("other")).unwrap();
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("edit worktree setup script");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.simulate_input("make setup");
    cx.run_until_parked();
    let first = view.read_with(cx, |this, _| this.presentation.project.clone().unwrap());
    view.update(cx, |this, _| this.presentation.project = Ok(other.clone()));
    let save = cx.debug_bounds("settings-setup-script-save").unwrap();
    cx.simulate_click(save.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let script = |project: &ProjectId, cx: &mut VisualTestContext| {
        view.read_with(cx, |this, cx| {
            this.state
                .read(cx)
                .store()
                .workspace()
                .setup_script(project)
                .map(str::to_owned)
        })
    };
    assert!(
        cx.debug_bounds("settings-setup-script-error").is_none(),
        "the save neither ran nor failed"
    );
    assert_eq!(script(&other, cx), None, "nothing lands on the new project");
    assert_eq!(script(&first, cx), None);
    click(cx, "settings-setup-script");
    cx.simulate_input("make other");
    let save = cx.debug_bounds("settings-setup-script-save").unwrap();
    cx.simulate_click(save.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        script(&other, cx).as_deref(),
        Some("make other"),
        "the reloaded field saves to the project it now shows"
    );
    assert_eq!(script(&first, cx), None);
}
