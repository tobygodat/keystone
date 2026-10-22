//! What `scripts/probe-fork.py` recorded from Claude Code 2.1.301 and Codex 0.163.0,
//! without inference, and what Keystone's native fork relies on.

use serde_json::Value;

fn probe() -> Value {
    serde_json::from_str(include_str!("fixtures/fork-probe.json")).unwrap()
}

fn kept(run: &Value) -> (u64, u64) {
    let tokens = &run["kept_message_tokens"];
    (
        tokens["userMessageTokens"].as_u64().unwrap(),
        tokens["assistantMessageTokens"].as_u64().unwrap(),
    )
}

#[test]
fn claude_forks_through_the_entry_it_is_given_and_refuses_one_it_cannot_find() {
    let claude = &probe()["claude"];
    assert_eq!(claude["version"], "2.1.301 (Claude Code)");
    let runs = &claude["runs"];
    for run in [
        "through_turn_1",
        "through_turn_2",
        "whole",
        "at_turn_3_prompt",
    ] {
        assert_eq!(runs[run]["initialize"], "success", "{run}");
        assert_eq!(runs[run]["exit"], 0, "{run}");
    }
    let (one, two, whole) = (
        kept(&runs["through_turn_1"]),
        kept(&runs["through_turn_2"]),
        kept(&runs["whole"]),
    );
    assert!(
        one.0 < two.0 && two.0 < whole.0,
        "{one:?} {two:?} {whole:?}"
    );
    assert!(
        one.1 < two.1 && two.1 < whole.1,
        "{one:?} {two:?} {whole:?}"
    );
    // The cut is inclusive: naming the third turn's prompt keeps that prompt, so a
    // fork names the last entry of the turn it keeps.
    let at_prompt = kept(&runs["at_turn_3_prompt"]);
    assert_eq!(at_prompt, (whole.0, two.1));
    assert_eq!(
        runs["unknown_entry"]["stderr_tail"],
        "No message found with message.uuid of: not-an-entry"
    );
    assert_eq!(runs["unknown_entry"]["exit"], 1);
    assert_eq!(
        runs["unknown_entry"]["frames"][0]["subtype"],
        "error_during_execution"
    );
    assert_eq!(runs["unknown_session"]["exit"], 1);
    // Resuming as a fork writes nothing before the first message.
    assert_eq!(claude["source_unchanged"], true);
    assert_eq!(claude["session_files_after"].as_array().unwrap().len(), 1);
}

#[test]
fn codex_forks_through_a_turn_and_leaves_the_source_alone() {
    let codex = &probe()["codex"];
    assert_eq!(codex["version"], "codex-cli 0.163.0");
    assert_eq!(codex["fork_method_listed"], true);
    assert!(
        codex["fork_params"]
            .as_array()
            .unwrap()
            .contains(&Value::from("lastTurnId"))
    );
    let ids = |turns: &Value| -> Vec<String> {
        turns
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn[0].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids(&codex["source_turns"]), ["turn-1", "turn-2", "turn-3"]);
    let fork = &codex["fork_through_turn_2"];
    assert_eq!(fork["forkedFromId"], "source");
    // A fork keeps the source's turn IDs, so a fork point names the same turn in both.
    assert_eq!(ids(&fork["turns"]), ["turn-1", "turn-2"]);
    assert_eq!(
        ids(&codex["fork_whole"]["turns"]),
        ["turn-1", "turn-2", "turn-3"]
    );
    assert_eq!(codex["source_turns_after"], codex["source_turns"]);
    assert_eq!(
        codex["unknown_turn"]["message"],
        "turn not found: not-a-turn"
    );
}
