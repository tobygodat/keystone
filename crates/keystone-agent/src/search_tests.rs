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
    database.append_logs(session, &records, None).unwrap();
}

fn rand_key() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// What a message's row draws: a user message as typed, and a reply's Markdown through
/// [`markdown`].
fn drawn(_session: u64, item: &Item) -> Vec<String> {
    match item.kind {
        ItemKind::AgentMessage => markdown(&item.text),
        _ => vec![item.text.clone()],
    }
}

/// A stand-in for the Markdown a reply's row draws: bold markers go, and a link shows
/// its label without its address.
fn markdown(source: &str) -> Vec<String> {
    let mut out = String::new();
    let mut rest = source.replace("**", "");
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|at| open + at) else {
            break;
        };
        let end = rest[close..]
            .find(')')
            .map_or(rest.len(), |at| close + at + 1);
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..close]);
        rest = rest[end..].to_owned();
    }
    out.push_str(&rest);
    vec![out]
}

fn search(database: &Database, query: &str) -> super::Found {
    messages(database, &Query::new(query).unwrap(), drawn, || false)
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
    let result = messages(&database, &Query::new("needle").unwrap(), drawn, || true).unwrap();
    assert_eq!(result, None);
}

#[test]
fn a_reply_is_searched_as_its_drawn_text() {
    let database = Database::in_memory().unwrap();
    log(
        &database,
        1,
        &[item(
            "a",
            ItemKind::AgentMessage,
            "See [the guide](https://zebra.example/guide) for the **sharp needle**.",
        )],
    );
    // The link's address is in the source, not in what the row draws.
    assert!(search(&database, "zebra").hits.is_empty());
    for query in ["the guide for", "sharp needle"] {
        let found = search(&database, query);
        assert_eq!(found.hits.len(), 1, "{query}");
        let snippet = &found.hits[0].snippet;
        assert_eq!(snippet.text, "See the guide for the sharp needle.");
        assert_eq!(snippet.text[snippet.marked.clone()].to_lowercase(), query);
    }
}

#[test]
fn a_user_message_is_searched_as_typed() {
    let database = Database::in_memory().unwrap();
    log(
        &database,
        1,
        &[item("u", ItemKind::UserMessage, "keep **these** stars")],
    );
    assert_eq!(search(&database, "**these**").hits.len(), 1);
    assert!(search(&database, "keep these").hits.is_empty());
}

#[test]
fn a_control_character_inside_a_word_is_still_found() {
    let database = Database::in_memory().unwrap();
    log(
        &database,
        1,
        &[item("u", ItemKind::UserMessage, "a bell\u{7}ringer here")],
    );
    let found = search(&database, "bell\u{7}ringer");
    assert_eq!(found.hits.len(), 1, "the screen skips what JSON escapes");
}
