//! Claude Code's own session store, read without starting Claude.
//!
//! Claude Code keeps each session as a JSON Lines file named after the session ID in a
//! folder under `<config dir>/projects/`, where the config dir is `CLAUDE_CONFIG_DIR`
//! or `~/.claude`. Records form a chain through `parentUuid`. Only the session's own
//! file is read: it is found by name in any project folder, without guessing how
//! Claude names folders, and never through a link that leaves the config dir. Nothing
//! the file says is followed or run. Synthetic fixtures under
//! `tests/fixtures/recovery/` pin the layout this reader expects; an unknown layout
//! is reported as not recovered.

use super::{Limit, Snapshot, SnapshotTurn};
use crate::claude::{DENIED, exit_code_in, join_text, tool_kind};
use crate::item::{Item, ItemKind, ItemStatus, TurnStatus, cap_output};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Most bytes read from one session file: its end, when it is longer.
pub const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
/// Longer records are skipped as malformed.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Most records decoded from one file.
pub const MAX_RECORDS: usize = 20_000;

pub const NOT_FOUND: &str =
    "Claude Code has no saved conversation for this session in this account's folder";
const UNSAFE_ID: &str = "the saved Claude session ID is not one Keystone can look up safely";

/// Reads session `session` from the config dir `root`. `cwd` is the session's working
/// directory, which the file must agree with. An error means nothing was read.
pub fn read(root: &Path, session: &str, cwd: &Path) -> Result<Snapshot, String> {
    if session.is_empty()
        || session.len() > 128
        || !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(UNSAFE_ID.into());
    }
    let root = fs::canonicalize(root).map_err(|_| NOT_FOUND.to_owned())?;
    let projects = fs::canonicalize(root.join("projects"))
        .ok()
        .filter(|projects| projects.starts_with(&root) && projects.is_dir())
        .ok_or_else(|| NOT_FOUND.to_owned())?;
    let name = format!("{session}.jsonl");
    let mut found: Vec<PathBuf> = Vec::new();
    let entries = fs::read_dir(&projects)
        .map_err(|error| format!("Claude Code's projects folder could not be read: {error}"))?;
    for entry in entries.flatten() {
        let Ok(folder) = fs::canonicalize(entry.path()) else {
            continue;
        };
        if !folder.starts_with(&projects) || !folder.is_dir() {
            continue;
        }
        let path = folder.join(&name);
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_file())
            && !found.contains(&path)
        {
            found.push(path);
        }
    }
    let mut snapshots = Vec::new();
    for path in &found {
        snapshots.push(read_file(path, &projects, cwd).map_err(|error| {
            format!("Claude Code's saved conversation could not be read: {error}")
        })?);
    }
    let mut matching: Vec<Snapshot> = snapshots
        .into_iter()
        .filter(|snapshot| !snapshot.limits.contains(&Limit::IdentityMismatch))
        .collect();
    match (found.len(), matching.len()) {
        (0, _) => Err(NOT_FOUND.into()),
        (_, 0) => Ok(Snapshot {
            turns: Vec::new(),
            limits: vec![Limit::IdentityMismatch],
        }),
        (_, 1) => Ok(matching.remove(0)),
        _ => Ok(Snapshot {
            turns: Vec::new(),
            limits: vec![Limit::Branched],
        }),
    }
}

#[derive(PartialEq)]
struct Identity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    inode: u64,
}

fn identity(meta: &fs::Metadata) -> Identity {
    Identity {
        len: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        inode: std::os::unix::fs::MetadataExt::ino(meta),
    }
}

/// Opens `path` without following a final symlink and checks that what was opened is
/// a regular file inside `projects`, so a swap after the folder scan reads nothing
/// outside Claude's folder.
fn open_inside(path: &Path, projects: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_NONBLOCK,
    );
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("it is not a regular file"));
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let opened = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        if !opened.starts_with(projects) {
            return Err(std::io::Error::other("it is outside Claude Code's folder"));
        }
    }
    Ok(file)
}

fn read_file(path: &Path, projects: &Path, cwd: &Path) -> std::io::Result<Snapshot> {
    let mut file = open_inside(path, projects)?;
    let before = identity(&file.metadata()?);
    let start = before.len.saturating_sub(MAX_SCAN_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    (&mut file).take(MAX_SCAN_BYTES).read_to_end(&mut bytes)?;
    let mut limits = Vec::new();
    if identity(&file.metadata()?) != before
        || fs::symlink_metadata(path).map(|meta| identity(&meta)).ok() != Some(before)
    {
        limits.push(Limit::Unstable);
    }
    let mut data = &bytes[..];
    if start > 0 {
        limits.push(Limit::OlderNotRead);
        data = data
            .iter()
            .position(|b| *b == b'\n')
            .map_or(&[][..], |n| &data[n + 1..]);
    }
    match data.iter().rposition(|b| *b == b'\n') {
        Some(end) if end + 1 < data.len() => {
            limits.push(Limit::TornTail);
            data = &data[..=end];
        }
        None if !data.is_empty() => {
            limits.push(Limit::TornTail);
            data = &[];
        }
        _ => {}
    }
    let cwd = cwd.to_string_lossy();
    let mut malformed = 0;
    let mut records: Vec<Value> = Vec::new();
    let mut checked_cwd = false;
    // The latest records matter most: past the cap, the oldest are dropped. Found by
    // scanning back from the end, so the dropped lines are never indexed.
    let (mut kept, mut from, mut end) = (0, 0, data.len());
    while end > 0 {
        // `data` ends in a newline, so each line here is `data[start..end]` with its own.
        let start = data[..end - 1]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |n| n + 1);
        if end - start > 1 {
            if kept == MAX_RECORDS {
                limits.push(Limit::OlderNotRead);
                from = end;
                break;
            }
            kept += 1;
        }
        end = start;
    }
    for line in data[from..]
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record = (line.len() <= MAX_LINE_BYTES)
            .then(|| serde_json::from_slice::<Value>(line).ok())
            .flatten();
        let Some(record) = record.filter(Value::is_object) else {
            malformed += 1;
            continue;
        };
        if record["isSidechain"] == true || record["uuid"].as_str().is_none() {
            continue;
        }
        if !checked_cwd && let Some(stored) = record["cwd"].as_str() {
            checked_cwd = true;
            if stored != cwd {
                return Ok(Snapshot {
                    turns: Vec::new(),
                    limits: vec![Limit::IdentityMismatch],
                });
            }
        }
        records.push(record);
    }
    if malformed > 0 {
        limits.push(Limit::Malformed { count: malformed });
    }
    let chain = match chain(&records) {
        Ok(chain) => chain,
        Err(limit) => {
            limits.push(limit);
            return Ok(Snapshot {
                turns: Vec::new(),
                limits,
            });
        }
    };
    let mut turns = decode(&chain, &mut limits);
    turns.retain(|turn| !turn.items.is_empty() || turn.end.is_some());
    Ok(Snapshot { turns, limits })
}

/// The one branch from the root to the latest message, oldest first. A store with two
/// message branches is not guessed at.
fn chain(records: &[Value]) -> Result<Vec<&Value>, Limit> {
    let by_id: HashMap<&str, &Value> = records
        .iter()
        .filter_map(|r| Some((r["uuid"].as_str()?, r)))
        .collect();
    let parents: HashSet<&str> = records.iter().filter_map(parent).collect();
    let leaves: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r["type"] != "system" && !parents.contains(r["uuid"].as_str().unwrap_or_default())
        })
        .collect();
    let leaf = match leaves[..] {
        [] => return Ok(Vec::new()),
        [leaf] => leaf,
        _ => return Err(Limit::Branched),
    };
    let mut chain = vec![leaf];
    let mut seen = HashSet::new();
    let mut current = leaf;
    while let Some(parent) = parent(current) {
        let Some(next) = by_id.get(parent) else {
            break;
        };
        if !seen.insert(parent) {
            return Err(Limit::Branched);
        }
        chain.push(next);
        current = next;
    }
    chain.reverse();
    Ok(chain)
}

/// A record's parent. A compaction boundary starts a new chain with no `parentUuid`
/// and names the last record before it in `logicalParentUuid`.
fn parent(record: &Value) -> Option<&str> {
    record["parentUuid"]
        .as_str()
        .or_else(|| record["logicalParentUuid"].as_str())
}

fn interrupted_note(text: &str) -> bool {
    text.starts_with("[Request interrupted")
}

fn decode(chain: &[&Value], limits: &mut Vec<Limit>) -> Vec<SnapshotTurn> {
    let mut turns: Vec<SnapshotTurn> = Vec::new();
    let mut blocks: HashMap<String, usize> = HashMap::new();
    let mut unsupported = 0;
    let mut compacted = false;
    for record in chain {
        match record["type"].as_str() {
            Some("system") => {
                compacted |= record["subtype"] == "compact_boundary";
            }
            Some("user") => {
                if record["isCompactSummary"] == true {
                    compacted = true;
                    continue;
                }
                if record["isMeta"] == true {
                    continue;
                }
                let uuid = record["uuid"].as_str().unwrap_or_default();
                let text = match &record["message"]["content"] {
                    Value::String(text) => Some(text.clone()),
                    Value::Array(parts) => {
                        for part in parts.iter().filter(|p| p["type"] == "tool_result") {
                            complete_tool(&mut turns, part);
                        }
                        let text_parts = parts.iter().any(|p| p["type"] == "text");
                        text_parts.then(|| join_text(parts))
                    }
                    _ => None,
                };
                let Some(text) = text else { continue };
                if interrupted_note(&text) {
                    if let Some(turn) = turns.last_mut() {
                        turn.end = Some(TurnStatus::Interrupted);
                    }
                    continue;
                }
                turns.push(SnapshotTurn {
                    items: vec![Item {
                        id: uuid.to_owned(),
                        kind: ItemKind::UserMessage,
                        text,
                        status: ItemStatus::Completed,
                    }],
                    end: None,
                });
            }
            Some("assistant") => {
                if record["isApiErrorMessage"] == true {
                    continue;
                }
                let message = &record["message"];
                let Some(id) = message["id"].as_str() else {
                    unsupported += 1;
                    continue;
                };
                let Some(content) = message["content"].as_array() else {
                    continue;
                };
                if turns.is_empty() {
                    turns.push(SnapshotTurn::default());
                }
                let turn = turns.last_mut().expect("pushed above");
                for block in content {
                    let counter = blocks.entry(id.to_owned()).or_default();
                    let index = *counter;
                    *counter += 1;
                    let text_of = |key: &str| block[key].as_str().unwrap_or_default().to_owned();
                    match block["type"].as_str() {
                        Some("text") => turn.items.push(Item {
                            id: format!("{id}:{index}"),
                            kind: ItemKind::AgentMessage,
                            text: text_of("text"),
                            status: ItemStatus::Completed,
                        }),
                        // Reasoning and its signatures are not recovered.
                        Some("thinking" | "redacted_thinking") => {}
                        Some("tool_use") => turn.items.push(Item {
                            id: text_of("id"),
                            kind: tool_kind(
                                block["name"].as_str().unwrap_or_default(),
                                &block["input"],
                            ),
                            text: String::new(),
                            status: ItemStatus::Interrupted,
                        }),
                        _ => unsupported += 1,
                    }
                }
            }
            _ => {}
        }
    }
    if compacted {
        limits.push(Limit::Compacted);
    }
    if unsupported > 0 {
        limits.push(Limit::Unsupported { count: unsupported });
    }
    turns
}

/// Records a tool result on its call, as a live session does.
fn complete_tool(turns: &mut [SnapshotTurn], part: &Value) {
    let id = part["tool_use_id"].as_str().unwrap_or_default();
    let Some(item) = turns
        .iter_mut()
        .rev()
        .flat_map(|turn| turn.items.iter_mut())
        .find(|item| item.id == id && item.status == ItemStatus::Interrupted)
    else {
        return;
    };
    let is_error = part["is_error"].as_bool().unwrap_or(false);
    let text = match &part["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => join_text(parts),
        _ => String::new(),
    };
    let declined = is_error && text == DENIED;
    if let ItemKind::Command { exit_code, .. } = &mut item.kind {
        *exit_code = match (declined, is_error) {
            (true, _) => None,
            (false, false) => Some(0),
            (false, true) => exit_code_in(&text),
        };
    }
    item.status = match (declined, is_error) {
        (true, _) => ItemStatus::Declined,
        (false, true) => ItemStatus::Failed,
        (false, false) => ItemStatus::Completed,
    };
    item.text = cap_output(&text);
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn store(records: &[serde_json::Value]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("projects/p");
        fs::create_dir_all(&folder).unwrap();
        let text: String = records.iter().map(|r| format!("{r}\n")).collect();
        fs::write(folder.join("s.jsonl"), text).unwrap();
        let root = dir.path().to_owned();
        (dir, root)
    }

    fn user(uuid: &str, parent: Option<&str>, text: &str) -> serde_json::Value {
        serde_json::json!({"type": "user", "uuid": uuid, "parentUuid": parent, "cwd": "/w",
            "sessionId": "s", "message": {"role": "user", "content": text}})
    }

    #[test]
    fn a_long_store_keeps_its_latest_records() {
        let mut records = vec![user("u0", None, "first")];
        for n in 1..=MAX_RECORDS + 10 {
            records.push(user(
                &format!("u{n}"),
                Some(&format!("u{}", n - 1)),
                &format!("m{n}"),
            ));
        }
        let (_dir, root) = store(&records);
        let snapshot = read(&root, "s", Path::new("/w")).unwrap();
        assert!(snapshot.limits.contains(&Limit::OlderNotRead));
        let last = snapshot.turns.last().unwrap().items.last().unwrap();
        assert_eq!(last.text, format!("m{}", MAX_RECORDS + 10));
    }

    #[test]
    fn a_compacted_store_reads_across_its_boundary_as_one_branch() {
        let records = [
            user("u1", None, "before"),
            serde_json::json!({"type": "system", "subtype": "compact_boundary", "uuid": "b",
                "parentUuid": null, "logicalParentUuid": "u1", "cwd": "/w", "sessionId": "s"}),
            serde_json::json!({"type": "user", "uuid": "sum", "parentUuid": "b", "cwd": "/w",
                "sessionId": "s", "isCompactSummary": true,
                "message": {"role": "user", "content": "summary"}}),
            user("u2", Some("sum"), "after"),
        ];
        let (_dir, root) = store(&records);
        let snapshot = read(&root, "s", Path::new("/w")).unwrap();
        assert!(
            !snapshot.limits.contains(&Limit::Branched),
            "{:?}",
            snapshot.limits
        );
        assert!(snapshot.limits.contains(&Limit::Compacted));
        let texts: Vec<&str> = snapshot
            .turns
            .iter()
            .flat_map(|turn| &turn.items)
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(texts, ["before", "after"]);
    }

    /// What the folder scan saw can be swapped before the open: a final symlink, or a
    /// project folder replaced by a link out of Claude's folder, is never read.
    #[test]
    fn a_swapped_path_is_not_followed_out_of_claudes_folder() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let outside = dir.path().join("outside");
        fs::create_dir_all(projects.join("p")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("s.jsonl"), "{}\n").unwrap();
        fs::write(projects.join("p/s.jsonl"), "{}\n").unwrap();
        let projects = fs::canonicalize(&projects).unwrap();
        assert!(open_inside(&projects.join("p/s.jsonl"), &projects).is_ok());

        fs::remove_file(projects.join("p/s.jsonl")).unwrap();
        std::os::unix::fs::symlink(outside.join("s.jsonl"), projects.join("p/s.jsonl")).unwrap();
        assert!(open_inside(&projects.join("p/s.jsonl"), &projects).is_err());

        fs::remove_dir_all(projects.join("p")).unwrap();
        std::os::unix::fs::symlink(&outside, projects.join("p")).unwrap();
        let error = open_inside(&projects.join("p/s.jsonl"), &projects).unwrap_err();
        assert!(error.to_string().contains("outside"), "{error}");
    }
}
