//! Per-session conversation state: rows to show, the running turn, and pending
//! approvals. Provider events are applied here in batches.
//!
//! Nothing here touches the disk. Entries to log collect in an outbox that the app
//! hands to [`keystone_agent::log::Logs`], and saved history arrives through
//! [`Conversation::restore`].

use keystone_agent::log::{Entry, Write};
use keystone_agent::{
    Approval, Event, Item, ItemKind, ItemStatus, OUTPUT_CAP, TurnStatus, cap_output,
};

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
    /// In the order they first appeared. Rows are only appended or replaced in place.
    pub rows: Vec<Row>,
    pub running: bool,
    /// Approval requests waiting for an answer, oldest first.
    pub approvals: Vec<Approval>,
    pub error: Option<String>,
    /// Set while the log cannot be written. Kept apart from provider errors, so it
    /// stays visible until saving works again.
    pub save_error: Option<String>,
    /// Where the current turn's rows begin. Item IDs are matched only from here, so
    /// an ID a provider reuses in a later turn never replaces an earlier row.
    turn_start: usize,
    /// The logged place of `rows[0]`; row `i` is logged as `seq_base + i`. After a
    /// restore it continues past every place already in the log.
    seq_base: u64,
    /// While saved history is loading, what happens meanwhile waits here.
    pending: Option<Vec<Pending>>,
    writes: Vec<Write>,
}

enum Pending {
    Local(String),
    Event(Event),
}

impl Conversation {
    /// A conversation whose saved history has been requested but not restored yet.
    pub fn loading() -> Self {
        Self {
            pending: Some(Vec::new()),
            ..Self::default()
        }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Rebuilds the rows from the session's log, in the order they were first shown,
    /// then applies anything that happened while loading.
    pub fn restore(&mut self, entries: Result<Vec<Entry>, String>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match entries {
            Ok(entries) => self.rebuild(entries),
            Err(error) => self.error = Some(format!("Could not read this conversation. {error}")),
        }
        for pending in pending {
            match pending {
                Pending::Local(text) => self.push_local(&text),
                Pending::Event(event) => {
                    self.apply(event);
                }
            }
        }
    }

    fn rebuild(&mut self, entries: Vec<Entry>) {
        // Each row's place: its logged `seq`, or for older logs, its order in the file.
        let mut places: Vec<u64> = Vec::new();
        let mut next = 0;
        let mut add = |rows: &mut Vec<Row>, row: Row, seq: Option<u64>| {
            let place = seq.unwrap_or(next);
            next = next.max(place + 1);
            rows.push(row);
            places.push(place);
        };
        let mut open_turn = false;
        for entry in entries {
            match entry {
                Entry::TurnStarted => open_turn = true,
                Entry::Item { item, seq } => match self.position_for(&item) {
                    Some(index) => self.rows[index] = Row::Item(item),
                    None => add(&mut self.rows, Row::Item(item), seq),
                },
                Entry::Approval {
                    action,
                    allowed,
                    seq,
                } => add(&mut self.rows, Row::Decision { action, allowed }, seq),
                Entry::TurnEnded { status, error, seq } => {
                    open_turn = false;
                    if status != TurnStatus::Completed {
                        add(&mut self.rows, Row::TurnEnded { status, error }, seq);
                    }
                    self.turn_start = self.rows.len();
                }
            }
        }
        if open_turn {
            add(
                &mut self.rows,
                Row::TurnEnded {
                    status: TurnStatus::Interrupted,
                    error: Some("Keystone closed during this turn".into()),
                },
                None,
            );
        }
        let mut ordered: Vec<(u64, Row)> = places.into_iter().zip(self.rows.drain(..)).collect();
        // Stable, so rows with the same place keep their order in the file.
        ordered.sort_by_key(|(place, _)| *place);
        self.rows = ordered.into_iter().map(|(_, row)| row).collect();
        self.turn_start = self.rows.len();
        self.seq_base = next.saturating_sub(self.rows.len() as u64);
    }

    /// Shows and logs a sent message at once. Steers are not always echoed back, so
    /// the message is logged now; the provider's copy replaces it if one arrives.
    pub fn push_local(&mut self, text: &str) {
        if let Some(pending) = &mut self.pending {
            pending.push(Pending::Local(text.to_owned()));
            return;
        }
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
        if !self.running {
            self.turn_start = self.rows.len();
        }
        let index = self.push(Row::Item(item.clone()));
        self.log_item(index, item);
        self.running = true;
        self.error = None;
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && !self.running && self.error.is_none()
    }

    /// Applies one provider event. Returns the thread ID when the provider opened one.
    pub fn apply(&mut self, event: Event) -> Option<String> {
        if let Some(pending) = &mut self.pending {
            // The thread is saved with the session, not in this log, so it never waits.
            if let Event::ThreadReady { thread_id } = event {
                return Some(thread_id);
            }
            pending.push(Pending::Event(event));
            return None;
        }
        match event {
            Event::ThreadReady { thread_id } => return Some(thread_id),
            Event::TurnStarted => {
                self.running = true;
                self.error = None;
                self.write(Entry::TurnStarted, false);
            }
            Event::ItemStarted(item) => {
                self.place(item);
            }
            Event::ItemDelta { item_id, text } => {
                if let Some(index) = self.position(&item_id)
                    && let Row::Item(item) = &mut self.rows[index]
                {
                    item.text.push_str(&text);
                    // Command output can be huge. Cap it while it streams, not only at the
                    // end; trimming at three times the cap keeps the cost occasional.
                    if matches!(item.kind, ItemKind::Command { .. })
                        && item.text.len() > OUTPUT_CAP * 6
                    {
                        item.text = cap_output(&item.text);
                    }
                }
            }
            Event::ItemCompleted(item) => {
                let index = self.place(item.clone());
                self.log_item(index, item);
            }
            Event::ApprovalRequested(approval) => self.approvals.push(approval),
            Event::ApprovalResolved { request } => self.approvals.retain(|a| a.request != request),
            Event::TurnCompleted { status, error } => {
                self.running = false;
                self.approvals.clear();
                // A stopped turn leaves items the provider never completes. Keep what
                // streamed, so the partial reply survives a restart.
                let unfinished: Vec<(usize, Item)> = self
                    .rows
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(index, row)| match row {
                        Row::Item(item) if item.status == ItemStatus::InProgress => {
                            item.status = ItemStatus::Failed;
                            Some((index, item.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                for (index, item) in unfinished {
                    self.log_item(index, item);
                }
                let shown = status != TurnStatus::Completed;
                let entry = Entry::TurnEnded {
                    status,
                    error: error.clone(),
                    seq: shown.then(|| self.seq(self.rows.len())),
                };
                self.write(entry, true);
                if shown {
                    self.push(Row::TurnEnded { status, error });
                }
                self.turn_start = self.rows.len();
            }
            Event::Failed { message } => {
                self.running = false;
                self.error = Some(message);
                self.turn_start = self.rows.len();
            }
            Event::Warning { message } => self.error = Some(message),
        }
        None
    }

    /// Records the user's answer to the oldest pending approval.
    pub fn decide(&mut self, allowed: bool) -> Option<Approval> {
        if self.approvals.is_empty() {
            return None;
        }
        let approval = self.approvals.remove(0);
        let entry = Entry::Approval {
            action: approval.action.clone(),
            allowed,
            seq: Some(self.seq(self.rows.len())),
        };
        self.write(entry, true);
        self.push(Row::Decision {
            action: approval.action.clone(),
            allowed,
        });
        Some(approval)
    }

    /// Entries to append to the session's log, in order.
    pub fn take_writes(&mut self) -> Vec<Write> {
        std::mem::take(&mut self.writes)
    }

    /// Puts `item` in its existing row, or a new one, and returns the row.
    fn place(&mut self, item: Item) -> usize {
        match self.position_for(&item) {
            Some(index) => {
                self.rows[index] = Row::Item(item);
                index
            }
            None => self.push(Row::Item(item)),
        }
    }

    fn push(&mut self, row: Row) -> usize {
        self.rows.push(row);
        self.rows.len() - 1
    }

    fn seq(&self, index: usize) -> u64 {
        self.seq_base + index as u64
    }

    fn log_item(&mut self, index: usize, item: Item) {
        let seq = Some(self.seq(index));
        self.write(Entry::Item { item, seq }, false);
    }

    fn write(&mut self, entry: Entry, sync: bool) {
        self.writes.push(Write { entry, sync });
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

    fn user(id: &str, text: &str) -> Item {
        Item {
            id: id.into(),
            kind: ItemKind::UserMessage,
            text: text.into(),
            status: ItemStatus::Completed,
        }
    }

    fn texts(conversation: &Conversation) -> Vec<&str> {
        conversation
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) => Some(item.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Stands in for the session's log file: keeps what was written, in order.
    #[derive(Default)]
    struct Log(Vec<Entry>);

    impl Log {
        fn save(&mut self, conversation: &mut Conversation) {
            let writes = conversation.take_writes();
            self.0.extend(writes.into_iter().map(|write| write.entry));
        }

        fn restore(&self) -> Conversation {
            let mut conversation = Conversation::loading();
            conversation.restore(Ok(self.0.clone()));
            conversation
        }
    }

    /// Applies each event and saves what it logged, as the app does.
    fn feed(live: &mut Conversation, log: &mut Log, events: impl IntoIterator<Item = Event>) {
        for event in events {
            live.apply(event);
            log.save(live);
        }
    }

    fn completed() -> Event {
        Event::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
        }
    }

    #[test]
    fn events_stream_into_rows_and_reload_from_the_log() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        let thread = live.apply(Event::ThreadReady {
            thread_id: "t".into(),
        });
        assert_eq!(thread.as_deref(), Some("t"));
        feed(
            &mut live,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemStarted(message("m", "", ItemStatus::InProgress)),
                Event::ItemDelta {
                    item_id: "m".into(),
                    text: "Hel".into(),
                },
                Event::ItemDelta {
                    item_id: "m".into(),
                    text: "lo".into(),
                },
            ],
        );
        assert_eq!(
            live.rows,
            vec![Row::Item(message("m", "Hello", ItemStatus::InProgress))]
        );
        assert!(live.running);

        live.apply(Event::ApprovalRequested(Approval {
            request: RequestId("1".into()),
            item_id: None,
            action: "rm -rf build".into(),
            detail: None,
        }));
        assert_eq!(live.decide(false).unwrap().action, "rm -rf build");
        log.save(&mut live);
        feed(
            &mut live,
            &mut log,
            [
                Event::ItemCompleted(message("m", "Hello", ItemStatus::Completed)),
                Event::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    error: None,
                },
            ],
        );
        assert!(!live.running);
        // The sync points: the decision and the end of the turn.
        let synced: Vec<_> = live.take_writes();
        assert!(synced.is_empty(), "everything was saved");

        let reloaded = log.restore();
        assert_eq!(reloaded.rows, live.rows, "restored as it was shown");
        assert_eq!(
            reloaded.rows,
            vec![
                Row::Item(message("m", "Hello", ItemStatus::Completed)),
                Row::Decision {
                    action: "rm -rf build".into(),
                    allowed: false
                },
                Row::TurnEnded {
                    status: TurnStatus::Interrupted,
                    error: None
                },
            ]
        );
    }

    #[test]
    fn a_steer_is_restored_after_the_reply_it_interrupted() {
        for echoed in [true, false] {
            let (mut live, mut log) = (Conversation::default(), Log::default());
            live.push_local("Write a poem");
            log.save(&mut live);
            feed(
                &mut live,
                &mut log,
                [
                    Event::TurnStarted,
                    Event::ItemStarted(user("u1", "Write a poem")),
                    Event::ItemCompleted(user("u1", "Write a poem")),
                    Event::ItemStarted(message("m1", "Roses", ItemStatus::InProgress)),
                ],
            );
            // The steer is shown, and logged, while the first reply still streams.
            live.push_local("Make it rhyme");
            log.save(&mut live);
            let mut rest = vec![Event::ItemCompleted(message(
                "m1",
                "Roses are red",
                ItemStatus::Completed,
            ))];
            if echoed {
                rest.push(Event::ItemStarted(user("u2", "Make it rhyme")));
                rest.push(Event::ItemCompleted(user("u2", "Make it rhyme")));
            }
            rest.push(Event::ItemCompleted(message(
                "m2",
                "Violets are blue",
                ItemStatus::Completed,
            )));
            rest.push(completed());
            feed(&mut live, &mut log, rest);

            let shown = [
                "Write a poem",
                "Roses are red",
                "Make it rhyme",
                "Violets are blue",
            ];
            assert_eq!(texts(&live), shown);
            assert_eq!(texts(&log.restore()), shown, "echoed: {echoed}");
        }
    }

    #[test]
    fn an_old_log_keeps_file_order_and_new_rows_follow_it() {
        // Written before entries carried their place.
        let old = [
            r#"{"entry":"item","item":{"id":"local:1-0","kind":{"type":"user_message"},"text":"Old question","status":"completed"}}"#,
            r#"{"entry":"turn_started"}"#,
            r#"{"entry":"item","item":{"id":"local:2-1","kind":{"type":"user_message"},"text":"Old steer","status":"completed"}}"#,
            r#"{"entry":"item","item":{"id":"m","kind":{"type":"agent_message"},"text":"Old reply","status":"completed"}}"#,
            r#"{"entry":"turn_ended","status":"completed","error":null}"#,
        ];
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1.jsonl"), old.join("\n")).unwrap();
        let mut log = Log(keystone_agent::log::read(dir.path(), 1).unwrap());
        assert_eq!(log.0.len(), old.len());
        let mut live = log.restore();
        assert_eq!(texts(&live), ["Old question", "Old steer", "Old reply"]);

        live.push_local("New question");
        log.save(&mut live);
        feed(
            &mut live,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemCompleted(message("m", "New reply", ItemStatus::Completed)),
                completed(),
            ],
        );
        let shown = [
            "Old question",
            "Old steer",
            "Old reply",
            "New question",
            "New reply",
        ];
        assert_eq!(texts(&live), shown);
        let mut again = log.restore();
        assert_eq!(texts(&again), shown);

        // And places keep growing across a second restart.
        again.push_local("Third question");
        log.save(&mut again);
        assert_eq!(texts(&log.restore()).last(), Some(&"Third question"));
    }

    #[test]
    fn what_happens_while_loading_is_applied_after_the_history() {
        let mut log = Log::default();
        let mut first = Conversation::default();
        first.push_local("Earlier");
        log.save(&mut first);
        feed(&mut first, &mut log, [Event::TurnStarted, completed()]);

        let mut live = Conversation::loading();
        live.push_local("Sent while loading");
        let thread = live.apply(Event::ThreadReady {
            thread_id: "t".into(),
        });
        assert_eq!(thread.as_deref(), Some("t"), "the thread is not held back");
        live.apply(Event::TurnStarted);
        assert!(live.rows.is_empty() && live.take_writes().is_empty());
        assert!(live.is_loading());

        live.restore(Ok(log.0.clone()));
        log.save(&mut live);
        assert!(!live.is_loading() && live.running);
        assert_eq!(texts(&live), ["Earlier", "Sent while loading"]);
        assert_eq!(texts(&log.restore()), ["Earlier", "Sent while loading"]);
    }

    #[test]
    fn a_log_that_cannot_be_read_shows_an_error() {
        let mut live = Conversation::loading();
        live.restore(Err("permission denied".into()));
        assert!(!live.is_loading());
        assert!(live.error.as_deref().unwrap().contains("permission denied"));
    }

    #[test]
    fn the_providers_copy_replaces_a_locally_shown_message() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        live.push_local("Fix the build");
        live.push_local("Never echoed");
        log.save(&mut live);
        let echoed = user("u1", "Fix the build");
        feed(
            &mut live,
            &mut log,
            [
                Event::ItemStarted(echoed.clone()),
                Event::ItemCompleted(echoed.clone()),
            ],
        );
        assert_eq!(live.rows[0], Row::Item(echoed.clone()));

        // After a restart: the echoed message once, and the un-echoed steer kept.
        let mut reloaded = log.restore();
        assert_eq!(reloaded.rows.len(), 2);
        assert_eq!(reloaded.rows[0], Row::Item(echoed));
        assert!(matches!(&reloaded.rows[1], Row::Item(item) if item.text == "Never echoed"));

        // Messages sent after the restart never take the place of earlier ones.
        reloaded.push_local("After restart");
        log.save(&mut reloaded);
        assert_eq!(
            texts(&log.restore()),
            ["Fix the build", "Never echoed", "After restart"]
        );
    }

    #[test]
    fn an_id_reused_in_a_later_turn_adds_a_row() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        for text in ["first", "second"] {
            live.push_local(text);
            log.save(&mut live);
            feed(
                &mut live,
                &mut log,
                [
                    Event::TurnStarted,
                    Event::ItemCompleted(message("reused", text, ItemStatus::Completed)),
                    completed(),
                ],
            );
        }
        for conversation in [&live, &log.restore()] {
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
    fn approvals_wait_in_order_and_each_is_answered() {
        let mut live = Conversation::default();
        let approval = |id: &str| Approval {
            request: RequestId(id.into()),
            item_id: None,
            action: format!("cmd {id}"),
            detail: None,
        };
        live.apply(Event::ApprovalRequested(approval("1")));
        live.apply(Event::ApprovalRequested(approval("2")));
        assert_eq!(live.decide(true).unwrap().action, "cmd 1");
        assert_eq!(live.approvals, vec![approval("2")]);
        assert!(live.take_writes()[0].sync, "a decision is synced");
        live.apply(Event::ApprovalResolved {
            request: RequestId("2".into()),
        });
        assert!(live.approvals.is_empty());
    }

    /// Real codex-cli 0.162.1 traffic through the driver: the third turn was stopped
    /// mid-reply, and Codex never completed that message.
    #[test]
    fn a_real_stopped_codex_reply_survives_reload() {
        use keystone_agent::codex::{CodexDriver, Command, Outputs};
        const THREAD: &str = "01a1418d-549d-7553-bd08-4183fac44aa7";
        let fixture =
            include_str!("../../keystone-agent/tests/fixtures/codex-0.162.1-thread.jsonl");
        let mut out = Outputs::default();
        let mut driver = CodexDriver::new("test", &mut out);
        driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
        driver.command(
            Command::Open {
                session: 12,
                cwd: "/tmp/repo".into(),
                resume: None,
            },
            &mut out,
        );
        driver.on_line(
            format!(r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}"}}}}}}"#).as_bytes(),
            &mut out,
        );
        for line in fixture.lines() {
            driver.on_line(line.as_bytes(), &mut out);
        }
        let (mut live, mut log) = (Conversation::default(), Log::default());
        feed(&mut live, &mut log, out.events.into_iter().map(|(_, e)| e));
        let last_reply = |c: &Conversation| {
            c.rows.iter().rev().find_map(|row| match row {
                Row::Item(item) if item.kind == ItemKind::AgentMessage => Some(item.text.clone()),
                _ => None,
            })
        };
        let streamed = last_reply(&live).expect("the stopped reply streamed some text");
        assert!(!streamed.is_empty());
        let reloaded = log.restore();
        assert_eq!(last_reply(&reloaded), Some(streamed));
        assert_eq!(reloaded.rows, live.rows);
    }

    #[test]
    fn a_stopped_reply_keeps_its_streamed_text() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        feed(
            &mut live,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemStarted(message("m", "", ItemStatus::InProgress)),
                Event::ItemDelta {
                    item_id: "m".into(),
                    text: "1\n2".into(),
                },
                Event::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    error: None,
                },
            ],
        );
        assert_eq!(
            log.restore().rows[0],
            Row::Item(message("m", "1\n2", ItemStatus::Failed))
        );
    }

    #[test]
    fn a_turn_open_at_shutdown_reloads_as_interrupted() {
        let (mut live, mut log) = (Conversation::default(), Log::default());
        feed(
            &mut live,
            &mut log,
            [
                Event::TurnStarted,
                Event::ItemCompleted(message("m", "partial", ItemStatus::Completed)),
            ],
        );
        assert!(matches!(
            log.restore().rows.last(),
            Some(Row::TurnEnded {
                status: TurnStatus::Interrupted,
                error: Some(_)
            })
        ));
    }
}
