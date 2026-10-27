//! Importing a conversation a provider already has into a new Keystone session: the
//! ones that ran in a folder, and the log rows the session starts with. Both read the
//! provider's store through the read-only recovery readers ([`crate::recovery`]) and
//! never write to it.

use crate::item::{ItemKind, TurnStatus};
use crate::log::Entry;
use crate::recovery::{self, Limit, Snapshot};
use serde_json::Value;
use std::path::Path;

/// How many of a conversation's newest turns an import shows.
pub const TURNS: usize = 50;

/// How many of a folder's newest conversations are listed.
pub const LISTED: usize = 50;

/// One conversation a provider has for a folder.
#[derive(Clone, Debug, PartialEq)]
pub struct Listed {
    /// The provider's own ID, which the imported session resumes.
    pub id: String,
    /// The provider's title, else the first message.
    pub title: String,
    /// When it last changed, in unix milliseconds.
    pub updated: Option<u64>,
    /// How many turns it has, unless the provider could not count them.
    pub turns: Option<usize>,
    /// Only part of the history was counted, so it has more than `turns`.
    pub more: bool,
}

/// The conversations Claude Code keeps in the config dir `root` that ran in `cwd`,
/// newest first. One its reader cannot follow, such as a branched one, is left out.
pub fn claude_sessions(root: &Path, cwd: &Path) -> Result<Vec<Listed>, String> {
    let mut listed = Vec::new();
    for (id, modified) in recovery::claude::sessions(root, cwd)?
        .into_iter()
        .take(LISTED)
    {
        let Ok(snapshot) = recovery::claude::read(root, &id, cwd) else {
            continue;
        };
        if snapshot.turns.is_empty() {
            continue;
        }
        let updated = modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as u64);
        listed.push(Listed {
            title: first_message(&snapshot).unwrap_or_default(),
            id,
            updated,
            turns: Some(snapshot.turns.len()),
            more: snapshot.limits.contains(&Limit::OlderNotRead),
        });
    }
    Ok(listed)
}

fn first_message(snapshot: &Snapshot) -> Option<String> {
    snapshot
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .find(|item| item.kind == ItemKind::UserMessage)
        .map(|item| item.text.clone())
}

/// Decodes a Codex `thread/list` result. Turns are not counted yet.
pub fn codex_threads(result: &Value) -> Result<Vec<Listed>, String> {
    let Some(data) = result["data"].as_array() else {
        return Err("its answer is in a form Keystone does not recognize".into());
    };
    Ok(data
        .iter()
        .filter_map(|thread| {
            let id = thread["id"].as_str().filter(|id| !id.is_empty())?;
            let title = thread["name"]
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .or_else(|| thread["preview"].as_str())
                .unwrap_or_default();
            Some(Listed {
                id: id.to_owned(),
                title: title.to_owned(),
                updated: thread["updatedAt"].as_u64().map(|secs| secs * 1000),
                turns: None,
                more: false,
            })
        })
        .collect())
}

/// The log a session imported from `snapshot` starts with: its newest [`TURNS`]
/// turns, after [`Entry::OlderNotImported`] when older ones are left out. A read that
/// found nothing to import, or a conversation that no longer matches what was listed,
/// such as one that branched or moved folder, is refused.
pub fn log(snapshot: &Snapshot) -> Result<Vec<Entry>, String> {
    if snapshot.turns.is_empty() || snapshot.limits.iter().any(Limit::is_conflict) {
        return Err("It changed since it was listed.".into());
    }
    let skip = snapshot.turns.len().saturating_sub(TURNS);
    let mut entries = Vec::new();
    if skip > 0 || snapshot.limits.contains(&Limit::OlderNotRead) {
        entries.push(Entry::OlderNotImported);
    }
    for turn in &snapshot.turns[skip..] {
        entries.extend(turn.items.iter().map(|item| Entry::Item {
            item: item.clone(),
            seq: None,
            run: None,
        }));
        entries.push(Entry::TurnEnded {
            status: turn.end.unwrap_or(TurnStatus::Completed),
            error: None,
            limit: None,
            seq: None,
            run: None,
        });
    }
    Ok(entries)
}

/// `snapshot` from the oldest item of `shown` on, for a session imported with its
/// older history left out, which shows only `shown`. A recovery then does not report
/// that older history as differing from the transcript. With none of `shown` in it,
/// `snapshot` is kept whole.
pub fn imported_part(mut snapshot: Snapshot, shown: &[crate::Item]) -> Snapshot {
    let ids: std::collections::HashSet<&str> = shown.iter().map(|item| item.id.as_str()).collect();
    let first = snapshot.turns.iter().enumerate().find_map(|(turn, t)| {
        t.items
            .iter()
            .position(|item| ids.contains(item.id.as_str()))
            .map(|item| (turn, item))
    });
    if let Some((turn, item)) = first {
        snapshot.turns.drain(..turn);
        snapshot.turns[0].items.drain(..item);
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{Item, ItemStatus};
    use crate::recovery::SnapshotTurn;
    use serde_json::json;

    fn turn(n: usize) -> SnapshotTurn {
        let item = |id: String, kind| Item {
            id,
            kind,
            text: format!("{n}"),
            status: ItemStatus::Completed,
        };
        SnapshotTurn {
            items: vec![
                item(format!("u{n}"), ItemKind::UserMessage),
                item(format!("a{n}"), ItemKind::AgentMessage),
            ],
            end: None,
        }
    }

    fn texts(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Item { item, .. } => Some(item.id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_short_history_is_imported_whole_one_turn_at_a_time() {
        let mut snapshot = Snapshot {
            turns: vec![turn(1), turn(2)],
            limits: Vec::new(),
        };
        snapshot.turns[1].end = Some(TurnStatus::Interrupted);
        let entries = log(&snapshot).unwrap();
        assert_eq!(texts(&entries), ["u1", "a1", "u2", "a2"]);
        assert!(!entries.contains(&Entry::OlderNotImported));
        let ends: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::TurnEnded { status, .. } => Some(*status),
                _ => None,
            })
            .collect();
        assert_eq!(ends, [TurnStatus::Completed, TurnStatus::Interrupted]);
        assert!(matches!(entries.last(), Some(Entry::TurnEnded { .. })));
    }

    #[test]
    fn only_the_newest_turns_are_imported_after_a_note() {
        let snapshot = Snapshot {
            turns: (0..TURNS + 3).map(turn).collect(),
            limits: Vec::new(),
        };
        let entries = log(&snapshot).unwrap();
        assert_eq!(entries[0], Entry::OlderNotImported);
        let ids = texts(&entries);
        assert_eq!(ids.len(), TURNS * 2);
        assert_eq!(ids[0], "u3");
        assert_eq!(ids.last().unwrap(), &format!("a{}", TURNS + 2));

        // A store read only in part has older turns too, however few were read.
        let partial = Snapshot {
            turns: vec![turn(1)],
            limits: vec![Limit::OlderNotRead],
        };
        assert_eq!(log(&partial).unwrap()[0], Entry::OlderNotImported);
    }

    #[test]
    fn a_recovery_after_an_import_ignores_the_history_left_out() {
        let snapshot = Snapshot {
            turns: (0..TURNS + 3).map(turn).collect(),
            limits: Vec::new(),
        };
        let shown: Vec<_> = log(&snapshot)
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Item { item, .. } => Some(item),
                _ => None,
            })
            .collect();
        let earlier = |batch: Option<recovery::RecoveryBatch>| {
            batch.is_some_and(|batch| {
                batch
                    .report
                    .limits
                    .iter()
                    .any(|limit| matches!(limit, Limit::EarlierHistoryDiffers { .. }))
            })
        };
        assert!(earlier(recovery::reconcile(&shown, false, &snapshot)));

        let part = imported_part(snapshot, &shown);
        assert_eq!(part.turns.len(), TURNS);
        assert!(!earlier(recovery::reconcile(&shown, false, &part)));
    }

    #[test]
    fn a_conversation_with_nothing_to_import_or_that_conflicts_is_refused() {
        let empty = Snapshot::default();
        assert!(log(&empty).is_err());
        for conflict in [Limit::Branched, Limit::IdentityMismatch] {
            let changed = Snapshot {
                turns: vec![turn(1)],
                limits: vec![conflict],
            };
            assert!(log(&changed).is_err());
        }
    }

    fn record(uuid: &str, parent: Option<&str>, cwd: &str, text: &str) -> String {
        let record = json!({"type": "user", "uuid": uuid, "parentUuid": parent, "cwd": cwd,
            "sessionId": "s", "message": {"role": "user", "content": text}});
        format!("{record}\n")
    }

    #[test]
    fn claude_sessions_are_those_that_ran_in_the_folder_newest_first_and_untouched() {
        let root = tempfile::tempdir().unwrap();
        let write = |folder: &str, name: &str, text: String| {
            let folder = root.path().join("projects").join(folder);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(name), text).unwrap();
        };
        let older = record("u1", None, "/w", "Older one");
        write("-w", "older.jsonl", older);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer =
            record("v1", None, "/w", "Newer one") + &record("v2", Some("v1"), "/w", "again");
        write("-w", "newer.jsonl", newer);
        write(
            "-other",
            "elsewhere.jsonl",
            record("x1", None, "/other", "Elsewhere"),
        );
        // A subagent's records come first here, and name another folder.
        let side = json!({"type": "user", "uuid": "s1", "isSidechain": true, "cwd": "/other",
            "message": {"role": "user", "content": "side"}});
        write(
            "-w",
            "sidechain-first.jsonl",
            format!("{side}\n") + &record("y1", None, "/w", "Main"),
        );
        write("-w", "notes.txt", record("z1", None, "/w", "not a session"));
        write(
            "-w",
            "unsafe id.jsonl",
            record("z2", None, "/w", "bad name"),
        );
        let before: Vec<_> = walk(root.path());

        let listed = claude_sessions(root.path(), Path::new("/w")).unwrap();

        let ids: Vec<&str> = listed.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, ["sidechain-first", "newer", "older"]);
        assert_eq!(listed[1].title, "Newer one");
        assert_eq!(listed[1].turns, Some(2));
        assert!(!listed[1].more);
        assert!(listed[1].updated.unwrap() >= listed[2].updated.unwrap());
        assert_eq!(walk(root.path()), before);
        assert!(
            claude_sessions(&root.path().join("absent"), Path::new("/w"))
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_session_recorded_under_another_name_for_the_folder_is_found_and_read() {
        let root = tempfile::tempdir().unwrap();
        let place = tempfile::tempdir().unwrap();
        let real = place.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = place.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let folder = root.path().join("projects/-alias");
        std::fs::create_dir_all(&folder).unwrap();
        let text = record("a1", None, alias.to_str().unwrap(), "Through the alias");
        std::fs::write(folder.join("aliased.jsonl"), text).unwrap();

        let real = std::fs::canonicalize(&real).unwrap();
        let listed = claude_sessions(root.path(), &real).unwrap();

        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].title, "Through the alias");
        let snapshot = recovery::claude::read(root.path(), "aliased", &real).unwrap();
        assert!(log(&snapshot).is_ok(), "{:?}", snapshot.limits);
    }

    /// Every file under `root` with its bytes and modification time.
    fn walk(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>, std::time::SystemTime)> {
        let mut files = Vec::new();
        let mut folders = vec![root.to_owned()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(folder).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    folders.push(path);
                } else {
                    let modified = entry.metadata().unwrap().modified().unwrap();
                    files.push((path.clone(), std::fs::read(&path).unwrap(), modified));
                }
            }
        }
        files.sort();
        files
    }

    #[test]
    fn codex_threads_take_the_name_else_the_preview() {
        let page = json!({"data": [
            {"id": "t1", "name": "Named", "preview": "first message", "updatedAt": 1_700_000_000},
            {"id": "t2", "name": null, "preview": "only a preview", "updatedAt": 1_700_000_100},
            {"id": "", "preview": "no id"}
        ], "nextCursor": null});
        let listed = codex_threads(&page).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].title, "Named");
        assert_eq!(listed[0].updated, Some(1_700_000_000_000));
        assert_eq!(listed[1].title, "only a preview");
        assert!(codex_threads(&json!({})).is_err());
    }
}
