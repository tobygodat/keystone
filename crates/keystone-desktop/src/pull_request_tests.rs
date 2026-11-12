use super::*;
use crate::windows::open;
use crate::{AppState, install_app_state, open_windows, request_quit};
use gpui::{AnyWindowHandle, Entity, TestAppContext, VisualTestContext};
use keystone_core::{Provider, Store, WorkingDirectory};
use std::fs;
use std::path::Path;
use std::process::Command;

/// Puts a stand-in `gh` first on `PATH`, once for the whole process. It runs the
/// repository's own `.git/fake-gh`, so each test scripts its own, and otherwise acts
/// signed out. It never reaches GitHub or the user's gh login.
pub(crate) fn fake_gh() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("keystone-fake-gh-pr-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        keystone_core::test_support::write_executable(
            &gh,
            "#!/bin/sh\n\
             [ -x \"$PWD/.git/fake-gh\" ] && exec \"$PWD/.git/fake-gh\" \"$@\"\n\
             echo 'To get started with GitHub CLI, please run:  gh auth login' >&2\n\
             exit 4\n",
        );
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path =
            std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&path))).unwrap();
        // SAFETY: as for the scripted providers; every test only adds to PATH.
        unsafe { std::env::set_var("PATH", path) };
    });
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

/// A gh for `repo` that records each call, one argument per line and `---` after
/// each, and answers from the stage named in `.git/stage`: `view-<stage>.json` and
/// `comments-<stage>.json`. A merge records itself and moves the stage to `merged`.
fn script_gh(repo: &Path) {
    let path = repo.join(".git/fake-gh");
    keystone_core::test_support::write_executable(
        &path,
        r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done >> .git/fake-gh-argv
echo --- >> .git/fake-gh-argv
stage=$(cat .git/stage)
case "$1 $2" in
  "pr view")
    if [ "$3" = --json ]; then
      [ "$stage" = none ] && { echo 'no pull requests found for branch "fix"' >&2; exit 1; }
      echo '{"number":12,"url":"https://github.com/o/r/pull/12","state":"OPEN"}'
    else
      cat ".git/view-$stage.json"
    fi ;;
  "repo view") echo '{"squashMergeAllowed":true,"mergeCommitAllowed":true,"rebaseMergeAllowed":false}' ;;
  "run view")
    [ -f .git/slow ] && sleep 1
    [ -f .git/log.txt ] || { echo 'failed to get run log: log not found' >&2; exit 1; }
    cat .git/log.txt ;;
  api*)
    case "$*" in
      *--method*) echo merged > .git/stage; echo '{"merged":true}' ;;
      *" user "*) echo me ;;
      *) cat ".git/comments-$stage.json" 2>/dev/null || echo '[]' ;;
    esac ;;
esac
"#,
    );
}

fn view_json(state: &str, merge_state: &str, decision: &str, checks: &str) -> String {
    format!(
        r#"{{"title":"Fix the build","state":"{state}","isDraft":false,"baseRefName":"main","headRefName":"fix","headRefOid":"abc123","reviewDecision":"{decision}","mergeStateStatus":"{merge_state}","statusCheckRollup":[{checks}],"reviews":[]}}"#
    )
}

const RUNNING: &str = r#"{"__typename":"CheckRun","name":"test","workflowName":"CI","status":"IN_PROGRESS","conclusion":"","startedAt":"2026-10-25T08:00:00Z","completedAt":"0001-01-01T00:00:00Z","detailsUrl":""}"#;
const FAILED: &str = r#"{"__typename":"CheckRun","name":"test","workflowName":"CI","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-10-25T08:00:00Z","completedAt":"2026-10-25T08:01:12Z","detailsUrl":"https://github.com/o/r/actions/runs/11/job/22"}"#;
const PASSED: &str = r#"{"__typename":"CheckRun","name":"test","workflowName":"CI","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-10-25T08:02:00Z","completedAt":"2026-10-25T08:03:00Z","detailsUrl":""}"#;

/// `repo` on branch `fix` with gh scripted through every stage, and `other` inside
/// it, a repository of its own whose branch has no pull request.
fn fixture(root: &Path) {
    for repo in [root.to_owned(), root.join("other")] {
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "fix"]);
        script_gh(&repo);
    }
    fs::write(root.join("other/.git/stage"), "none").unwrap();
    let git_dir = root.join(".git");
    let stages = [
        (
            "running",
            view_json("OPEN", "BLOCKED", "REVIEW_REQUIRED", RUNNING),
        ),
        (
            "failing",
            view_json("OPEN", "BLOCKED", "REVIEW_REQUIRED", FAILED),
        ),
        (
            "review",
            view_json("OPEN", "BLOCKED", "CHANGES_REQUESTED", FAILED),
        ),
        ("mergeable", view_json("OPEN", "CLEAN", "APPROVED", PASSED)),
        // No conflicts, but the base branch's rules still block it.
        ("blocked", view_json("OPEN", "BLOCKED", "APPROVED", PASSED)),
        ("merged", view_json("MERGED", "UNKNOWN", "APPROVED", PASSED)),
    ];
    for (stage, json) in stages {
        fs::write(git_dir.join(format!("view-{stage}.json")), json).unwrap();
    }
    let comment =
        r#"[{"id":7,"user":{"login":"ana"},"path":"src/lib.rs","line":42,"body":"Rename this"}]"#;
    for stage in ["review", "mergeable", "blocked", "merged"] {
        fs::write(git_dir.join(format!("comments-{stage}.json")), comment).unwrap();
    }
    stage(root, "running");
}

fn stage(repo: &Path, stage: &str) {
    fs::write(repo.join(".git/stage"), stage).unwrap();
}

fn calls(repo: &Path) -> String {
    fs::read_to_string(repo.join(".git/fake-gh-argv")).unwrap_or_default()
}

/// Session 1 in the repository and 2 in `other`, with the first window showing 1.
fn seeded(root: &Path) -> std::path::PathBuf {
    let path = root.join(".git/workspace.json");
    let mut store = Store::open(&path).unwrap();
    for (n, folder) in [root.to_owned(), root.join("other")].iter().enumerate() {
        let directory = WorkingDirectory::new(folder).unwrap();
        store
            .create(Provider::Claude, &directory, n as u64 + 1)
            .unwrap();
    }
    let project = keystone_core::ProjectId::new(root).unwrap();
    store
        .open_project(keystone_core::WindowId::FIRST, &project, None, Some(1), 10)
        .unwrap();
    path
}

fn launch(path: &Path, cx: &mut TestAppContext) -> (Entity<Keystone>, AnyWindowHandle) {
    let store = Store::open(path).unwrap();
    cx.executor().allow_parking();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state: Entity<AppState> = install_app_state(store, cx);
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            open(state.clone(), id, placement, true, false, cx).unwrap();
        }
    });
    cx.run_until_parked();
    let view = cx.update(|cx| open_windows(cx))[0].clone();
    let handle = cx.read(|cx| view.read(cx).handle);
    (view, handle)
}

fn visual(handle: AnyWindowHandle, cx: &mut TestAppContext) -> &mut VisualTestContext {
    handle
        .update(cx, |_, window, _| window.activate_window())
        .unwrap();
    let cx = VisualTestContext::from_window(handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    cx.run_until_parked();
    cx
}

fn shown(cx: &mut VisualTestContext, id: &'static str) -> bool {
    cx.debug_bounds(id).is_some()
}

fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} is shown"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

/// Runs the app until gh's thread has answered and `done` holds.
fn until(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn report(
    view: &Entity<Keystone>,
    session: u64,
    cx: &mut VisualTestContext,
) -> Option<PullRequestReport> {
    view.read_with(cx, |view, cx| {
        Some(
            view.state
                .read(cx)
                .session(session)?
                .pull_request_watch()?
                .report
                .clone(),
        )
    })
}

fn idle(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |view, cx| {
        let monitor = &view.state.read(cx).pull_requests;
        monitor.watches.values().all(|w| {
            !w.fetching
                && !matches!(
                    w.merge,
                    MergeStep::Asking | MergeStep::Merging | MergeStep::Merged
                )
        })
    })
}

fn attention(
    view: &Entity<Keystone>,
    cx: &mut VisualTestContext,
) -> Vec<Option<keystone_core::Attention>> {
    view.read_with(cx, |view, cx| {
        view.state
            .read(cx)
            .store()
            .workspace()
            .sessions()
            .iter()
            .map(|s| s.attention(false, false, false))
            .collect()
    })
}

#[gpui::test]
fn the_pull_request_tab_follows_checks_and_reviews_and_merges_only_once_confirmed(
    app: &mut TestAppContext,
) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    let attention_before = attention(&view, cx);

    cx.simulate_keystrokes("secondary-alt-p");
    until(cx, "the branch's pull request is found", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-title")
    });
    assert_eq!(
        view.read_with(cx, |view, cx| view
            .state
            .read(cx)
            .session(1)
            .unwrap()
            .pull_request()
            .cloned()),
        Some(PullRequest {
            number: 12,
            url: "https://github.com/o/r/pull/12".into()
        }),
        "a pull request found by branch is saved on the session"
    );
    assert_eq!(
        report(&view, 1, cx).unwrap().checks[0].state,
        CheckState::Pending
    );
    assert!(shown(cx, "pull-request-check-0"));
    assert!(
        !shown(cx, "pull-request-merge"),
        "nothing to merge while the base branch's rules block it"
    );

    stage(&root, "failing");
    click(cx, "pull-request-refresh");
    until(cx, "the check fails", |cx| {
        idle(&view, cx) && report(&view, 1, cx).unwrap().checks[0].state == CheckState::Failed
    });
    assert!(
        !shown(cx, "session-pull-request-dot-1"),
        "a failure arriving in view is seen at once"
    );

    click(cx, "session-2");
    until(cx, "the other session's branch has none", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-empty")
    });
    stage(&root, "review");
    let before = calls(&root);
    cx.executor().advance_clock(SHOWN);
    cx.run_until_parked();
    assert_eq!(
        calls(&root),
        before,
        "out of view, the next ask waits longer"
    );
    cx.executor().advance_clock(HIDDEN);
    until(cx, "the review arrives out of view", |cx| {
        idle(&view, cx) && report(&view, 1, cx).unwrap().comments.len() == 1
    });
    assert!(
        shown(cx, "session-pull-request-dot-1"),
        "the row shows a dot"
    );
    assert!(!shown(cx, "session-pull-request-dot-2"));
    assert_eq!(
        attention(&view, cx),
        attention_before,
        "Needs action is unchanged"
    );

    click(cx, "session-1");
    until(cx, "showing it asks gh again", |cx| idle(&view, cx));
    assert!(
        !shown(cx, "session-pull-request-dot-1"),
        "viewing the tab clears the dot"
    );
    assert!(shown(cx, "pull-request-comment-0"));
    assert!(shown(cx, "pull-request-review-decision"));

    stage(&root, "mergeable");
    click(cx, "pull-request-refresh");
    until(cx, "it can merge", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-merge")
    });
    click(cx, "pull-request-merge");
    until(cx, "the confirmation opens", |cx| {
        idle(&view, cx) && shown(cx, "merge-dialog-confirm")
    });
    assert!(shown(cx, "merge-method-squash") && shown(cx, "merge-method-merge"));
    assert!(
        !shown(cx, "merge-method-rebase"),
        "only what the repository allows"
    );
    click(cx, "merge-dialog-cancel");
    assert!(!shown(cx, "merge-dialog-confirm"));
    assert!(!calls(&root).contains("PUT"), "Cancel merges nothing");

    click(cx, "pull-request-merge");
    until(cx, "the confirmation opens again", |cx| {
        idle(&view, cx) && shown(cx, "merge-dialog-confirm")
    });
    stage(&root, "blocked");
    click(cx, "merge-dialog-confirm");
    until(cx, "GitHub's rules block it now", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-failed")
    });
    assert!(
        !calls(&root).contains("PUT"),
        "a merge the rules block is never sent, even for a user who may bypass them"
    );
    stage(&root, "mergeable");
    click(cx, "pull-request-refresh");
    until(cx, "the refusal clears", |cx| {
        idle(&view, cx) && !shown(cx, "pull-request-failed")
    });

    click(cx, "pull-request-merge");
    until(cx, "the confirmation opens a third time", |cx| {
        idle(&view, cx) && shown(cx, "merge-dialog-confirm")
    });
    click(cx, "merge-dialog-confirm");
    until(cx, "merged", |cx| {
        idle(&view, cx) && report(&view, 1, cx).is_some_and(|r| r.state == PullRequestState::Merged)
    });
    assert!(
        calls(&root).contains(
            "---\napi\n--hostname\ngithub.com\n--method\nPUT\nrepos/o/r/pulls/12/merge\n-f\nmerge_method=squash\n-f\nsha=abc123\n---\n"
        ),
        "squash, the default the repository allows, for the head the user saw: {}",
        calls(&root)
    );
    assert!(!calls(&root).contains("--auto") && !calls(&root).contains("pr\nmerge"));
    assert!(!shown(cx, "pull-request-merge"));

    let after = calls(&root);
    cx.executor().advance_clock(LONGEST);
    cx.run_until_parked();
    assert_eq!(
        calls(&root),
        after,
        "a merged pull request is not asked about again"
    );

    cx.update(|_, cx| request_quit(cx, true));
    cx.run_until_parked();
    drop(view);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    assert_eq!(
        report(&view, 1, cx).map(|r| (r.state, r.comments.len())),
        Some((PullRequestState::Merged, 1)),
        "the last report is restored"
    );
    assert!(shown(cx, "pull-request-title"), "and the tab with it");
    let editor = root.join("editor-log");
    let script = root.join("editor");
    keystone_core::test_support::write_executable(
        &script,
        format!("#!/bin/sh\necho \"$1\" > '{}'\n", editor.display()),
    );
    // SAFETY: as for PATH; no other test reads VISUAL.
    unsafe { std::env::set_var("VISUAL", &script) };
    click(cx, "pull-request-comment-0");
    until(cx, "the editor opens the restored comment's file", |_| {
        fs::read_to_string(&editor).is_ok_and(|text| !text.is_empty())
    });
    assert!(
        fs::read_to_string(&editor)
            .unwrap()
            .contains(&format!("{}", root.join("src/lib.rs").display())),
        "a merged pull request is not asked about again, yet its comments still open"
    );
}

#[gpui::test]
fn a_missing_login_shows_the_command_to_run(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    fs::remove_file(root.join(".git/fake-gh")).unwrap();
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    cx.simulate_keystrokes("secondary-alt-p");
    until(cx, "gh answers", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-hint-gh-signed-out")
    });
    assert!(shown(cx, "pull-request-hint-command"));
    assert!(!shown(cx, "pull-request-title"));
}

/// The first report counts as seen, so a pull request linked while no window shows
/// it is asked about at once, before a quick check could fail into that baseline.
#[gpui::test]
fn a_check_failing_soon_after_linking_out_of_view_still_shows_a_dot(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    let link = PullRequest {
        number: 12,
        url: "https://github.com/o/r/pull/12".into(),
    };
    view.update(cx, |view, cx| {
        view.state
            .update(cx, |state, cx| state.set_pull_request(1, link, cx))
            .unwrap()
    });
    until(cx, "the new pull request is asked about at once", |cx| {
        idle(&view, cx) && report(&view, 1, cx).is_some()
    });
    stage(&root, "failing");
    cx.executor().advance_clock(HIDDEN);
    until(cx, "the check fails out of view", |cx| {
        idle(&view, cx)
            && report(&view, 1, cx).is_some_and(|r| r.checks[0].state == CheckState::Failed)
    });
    assert!(shown(cx, "session-pull-request-dot-1"));
}

/// Polls and merges run side by side, so gh's answer to an ask made before a merge
/// was refused must not wipe the refusal before the user reads it.
#[gpui::test]
fn a_refusal_stays_until_gh_is_asked_again_after_it(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    let state = view.read_with(cx, |view, _| view.state.clone());
    let refused = |cx: &mut VisualTestContext| {
        state.read_with(cx, |state, _| {
            matches!(state.pull_requests.watches[&1].merge, MergeStep::Failed(_))
        })
    };
    state.update(cx, |state, cx| {
        let watch = state.pull_requests.watches.entry(1).or_default();
        watch.asks = 3;
        let error = github::Error::Failed {
            output: "refused".into(),
        };
        state.merge_step(1, MergeStep::Failed(error), cx);
        state.pull_request_answered(1, 3, Ok(None), cx);
    });
    assert!(
        refused(cx),
        "an answer asked for before the refusal keeps it"
    );
    state.update(cx, |state, cx| {
        state.pull_request_answered(1, 4, Ok(None), cx);
    });
    assert!(!refused(cx), "one asked for after it clears it");
}

fn link_both(view: &Entity<Keystone>, sessions: &[u64], cx: &mut VisualTestContext) {
    let link = PullRequest {
        number: 12,
        url: "https://github.com/o/r/pull/12".into(),
    };
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            for &session in sessions {
                state.set_pull_request(session, link.clone(), cx).unwrap();
            }
        })
    });
}

fn views(repo: &Path) -> usize {
    calls(repo).matches("pr\nview\nhttps://").count()
}

/// A failure out of view notifies once, and the switch in Settings silences it.
#[gpui::test]
fn a_failed_check_out_of_view_notifies_once_unless_switched_off(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    view.update(cx, |view, cx| view.select(2, cx));
    link_both(&view, &[1], cx);
    until(cx, "the first report", |cx| {
        idle(&view, cx) && report(&view, 1, cx).is_some()
    });
    assert!(
        cx.shown_system_notifications().is_empty(),
        "the first report is no news"
    );
    stage(&root, "failing");
    cx.executor().advance_clock(HIDDEN);
    until(cx, "the check fails", |cx| {
        idle(&view, cx)
            && report(&view, 1, cx).is_some_and(|r| r.checks[0].state == CheckState::Failed)
    });
    let bodies = |cx: &mut VisualTestContext| -> Vec<String> {
        cx.shown_system_notifications()
            .into_iter()
            .map(|n| n.body.to_string())
            .collect()
    };
    assert_eq!(bodies(cx), ["Check failed"]);

    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            let notifications = keystone_core::Notifications {
                pull_requests: false,
                ..Default::default()
            };
            state.set_notifications(notifications, cx).unwrap();
        })
    });
    stage(&root, "review");
    cx.executor().advance_clock(HIDDEN);
    until(cx, "the review arrives", |cx| {
        idle(&view, cx) && report(&view, 1, cx).is_some_and(|r| r.comments.len() == 1)
    });
    assert_eq!(
        bodies(cx),
        ["Check failed"],
        "switched off, the review sends none"
    );
}

/// After a handoff both sessions link the pull request; only the newest asks gh,
/// shows a dot and notifies.
#[gpui::test]
fn one_pull_request_linked_from_two_sessions_is_watched_once(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let path = root.join(".git/workspace.json");
    let mut store = Store::open(&path).unwrap();
    let directory = WorkingDirectory::new(&root).unwrap();
    for id in [1, 2, 3] {
        store.create(Provider::Claude, &directory, id).unwrap();
    }
    let link = PullRequest {
        number: 12,
        url: "https://github.com/o/r/pull/12".into(),
    };
    // As a handoff leaves it: the source and the new session link one pull request.
    for id in [2, 3] {
        store.set_pull_request(id, link.clone()).unwrap();
    }
    let project = keystone_core::ProjectId::new(&root).unwrap();
    store
        .open_project(keystone_core::WindowId::FIRST, &project, None, Some(1), 10)
        .unwrap();
    drop(store);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    until(cx, "the first report", |cx| {
        idle(&view, cx) && report(&view, 3, cx).is_some()
    });
    assert_eq!(views(&root), 1, "only the newest session asks gh");
    assert!(report(&view, 2, cx).is_none());

    stage(&root, "failing");
    cx.executor().advance_clock(HIDDEN);
    until(cx, "the check fails", |cx| {
        idle(&view, cx)
            && report(&view, 3, cx).is_some_and(|r| r.checks[0].state == CheckState::Failed)
    });
    assert_eq!(views(&root), 2);
    assert!(shown(cx, "session-pull-request-dot-3"));
    assert!(!shown(cx, "session-pull-request-dot-2"));
    assert_eq!(cx.shown_system_notifications().len(), 1);
}

/// Each failure in a row doubles the wait, up to half an hour.
#[gpui::test]
fn failures_back_off_up_to_half_an_hour(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    stage(&root, "gone");
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    link_both(&view, &[1], cx);
    until(cx, "the first ask fails", |cx| {
        idle(&view, cx) && views(&root) == 1
    });
    for (wait, asks) in [(HIDDEN * 2, 2), (HIDDEN * 4, 3), (LONGEST, 4), (LONGEST, 5)] {
        cx.executor().advance_clock(wait - Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(views(&root), asks - 1, "not yet, {}s", wait.as_secs());
        cx.executor().advance_clock(Duration::from_secs(1));
        until(cx, "asked again", |cx| {
            idle(&view, cx) && views(&root) == asks
        });
    }
}

fn composer(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer.read(cx).value().to_owned())
}

fn draft(view: &Entity<Keystone>, session: u64, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| {
        view.state
            .read(cx)
            .session(session)
            .unwrap()
            .draft()
            .to_owned()
    })
}

/// A failed steps' log longer than twice the 32 KiB kept from each end, with a marker
/// at each end and one in the middle that the excerpt leaves out.
fn long_log(root: &Path) {
    let filler = |n: usize| "test\tRun cargo test\tline of output\n".repeat(n);
    fs::write(
        root.join(".git/log.txt"),
        format!(
            "FIRST-LINE\n{}MIDDLE-LINE\n{}error: LAST-LINE\n",
            filler(1200),
            filler(1200)
        ),
    )
    .unwrap();
}

#[gpui::test]
fn repair_and_send_put_the_check_and_comment_in_the_composer_and_send_nothing(
    app: &mut TestAppContext,
) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    long_log(&root);
    stage(&root, "failing");
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    cx.simulate_keystrokes("secondary-alt-p");
    until(cx, "the failed check shows", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-repair-0")
    });
    assert!(
        !shown(cx, "pull-request-repair-1"),
        "only a failed check offers a repair"
    );

    click(cx, "pull-request-repair-0");
    until(cx, "the log reaches the composer", |cx| {
        composer(&view, cx).contains("LAST-LINE")
    });
    let text = composer(&view, cx);
    assert!(
        text.starts_with(
            "Fix the failing check `CI / test` on https://github.com/o/r/pull/12.\n\n```\nFIRST-LINE\n"
        ),
        "{}",
        &text[..200]
    );
    assert!(text.ends_with("error: LAST-LINE\n```"));
    assert!(text.contains("bytes omitted") && !text.contains("MIDDLE-LINE"));
    assert!(text.len() < 2 * keystone_agent::OUTPUT_CAP + 200);
    assert!(
        calls(&root).contains("run\nview\n--job\n22\n--log-failed\n--repo\ngithub.com/o/r\n---"),
        "{}",
        calls(&root)
    );
    assert_eq!(draft(&view, 1, cx), text, "the draft is saved");

    stage(&root, "review");
    click(cx, "pull-request-refresh");
    until(cx, "the comment shows", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-send-0")
    });
    let opened = calls(&root);
    click(cx, "pull-request-send-0");
    let text = composer(&view, cx);
    assert!(
        text.ends_with(
            "```\nAddress ana's review comment on src/lib.rs:42 in https://github.com/o/r/pull/12:\n\n> Rename this"
        ),
        "the comment goes on a line after the log: {}",
        &text[text.len() - 200..]
    );
    assert_eq!(calls(&root), opened, "Send to asks gh for nothing");
    assert_eq!(draft(&view, 1, cx), text);
    view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        assert!(!state.session(1).unwrap().started(), "nothing was sent");
    });
    assert!(
        !calls(&root).contains("--method"),
        "nothing was pushed or merged"
    );

    fs::write(
        root.join(".git/view-merged.json"),
        view_json("MERGED", "UNKNOWN", "APPROVED", FAILED),
    )
    .unwrap();
    stage(&root, "merged");
    click(cx, "pull-request-refresh");
    until(cx, "the pull request is merged", |cx| {
        idle(&view, cx) && report(&view, 1, cx).unwrap().state == PullRequestState::Merged
    });
    assert!(shown(cx, "pull-request-check-0") && shown(cx, "pull-request-comment-0"));
    assert!(!shown(cx, "pull-request-repair-0") && !shown(cx, "pull-request-send-0"));
}

#[gpui::test]
fn a_log_that_arrives_after_switching_sessions_goes_to_its_own_draft(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    long_log(&root);
    fs::write(root.join(".git/slow"), "").unwrap();
    stage(&root, "failing");
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    cx.simulate_keystrokes("secondary-alt-p");
    until(cx, "the failed check shows", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-repair-0")
    });
    view.update(cx, |view, cx| {
        view.composer
            .update(cx, |input, cx| input.set_value("Kept".into(), cx))
    });
    click(cx, "pull-request-repair-0");
    click(cx, "session-2");
    until(cx, "the log reaches session 1's draft", |cx| {
        draft(&view, 1, cx).contains("LAST-LINE")
    });
    assert!(draft(&view, 1, cx).starts_with("Kept\nFix the failing check"));
    assert!(!composer(&view, cx).contains("LAST-LINE"));
    assert!(!draft(&view, 2, cx).contains("LAST-LINE"));
}

#[gpui::test]
fn a_log_gh_cannot_read_leaves_the_check_link_and_says_why(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    stage(&root, "failing");
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    cx.simulate_keystrokes("secondary-alt-p");
    until(cx, "the failed check shows", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-repair-0")
    });
    click(cx, "pull-request-repair-0");
    until(cx, "gh's refusal shows", |cx| {
        view.read_with(cx, |view, _| view.repair.is_none() && view.error.is_some())
    });
    assert_eq!(
        view.read_with(cx, |view, _| view.error.clone()),
        Some("Log not read. failed to get run log: log not found".into())
    );
    assert_eq!(
        composer(&view, cx),
        "Fix the failing check `CI / test` on https://github.com/o/r/pull/12.\n\nhttps://github.com/o/r/actions/runs/11/job/22"
    );
}

#[gpui::test]
fn a_prefill_keeps_the_selection_and_goes_on_lines_of_its_own(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    stage(&root, "review");
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    cx.simulate_keystrokes("secondary-alt-p");
    until(cx, "the comment shows", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-send-0")
    });
    view.update_in(cx, |view, window, cx| {
        view.composer.update(cx, |input, cx| {
            input.set_value("Selected draft".into(), cx);
            input.select_all(&keystone_ui::text_input::SelectAll, window, cx);
        })
    });
    click(cx, "pull-request-send-0");
    assert_eq!(
        composer(&view, cx),
        "Selected draft\nAddress ana's review comment on src/lib.rs:42 in https://github.com/o/r/pull/12:\n\n> Rename this"
    );
    assert_eq!(crate::on_own_line("a", "b", " c"), "\nb\n");
    assert_eq!(crate::on_own_line("a\n", "b", "\nc"), "b");
    assert_eq!(crate::on_own_line("", "b", ""), "b");
}

#[gpui::test]
fn a_log_for_a_session_another_window_shows_lands_in_that_windows_composer(
    app: &mut TestAppContext,
) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    long_log(&root);
    fs::write(root.join(".git/slow"), "").unwrap();
    stage(&root, "failing");
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    handle
        .update(app, |_, window, cx| {
            view.update(cx, |this, cx| this.open_new_window(Some(1), window, cx))
        })
        .unwrap();
    app.run_until_parked();
    let other = app
        .update(|cx| open_windows(cx))
        .into_iter()
        .find(|w| *w != view)
        .unwrap();
    let cx = visual(handle, app);
    cx.simulate_keystrokes("secondary-alt-p");
    until(cx, "the failed check shows", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-repair-0")
    });
    click(cx, "pull-request-repair-0");
    click(cx, "session-2");
    until(cx, "the log reaches the other window", |cx| {
        composer(&other, cx).contains("LAST-LINE")
    });
    assert!(draft(&view, 1, cx).contains("LAST-LINE"), "and is saved");
    assert!(!composer(&view, cx).contains("LAST-LINE"));
}

#[test]
fn a_repair_prompt_fences_the_log_past_its_backticks_and_links_a_check_without_one() {
    let link = PullRequest {
        number: 3,
        url: "https://github.com/o/r/pull/3".into(),
    };
    let check = |workflow: &str, url: &str| Check {
        name: "build".into(),
        workflow: workflow.into(),
        state: CheckState::Failed,
        summary: "Failure".into(),
        started_at: None,
        completed_at: None,
        url: url.into(),
    };
    assert_eq!(
        repair_prompt(&link, &check("CI", ""), Some("a ```` fence\n\n")),
        "Fix the failing check `CI / build` on https://github.com/o/r/pull/3.\n\n`````\na ```` fence\n`````"
    );
    assert_eq!(
        repair_prompt(&link, &check("", "https://ci.example/9"), None),
        "Fix the failing check `build` on https://github.com/o/r/pull/3.\n\nhttps://ci.example/9"
    );
    let comment = ReviewComment {
        id: 1,
        author: "ana".into(),
        path: "README.md".into(),
        line: None,
        body: "Two lines\n\nhere".into(),
    };
    assert_eq!(
        comment_prompt(&link, &comment),
        "Address ana's review comment on README.md in https://github.com/o/r/pull/3:\n\n> Two lines\n>\n> here"
    );
}

#[gpui::test]
fn a_squeezed_panel_keeps_a_failed_checks_name_readable(app: &mut TestAppContext) {
    fake_gh();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    stage(&root, "failing");
    let path = seeded(&root);
    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    cx.simulate_resize(gpui::size(px(1600.), px(900.)));
    cx.run_until_parked();
    cx.simulate_keystrokes("secondary-alt-p");
    until(cx, "the failed check shows", |cx| {
        idle(&view, cx) && shown(cx, "pull-request-repair-0")
    });
    // Before the resize settles, the panel and sidebar shrink to their minimums.
    cx.simulate_resize(gpui::size(px(960.), px(720.)));
    cx.run_until_parked();
    let name = cx.debug_bounds("pull-request-check-0-name").unwrap();
    assert!(
        name.size.width >= px(72.),
        "the check's name keeps room to read: {:?}",
        name.size
    );
}
