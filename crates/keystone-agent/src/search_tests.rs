use super::messages;
use crate::item::{Item, ItemKind, ItemStatus};
use crate::log::Entry;
use keystone_core::Database;
use keystone_core::search::Query;

fn item(id: &str, kind: ItemKind, text: &str) -> Entry {
    Entry::Item {
        item: Item {
            id: id.into(),
            kind,
            text: text.into(),
            status: ItemStatus::Completed,
        },
        seq: None,
        run: None,
    }
}

fn log(database: &Database, session: u64, entries: &[Entry]) {
    let records: Vec<(String, String)> = entries
        .iter()
        .enumerate()
        .map(|(n, entry)| {
            (
                format!("test-{session}-{n}-{}", rand_key()),
                serde_json::to_string(entry).unwrap(),
            )
        })
        .collect();
    database.append_logs(session, &records).unwrap();
}

fn rand_key() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn search(database: &Database, query: &str) -> super::Found {
    messages(database, &Query::new(query).unwrap(), || false)
        .unwrap()
        .unwrap()
}

#[test]
fn each_session_shows_its_newest_message_that_matches_newest_first() {
    let database = Database::in_memory().unwrap();
    log(
        &database,
        1,
        &[
            item("u1", ItemKind::UserMessage, "Where is the Needle kept?"),
            item("a1", ItemKind::AgentMessage, "The needle is in the drawer."),
        ],
    );
    log(
        &database,
        2,
        &[item("a2", ItemKind::AgentMessage, "No match in this one.")],
    );
    log(
        &database,
        3,
        &[item("u3", ItemKind::UserMessage, "thread the NEEDLE")],
    );
    let found = search(&database, "needle");
    let hits: Vec<_> = found
        .hits
        .iter()
        .map(|hit| {
            (
                hit.session,
                hit.item.as_str(),
                &hit.snippet.text[hit.snippet.marked.clone()],
            )
        })
        .collect();
    assert_eq!(hits, [(3, "u3", "NEEDLE"), (1, "a1", "needle")]);
    assert_eq!(found.hits[1].snippet.text, "The needle is in the drawer.");
}

#[test]
fn commands_reasoning_and_tools_are_not_searched() {
    let database = Database::in_memory().unwrap();
    log(
        &database,
        1,
        &[
            item("r", ItemKind::Reasoning, "thinking about needle"),
            item(
                "c",
                ItemKind::Command {
                    command: "grep needle".into(),
                    exit_code: Some(0),
                },
                "needle.rs",
            ),
            item(
                "t",
                ItemKind::Tool {
                    name: "needle".into(),
                },
                "needle",
            ),
        ],
    );
    assert!(search(&database, "needle").hits.is_empty());
}

#[test]
fn only_entries_whose_saved_text_holds_the_query_are_parsed() {
    let database = Database::in_memory().unwrap();
    let mut entries: Vec<Entry> = (0..200)
        .map(|n| {
            item(
                &format!("m{n}"),
                ItemKind::AgentMessage,
                &format!("message {n}"),
            )
        })
        .collect();
    entries.insert(
        50,
        item("hit", ItemKind::UserMessage, "a rare word: zephyr"),
    );
    log(&database, 7, &entries);
    let found = search(&database, "Zephyr");
    assert_eq!(found.scanned, 201);
    assert_eq!(found.parsed, 1);
    assert_eq!(found.hits.len(), 1);
}

#[test]
fn a_query_with_a_quote_still_finds_the_message() {
    let database = Database::in_memory().unwrap();
    log(
        &database,
        1,
        &[item("u", ItemKind::UserMessage, "say \"hello\" back")],
    );
    let found = search(&database, "\"hello\"");
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.parsed, found.scanned);
}

#[test]
fn a_cancelled_search_returns_nothing() {
    let database = Database::in_memory().unwrap();
    let entries: Vec<Entry> = (0..3000)
        .map(|n| item(&format!("m{n}"), ItemKind::AgentMessage, "needle"))
        .collect();
    log(&database, 1, &entries);
    let result = messages(&database, &Query::new("needle").unwrap(), || true).unwrap();
    assert_eq!(result, None);
}
