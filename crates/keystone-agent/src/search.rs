//! Search across sessions: for each session, the newest message that holds the query.
//! It reads the saved log, so it covers every session in the workspace, settled ones and
//! those on other machines included, whether or not their history is loaded.

use crate::item::ItemKind;
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
pub fn messages(
    database: &Database,
    query: &Query,
    cancelled: impl Fn() -> bool,
) -> io::Result<Option<Found>> {
    // The log saves text as JSON, which escapes quotes, backslashes and control
    // characters. For a query without them, text that holds the query has it in its
    // saved form too, so other entries need no parsing.
    let screen = !query
        .typed()
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control());
    let kinds = [
        Finder::new(br#""kind":{"type":"user_message"}"#),
        Finder::new(br#""kind":{"type":"agent_message"}"#),
    ];
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
        if done.contains(&session) || !message(payload) || (screen && !query.is_in(payload)) {
            return true;
        }
        found.parsed += 1;
        let Ok(Entry::Item { item, .. }) = serde_json::from_str(payload) else {
            return true;
        };
        if !matches!(item.kind, ItemKind::UserMessage | ItemKind::AgentMessage) {
            return true;
        }
        if let Some(first) = query.find(&item.text).into_iter().next() {
            done.insert(session);
            found.hits.push(MessageHit {
                session,
                snippet: snippet(&item.text, first),
                item: item.id,
            });
        }
        true
    })?;
    Ok((!stopped).then_some(found))
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
