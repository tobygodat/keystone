//! Per-session conversation state: rows to show, the running turn, a pending approval,
//! and the session's log. Provider events are applied here in batches.

use keystone_agent::log::{self, Entry, SessionLog};
use keystone_agent::{Approval, Event, Item, ItemKind, ItemStatus, TurnStatus};
use std::path::Path;

/// ID prefix for a sent message shown before the provider echoes it.
const LOCAL: &str = "local:";

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Item(Item),
    /// A turn that did not complete normally.
    TurnEnded {
        status: TurnStatus,
        error: Option<String>,
    },
    /// A recorded approval decision.
    Decision {
        action: String,
        allowed: bool,
    },
}

#[derive(Default)]
pub struct Conversation {
    pub rows: Vec<Row>,
    pub running: bool,
    pub approval: Option<Approval>,
    pub error: Option<String>,
    /// Where the current turn's rows begin. Item IDs are matched only from here, so
    /// an ID a provider reuses in a later turn never replaces an earlier row.
    turn_start: usize,
    log: Option<SessionLog>,
    log_error: bool,
}

impl Conversation {
    /// Rebuilds the visible rows from the session's log.
    pub fn load(dir: &Path, session: u64) -> Self {
        let mut conversation = Self::default();
        let entries = match log::read(dir, session) {
            Ok(entries) => entries,
            Err(error) => {
                conversation.error = Some(format!("Could not read this conversation. {error}"));
                return conversation;
            }
        };
        let mut open_turn = false;
        for entry in entries {
            match entry {
                Entry::TurnStarted => open_turn = true,
                Entry::Item { item } => match conversation.position_for(&item) {
                    Some(index) => conversation.rows[index] = Row::Item(item),
                    None => conversation.rows.push(Row::Item(item)),
                },
                Entry::Approval { action, allowed } => {
                    conversation.rows.push(Row::Decision { action, allowed })
                }
                Entry::TurnEnded { status, error } => {
                    open_turn = false;
                    if status != TurnStatus::Completed {
                        conversation.rows.push(Row::TurnEnded { status, error });
                    }
                    conversation.turn_start = conversation.rows.len();
                }
            }
        }
        if open_turn {
            conversation.rows.push(Row::TurnEnded {
                status: TurnStatus::Interrupted,
                error: Some("Keystone closed during this turn".into()),
            });
        }
        conversation.turn_start = conversation.rows.len();
        conversation
    }

    /// Shows and saves a sent message at once. Steers are not always echoed back, so
    /// the message is logged now; the provider's copy replaces it if one arrives.
    pub fn push_local(&mut self, dir: &Path, session: u64, text: &str) {
        // Unique across restarts: a reloaded log already holds earlier local IDs.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let item = Item {
            id: format!("{LOCAL}{nanos}-{}", self.rows.len()),
            kind: ItemKind::UserMessage,
            text: text.to_owned(),
            status: ItemStatus::Completed,
        };
        self.write(dir, session, &Entry::Item { item: item.clone() }, false);
        if !self.running {
            self.turn_start = self.rows.len();
        }
        self.rows.push(Row::Item(item));
        self.running = true;
        self.error = None;
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && !self.running && self.error.is_none()
    }

    /// Applies one provider event. Returns the thread ID when the provider opened one.
    pub fn apply(&mut self, dir: &Path, session: u64, event: Event) -> Option<String> {
        match event {
            Event::ThreadReady { thread_id } => return Some(thread_id),
            Event::TurnStarted => {
                self.running = true;
                self.error = None;
                self.write(dir, session, &Entry::TurnStarted, false);
            }
            Event::ItemStarted(item) => match self.position_for(&item) {
                Some(index) => self.rows[index] = Row::Item(item),
                None => self.rows.push(Row::Item(item)),
            },
            Event::ItemDelta { item_id, text } => {
                if let Some(index) = self.position(&item_id)
                    && let Row::Item(item) = &mut self.rows[index]
                {
                    item.text.push_str(&text);
                }
            }
            Event::ItemCompleted(item) => {
                self.write(dir, session, &Entry::Item { item: item.clone() }, false);
                match self.position_for(&item) {
                    Some(index) => self.rows[index] = Row::Item(item),
                    None => self.rows.push(Row::Item(item)),
                }
            }
            Event::ApprovalRequested(approval) => self.approval = Some(approval),
            Event::ApprovalResolved { request } => {
                if self.approval.as_ref().is_some_and(|a| a.request == request) {
                    self.approval = None;
                }
            }
            Event::TurnCompleted { status, error } => {
                self.running = false;
                self.approval = None;
                // A stopped turn leaves items the provider never completes. Keep what
                // streamed, so the partial reply survives a restart.
                let unfinished: Vec<Item> = self
                    .rows
                    .iter_mut()
                    .filter_map(|row| match row {
                        Row::Item(item) if item.status == ItemStatus::InProgress => {
                            item.status = ItemStatus::Failed;
                            Some(item.clone())
                        }
                        _ => None,
                    })
                    .collect();
                for item in unfinished {
                    self.write(dir, session, &Entry::Item { item }, false);
                }
                let entry = Entry::TurnEnded {
                    status,
                    error: error.clone(),
                };
                self.write(dir, session, &entry, true);
                if status != TurnStatus::Completed {
                    self.rows.push(Row::TurnEnded { status, error });
                }
                self.turn_start = self.rows.len();
            }
            Event::Failed { message } => {
                self.running = false;
                self.error = Some(message);
                self.turn_start = self.rows.len();
            }
        }
        None
    }

    /// Records the user's answer to the pending approval.
    pub fn decide(&mut self, dir: &Path, session: u64, allowed: bool) -> Option<Approval> {
        let approval = self.approval.take()?;
        let entry = Entry::Approval {
            action: approval.action.clone(),
            allowed,
        };
        self.write(dir, session, &entry, true);
        self.rows.push(Row::Decision {
            action: approval.action.clone(),
            allowed,
        });
        Some(approval)
    }

    /// The row for `item`: the same ID, or the local copy of the same user message.
    fn position_for(&self, item: &Item) -> Option<usize> {
        self.position(&item.id).or_else(|| {
            (item.kind == ItemKind::UserMessage).then(|| {
                self.rows[self.turn_start..]
                    .iter()
                    .position(|row| {
                        matches!(row, Row::Item(local) if local.id.starts_with(LOCAL) && local.text == item.text)
                    })
                    .map(|index| self.turn_start + index)
            })?
        })
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.rows[self.turn_start..]
            .iter()
            .rposition(|row| matches!(row, Row::Item(item) if item.id == id))
            .map(|index| self.turn_start + index)
    }

    fn write(&mut self, dir: &Path, session: u64, entry: &Entry, sync: bool) {
        if self.log.is_none() && !self.log_error {
            match SessionLog::open(dir, session) {
                Ok(log) => self.log = Some(log),
                Err(error) => self.log_failed(error),
            }
        }
        let Some(log) = self.log.as_mut() else { return };
        let result = log
            .append(entry)
            .and_then(|()| if sync { log.sync() } else { Ok(()) });
        if let Err(error) = result {
            self.log = None;
            self.log_failed(error);
        }
    }

    fn log_failed(&mut self, error: std::io::Error) {
        self.log_error = true;
        self.error = Some(format!("This conversation is not being saved. {error}"));
    }
}

/// Whether a row has anything to show. Reasoning without a summary is skipped.
pub fn visible(row: &Row) -> bool {
    !matches!(row, Row::Item(item) if item.kind == ItemKind::Reasoning && item.text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_agent::RequestId;

    fn message(id: &str, text: &str, status: ItemStatus) -> Item {
        Item {
            id: id.into(),
            kind: ItemKind::AgentMessage,
            text: text.into(),
            status,
        }
    }

    #[test]
    fn events_stream_into_rows_and_reload_from_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Conversation::default();
        let thread = live.apply(
            dir.path(),
            3,
            Event::ThreadReady {
                thread_id: "t".into(),
            },
        );
        assert_eq!(thread.as_deref(), Some("t"));
        live.apply(dir.path(), 3, Event::TurnStarted);
        live.apply(
            dir.path(),
            3,
            Event::ItemStarted(message("m", "", ItemStatus::InProgress)),
        );
        live.apply(
            dir.path(),
            3,
            Event::ItemDelta {
                item_id: "m".into(),
                text: "Hel".into(),
            },
        );
        live.apply(
            dir.path(),
            3,
            Event::ItemDelta {
                item_id: "m".into(),
                text: "lo".into(),
            },
        );
        assert_eq!(
            live.rows,
            vec![Row::Item(message("m", "Hello", ItemStatus::InProgress))]
        );
        assert!(live.running);

        live.apply(
            dir.path(),
            3,
            Event::ApprovalRequested(Approval {
                request: RequestId("1".into()),
                item_id: None,
                action: "rm -rf build".into(),
                detail: None,
            }),
        );
        assert_eq!(
            live.decide(dir.path(), 3, false).unwrap().action,
            "rm -rf build"
        );
        live.apply(
            dir.path(),
            3,
            Event::ItemCompleted(message("m", "Hello", ItemStatus::Completed)),
        );
        live.apply(
            dir.path(),
            3,
            Event::TurnCompleted {
                status: TurnStatus::Interrupted,
                error: None,
            },
        );
        assert!(!live.running);

        let reloaded = Conversation::load(dir.path(), 3);
        assert_eq!(
            reloaded.rows,
            vec![
                Row::Decision {
                    action: "rm -rf build".into(),
                    allowed: false
                },
                Row::Item(message("m", "Hello", ItemStatus::Completed)),
                Row::TurnEnded {
                    status: TurnStatus::Interrupted,
                    error: None
                },
            ]
        );
    }

    #[test]
    fn the_providers_copy_replaces_a_locally_shown_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Conversation::default();
        live.push_local(dir.path(), 5, "Fix the build");
        live.push_local(dir.path(), 5, "Never echoed");
        let echoed = Item {
            id: "u1".into(),
            kind: ItemKind::UserMessage,
            text: "Fix the build".into(),
            status: ItemStatus::Completed,
        };
        live.apply(dir.path(), 5, Event::ItemStarted(echoed.clone()));
        live.apply(dir.path(), 5, Event::ItemCompleted(echoed.clone()));
        assert_eq!(live.rows[0], Row::Item(echoed.clone()));

        // After a restart: the echoed message once, and the un-echoed steer kept.
        let mut reloaded = Conversation::load(dir.path(), 5);
        assert_eq!(reloaded.rows.len(), 2);
        assert_eq!(reloaded.rows[0], Row::Item(echoed));
        assert!(matches!(&reloaded.rows[1], Row::Item(item) if item.text == "Never echoed"));

        // Messages sent after the restart never take the place of earlier ones.
        reloaded.push_local(dir.path(), 5, "After restart");
        let again = Conversation::load(dir.path(), 5);
        let texts: Vec<_> = again
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) => Some(item.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Fix the build", "Never echoed", "After restart"]);
    }

    #[test]
    fn an_id_reused_in_a_later_turn_adds_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Conversation::default();
        for text in ["first", "second"] {
            live.push_local(dir.path(), 9, text);
            live.apply(dir.path(), 9, Event::TurnStarted);
            live.apply(
                dir.path(),
                9,
                Event::ItemCompleted(message("reused", text, ItemStatus::Completed)),
            );
            live.apply(
                dir.path(),
                9,
                Event::TurnCompleted {
                    status: TurnStatus::Completed,
                    error: None,
                },
            );
        }
        for conversation in [&live, &Conversation::load(dir.path(), 9)] {
            let replies: Vec<_> = conversation
                .rows
                .iter()
                .filter_map(|row| match row {
                    Row::Item(item) if item.kind == ItemKind::AgentMessage => {
                        Some(item.text.as_str())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(replies, ["first", "second"]);
        }
    }

    #[test]
    fn a_stopped_reply_keeps_its_streamed_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Conversation::default();
        live.apply(dir.path(), 6, Event::TurnStarted);
        live.apply(
            dir.path(),
            6,
            Event::ItemStarted(message("m", "", ItemStatus::InProgress)),
        );
        live.apply(
            dir.path(),
            6,
            Event::ItemDelta {
                item_id: "m".into(),
                text: "1\n2".into(),
            },
        );
        live.apply(
            dir.path(),
            6,
            Event::TurnCompleted {
                status: TurnStatus::Interrupted,
                error: None,
            },
        );
        let reloaded = Conversation::load(dir.path(), 6);
        assert_eq!(
            reloaded.rows[0],
            Row::Item(message("m", "1\n2", ItemStatus::Failed))
        );
    }

    #[test]
    fn a_turn_open_at_shutdown_reloads_as_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Conversation::default();
        live.apply(dir.path(), 4, Event::TurnStarted);
        live.apply(
            dir.path(),
            4,
            Event::ItemCompleted(message("m", "partial", ItemStatus::Completed)),
        );
        drop(live);
        let reloaded = Conversation::load(dir.path(), 4);
        assert!(matches!(
            reloaded.rows.last(),
            Some(Row::TurnEnded {
                status: TurnStatus::Interrupted,
                error: Some(_)
            })
        ));
    }
}
