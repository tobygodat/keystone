//! A launched session's result, delivered to the session whose agent launched it.
//!
//! When the launched session's turn ends, its result is saved as ready. Once the
//! parent is idle and nothing holds it back, the result starts the parent's next
//! turn, as a message the transcript shows as "Result from" the launched session.
//! The delivery is saved before the message goes, so it is never sent twice.

use super::*;
use keystone_core::DeliveryState;

const HEADER: &str = "Result from session ";
const HINT: &str = "Read its whole transcript with session_read.";
/// Bytes of a failure's text kept in the outcome line.
const ERROR_CAP: usize = 1024;

/// How a launched session's turn ended.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outcome {
    Finished,
    Stopped,
    Failed(String),
}

impl Outcome {
    /// How `event`, which ended a turn, ended it.
    pub(crate) fn of(event: &Event) -> Self {
        match event {
            Event::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            } => Self::Finished,
            Event::TurnCompleted {
                status: TurnStatus::Interrupted,
                stopped: true,
                ..
            } => Self::Stopped,
            Event::TurnCompleted { error, status, .. } => {
                Self::Failed(error.clone().unwrap_or_else(|| match status {
                    TurnStatus::Interrupted => "its turn was interrupted".into(),
                    _ => "its turn failed".into(),
                }))
            }
            Event::Failed { message } => Self::Failed(message.clone()),
            _ => Self::Failed("its turn ended".into()),
        }
    }

    fn sentence(&self) -> String {
        match self {
            Self::Finished => "It finished.".into(),
            Self::Stopped => "It was stopped.".into(),
            Self::Failed(error) => {
                let line = error.split_whitespace().collect::<Vec<_>>().join(" ");
                let mut end = line.len().min(ERROR_CAP);
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                let cut = if end < line.len() { "\u{2026}" } else { "" };
                format!("It failed: {}{cut}", &line[..end])
            }
        }
    }
}

/// Launched sessions whose turn end was saved but whose result was not, because
/// Keystone closed in between, with how their turn ended. A turn that was still
/// running when Keystone closed has no result.
pub(crate) fn unsaved_results(workspace: &keystone_core::Workspace) -> HashMap<u64, Outcome> {
    workspace
        .deliveries()
        .iter()
        .filter(|d| *d.state() == DeliveryState::Working)
        .filter_map(|d| {
            let session = workspace.sessions().iter().find(|s| s.id() == d.child())?;
            // A message its machine hasn't answered starts the turn whose end is the
            // result.
            if session.pending().iter().any(|send| !send.refused) {
                return None;
            }
            let outcome = match session.last_turn()? {
                keystone_core::LastTurn::Finished => Outcome::Finished,
                keystone_core::LastTurn::Failed => Outcome::Failed("its turn failed".into()),
                keystone_core::LastTurn::Running => return None,
            };
            Some((d.child(), outcome))
        })
        .collect()
}

/// The message that brings `child`'s result to its parent: who sent it, how its turn
/// ended, and its last reply, both ends kept when long.
pub(crate) fn message(child: u64, title: &str, outcome: &Outcome, reply: Option<&str>) -> String {
    let reply = reply.map_or_else(
        || "It sent no reply.".to_owned(),
        keystone_agent::cap_output,
    );
    format!(
        "{HEADER}{child} (\u{201c}{title}\u{201d}), which you launched.\n{}\n{HINT}\n\n{reply}",
        outcome.sentence()
    )
}

/// A result message read back: the session it came from, the sentence saying how its
/// turn ended, and its reply.
#[derive(Debug, PartialEq)]
pub(crate) struct ResultMessage<'a> {
    pub child: u64,
    pub outcome: &'a str,
    pub reply: &'a str,
}

/// Reads a message [`message`] wrote. Anything else is `None`.
pub(crate) fn parse(text: &str) -> Option<ResultMessage<'_>> {
    let (head, reply) = text.split_once("\n\n")?;
    let mut lines = head.split('\n');
    let first = lines.next()?.strip_prefix(HEADER)?;
    let digits = first.find(|c: char| !c.is_ascii_digit())?;
    let child = first[..digits].parse().ok()?;
    let outcome = lines.next()?;
    (lines.next()? == HINT && lines.next().is_none()).then_some(ResultMessage {
        child,
        outcome,
        reply,
    })
}

/// Why a ready result has not started its parent's turn yet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Hold {
    /// The user settled the parent. A result never wakes a settled session.
    Settled,
    /// The parent's history is loading, which decides whether it needs a recovery check.
    Loading,
    /// Crash recovery holds the parent's next message.
    Recovery,
    /// The parent's turn is running. The result goes once it ends.
    Turn,
    /// An approval request or a widget waits for the user.
    Answer,
}

impl Hold {
    /// What the parent's transcript says while a result from `title` waits.
    pub(crate) fn notice(self, title: &str) -> String {
        let from = format!("Result from \u{201c}{title}\u{201d}");
        match self {
            Self::Settled => format!("{from} waits until you un-settle this session."),
            Self::Loading | Self::Recovery => format!("{from} is waiting."),
            Self::Turn => format!("{from} arrives when this turn ends."),
            Self::Answer => format!("{from} arrives after your answer."),
        }
    }
}

/// The delivered results a search can tell from typed messages off the UI thread: for
/// each launched session still in the workspace whose result was delivered, the message
/// and every session that continues its parent.
#[derive(Default)]
pub(crate) struct DeliveredResults(HashMap<u64, (String, std::collections::HashSet<u64>)>);

impl DeliveredResults {
    /// The launched session a delivered result in `parent`'s transcript came from, as
    /// [`AppState::result_source`] tells it while that session exists.
    pub(crate) fn source(&self, parent: u64, text: &str) -> Option<u64> {
        let child = parse(text)?.child;
        let (message, parents) = self.0.get(&child)?;
        (parents.contains(&parent) && message.trim() == text.trim()).then_some(child)
    }
}

impl AppState {
    /// A snapshot of the delivered results, for a search on a background thread. It is
    /// read again only when the workspace changed since the last one.
    pub(crate) fn delivered_results(&self) -> Arc<DeliveredResults> {
        let revision = self.store.revision();
        if let Some((read, results)) = self.delivered.borrow().as_ref()
            && *read == revision
        {
            return results.clone();
        }
        let results = Arc::new(self.read_delivered_results());
        *self.delivered.borrow_mut() = Some((revision, results.clone()));
        results
    }

    /// The delivered results, each with the sessions that continue its parent: the
    /// parent and, through the handoff links read forward once, every session whose
    /// chain of handoffs leads back to it, as [`AppState::continues`] tells it.
    fn read_delivered_results(&self) -> DeliveredResults {
        let workspace = self.store.workspace();
        let sessions: std::collections::HashSet<u64> =
            workspace.sessions().iter().map(|s| s.id()).collect();
        let mut continuations: HashMap<u64, Vec<u64>> = HashMap::new();
        for session in workspace.sessions() {
            if let Some(from) = session.handed_off_from() {
                continuations.entry(from).or_default().push(session.id());
            }
        }
        DeliveredResults(
            workspace
                .deliveries()
                .iter()
                .filter(|d| sessions.contains(&d.child()))
                .filter_map(|d| match d.state() {
                    DeliveryState::Delivered { message } => {
                        let mut parents = std::collections::HashSet::new();
                        if sessions.contains(&d.parent()) {
                            parents.insert(d.parent());
                        }
                        let mut next = vec![d.parent()];
                        while let Some(id) = next.pop() {
                            for &later in continuations.get(&id).into_iter().flatten() {
                                if parents.insert(later) {
                                    next.push(later);
                                }
                            }
                        }
                        Some((d.child(), (message.clone(), parents)))
                    }
                    _ => None,
                })
                .collect(),
        )
    }

    /// Something changed that may let a ready result go. The next pump sends it.
    pub(crate) fn deliveries_due(&mut self) {
        self.deliveries_due = true;
        (self.wake)();
    }

    /// The session a result for `parent` goes to: the newest session that continues
    /// `parent` through handoffs and has sent its first message, else `parent`. A
    /// second opinion only reads the session it is on, so it continues nothing.
    pub(crate) fn recipient(&self, parent: u64) -> u64 {
        self.store
            .workspace()
            .sessions()
            .iter()
            .filter(|s| {
                s.id() != parent && s.last_turn().is_some() && self.continues(s.id(), parent)
            })
            .map(|s| s.id())
            .max()
            .unwrap_or(parent)
    }

    /// Whether `session` is `parent` or continues it through handoffs.
    fn continues(&self, session: u64, parent: u64) -> bool {
        let mut current = Some(session);
        while let Some(id) = current {
            if id == parent {
                return true;
            }
            current = self.session(id).and_then(|s| s.handed_off_from());
        }
        false
    }

    /// What holds back a result for `recipient`, or `None` when it can go now.
    pub(crate) fn result_hold(&self, recipient: u64) -> Option<Hold> {
        let Some(session) = self.session(recipient) else {
            return Some(Hold::Loading);
        };
        if session.settled() {
            return Some(Hold::Settled);
        }
        let Some(conversation) = self.conversations.get(&recipient) else {
            return Some(Hold::Loading);
        };
        if conversation.is_loading() {
            Some(Hold::Loading)
        } else if conversation.recovery.is_some() {
            Some(Hold::Recovery)
        } else if self.runs_turn(recipient) {
            Some(Hold::Turn)
        } else if conversation.settle_blocker().is_some() || self.pressing.contains(&recipient) {
            Some(Hold::Answer)
        } else {
            None
        }
    }

    /// The results ready for `session` that have not gone yet, each with its launched
    /// session's ID and title and what holds it back.
    pub(crate) fn waiting_results(&self, session: u64) -> Vec<(u64, String, Hold)> {
        let Some(hold) = self.result_hold(session) else {
            return Vec::new();
        };
        self.store
            .workspace()
            .deliveries()
            .iter()
            .filter(|d| {
                self.recipient(d.parent()) == session
                    && matches!(d.state(), DeliveryState::Ready { .. })
            })
            .map(|d| {
                let title = self.session(d.child()).map_or("", |s| s.title());
                (d.child(), title.to_owned(), hold)
            })
            .collect()
    }

    /// The launched session a delivered result in `parent`'s transcript came from: the
    /// one whose delivered message is exactly `text`. Text that only looks like a result
    /// names no session.
    pub(crate) fn result_source(&self, parent: u64, text: &str) -> Option<u64> {
        let child = parse(text)?.child;
        self.store
            .workspace()
            .delivery(child)
            .filter(|d| {
                self.continues(parent, d.parent())
                    && matches!(d.state(), DeliveryState::Delivered { message } if message.trim() == text.trim())
            })
            .map(|_| child)
    }

    /// Saves the result of each launched session whose turn ended, once its history
    /// has loaded: its last reply in the turn, and how the turn ended.
    pub(super) fn save_results(&mut self) {
        let unloaded: Vec<u64> = self
            .finished
            .keys()
            .copied()
            .filter(|child| !self.conversations.contains_key(child))
            .collect();
        for child in unloaded {
            self.ensure_history(child);
        }
        let loaded: Vec<u64> = self
            .finished
            .keys()
            .copied()
            .filter(|child| {
                self.conversations
                    .get(child)
                    .is_some_and(|c| !c.is_loading())
            })
            .collect();
        for child in loaded {
            let outcome = self.finished.remove(&child).expect("listed above");
            let Some(title) = self.session(child).map(|s| s.title().to_owned()) else {
                continue;
            };
            let rows = &self.conversations[&child].rows;
            let reply = rows
                .iter()
                .rev()
                .take_while(
                    |row| !matches!(row, Row::Item(item) if item.kind == ItemKind::UserMessage),
                )
                .find_map(|row| match row {
                    Row::Item(item) if item.kind == ItemKind::AgentMessage => {
                        Some(item.text.as_str())
                    }
                    _ => None,
                })
                .filter(|reply| !reply.trim().is_empty());
            let message = message(child, &title, &outcome, reply);
            let _ = self
                .store
                .set_delivery(child, DeliveryState::Ready { message });
            self.deliveries_due = true;
        }
    }

    /// Starts a turn of each idle parent with a result ready for it, one result per
    /// parent at a time. A held result waits, shown in its parent's transcript.
    pub(super) fn deliver_results(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Running
            || self.delivering
            || !self.deliveries_due && self.unsent.is_empty()
        {
            return;
        }
        // A result waits for a failed save to be saved; saving it wakes the app.
        if self.store.save_error().is_some() {
            self.store.retry();
            return;
        }
        self.delivering = true;
        if std::mem::take(&mut self.deliveries_due) {
            let ready: Vec<(u64, u64, String)> = self
                .store
                .workspace()
                .deliveries()
                .iter()
                .filter_map(|d| match d.state() {
                    DeliveryState::Ready { message } => {
                        Some((d.child(), d.parent(), message.clone()))
                    }
                    _ => None,
                })
                .collect();
            for (child, parent, message) in ready {
                let recipient = self.recipient(parent);
                if self
                    .unsent
                    .iter()
                    .any(|unsent| self.recipient(unsent.1) == recipient)
                {
                    continue;
                }
                match self.result_hold(recipient) {
                    Some(Hold::Loading) => {
                        self.ensure_history(recipient);
                        continue;
                    }
                    Some(_) => continue,
                    None => {}
                }
                // Saved before the message goes, so a crash after this never sends it
                // again. Saving it wakes the app to send it.
                let delivered = DeliveryState::Delivered {
                    message: message.clone(),
                };
                match self.store.set_delivery(child, delivered) {
                    Ok(applied) => {
                        self.store.wake_when_durable(applied.revision);
                        self.unsent.push((child, parent, message, applied.revision));
                    }
                    Err(error) => {
                        self.conversation(recipient).error = Some(format!(
                            "Result from session {child} not delivered. {error}"
                        ));
                        self.commit(recipient);
                    }
                }
            }
        }
        for (child, parent, message, revision) in std::mem::take(&mut self.unsent) {
            // A send below runs a pump, which can observe or settle this result.
            let delivered = DeliveryState::Delivered {
                message: message.clone(),
            };
            if self.store.workspace().delivery(child).map(|d| d.state()) != Some(&delivered) {
                continue;
            }
            // Where the work continues, and what holds it, may have changed while the
            // save landed. A result held now waits as a ready one, until what holds it
            // lets results go again.
            let recipient = self.recipient(parent);
            match self.result_hold(recipient) {
                Some(Hold::Loading) => {
                    self.ensure_history(recipient);
                    self.unsent.push((child, parent, message, revision));
                    continue;
                }
                Some(_) => {
                    let _ = self
                        .store
                        .set_delivery(child, DeliveryState::Ready { message });
                    continue;
                }
                None => {}
            }
            if !self.store.is_durable(revision) {
                self.unsent.push((child, parent, message, revision));
                continue;
            }
            let draft = ComposerDraft {
                session: recipient,
                text: message.clone(),
                attachments: Vec::new(),
            };
            match self.send_message_from(draft, None, SendKind::Result, cx) {
                Sent::Yes => {}
                // It waits for the changes saved before it. Saving them wakes the app.
                Sent::Later => {
                    let saving = self.store.revision();
                    self.store.wake_when_durable(saving);
                    self.unsent.push((child, parent, message, saving));
                }
                // It is tried again when something next makes it due.
                Sent::No => {
                    let _ = self
                        .store
                        .set_delivery(child, DeliveryState::Ready { message });
                }
            }
        }
        self.delivering = false;
        if self.deliveries_due {
            (self.wake)();
        }
        self.refresh(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_message_reads_back_and_other_text_does_not() {
        let text = message(
            12,
            "Fix tests",
            &Outcome::Finished,
            Some("All green.\n\nDone."),
        );
        assert_eq!(
            text,
            "Result from session 12 (\u{201c}Fix tests\u{201d}), which you launched.\nIt finished.\nRead its whole transcript with session_read.\n\nAll green.\n\nDone."
        );
        assert_eq!(
            parse(&text),
            Some(ResultMessage {
                child: 12,
                outcome: "It finished.",
                reply: "All green.\n\nDone."
            })
        );
        let failed = message(
            3,
            "Odd \u{201c}title\u{201d}, which you launched.",
            &Outcome::Failed("exit\n\ncode 2".into()),
            None,
        );
        let read = parse(&failed).unwrap();
        assert_eq!((read.child, read.outcome), (3, "It failed: exit code 2"));
        assert_eq!(read.reply, "It sent no reply.");
        for other in [
            "hello",
            "Result from session 12",
            "Result from session x (\u{201c}t\u{201d}), which you launched.\nIt finished.\nRead its whole transcript with session_read.\n\nbody",
            "Result from session 12 (\u{201c}t\u{201d}), which you launched.\nIt finished.\n\nbody",
        ] {
            assert_eq!(parse(other), None, "{other}");
        }
    }

    #[test]
    fn a_long_failure_keeps_one_short_line_and_a_long_reply_keeps_both_ends() {
        let error = "\u{e9}".repeat(ERROR_CAP);
        let sentence = Outcome::Failed(error).sentence();
        assert!(sentence.len() <= "It failed: ".len() + ERROR_CAP + "\u{2026}".len());
        assert!(sentence.ends_with('\u{2026}'));
        let reply = format!("start{}end", "x".repeat(keystone_agent::OUTPUT_CAP * 3));
        let text = message(1, "t", &Outcome::Stopped, Some(&reply));
        let read = parse(&text).unwrap();
        assert_eq!(read.outcome, "It was stopped.");
        assert!(read.reply.starts_with("start") && read.reply.ends_with("end"));
        assert!(read.reply.len() < reply.len());
    }

    #[test]
    fn each_way_a_turn_ends_has_its_outcome() {
        let completed = |status, error: Option<&str>, stopped| Event::TurnCompleted {
            status,
            error: error.map(str::to_owned),
            stopped,
        };
        assert_eq!(
            Outcome::of(&completed(TurnStatus::Completed, None, false)),
            Outcome::Finished
        );
        assert_eq!(
            Outcome::of(&completed(TurnStatus::Interrupted, None, true)),
            Outcome::Stopped
        );
        assert_eq!(
            Outcome::of(&completed(TurnStatus::Interrupted, None, false)),
            Outcome::Failed("its turn was interrupted".into())
        );
        assert_eq!(
            Outcome::of(&completed(TurnStatus::Failed, Some("quota"), false)),
            Outcome::Failed("quota".into())
        );
        assert_eq!(
            Outcome::of(&Event::Failed {
                message: "no claude".into()
            }),
            Outcome::Failed("no claude".into())
        );
    }
}
