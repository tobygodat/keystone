//! How much of each streaming reply is on screen.
//!
//! Providers deliver reply text in bursts. The conversation keeps every byte at once,
//! so logs, approvals, and turn state never wait. Only the drawn reply trails it,
//! catching up with each burst within [`CATCH_UP`]. The shown text is the source of
//! the row's [`IncrementalParser`], always a prefix of the reply ending on a grapheme
//! boundary.

use keystone_agent::{Item, ItemStatus};
use keystone_ui::markdown::{Document, IncrementalParser};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_segmentation::GraphemeCursor;

pub const TICK: Duration = Duration::from_millis(16);
/// The longest a burst is held back. Text arriving during a burst shares its deadline.
const CATCH_UP: Duration = Duration::from_millis(120);
/// Graphemes shown at once when a reply that was caught up grows, so a small delta
/// has no added wait.
const LEAD: usize = 8;

pub type Key = (u64, usize);

#[derive(Default)]
pub struct Replies {
    rows: HashMap<Key, Reply>,
}

#[derive(Default)]
struct Reply {
    parser: IncrementalParser,
    burst: Option<Burst>,
}

struct Burst {
    last: Instant,
    deadline: Instant,
}

impl Replies {
    /// Takes in a reply row of the session on screen after it changed. Text added to a
    /// streaming reply is paced; anything else is shown as it is.
    #[cfg(test)]
    pub fn observe(&mut self, key: Key, item: &Item, now: Instant) {
        self.observe_text(key, &item.text, item.status, now);
    }

    pub fn observe_text(&mut self, key: Key, text: &str, status: ItemStatus, now: Instant) {
        let fresh = !self.rows.contains_key(&key);
        if fresh && status != ItemStatus::InProgress {
            return;
        }
        let reply = self.rows.entry(key).or_default();
        if status != ItemStatus::InProgress {
            reply.parser.finish(text);
            reply.burst = None;
            return;
        }
        if fresh || !text.starts_with(reply.parser.source()) {
            reply.parser.set_text(text);
            reply.burst = None;
            return;
        }
        let shown = reply.parser.source().len();
        if text.len() == shown {
            return;
        }
        match &reply.burst {
            None => {
                reply.advance(text, LEAD);
                if reply.parser.source().len() < text.len() {
                    reply.burst = Some(Burst {
                        last: now,
                        deadline: now + CATCH_UP,
                    });
                }
            }
            Some(burst) if now >= burst.deadline => {
                reply.parser.set_text(text);
                reply.burst = None;
            }
            Some(_) => {}
        }
    }

    /// Advances every pacing reply to `now`. `canonical` gives a row's current item.
    /// Returns the rows whose shown text changed.
    pub fn tick<'a>(
        &mut self,
        now: Instant,
        canonical: impl Fn(Key) -> Option<&'a Item>,
    ) -> Vec<Key> {
        let mut changed = Vec::new();
        self.rows.retain(|&key, reply| {
            let Some(burst) = &mut reply.burst else {
                return true;
            };
            let Some(item) = canonical(key) else {
                return false;
            };
            if item.status != ItemStatus::InProgress
                || now >= burst.deadline
                || !item.text.starts_with(reply.parser.source())
            {
                reply.parser.set_text(&item.text);
                reply.burst = None;
            } else {
                let rest = &item.text[reply.parser.source().len()..];
                let remaining = unicode_segmentation::UnicodeSegmentation::graphemes(rest, true)
                    .count() as u128;
                let elapsed = now.saturating_duration_since(burst.last).as_nanos();
                let window = burst
                    .deadline
                    .saturating_duration_since(burst.last)
                    .as_nanos();
                let count = (remaining * elapsed).div_ceil(window.max(1)).max(1);
                burst.last = now;
                reply.advance(&item.text, count.min(usize::MAX as u128) as usize);
                if reply.parser.source().len() == item.text.len() {
                    reply.burst = None;
                }
            }
            changed.push(key);
            true
        });
        changed
    }

    pub fn pacing(&self) -> bool {
        self.rows.values().any(|reply| reply.burst.is_some())
    }

    /// Shows every reply of `session` in full at its next draw. Returns the rows that
    /// were pacing.
    pub fn settle(&mut self, session: u64) -> Vec<usize> {
        self.rows
            .iter_mut()
            .filter(|((s, _), reply)| *s == session && reply.burst.is_some())
            .map(|(&(_, index), reply)| {
                reply.burst = None;
                index
            })
            .collect()
    }

    /// Drops what was shown of `session`'s replies. Its rows went unobserved while it
    /// was off screen, so their shown text is stale; they start again from the reply.
    pub fn forget(&mut self, session: u64) {
        self.rows.retain(|&(s, _), _| s != session);
    }

    /// The Markdown to draw for a reply row. A reply that is not pacing shows `item`
    /// in full. While it streams, unclosed inline markers show as closed, so styling
    /// does not jump when the closer arrives.
    pub fn document(&mut self, key: Key, item: &Item) -> Arc<Document> {
        let reply = self.rows.entry(key).or_default();
        if reply.burst.is_some() {
            return reply.parser.display();
        }
        if item.status == ItemStatus::InProgress {
            reply.parser.set_text(&item.text);
            reply.parser.display()
        } else {
            reply.parser.finish(&item.text)
        }
    }
}

impl Reply {
    /// Shows up to `count` more graphemes of `text`, which extends the shown text. A
    /// shown end that later text joined into a grapheme counts as inside it.
    fn advance(&mut self, text: &str, count: usize) {
        let mut cursor = GraphemeCursor::new(self.parser.source().len(), text.len(), true);
        let mut end = self.parser.source().len();
        for _ in 0..count {
            match cursor.next_boundary(text, 0) {
                Ok(Some(next)) => end = next,
                _ => {
                    end = text.len();
                    break;
                }
            }
        }
        self.parser.set_text(&text[..end]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_agent::ItemKind;

    const KEY: Key = (1, 0);

    fn item(text: &str, status: ItemStatus) -> Item {
        Item {
            id: "reply".into(),
            kind: ItemKind::AgentMessage,
            text: text.into(),
            status,
        }
    }

    fn streaming(text: &str) -> Item {
        item(text, ItemStatus::InProgress)
    }

    fn shown(replies: &Replies) -> &str {
        replies.rows[&KEY].parser.source()
    }

    fn ms(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn a_burst_reveals_over_frames_and_is_caught_up_by_its_deadline() {
        let start = Instant::now();
        let mut replies = Replies::default();
        replies.observe(KEY, &streaming(""), start);
        let burst = streaming(&"word ".repeat(200));
        replies.observe(KEY, &burst, start);
        assert_eq!(shown(&replies), "word wor", "a small lead shows at once");
        assert!(replies.pacing());

        let mut previous = shown(&replies).len();
        let mut frames = 0;
        let mut now = start;
        while replies.pacing() {
            now += TICK;
            assert!(now <= ms(start, 120 + 16), "caught up by the deadline");
            replies.tick(now, |_| Some(&burst));
            let len = shown(&replies).len();
            assert!(len > previous, "each frame shows more");
            assert!(burst.text.starts_with(shown(&replies)));
            previous = len;
            frames += 1;
        }
        assert_eq!(shown(&replies), burst.text);
        assert!(frames >= 5, "spread over frames, not one jump: {frames}");
    }

    #[test]
    fn text_arriving_during_a_burst_shares_its_deadline() {
        let start = Instant::now();
        let mut replies = Replies::default();
        replies.observe(KEY, &streaming(""), start);
        let short = streaming(&"a".repeat(100));
        replies.observe(KEY, &short, start);
        replies.tick(ms(start, 100), |_| Some(&short));
        let longer = streaming(&"a".repeat(1000));
        replies.observe(KEY, &longer, ms(start, 110));
        replies.tick(ms(start, 120), |_| Some(&longer));
        assert_eq!(shown(&replies), longer.text, "the deadline did not move");
        assert!(!replies.pacing(), "idle once caught up");
        assert!(replies.tick(ms(start, 200), |_| Some(&longer)).is_empty());
    }

    #[test]
    fn text_seen_after_an_overdue_deadline_shows_at_once() {
        let start = Instant::now();
        let mut replies = Replies::default();
        replies.observe(KEY, &streaming(""), start);
        replies.observe(KEY, &streaming(&"a".repeat(100)), start);
        replies.observe(KEY, &streaming(&"a".repeat(200)), ms(start, 500));
        assert_eq!(shown(&replies).len(), 200);
        assert!(!replies.pacing());
    }

    #[test]
    fn completion_replacement_and_settling_show_the_reply_as_it_is() {
        let start = Instant::now();
        let mut replies = Replies::default();
        replies.observe(KEY, &streaming(""), start);
        replies.observe(KEY, &streaming(&"old ".repeat(50)), start);
        let replaced = streaming("A different reply that is long enough to pace.");
        replies.observe(KEY, &replaced, start);
        assert_eq!(shown(&replies), replaced.text, "no old text left to reveal");

        replies.observe(
            KEY,
            &streaming(&format!("{} More words to pace.", replaced.text)),
            start,
        );
        assert!(replies.pacing());
        let done = item("Final text.", ItemStatus::Completed);
        replies.observe(KEY, &done, start);
        assert_eq!(shown(&replies), "Final text.");
        assert!(!replies.pacing());

        let more = streaming(&format!("Final text.{}", " and more".repeat(20)));
        replies.observe(KEY, &more, start);
        assert_eq!(replies.settle(1), vec![0]);
        assert_eq!(
            replies.document(KEY, &more),
            Document::parse(&more.text).into()
        );
    }

    #[test]
    fn a_reply_first_seen_mid_stream_is_not_replayed() {
        let start = Instant::now();
        let mut replies = Replies::default();
        let reply = streaming(&"history ".repeat(100));
        replies.observe(KEY, &reply, start);
        assert_eq!(shown(&replies), reply.text);
        assert!(!replies.pacing());
    }

    #[test]
    fn shown_text_never_splits_a_grapheme() {
        let start = Instant::now();
        let mut replies = Replies::default();
        replies.observe(KEY, &streaming(""), start);
        let text = "e\u{301}👩‍👩‍👧‍👦🇨🇭 नमस्ते".repeat(20);
        let reply = streaming(&text);
        replies.observe(KEY, &reply, start);
        let mut now = start;
        let mut graphemes = 0;
        while replies.pacing() {
            let source = shown(&replies);
            let mut cursor = GraphemeCursor::new(source.len(), text.len(), true);
            assert!(cursor.is_boundary(&text, 0).unwrap(), "{source:?}");
            graphemes += 1;
            now += Duration::from_millis(4);
            replies.tick(now, |_| Some(&reply));
        }
        assert!(graphemes > 2);
        assert_eq!(shown(&replies), text);
    }

    #[test]
    fn a_shown_end_that_later_text_joins_moves_past_the_whole_grapheme() {
        let start = Instant::now();
        let mut replies = Replies::default();
        replies.observe(KEY, &streaming("Family 👩"), start);
        let joined = streaming(&format!("Family 👩\u{200d}👧{}", " and more".repeat(20)));
        replies.observe(KEY, &joined, start);
        assert!(shown(&replies).starts_with("Family 👩\u{200d}👧"));
    }

    #[test]
    fn a_removed_row_stops_pacing() {
        let start = Instant::now();
        let mut replies = Replies::default();
        replies.observe(KEY, &streaming(""), start);
        replies.observe(KEY, &streaming(&"a".repeat(100)), start);
        replies.tick(ms(start, 16), |_| None);
        assert!(!replies.pacing());
    }
}
