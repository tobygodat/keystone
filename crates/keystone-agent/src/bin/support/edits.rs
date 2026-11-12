//! File edits the fakes report, in the frames the real providers sent (see
//! `tests/fixtures/edits/`). A prompt containing "replay edits" reports the recorded
//! edits, and one containing "long edit N" reports one edit of N scripted lines, in the
//! recorded edit's shape. Nothing is written to disk.

use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replays so far, so each one's message and tool IDs are new.
#[allow(dead_code)]
static REPLAYS: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
const CLAUDE: &str = include_str!("../../../tests/fixtures/edits/claude-2.1.322-edits.jsonl");
#[allow(dead_code)]
const CODEX: &str = include_str!("../../../tests/fixtures/edits/codex-0.167.1-edits.jsonl");

/// The N of "long edit N".
fn long(text: &str) -> Option<usize> {
    let rest = &text[text.find("long edit ")? + "long edit ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// N scripted lines in unified diff form: two of every three added, the third context.
fn scripted(n: usize) -> Vec<String> {
    (1..=n)
        .map(|k| match k % 3 {
            0 => format!(" scripted context line {k} (test data)"),
            _ => format!("+scripted added line {k} (test data)"),
        })
        .collect()
}

/// Claude's `assistant` and `user` frames for each edit, in order: the recorded Edit and
/// Write calls with the probe's repository replaced by `cwd`, one scripted Edit of
/// `long.txt`, or, for "edit without newline", an Edit of `a.txt` in the shape Claude
/// Code's jsdiff gives `one\ntwo\nthree` with `two` made `TWO`: the file has no final
/// newline, so the hunk ends with the marker line.
#[allow(dead_code)]
pub fn claude(text: &str, cwd: &str) -> Vec<Value> {
    if text.contains("edit without newline") {
        let lines = [
            " one",
            "-two",
            "+TWO",
            " three",
            "\\ No newline at end of file",
        ];
        return scripted_edit(cwd, "a.txt", (1, 3), (1, 3), &lines);
    }
    if let Some(n) = long(text) {
        let lines = scripted(n);
        let context = lines.iter().filter(|line| line.starts_with(' ')).count();
        return scripted_edit(cwd, "long.txt", (1, context), (1, lines.len()), &lines);
    }
    if !text.contains("replay edits") {
        return Vec::new();
    }
    claude_replay(cwd)
}

/// One scripted Edit of `file` under `cwd`, as Claude Code sends it, with one hunk:
/// `(start, lines)` on each side, then its marked lines.
#[allow(dead_code)]
fn scripted_edit<S: serde::Serialize>(
    cwd: &str,
    file: &str,
    old: (usize, usize),
    new: (usize, usize),
    lines: &[S],
) -> Vec<Value> {
    let replay = REPLAYS.fetch_add(1, Ordering::Relaxed);
    let tool = format!("fake-edit-{}-{replay}", std::process::id());
    let path = format!("{cwd}/{file}");
    vec![
        json!({"type": "assistant", "message": {"id": format!("{tool}-message"), "role": "assistant",
               "content": [{"type": "tool_use", "id": tool, "name": "Edit",
                            "input": {"file_path": path, "old_string": "", "new_string": "", "replace_all": false}}]},
               "parent_tool_use_id": null}),
        json!({"type": "user", "message": {"role": "user", "content": [{"tool_use_id": tool, "type": "tool_result",
               "content": format!("The file {path} has been updated successfully.")}]},
               "parent_tool_use_id": null,
               "tool_use_result": {"filePath": path, "structuredPatch": [
                   {"oldStart": old.0, "oldLines": old.1, "newStart": new.0, "newLines": new.1, "lines": lines}],
                   "userModified": false, "replaceAll": false}}),
    ]
}

/// The recorded Edit and Write calls, under `cwd`.
#[allow(dead_code)]
fn claude_replay(cwd: &str) -> Vec<Value> {
    let replay = REPLAYS.fetch_add(1, Ordering::Relaxed);
    let text = CLAUDE
        .replace("/tmp/probe/claude-edits", cwd)
        .replace(
            "\"toolu_",
            &format!("\"toolu_{}_{replay}_", std::process::id()),
        )
        .replace("\"msg_", &format!("\"msg_{}_{replay}_", std::process::id()));
    let frames: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let edit = |block: &Value| matches!(block["name"].as_str(), Some("Edit" | "Write"));
    let tools: Vec<String> = frames
        .iter()
        .filter(|f| f["type"] == "assistant")
        .flat_map(|f| {
            f["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|block| block["type"] == "tool_use" && edit(block))
        .filter_map(|block| block["id"].as_str().map(str::to_owned))
        .collect();
    let names = |frame: &Value| -> bool {
        frame["message"]["content"]
            .as_array()
            .is_some_and(|content| {
                content.iter().any(|block| {
                    let id = block["id"].as_str().or(block["tool_use_id"].as_str());
                    id.is_some_and(|id| tools.iter().any(|tool| tool == id))
                })
            })
    };
    frames
        .into_iter()
        .filter(|f| (f["type"] == "assistant" || f["type"] == "user") && names(f))
        .collect()
}

/// Codex's `fileChange` items, each to send started and then completed: the recorded
/// change, or one scripted update of `long.txt`, with absolute paths in `folder`, the
/// thread's folder, as Codex sends them.
#[allow(dead_code)]
pub fn codex(text: &str, turn: &str, folder: &str) -> Vec<Value> {
    if let Some(n) = long(text) {
        let lines = scripted(n);
        let context = lines.iter().filter(|line| line.starts_with(' ')).count();
        let diff = format!(
            "@@ -1,{context} +1,{} @@\n{}\n",
            lines.len(),
            lines.join("\n")
        );
        return vec![
            json!({"type": "fileChange", "id": format!("long-edit-{turn}"),
            "changes": [{"path": format!("{folder}/long.txt"), "kind": {"type": "update", "move_path": null}, "diff": diff}],
            "status": "completed"}),
        ];
    }
    if !text.contains("replay edits") {
        return Vec::new();
    }
    CODEX
        .replace("/tmp/probe/codex-edits", folder)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|frame| {
            frame["method"] == "item/completed" && frame["params"]["item"]["type"] == "fileChange"
        })
        .map(|frame| {
            let mut item = frame["params"]["item"].clone();
            item["id"] = json!(format!("{}-{turn}", item["id"].as_str().unwrap_or("edit")));
            item
        })
        .collect()
}
