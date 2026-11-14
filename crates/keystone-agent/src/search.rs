//! Search across sessions: for each session, the newest message that holds the query.
//! It reads the saved log, so it covers every session in the workspace, settled ones and
//! those on other machines included, whether or not their history is loaded.

use crate::item::{Item, ItemKind};
use crate::log::Entry;
use keystone_core::Database;
use keystone_core::search::{Query, Snippet, snippet};
use memchr::memmem::Finder;
use std::collections::HashSet;
use std::io;

/// A message that holds the query, and the session it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageHit {
    pub session: u64,
    /// The message's item ID, which finds its row in the session's transcript.
    pub item: String,
    /// The line around the message's first match.
    pub snippet: Snippet,
}

/// What a search found, newest message first, and the work it took.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub hits: Vec<MessageHit>,
    /// Log entries read.
    pub scanned: usize,
    /// Log entries parsed: only saved messages whose text holds the query are.
    pub parsed: usize,
}

/// Rows read between checks of `cancelled`.
const CHECK_EVERY: usize = 1024;

/// How far into a saved entry its kind is looked for. An item's kind follows its ID,
/// near the start, so a long command output is never read to learn it is not a
/// message.
const KIND_WITHIN: usize = 512;

/// The newest user or agent message of each session that holds `query`, newest first.
/// Returns `None` when `cancelled` turns true first, so a newer query can take over.
/// Tool output, reasoning and commands are not searched: the sidebar finds what was
/// said.
///
/// Each message is searched as the texts its row draws, which `drawn` gives for the
/// session and the message: a reply's Markdown texts, so a link's address or the
/// asterisks around a bold word are not found, just as find in the session does not
/// find them.
pub fn messages<T: AsRef<str>>(
    database: &Database,
    query: &Query,
    drawn: impl Fn(u64, &Item) -> Vec<T>,
    cancelled: impl Fn() -> bool,
) -> io::Result<Option<Found>> {
    // Markdown's syntax is ASCII punctuation and whitespace, and the log saves text as
    // JSON, which escapes quotes, backslashes and control characters. So a message whose
    // drawn text holds the query holds the query's longest run of other characters in
    // its saved form, and other entries need no parsing.
    let screen = longest_word(query.typed()).and_then(Query::new);
    let kinds = [
        Finder::new(br#""kind":{"type":"user_message"}"#),
        Finder::new(br#""kind":{"type":"agent_message"}"#),
    ];
    // Whether an entry is a saved message, by its kind near its start.
    let message = |payload: &str| {
        let head = &payload.as_bytes()[..payload.len().min(KIND_WITHIN)];
        kinds.iter().any(|kind| kind.find(head).is_some())
    };
    let mut found = Found::default();
    let mut done = HashSet::new();
    let mut stopped = false;
    database.scan_logs(|session, payload| {
        found.scanned += 1;
        if found.scanned.is_multiple_of(CHECK_EVERY) && cancelled() {
            stopped = true;
            return false;
        }
        if done.contains(&session) {
            return true;
        }
        if !message(payload) || screen.as_ref().is_some_and(|word| !word.is_in(payload)) {
            return true;
        }
        found.parsed += 1;
        let Ok(Entry::Item { item, .. }) = serde_json::from_str(payload) else {
            return true;
        };
        if !matches!(item.kind, ItemKind::UserMessage | ItemKind::AgentMessage) {
            return true;
        }
        let texts = drawn(session, &item);
        if let Some((text, first)) = texts.iter().find_map(|text| {
            let text = text.as_ref();
            Some((text, query.find(text).into_iter().next()?))
        }) {
            done.insert(session);
            found.hits.push(MessageHit {
                session,
                snippet: snippet(text, first),
                item: item.id,
            });
        }
        true
    })?;
    Ok((!stopped).then_some(found))
}

/// The longest run of `typed` without whitespace, ASCII punctuation or control
/// characters, if any.
fn longest_word(typed: &str) -> Option<&str> {
    typed
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation() || c.is_control())
        .max_by_key(|word| word.chars().count())
        .filter(|word| !word.is_empty())
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
