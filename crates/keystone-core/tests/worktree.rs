use keystone_core::{Provider, SessionContext, Store, WindowId, WorkingDirectory, worktree};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::{TempDir, tempdir};

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn repository() -> TempDir {
    // Canonical, like the folders Keystone records: macOS's temporary folder sits
    // behind a symlink.
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    git(dir.path(), &["init", "--initial-branch=main"]);
    let config = dir.path().join(".git/config");
    let mut settings = fs::read_to_string(&config).unwrap();
    settings.push_str("\n[user]\n\tname = Worktree test\n\temail = worktree@example.invalid\n");
    fs::write(config, settings).unwrap();
    fs::write(dir.path().join("tracked"), "base\n").unwrap();
    git(dir.path(), &["add", "tracked"]);
    git(dir.path(), &["commit", "-m", "base"]);
    git(
        dir.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    dir
}

#[test]
fn create_inspect_remove_retains_branch_and_uses_private_exclude() {
    let repo = repository();
    fs::write(
        repo.path().join(".git/info/exclude"),
        "existing-without-newline",
    )
    .unwrap();
    let context = worktree::create(repo.path(), "task-1", None).unwrap();
    assert_eq!(context.branch, "keystone/task-1");
    assert_eq!(
        context.repository.path(),
        repo.path().canonicalize().unwrap()
    );
    assert_eq!(
        context.directory.path(),
        repo.path().join(".keystone/worktrees/task-1")
    );
    assert_eq!(
        worktree::inspect(repo.path(), context.directory.path()).unwrap(),
        context
    );
    assert_eq!(
        git(context.directory.path(), &["rev-parse", "HEAD"]),
        git(repo.path(), &["rev-parse", "HEAD"])
    );
    assert_eq!(git(repo.path(), &["status", "--porcelain"]), "");
    assert!(!repo.path().join(".gitignore").exists());
    assert_eq!(
        fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap(),
        "existing-without-newline\n/.keystone/worktrees/\n"
    );
    assert!(worktree::create(repo.path(), "task-1", None).is_err());
    worktree::remove(&context).unwrap();
    assert!(!context.directory.path().exists());
    assert_eq!(
        git(repo.path(), &["rev-parse", "refs/heads/keystone/task-1"]),
        git(repo.path(), &["rev-parse", "HEAD"])
    );
    assert!(worktree::create(repo.path(), "task-1", None).is_err());
}

#[test]
fn removal_refuses_modified_untracked_ignored_and_unpushed_work() {
    let repo = repository();
    let context = worktree::create(repo.path(), "safety", None).unwrap();
    let path = context.directory.path();
    fs::write(path.join("tracked"), "changed").unwrap();
    assert!(worktree::remove(&context).is_err());
    fs::write(path.join("tracked"), "base\n").unwrap();
    fs::write(path.join("untracked"), "keep").unwrap();
    assert!(worktree::remove(&context).is_err());
    fs::remove_file(path.join("untracked")).unwrap();
    fs::write(repo.path().join(".git/info/exclude"), "ignored\n").unwrap();
    fs::write(path.join("ignored"), "keep").unwrap();
    assert!(worktree::remove(&context).is_err());
    fs::remove_file(path.join("ignored")).unwrap();
    git(path, &["commit", "--allow-empty", "-m", "unpushed"]);
    assert!(
        worktree::remove(&context)
            .unwrap_err()
            .to_string()
            .contains("commits")
    );
    git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/task", "keystone/safety"],
    );
    worktree::remove(&context).unwrap();
}

#[test]
fn inspect_rejects_main_nested_foreign_unregistered_and_swapped_worktrees() {
    let repo = repository();
    let other = repository();
    let context = worktree::create(repo.path(), "first", None).unwrap();
    let second = worktree::create(repo.path(), "second", None).unwrap();
    fs::create_dir(context.directory.path().join("nested")).unwrap();
    assert!(worktree::inspect(repo.path(), repo.path()).is_err());
    assert!(worktree::inspect(repo.path(), &context.directory.path().join("nested")).is_err());
    assert!(worktree::inspect(other.path(), context.directory.path()).is_err());
    assert!(worktree::inspect(repo.path(), other.path()).is_err());
    let moved = repo.path().join("moved");
    fs::rename(context.directory.path(), &moved).unwrap();
    assert!(worktree::inspect(repo.path(), &moved).is_err());
    fs::rename(second.directory.path(), context.directory.path()).unwrap();
    fs::rename(&moved, second.directory.path()).unwrap();
    assert!(worktree::inspect(repo.path(), context.directory.path()).is_err());
    assert!(worktree::remove(&context).is_err());
    assert!(context.directory.path().exists());
}

#[test]
fn unsafe_names_bases_and_directory_collisions_are_refused() {
    let repo = repository();
    for slug in [
        "",
        "..",
        "../escape",
        "-option",
        "a/b",
        "a b",
        "a\nb",
        "a.lock",
    ] {
        assert!(
            worktree::create(repo.path(), slug, None).is_err(),
            "{slug:?}"
        );
    }
    for base in ["", "--help", "-b", "HEAD\n"] {
        assert!(worktree::create(repo.path(), "valid", Some(base)).is_err());
    }
    let parent = repo.path().join(".keystone/worktrees");
    fs::create_dir_all(parent.join("collision")).unwrap();
    assert!(worktree::create(repo.path(), "collision", None).is_err());
    assert!(
        !git(repo.path(), &["branch", "--list", "keystone/collision"])
            .contains("keystone/collision")
    );
}

#[cfg(unix)]
#[test]
fn create_refuses_symlinked_parent_and_remove_refuses_changed_branch() {
    let repo = repository();
    let outside = tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.path().join(".keystone")).unwrap();
    assert!(worktree::create(repo.path(), "escape", None).is_err());
    assert!(!outside.path().join("worktrees").exists());
    fs::remove_file(repo.path().join(".keystone")).unwrap();
    let context = worktree::create(repo.path(), "safe", None).unwrap();
    git(context.directory.path(), &["switch", "-c", "different"]);
    assert!(worktree::remove(&context).is_err());
    assert!(context.directory.path().exists());
}

#[test]
fn default_base_is_the_given_linked_projects_head() {
    let repo = repository();
    let source = worktree::create(repo.path(), "source", None).unwrap();
    git(
        source.directory.path(),
        &["commit", "--allow-empty", "-m", "source"],
    );
    let context = worktree::create(source.directory.path(), "child", None).unwrap();
    assert_eq!(context.repository, source.repository);
    assert_eq!(
        git(context.directory.path(), &["rev-parse", "HEAD"]),
        git(source.directory.path(), &["rev-parse", "HEAD"])
    );
    let explicit = worktree::create(source.directory.path(), "explicit", Some("main")).unwrap();
    assert_eq!(
        git(explicit.directory.path(), &["rev-parse", "HEAD"]),
        git(repo.path(), &["rev-parse", "HEAD"])
    );
}

#[test]
fn session_v2_is_atomic_reloads_and_survives_handoff_alongside_v1() {
    let repo = repository();
    let data = tempdir().unwrap();
    let path = data.path().join("sessions.json");
    let context = worktree::create(repo.path(), "persist", None).unwrap();
    let mut store = Store::open(&path).unwrap();
    let first = store
        .create_worktree_showing(Provider::Claude, &context, 1, WindowId::FIRST, None)
        .unwrap()
        .value;
    assert_eq!(store.writes(), 1);
    let second = store
        .create_worktree_unselected(Provider::Codex, &context, 2, first)
        .unwrap()
        .value;
    assert_eq!(store.workspace().selected(), Some(first));
    assert_eq!(store.workspace().delivery(second).unwrap().parent(), first);
    store
        .create(
            Provider::Codex,
            &WorkingDirectory::new(repo.path()).unwrap(),
            3,
        )
        .unwrap();
    let handed = store
        .hand_off(
            first,
            Provider::Codex,
            "continue".into(),
            4,
            WindowId::FIRST,
            None,
        )
        .unwrap()
        .value;
    let snapshot = store.saved_snapshot().unwrap();
    let rows = snapshot["workspace"]["sessions"].as_array().unwrap();
    assert_eq!(rows[0]["execution_context"]["version"], 2);
    assert_eq!(
        rows[0]["execution_context"]["worktree"]["branch"],
        context.branch
    );
    assert_eq!(rows[2]["execution_context"]["version"], 1);
    assert!(rows[2]["execution_context"].get("worktree").is_none());
    drop(store);
    fs::remove_dir_all(context.directory.path()).unwrap();
    let restored = Store::open(&path).unwrap();
    for id in [first, second, handed] {
        let session = restored
            .workspace()
            .sessions()
            .iter()
            .find(|session| session.id() == id)
            .unwrap();
        assert_eq!(session.worktree(), Some(&context));
        assert!(
            matches!(session.context(), SessionContext::Recorded { directory, .. } if directory == &context.directory)
        );
    }
    assert!(restored.workspace().sessions()[2].worktree().is_none());
}

#[test]
fn failed_worktree_session_creation_leaves_no_partial_session_or_metadata() {
    let repo = repository();
    let data = tempdir().unwrap();
    let context = worktree::create(repo.path(), "atomic", None).unwrap();
    let mut store = Store::open(&data.path().join("sessions.json")).unwrap();
    let before = store.saved_snapshot().unwrap();
    let lock = rusqlite::Connection::open(store.path()).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    store
        .create_worktree_showing(Provider::Codex, &context, 1, WindowId::FIRST, None)
        .unwrap();
    assert!(store.flush().is_err());
    assert_eq!(store.writes(), 0);
    assert_eq!(store.workspace().sessions().len(), 1);
    assert_eq!(store.saved_snapshot().unwrap(), before);
    lock.execute_batch("ROLLBACK").unwrap();
    store.flush().unwrap();
    assert_eq!(store.workspace().sessions()[0].worktree(), Some(&context));
}

#[test]
fn removal_refuses_hidden_index_changes_and_commits_without_remote_refs() {
    let repo = repository();
    let context = worktree::create(repo.path(), "hidden", None).unwrap();
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        git(context.directory.path(), &["update-index", flag, "tracked"]);
        fs::write(context.directory.path().join("tracked"), "hidden changes").unwrap();
        assert!(worktree::remove(&context).is_err());
        fs::write(context.directory.path().join("tracked"), "base\n").unwrap();
        for clear in ["--no-assume-unchanged", "--no-skip-worktree"] {
            git(
                context.directory.path(),
                &["update-index", clear, "tracked"],
            );
        }
    }
    git(
        repo.path(),
        &["update-ref", "-d", "refs/remotes/origin/main"],
    );
    git(
        context.directory.path(),
        &["commit", "--allow-empty", "-m", "local only"],
    );
    let refused = worktree::remove(&context).unwrap_err().to_string();
    assert!(
        refused.contains("neither pushed nor on another local branch"),
        "{refused}"
    );
    assert!(context.directory.path().exists());
}

#[test]
fn removal_without_remotes_allows_a_branch_whose_commits_another_branch_holds() {
    let repo = repository();
    git(
        repo.path(),
        &["update-ref", "-d", "refs/remotes/origin/main"],
    );
    let context = worktree::create(repo.path(), "fresh", None).unwrap();
    worktree::remove(&context).unwrap();
    assert!(!context.directory.path().exists());
    git(
        repo.path(),
        &["rev-parse", "--verify", "refs/heads/keystone/fresh"],
    );
}

#[test]
fn unknown_v2_contexts_are_preserved_verbatim_on_unrelated_saves() {
    use serde_json::json;

    let data = tempdir().unwrap();
    let cases = [
        json!({"version": 3, "directory": "/gone", "origin": "created", "worktree": {"repository": "/repo", "branch": "branch"}}),
        json!({"version": 2, "directory": "/gone", "origin": "created"}),
        json!({"version": 2, "directory": "/gone", "origin": "created", "worktree": {"repository": "/repo", "branch": "branch", "future": true}}),
        json!({"version": 2, "directory": "/gone", "origin": "created", "worktree": {"repository": "relative", "branch": "branch"}}),
        json!({"version": 1, "directory": "/gone", "origin": "created", "worktree": null}),
    ];
    for (index, raw) in cases.into_iter().enumerate() {
        let path = data.path().join(format!("unknown-{index}.json"));
        fs::write(
            &path,
            json!({
                "version": 1, "next_id": 2,
                "workspace": {"selected": 1, "sessions": [{
                    "id": 1, "title": "Unknown", "provider": "Codex", "settled": false,
                    "draft": "", "execution_context": raw
                }]}
            })
            .to_string(),
        )
        .unwrap();
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store.workspace().sessions()[0].context(),
            SessionContext::Unsupported
        );
        assert!(store.workspace().sessions()[0].worktree().is_none());
        store.set_draft(1, "unrelated".into()).unwrap();
        drop(store);
        assert_eq!(
            Store::open(&path).unwrap().saved_snapshot().unwrap()["workspace"]["sessions"][0]["execution_context"],
            raw
        );
    }
}

#[cfg(unix)]
#[test]
fn a_worktree_folder_replaced_by_another_repository_is_not_intact() {
    let repo = repository();
    let context = worktree::create(repo.path(), "swap", None).unwrap();
    assert!(worktree::is_intact(&context));
    let other = repository();
    let elsewhere = worktree::create(other.path(), "swap", None).unwrap();
    git(
        repo.path(),
        &[
            "worktree",
            "remove",
            context.directory.path().to_str().unwrap(),
        ],
    );
    assert!(
        !worktree::is_intact(&context),
        "a removed folder is not intact"
    );
    std::os::unix::fs::symlink(elsewhere.directory.path(), context.directory.path()).unwrap();
    assert!(
        !worktree::is_intact(&context),
        "a symlink to another repository's worktree is not this worktree"
    );
}

#[test]
fn removal_names_the_files_and_the_branch_it_refuses_for() {
    let repo = repository();
    let context = worktree::create(repo.path(), "named", None).unwrap();
    fs::write(context.directory.path().join("draft.md"), "work\n").unwrap();
    let refused = worktree::remove(&context).unwrap_err().to_string();
    assert!(refused.contains("draft.md"), "{refused}");
    fs::remove_file(context.directory.path().join("draft.md")).unwrap();
    git(context.directory.path(), &["switch", "-c", "elsewhere"]);
    let refused = worktree::remove(&context).unwrap_err().to_string();
    assert!(
        refused.contains("now on elsewhere, not keystone/named"),
        "{refused}"
    );
    assert!(context.directory.path().exists());
}

#[test]
fn two_worktrees_of_one_repository_swapped_are_not_intact() {
    let repo = repository();
    let a = worktree::create(repo.path(), "a", None).unwrap();
    let b = worktree::create(repo.path(), "b", None).unwrap();
    let parked = repo.path().join("parked");
    fs::rename(a.directory.path(), &parked).unwrap();
    fs::rename(b.directory.path(), a.directory.path()).unwrap();
    fs::rename(&parked, b.directory.path()).unwrap();
    assert!(!worktree::is_intact(&a));
    assert!(!worktree::is_intact(&b));
}

#[test]
fn a_rename_is_named_once_in_the_refusal() {
    let repo = repository();
    let context = worktree::create(repo.path(), "renamed", None).unwrap();
    git(context.directory.path(), &["mv", "tracked", "moved"]);
    let refused = worktree::remove(&context).unwrap_err().to_string();
    assert!(refused.ends_with("ignored files: moved."), "{refused}");
}

#[test]
fn a_short_renamed_path_does_not_hide_the_next_file() {
    let repo = repository();
    std::fs::write(repo.path().join("a"), "short\n").unwrap();
    git(repo.path(), &["add", "a"]);
    git(repo.path(), &["commit", "-m", "short"]);
    let context = worktree::create(repo.path(), "short", None).unwrap();
    git(context.directory.path(), &["mv", "a", "b"]);
    std::fs::write(context.directory.path().join("notes.md"), "draft\n").unwrap();
    let refused = worktree::remove(&context).unwrap_err().to_string();
    assert!(
        refused.ends_with("ignored files: b, notes.md."),
        "{refused}"
    );
}
