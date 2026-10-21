//! Read-only crash recovery: what a provider's own store says happened in a session,
//! and how that lines up with Keystone's neutral log.
//!
//! A [`Snapshot`] is read from the provider without starting a turn or writing
//! anything: Claude Code's session file under its config dir ([`claude`]), or
//! Codex's stored history through the app-server ([`codex`]). [`reconcile`] then
//! compares it with the neutral items Keystone saved and returns a
//! [`RecoveryBatch`]: rows to add or extend, and a report of what could and could not
//! be recovered. Nothing is resent, resumed or answered. See "Crash recovery" in
//! `docs/design/providers.md`.

pub mod claude;
pub mod codex;

use crate::item::{Item, ItemKind, ItemStatus, TurnStatus};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// What a provider recorded for a session, oldest turn first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub turns: Vec<SnapshotTurn>,
    /// What the read could not cover.
    pub limits: Vec<Limit>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapshotTurn {
    /// Items with the IDs a live session gives them. A tool call with no recorded
    /// result is [`ItemStatus::Interrupted`].
    pub items: Vec<Item>,
    /// How the provider recorded the turn ending, or `None` when its store holds no
    /// reliable end.
    pub end: Option<TurnStatus>,
}

/// Something a recovery could not cover. Conflicts stop rows from being matched;
/// the rest only limit what was recovered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "limit", rename_all = "snake_case")]
pub enum Limit {
    /// The store's last record was cut off, so it was left out.
    TornTail,
    /// Records that could not be read.
    Malformed { count: u64 },
    /// Records of kinds Keystone does not recover.
    Unsupported { count: u64 },
    /// The store changed while it was read.
    Unstable,
    /// Only part of a large store was read, or a recovery too large to save was cut.
    Truncated,
    /// The read covered only the newest part of a long history. It is dropped when
    /// that part reaches back to what Keystone already shows.
    OlderNotRead,
    /// The provider compacted the conversation, so earlier records are summaries.
    Compacted,
    /// The provider returned summaries of some turns' items, not the items.
    ItemsNotLoaded,
    /// Tool calls with no recorded outcome.
    UnknownOutcome { count: u64 },
    /// The last message sent from Keystone is not in the provider's record.
    PromptNotRecorded,
    /// Provider records before the last matching one that Keystone's history lacks.
    EarlierHistoryDiffers { count: u64 },
    /// The store holds more than one branch of the conversation.
    Branched,
    /// The store names another working directory.
    IdentityMismatch,
    /// The provider reuses an item ID, so items cannot be matched safely.
    ReusedIds,
    /// Matching items appear in a different order.
    OrderDiffers,
    /// No provider item matches Keystone's history.
    NoCommonPoint,
    /// Messages Keystone saved as complete differ from the provider's copy.
    ConflictingText { count: u64 },
}

impl Limit {
    pub fn is_conflict(&self) -> bool {
        matches!(
            self,
            Limit::Branched
                | Limit::IdentityMismatch
                | Limit::ReusedIds
                | Limit::OrderDiffers
                | Limit::NoCommonPoint
                | Limit::ConflictingText { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Everything the provider recorded is shown, and how the turn ended is known.
    Recovered,
    /// Some of it could not be recovered, or the turn's outcome is unknown.
    Partial,
    /// Keystone's history and the provider's record disagree; nothing that disagrees
    /// was changed.
    Conflict,
}

/// What the interrupted turn's provider record says about how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "turn", content = "status", rename_all = "snake_case")]
pub enum TurnEvidence {
    /// No turn was open when Keystone lost the connection.
    Unaffected,
    /// The provider recorded the turn ending with this status.
    Recorded(TurnStatus),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub outcome: Outcome,
    /// Messages added.
    pub messages: u32,
    /// Commands, file changes and other tool calls added.
    pub tools: u32,
    /// Rows Keystone had partly that the provider's copy completed.
    pub extended: u32,
    pub turn: TurnEvidence,
    #[serde(default)]
    pub limits: Vec<Limit>,
}

impl RecoveryReport {
    /// Whether the user must acknowledge the report before the next message.
    pub fn needs_acknowledgement(&self) -> bool {
        self.outcome != Outcome::Recovered
    }
}

/// One change to the restored rows. Each names the row it applies to by item ID and,
/// for a replacement, the exact old value, so a stale operation never overwrites a
/// newer row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Replace { expected: Item, item: Item },
    Insert { after: String, item: Item },
}

/// What one recovery adds to a session's log, applied as a whole on every restore.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveryBatch {
    pub id: String,
    pub operations: Vec<Operation>,
    pub report: RecoveryReport,
    /// The item the report row follows, or `None` to follow every row.
    pub report_after: Option<String>,
    /// The batch ends the turn Keystone lost track of, so restores no longer show it
    /// as closed mid-turn.
    pub closes_turn: bool,
}

/// ID prefix of a message Keystone shows before the provider echoes it.
pub const LOCAL_PREFIX: &str = "local:";

/// Most bytes one batch may serialize to, below the log's retry budget.
pub const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

fn incomplete(item: &Item) -> bool {
    matches!(
        item.status,
        ItemStatus::InProgress | ItemStatus::Interrupted
    )
}

fn same_kind(a: &ItemKind, b: &ItemKind) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// Lines `snapshot` up with `neutral`, the items Keystone shows in order, and returns
/// what to save, or `None` when there is nothing to add or report. `open_turn` means
/// Keystone lost track of the last turn: it never saw it end.
///
/// Items are matched by ID only. A message shown before its echo may pair with the
/// provider's copy by exact text, when it is the only candidate between two matched
/// items. Rows Keystone has complete are never changed; a partial row is replaced only
/// by a copy that extends it. Provider items after the last match are added after it;
/// unmatched provider items before it are reported, not added.
pub fn reconcile(neutral: &[Item], open_turn: bool, snapshot: &Snapshot) -> Option<RecoveryBatch> {
    let mut limits = snapshot.limits.clone();
    let snap: Vec<(usize, &Item)> = snapshot
        .turns
        .iter()
        .enumerate()
        .flat_map(|(turn, t)| t.items.iter().map(move |item| (turn, item)))
        .collect();
    let mut seen = HashSet::new();
    let reused = snap.iter().any(|(_, item)| !seen.insert(item.id.as_str()));
    let mut ids: HashMap<&str, Option<usize>> = HashMap::new();
    for (index, item) in neutral.iter().enumerate() {
        if item.id.starts_with(LOCAL_PREFIX) {
            continue;
        }
        ids.entry(item.id.as_str())
            .and_modify(|slot| *slot = None)
            .or_insert(Some(index));
    }
    let mut matched: Vec<Option<usize>> = Vec::with_capacity(snap.len());
    let mut ambiguous = reused;
    for (_, item) in &snap {
        matched.push(match ids.get(item.id.as_str()) {
            Some(Some(index)) => Some(*index),
            Some(None) => {
                ambiguous = true;
                None
            }
            None => None,
        });
    }
    if ambiguous {
        limits.push(Limit::ReusedIds);
        matched.iter_mut().for_each(|slot| *slot = None);
    }
    // A message shown before its echo pairs by text with its one candidate between
    // the matched items around it.
    let direct = matched.clone();
    let mut paired = vec![false; snap.len()];
    if !ambiguous {
        for (s, (_, item)) in snap.iter().enumerate() {
            if direct[s].is_some() || item.kind != ItemKind::UserMessage {
                continue;
            }
            let before = direct[..s].iter().rev().find_map(|m| *m);
            let after = direct[s + 1..].iter().find_map(|m| *m);
            let window =
                |index: usize| before.is_none_or(|b| index > b) && after.is_none_or(|a| index < a);
            let candidates: Vec<usize> = (0..neutral.len())
                .filter(|&index| {
                    window(index)
                        && neutral[index].id.starts_with(LOCAL_PREFIX)
                        && neutral[index].kind == ItemKind::UserMessage
                        && neutral[index].text == item.text
                })
                .collect();
            let rivals = snap
                .iter()
                .enumerate()
                .filter(|(other, (_, o))| {
                    direct[*other].is_none()
                        && o.kind == ItemKind::UserMessage
                        && o.text == item.text
                        && direct[..*other].iter().rev().find_map(|m| *m) == before
                        && direct[*other + 1..].iter().find_map(|m| *m) == after
                })
                .count();
            if let [only] = candidates[..]
                && rivals == 1
            {
                matched[s] = Some(only);
                paired[s] = true;
            }
        }
    }
    let order: Vec<usize> = matched.iter().flatten().copied().collect();
    if order.windows(2).any(|pair| pair[0] >= pair[1]) {
        limits.push(Limit::OrderDiffers);
        matched.iter_mut().for_each(|slot| *slot = None);
    }
    let anchor = matched.iter().rposition(Option::is_some);
    if anchor.is_some() {
        limits.retain(|limit| *limit != Limit::OlderNotRead);
    } else if !snap.is_empty()
        && !limits.iter().any(Limit::is_conflict)
        && !limits.contains(&Limit::OlderNotRead)
    {
        limits.push(Limit::NoCommonPoint);
    }

    let mut operations = Vec::new();
    let (mut conflicting, mut earlier) = (0, 0);
    let mut cursor: Option<String> = None;
    // Item IDs in the order they will show, to place the report after the last.
    let mut shown: Vec<String> = neutral.iter().map(|item| item.id.clone()).collect();
    for (s, (_, item)) in snap.iter().enumerate() {
        match (matched[s], anchor) {
            (Some(index), _) => {
                let old = &neutral[index];
                if paired[s] {
                    operations.push(Operation::Replace {
                        expected: old.clone(),
                        item: (*item).clone(),
                    });
                    shown[index] = item.id.clone();
                } else if *item != old
                    && incomplete(old)
                    && same_kind(&old.kind, &item.kind)
                    && item.text.starts_with(&old.text)
                    && item.status != ItemStatus::InProgress
                {
                    operations.push(Operation::Replace {
                        expected: old.clone(),
                        item: (*item).clone(),
                    });
                } else if incomplete(old) {
                    // The provider's copy neither continues nor precedes what streamed.
                    if !same_kind(&old.kind, &item.kind)
                        || !(item.text.starts_with(&old.text) || old.text.starts_with(&item.text))
                    {
                        conflicting += 1;
                    }
                } else if matches!(old.kind, ItemKind::AgentMessage | ItemKind::UserMessage)
                    && old.text != item.text
                    // Builds before recovery saved partial replies as failed.
                    && !(old.status == ItemStatus::Failed && item.text.starts_with(&old.text))
                {
                    conflicting += 1;
                }
                cursor = Some(item.id.clone());
            }
            (None, Some(anchor)) if s > anchor => {
                let Some(after) = cursor.clone() else {
                    continue;
                };
                let position = shown
                    .iter()
                    .position(|id| *id == after)
                    .map_or(shown.len(), |p| p + 1);
                shown.insert(position, item.id.clone());
                operations.push(Operation::Insert {
                    after,
                    item: (*item).clone(),
                });
                cursor = Some(item.id.clone());
            }
            (None, Some(_)) => earlier += 1,
            (None, None) => {}
        }
    }
    if earlier > 0 {
        limits.push(Limit::EarlierHistoryDiffers { count: earlier });
    }
    if conflicting > 0 {
        limits.push(Limit::ConflictingText { count: conflicting });
    }

    let turn = if !open_turn {
        TurnEvidence::Unaffected
    } else {
        // The provider's end applies only if it recorded the last message Keystone sent.
        let prompt = neutral
            .iter()
            .rposition(|item| item.kind == ItemKind::UserMessage);
        let recorded = prompt.is_some_and(|prompt| matched.contains(&Some(prompt)));
        match (recorded, snapshot.turns.last().and_then(|t| t.end)) {
            (true, Some(status)) => TurnEvidence::Recorded(status),
            (true, None) => TurnEvidence::Unknown,
            (false, _) => {
                if prompt.is_some() && anchor.is_some() {
                    limits.push(Limit::PromptNotRecorded);
                }
                TurnEvidence::Unknown
            }
        }
    };
    // Rows are added right after the last match, so a gap the log marked there is where
    // they go, and its marker no longer says they are lost. Filled markers are the last
    // operations, so the size cap drops them first.
    let inserts = operations
        .iter()
        .filter(|operation| matches!(operation, Operation::Insert { .. }))
        .count();
    if inserts > 0
        && let Some(last) = anchor.and_then(|anchor| matched[anchor])
    {
        for marker in neutral[last + 1..]
            .iter()
            .take_while(|item| crate::log::lost_count(item).is_some())
        {
            operations.push(Operation::Replace {
                expected: marker.clone(),
                item: crate::log::filled(marker),
            });
        }
    }
    // Keep the batch within the log's budget: drop the latest additions first, so what
    // is kept still follows on from Keystone's history.
    // Each operation's size is counted once; the report and framing fit in the margin.
    // An operation too large to fit on its own is dropped too.
    let sizes: Vec<usize> = operations
        .iter()
        .map(|operation| serde_json::to_vec(operation).map_or(usize::MAX, |bytes| bytes.len() + 1))
        .collect();
    let mut total = sizes
        .iter()
        .fold(2usize, |sum, size| sum.saturating_add(*size));
    let fill = |operation: &Operation| matches!(operation, Operation::Replace { expected, .. } if crate::log::lost_count(expected).is_some());
    let mut truncated = false;
    while total > MAX_BATCH_BYTES - 64 * 1024
        && let Some(operation) = operations.pop()
    {
        total = total.saturating_sub(sizes[operations.len()]);
        truncated |= !fill(&operation);
        if let Operation::Insert { item, .. } = operation {
            shown.retain(|id| *id != item.id);
        }
    }
    if truncated {
        limits.push(Limit::Truncated);
        // Not everything the provider recorded is added, so no gap is filled.
        operations.retain(|operation| !fill(operation));
    }
    let (mut messages, mut tools, mut extended, mut unknown) = (0, 0, 0, 0);
    for operation in &operations {
        let item = match operation {
            Operation::Replace { expected, item } => {
                extended +=
                    u32::from(expected.id == item.id && crate::log::lost_count(expected).is_none());
                item
            }
            Operation::Insert { item, .. } => {
                match item.kind {
                    ItemKind::UserMessage | ItemKind::AgentMessage | ItemKind::Reasoning => {
                        messages += 1
                    }
                    _ => tools += 1,
                }
                item
            }
        };
        unknown += u64::from(item.status == ItemStatus::Interrupted);
    }
    if unknown > 0 {
        limits.push(Limit::UnknownOutcome { count: unknown });
    }
    let outcome = if limits.iter().any(Limit::is_conflict) {
        Outcome::Conflict
    } else if !limits.is_empty() || turn == TurnEvidence::Unknown {
        Outcome::Partial
    } else {
        Outcome::Recovered
    };
    if operations.is_empty() && !open_turn && outcome == Outcome::Recovered {
        return None;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    Some(RecoveryBatch {
        id: format!("recovery-{}-{nanos}", std::process::id()),
        operations,
        report: RecoveryReport {
            outcome,
            messages,
            tools,
            extended,
            turn,
            limits,
        },
        report_after: shown.last().cloned(),
        closes_turn: open_turn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: ItemKind, text: &str, status: ItemStatus) -> Item {
        Item {
            id: id.into(),
            kind,
            text: text.into(),
            status,
        }
    }

    fn user(id: &str, text: &str) -> Item {
        item(id, ItemKind::UserMessage, text, ItemStatus::Completed)
    }

    fn reply(id: &str, text: &str, status: ItemStatus) -> Item {
        item(id, ItemKind::AgentMessage, text, status)
    }

    fn snapshot(turns: Vec<(Vec<Item>, Option<TurnStatus>)>) -> Snapshot {
        Snapshot {
            turns: turns
                .into_iter()
                .map(|(items, end)| SnapshotTurn { items, end })
                .collect(),
            limits: Vec::new(),
        }
    }

    #[test]
    fn a_lost_tail_is_added_after_the_last_match_and_reports_a_recorded_end() {
        let neutral = [user("u1", "hi")];
        let snap = snapshot(vec![(
            vec![
                user("u1", "hi"),
                reply("m:0", "Hello", ItemStatus::Completed),
            ],
            Some(TurnStatus::Completed),
        )]);
        let batch = reconcile(&neutral, true, &snap).unwrap();
        assert_eq!(
            batch.operations,
            [Operation::Insert {
                after: "u1".into(),
                item: reply("m:0", "Hello", ItemStatus::Completed)
            }]
        );
        assert_eq!(batch.report.outcome, Outcome::Recovered);
        assert_eq!(
            batch.report.turn,
            TurnEvidence::Recorded(TurnStatus::Completed)
        );
        assert_eq!(batch.report.messages, 1);
        assert_eq!(batch.report_after.as_deref(), Some("m:0"));
        assert!(batch.closes_turn);
    }

    fn gap(id: &str) -> Item {
        let mut item = item(
            &format!("{}{id}", crate::log::LOSS_MARKER_PREFIX),
            ItemKind::Other {
                name: String::new(),
            },
            "",
            ItemStatus::Failed,
        );
        crate::log::with_lost_count(&mut item, 1);
        item
    }

    #[test]
    fn only_gaps_right_after_the_last_match_are_filled_and_not_counted_as_extended() {
        let neutral = [
            user("u1", "hi"),
            gap("a"),
            gap("b"),
            user("local:2", "unsent"),
            gap("c"),
        ];
        let snap = snapshot(vec![(
            vec![
                user("u1", "hi"),
                reply("m:0", "Hello", ItemStatus::Completed),
            ],
            Some(TurnStatus::Completed),
        )]);
        let batch = reconcile(&neutral, false, &snap).unwrap();
        let filled: Vec<_> = batch
            .operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::Replace { expected, item } => {
                    assert_eq!(crate::log::lost_count(item), None);
                    Some(expected.id.clone())
                }
                Operation::Insert { .. } => None,
            })
            .collect();
        assert_eq!(filled, [gap("a").id, gap("b").id]);
        assert_eq!(batch.report.extended, 0);
        assert_eq!(batch.report.messages, 1);
    }

    #[test]
    fn gaps_left_unfilled_for_room_do_not_make_a_recovery_partial() {
        let neutral = [user("u1", "hi"), gap("a")];
        let fits = MAX_BATCH_BYTES - 64 * 1024 - 400;
        let snap = snapshot(vec![(
            vec![
                user("u1", "hi"),
                reply("m:0", &"x".repeat(fits), ItemStatus::Completed),
            ],
            Some(TurnStatus::Completed),
        )]);
        let batch = reconcile(&neutral, false, &snap).unwrap();
        assert_eq!(
            batch.report.outcome,
            Outcome::Recovered,
            "{:?}",
            batch.report
        );
        assert!(matches!(&batch.operations[..], [Operation::Insert { .. }]));
    }

    #[test]
    fn a_truncated_recovery_fills_no_gap() {
        let neutral = [user("u1", "hi"), gap("a")];
        let snap = snapshot(vec![(
            std::iter::once(user("u1", "hi"))
                .chain((0..5).map(|n| {
                    reply(
                        &format!("m:{n}"),
                        &"x".repeat(MAX_BATCH_BYTES / 4),
                        ItemStatus::Completed,
                    )
                }))
                .collect(),
            Some(TurnStatus::Completed),
        )]);
        let batch = reconcile(&neutral, false, &snap).unwrap();
        assert!(batch.report.limits.contains(&Limit::Truncated));
        assert!(
            batch
                .operations
                .iter()
                .all(|operation| matches!(operation, Operation::Insert { .. }))
        );
    }

    #[test]
    fn a_partial_row_is_extended_only_by_a_copy_that_continues_it() {
        let neutral = [
            user("u1", "hi"),
            reply("m:0", "Hel", ItemStatus::Interrupted),
            reply("m:1", "Done", ItemStatus::Completed),
        ];
        let snap = snapshot(vec![(
            vec![
                user("u1", "hi"),
                reply("m:0", "Hello", ItemStatus::Completed),
                reply("m:1", "Different", ItemStatus::Completed),
            ],
            None,
        )]);
        let batch = reconcile(&neutral, false, &snap).unwrap();
        assert_eq!(
            batch.operations,
            [Operation::Replace {
                expected: reply("m:0", "Hel", ItemStatus::Interrupted),
                item: reply("m:0", "Hello", ItemStatus::Completed)
            }]
        );
        assert_eq!(batch.report.extended, 1);
        assert_eq!(batch.report.outcome, Outcome::Conflict);
        assert!(
            batch
                .report
                .limits
                .contains(&Limit::ConflictingText { count: 1 })
        );
        // A copy that does not continue the partial text leaves it alone.
        let other = snapshot(vec![(
            vec![user("u1", "hi"), reply("m:0", "Bye", ItemStatus::Completed)],
            None,
        )]);
        let batch = reconcile(&neutral[..2], false, &other);
        assert!(batch.is_none_or(|b| b.operations.is_empty()));
    }

    #[test]
    fn a_legacy_failed_row_is_never_replaced() {
        let neutral = [user("u1", "hi"), reply("m:0", "Hel", ItemStatus::Failed)];
        let snap = snapshot(vec![(
            vec![
                user("u1", "hi"),
                reply("m:0", "Hello", ItemStatus::Completed),
            ],
            Some(TurnStatus::Completed),
        )]);
        assert!(reconcile(&neutral, false, &snap).is_none());
    }

    #[test]
    fn an_unechoed_message_pairs_by_text_only_with_one_candidate() {
        let neutral = [user("u1", "a"), user("local:1-1", "again")];
        let snap = snapshot(vec![
            (vec![user("u1", "a")], Some(TurnStatus::Completed)),
            (
                vec![user("u2", "again"), reply("m", "ok", ItemStatus::Completed)],
                None,
            ),
        ]);
        let batch = reconcile(&neutral, true, &snap).unwrap();
        assert_eq!(
            batch.operations[0],
            Operation::Replace {
                expected: user("local:1-1", "again"),
                item: user("u2", "again")
            }
        );
        assert_eq!(batch.report.turn, TurnEvidence::Unknown);
        assert_eq!(batch.report.outcome, Outcome::Partial);
        // The same prompt twice in the window stops pairing.
        let twice = snapshot(vec![
            (vec![user("u1", "a")], None),
            (vec![user("u2", "again")], None),
            (vec![user("u3", "again")], None),
        ]);
        let batch = reconcile(&neutral, true, &twice).unwrap();
        assert!(
            !batch
                .operations
                .iter()
                .any(|op| matches!(op, Operation::Replace { .. }))
        );
        assert!(batch.report.limits.contains(&Limit::PromptNotRecorded));
    }

    #[test]
    fn reused_ids_and_reordered_matches_stop_alignment() {
        let neutral = [user("u1", "a"), reply("m", "x", ItemStatus::Completed)];
        let reused = snapshot(vec![
            (
                vec![user("u1", "a"), reply("m", "x", ItemStatus::Completed)],
                None,
            ),
            (
                vec![user("u2", "b"), reply("m", "y", ItemStatus::Completed)],
                None,
            ),
        ]);
        let batch = reconcile(&neutral, true, &reused).unwrap();
        assert!(batch.operations.is_empty());
        assert_eq!(batch.report.outcome, Outcome::Conflict);
        assert!(batch.report.limits.contains(&Limit::ReusedIds));
        let reordered = snapshot(vec![(
            vec![reply("m", "x", ItemStatus::Completed), user("u1", "a")],
            None,
        )]);
        let batch = reconcile(&neutral, true, &reordered).unwrap();
        assert!(batch.operations.is_empty());
        assert!(batch.report.limits.contains(&Limit::OrderDiffers));
    }

    #[test]
    fn neutral_history_ahead_of_the_provider_is_kept_and_earlier_gaps_reported() {
        let neutral = [
            user("u1", "a"),
            reply("m1", "x", ItemStatus::Completed),
            user("u2", "b"),
        ];
        let snap = snapshot(vec![(
            vec![
                user("u0", "older"),
                user("u1", "a"),
                reply("m1", "x", ItemStatus::Completed),
            ],
            Some(TurnStatus::Completed),
        )]);
        let batch = reconcile(&neutral, true, &snap).unwrap();
        assert!(batch.operations.is_empty());
        assert!(
            batch
                .report
                .limits
                .contains(&Limit::EarlierHistoryDiffers { count: 1 })
        );
        assert!(batch.report.limits.contains(&Limit::PromptNotRecorded));
        assert_eq!(batch.report.turn, TurnEvidence::Unknown);
    }

    #[test]
    fn external_continuation_turns_are_added_after_the_agreed_boundary() {
        let neutral = [user("u1", "a"), reply("m1", "x", ItemStatus::Completed)];
        let snap = snapshot(vec![
            (
                vec![user("u1", "a"), reply("m1", "x", ItemStatus::Completed)],
                None,
            ),
            (
                vec![
                    user("u2", "from a terminal"),
                    item(
                        "t",
                        ItemKind::Command {
                            command: "ls".into(),
                            exit_code: None,
                        },
                        "",
                        ItemStatus::Interrupted,
                    ),
                ],
                None,
            ),
        ]);
        let batch = reconcile(&neutral, false, &snap).unwrap();
        assert_eq!(batch.operations.len(), 2);
        assert_eq!((batch.report.messages, batch.report.tools), (1, 1));
        assert!(
            batch
                .report
                .limits
                .contains(&Limit::UnknownOutcome { count: 1 })
        );
        assert_eq!(batch.report.turn, TurnEvidence::Unaffected);
    }

    #[test]
    fn no_common_point_is_a_conflict_with_nothing_added() {
        let neutral = [user("u1", "a")];
        let snap = snapshot(vec![(vec![user("x", "other")], None)]);
        let batch = reconcile(&neutral, true, &snap).unwrap();
        assert!(batch.operations.is_empty());
        assert_eq!(batch.report.outcome, Outcome::Conflict);
        assert!(batch.report.limits.contains(&Limit::NoCommonPoint));
    }

    #[test]
    fn a_clean_check_saves_nothing() {
        let neutral = [user("u1", "a"), reply("m1", "x", ItemStatus::Completed)];
        let snap = snapshot(vec![(neutral.to_vec(), Some(TurnStatus::Completed))]);
        assert!(reconcile(&neutral, false, &snap).is_none());
    }

    #[test]
    fn an_oversized_batch_keeps_its_earliest_additions_and_says_so() {
        let neutral = [user("u1", "a")];
        let big = "x".repeat(MAX_BATCH_BYTES / 3);
        let items: Vec<Item> = std::iter::once(user("u1", "a"))
            .chain((0..4).map(|n| reply(&format!("m{n}"), &big, ItemStatus::Completed)))
            .collect();
        let batch = reconcile(&neutral, false, &snapshot(vec![(items, None)])).unwrap();
        assert!(batch.operations.len() < 4);
        assert!(serde_json::to_vec(&batch).unwrap().len() <= MAX_BATCH_BYTES);
        assert!(batch.report.limits.contains(&Limit::Truncated));
        assert_eq!(batch.report.outcome, Outcome::Partial);
    }

    #[test]
    fn a_single_operation_too_large_for_a_batch_is_dropped_and_reported() {
        let neutral = [user("u1", "a")];
        let huge = reply("m", &"x".repeat(MAX_BATCH_BYTES + 1), ItemStatus::Completed);
        let batch = reconcile(
            &neutral,
            true,
            &snapshot(vec![(vec![user("u1", "a"), huge], None)]),
        )
        .unwrap();
        assert!(batch.operations.is_empty());
        assert!(serde_json::to_vec(&batch).unwrap().len() <= MAX_BATCH_BYTES);
        assert!(batch.report.limits.contains(&Limit::Truncated));
        assert!(batch.report.needs_acknowledgement());
    }

    #[test]
    fn many_large_additions_are_trimmed_quickly() {
        let neutral = [user("u1", "a")];
        let text = "y".repeat(8 * 1024);
        let items: Vec<Item> = std::iter::once(user("u1", "a"))
            .chain((0..1500).map(|n| reply(&format!("m{n}"), &text, ItemStatus::Completed)))
            .collect();
        let started = std::time::Instant::now();
        let batch = reconcile(&neutral, false, &snapshot(vec![(items, None)])).unwrap();
        assert!(batch.report.limits.contains(&Limit::Truncated));
        assert!(serde_json::to_vec(&batch).unwrap().len() <= MAX_BATCH_BYTES);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_provider_copy_that_contradicts_an_interrupted_row_is_a_conflict() {
        let neutral = [user("u1", "hi"), reply("m", "Hel", ItemStatus::Interrupted)];
        let snap = snapshot(vec![(
            vec![
                user("u1", "hi"),
                reply("m", "Goodbye", ItemStatus::Completed),
            ],
            None,
        )]);
        let batch = reconcile(&neutral, true, &snap).unwrap();
        assert!(batch.operations.is_empty(), "Keystone's row is kept");
        assert!(
            batch
                .report
                .limits
                .contains(&Limit::ConflictingText { count: 1 })
        );
        assert_eq!(batch.report.outcome, Outcome::Conflict);
        // A provider copy that has less of the same text is no conflict.
        let snap = snapshot(vec![(
            vec![user("u1", "hi"), reply("m", "He", ItemStatus::Completed)],
            None,
        )]);
        assert!(
            reconcile(&neutral, true, &snap).is_none_or(|batch| !batch
                .report
                .limits
                .iter()
                .any(Limit::is_conflict))
        );
    }

    #[test]
    fn an_unread_older_history_is_partial_only_when_nothing_matches() {
        let reading = |items: Vec<Item>| Snapshot {
            turns: vec![SnapshotTurn { items, end: None }],
            limits: vec![Limit::OlderNotRead],
        };
        let neutral = [
            user("u1", "hi"),
            reply("m1", "Hel", ItemStatus::Interrupted),
        ];
        let overlap = reconcile(
            &neutral,
            true,
            &reading(vec![
                user("u1", "hi"),
                reply("m1", "Hello", ItemStatus::Completed),
            ]),
        )
        .unwrap();
        assert!(!overlap.report.limits.contains(&Limit::OlderNotRead));
        let apart = reconcile(&neutral, true, &reading(vec![user("u9", "later")])).unwrap();
        assert!(apart.report.limits.contains(&Limit::OlderNotRead));
        assert!(!apart.report.limits.contains(&Limit::NoCommonPoint));
        assert_eq!(apart.report.outcome, Outcome::Partial);
    }
}
