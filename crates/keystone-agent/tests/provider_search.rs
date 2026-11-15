//! A test never searches for a provider CLI, which on a developer's machine is the user's
//! real `claude` or `codex` (TOB-363), and nothing else turns the search off. This binary
//! is named `provider_search-<hash>` and built with the test features, so it also proves
//! that neither a binary's name nor its features can stop an app from finding a CLI.

use std::path::Path;
use std::process::{Command, Output};

/// In a child process the test below starts: arms the switch as `KEYSTONE_TEST_ARM`
/// says, searches for `KEYSTONE_TEST_SEARCH`, and prints what it found.
#[test]
#[ignore = "run in a child process by the tests below"]
fn search() {
    let Ok(name) = std::env::var("KEYSTONE_TEST_SEARCH") else {
        return;
    };
    match std::env::var("KEYSTONE_TEST_ARM").as_deref() {
        Ok("switch") => keystone_agent::find::refuse_path_search(),
        Ok("scripted") => {
            keystone_core::test_support::built_binary("keystone-fake-claude");
        }
        _ => {}
    }
    println!("found {:?}", keystone_agent::find::program(&name));
}

fn search_in(dir: &Path, name: &str, arm: &str) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "search", "--ignored", "--nocapture"])
        .env("PATH", dir)
        .env("HOME", dir)
        .env("KEYSTONE_TEST_SEARCH", name)
        .env("KEYSTONE_TEST_ARM", arm)
        .output()
        .unwrap()
}

fn stand_ins() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in ["claude", "codex", "keystone-test-tool"] {
        keystone_core::test_support::write_executable(&dir.path().join(name), "#!/bin/sh\n");
    }
    dir
}

fn found(output: &Output, path: &Path) -> bool {
    output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains(&format!("found Some({path:?})"))
}

fn refused(output: &Output, name: &str) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    !output.status.success()
        && stderr.contains(&format!("tried to find `{name}` on PATH"))
        && !String::from_utf8_lossy(&output.stdout).contains("found")
}

#[test]
fn without_the_switch_a_test_binary_still_finds_the_providers() {
    let dir = stand_ins();
    for name in ["claude", "codex"] {
        let output = search_in(dir.path(), name, "");
        assert!(found(&output, &dir.path().join(name)), "{output:?}");
    }
}

#[test]
fn the_switch_refuses_only_the_provider_search() {
    let dir = stand_ins();
    for name in ["claude", "codex"] {
        let output = search_in(dir.path(), name, "switch");
        assert!(refused(&output, name), "{output:?}");
    }
    let output = search_in(dir.path(), "keystone-test-tool", "switch");
    assert!(
        found(&output, &dir.path().join("keystone-test-tool")),
        "{output:?}"
    );
}

#[test]
fn finding_a_scripted_provider_turns_the_switch_on() {
    let dir = stand_ins();
    let output = search_in(dir.path(), "claude", "scripted");
    assert!(refused(&output, "claude"), "{output:?}");
}
