//! Codex's stored history, as the app-server returns it without resuming the thread.
//!
//! Keystone asks for the newest [`TURNS`] turns with `thread/turns/list` and full
//! items. When that fails, as on an app-server without it or one that will not list
//! full items for a paginated thread, it asks `thread/items/list` for the newest
//! [`ITEMS`] items. Both are bounded by count; neither loads the whole thread. Items use
//! the same mapping as live notifications, so IDs match what the neutral log saved.

use super::{Limit, Snapshot, SnapshotTurn};
use crate::codex::WireItem;
use crate::item::{ItemStatus, TurnStatus};
use serde_json::Value;

/// How many of the newest turns a recovery reads.
pub const TURNS: u64 = 20;

/// How many of the newest items a recovery reads when turns cannot be listed.
pub const ITEMS: u64 = 200;

/// Decodes a `thread/turns/list` result, newest turn first, keeping at most `keep`.
pub fn decode(result: &Value, keep: usize) -> Result<Snapshot, String> {
    let Some(turns) = result["data"].as_array() else {
        return Err("Codex returned history in a form Keystone does not recognize".into());
    };
    let mut limits = Vec::new();
    if !result["nextCursor"].is_null() {
        limits.push(Limit::OlderNotRead);
    }
    let mut snapshot: Vec<SnapshotTurn> = Vec::new();
    for turn in turns.iter().take(keep) {
        let end = match turn["status"].as_str() {
            Some("completed") => Some(TurnStatus::Completed),
            Some("interrupted") => Some(TurnStatus::Interrupted),
            Some("failed") => Some(TurnStatus::Failed),
            _ => None,
        };
        if matches!(turn["itemsView"].as_str(), Some("notLoaded" | "summary"))
            && !limits.contains(&Limit::ItemsNotLoaded)
        {
            limits.push(Limit::ItemsNotLoaded);
        }
        let (items, malformed) = items(turn["items"].as_array().into_iter().flatten());
        if malformed > 0 {
            limits.push(Limit::Malformed { count: malformed });
        }
        snapshot.push(SnapshotTurn { items, end });
    }
    snapshot.reverse();
    Ok(Snapshot {
        turns: snapshot,
        limits,
    })
}

/// Decodes a `thread/items/list` result, newest entry first. Each entry wraps an
/// item with the ID of its turn; entries of one turn form one turn whose end is
/// unknown, since items carry no turn status.
pub fn decode_items(result: &Value) -> Result<Snapshot, String> {
    let Some(data) = result["data"].as_array() else {
        return Err("Codex returned history in a form Keystone does not recognize".into());
    };
    let mut limits = Vec::new();
    if !result["nextCursor"].is_null() {
        limits.push(Limit::OlderNotRead);
    }
    let entries: Vec<&Value> = data.iter().take(ITEMS as usize).collect();
    let mut turns: Vec<SnapshotTurn> = Vec::new();
    let mut current: Option<&str> = None;
    let mut malformed = 0;
    for entry in entries.into_iter().rev() {
        let (mut items, bad) = items(std::iter::once(&entry["item"]));
        malformed += bad;
        if items.is_empty() {
            continue;
        }
        let turn = entry["turnId"].as_str();
        if turns.is_empty() || turn.is_none() || turn != current {
            current = turn;
            turns.push(SnapshotTurn {
                items: Vec::new(),
                end: None,
            });
        }
        if let Some(last) = turns.last_mut() {
            last.items.append(&mut items);
        }
    }
    if malformed > 0 {
        limits.push(Limit::Malformed { count: malformed });
    }
    Ok(Snapshot { turns, limits })
}

/// Maps stored items as live ones are mapped. An item still running when it was
/// stored has no recorded outcome.
fn items<'a>(values: impl Iterator<Item = &'a Value>) -> (Vec<crate::item::Item>, u64) {
    let (mut items, mut malformed) = (Vec::new(), 0);
    for value in values {
        // An item with no ID could never be matched, so it is not one Keystone knows.
        let Ok(wire) = <WireItem as serde::Deserialize>::deserialize(value) else {
            malformed += 1;
            continue;
        };
        if value["id"].as_str().is_none_or(str::is_empty) {
            malformed += 1;
            continue;
        }
        let mut item = wire.into_item(false);
        if item.status == ItemStatus::InProgress {
            item.status = ItemStatus::Interrupted;
        }
        items.push(item);
    }
    (items, malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::ItemKind;
    use serde_json::json;

    #[test]
    fn turns_list_pages_are_reversed_and_running_commands_have_no_outcome() {
        let page = json!({"data": [
            {"id": "t2", "status": "inProgress", "itemsView": "full", "items": [
                {"type": "userMessage", "id": "u2", "content": [{"type": "text", "text": "two"}]},
                {"type": "commandExecution", "id": "c", "command": "ls", "status": "inProgress"}
            ]},
            {"id": "t1", "status": "completed", "itemsView": "full", "items": [
                {"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "one"}]},
                {"type": "agentMessage", "id": "m1", "text": "ONE"}
            ]}
        ], "nextCursor": null});
        let snapshot = decode(&page, TURNS as usize).unwrap();
        assert_eq!(snapshot.turns.len(), 2);
        assert_eq!(snapshot.turns[0].end, Some(TurnStatus::Completed));
        assert_eq!(snapshot.turns[0].items[1].text, "ONE");
        assert_eq!(snapshot.turns[1].end, None);
        assert_eq!(snapshot.turns[1].items[1].status, ItemStatus::Interrupted);
        assert!(matches!(
            snapshot.turns[1].items[1].kind,
            ItemKind::Command { .. }
        ));
        assert!(snapshot.limits.is_empty());
    }

    #[test]
    fn summaries_and_unknown_shapes_are_reported_not_treated_as_empty() {
        let summary = json!({"data": [
            {"id": "t1", "status": "completed", "itemsView": "summary", "items": []}
        ], "nextCursor": null});
        assert_eq!(
            decode(&summary, TURNS as usize).unwrap().limits,
            [Limit::ItemsNotLoaded]
        );
        assert!(decode(&json!({"thread": {}}), TURNS as usize).is_err());
        assert!(decode_items(&json!({"thread": {}})).is_err());
    }

    #[test]
    fn only_the_newest_turns_and_items_are_read_and_older_ones_are_reported_unread() {
        let message = |n: usize| json!({"type": "agentMessage", "id": format!("m{n}"), "text": format!("{n}")});
        let turn = |n: usize| {
            json!({"id": format!("t{n}"), "status": "completed", "itemsView": "full",
                "items": [message(n)]})
        };
        let page = json!({"data": (0..25).rev().map(turn).collect::<Vec<_>>(), "nextCursor": "c"});
        let snapshot = decode(&page, TURNS as usize).unwrap();
        assert_eq!(snapshot.turns.len(), TURNS as usize);
        assert_eq!(snapshot.turns[0].items[0].text, "5");
        assert_eq!(snapshot.limits, [Limit::OlderNotRead]);
        let last = json!({"data": [turn(1)], "nextCursor": null});
        assert!(decode(&last, TURNS as usize).unwrap().limits.is_empty());

        // Codex 0.163's ThreadItemEntry: the item wrapped with its turn's ID.
        let entry = |n: usize| {
            json!({"turnId": format!("t{}", n / 100), "item": message(n),
                "startedAtMs": null, "completedAtMs": null})
        };
        let items =
            json!({"data": (0..250).rev().map(entry).collect::<Vec<_>>(), "nextCursor": "c"});
        let snapshot = decode_items(&items).unwrap();
        let read: Vec<&str> = snapshot
            .turns
            .iter()
            .flat_map(|turn| &turn.items)
            .map(|i| i.text.as_str())
            .collect();
        assert_eq!(read.len(), ITEMS as usize);
        assert_eq!((read[0], *read.last().unwrap()), ("50", "249"));
        assert_eq!(snapshot.turns.len(), 3, "t0 from 50, t1, t2");
        assert!(snapshot.turns.iter().all(|turn| turn.end.is_none()));
        assert_eq!(snapshot.limits, [Limit::OlderNotRead]);
        // A bare item where an entry belongs is reported, never read as an empty item.
        let bare = json!({"data": [message(1)], "nextCursor": null});
        let snapshot = decode_items(&bare).unwrap();
        assert!(snapshot.turns.is_empty());
        assert_eq!(snapshot.limits, [Limit::Malformed { count: 1 }]);
    }
}
