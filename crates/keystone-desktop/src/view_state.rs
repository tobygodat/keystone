use gpui::{ListAlignment, ListOffset, ListState, px};
use keystone_core::ViewId;
use std::collections::HashMap;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewKey {
    pub view: ViewId,
    pub session: u64,
}

struct Transcript {
    list: ListState,
    remeasured_top: bool,
}

#[derive(Default)]
pub struct ViewStates {
    transcripts: HashMap<ViewKey, Transcript>,
}

impl ViewStates {
    pub fn list(&mut self, key: ViewKey, rows: usize) -> ListState {
        self.transcripts
            .entry(key)
            .or_insert_with(|| Transcript {
                list: ListState::new(rows + 1, ListAlignment::Bottom, px(600.)),
                remeasured_top: false,
            })
            .list
            .clone()
    }

    pub fn get(&self, key: ViewKey) -> Option<&ListState> {
        self.transcripts.get(&key).map(|t| &t.list)
    }

    pub fn invalidate(
        &mut self,
        session: u64,
        rows: usize,
        changed: &[Range<usize>],
        status: bool,
    ) {
        let count = rows + 1;
        for (_, transcript) in self
            .transcripts
            .iter_mut()
            .filter(|(key, _)| key.session == session)
        {
            let list = &transcript.list;
            // Rows are only appended or replaced in place, so every row the list
            // already shows keeps its index; the status entry is always last.
            let old = list.item_count();
            let status_ix = old.saturating_sub(1).min(count - 1);
            // A splice moves the view to the start of a remeasured entry. Unless it
            // is following the bottom, which reads as the end of the list, keep it on
            // the same entry at the same offset.
            let top = list.logical_scroll_top();
            let scrolled = top.item_ix < old;
            let mut remeasured = status && top.item_ix >= status_ix;
            for range in changed {
                remeasured |= range.contains(&top.item_ix);
                let range = range.start..range.end.min(status_ix);
                if !range.is_empty() {
                    list.splice(range.clone(), range.len());
                }
            }
            // The status entry changes with almost anything, so it is always remeasured.
            if status {
                list.splice(status_ix..old, count - status_ix);
            }
            if scrolled {
                // Rows added before the status entry move it down; follow it.
                let item_ix = if top.item_ix == status_ix {
                    count - 1
                } else {
                    top.item_ix
                };
                list.scroll_to(ListOffset { item_ix, ..top });
                if remeasured && top.offset_in_item > px(0.) {
                    transcript.remeasured_top = true;
                }
            }
        }
    }

    pub fn take_remeasured_top(&mut self, key: ViewKey) -> bool {
        self.transcripts
            .get_mut(&key)
            .is_some_and(|t| std::mem::take(&mut t.remeasured_top))
    }

    /// Scrolls the view to its latest row. It then follows new rows until the reader
    /// scrolls up.
    pub fn scroll_to_end(&self, key: ViewKey) {
        if let Some(list) = self.get(key) {
            list.scroll_to(ListOffset {
                item_ix: list.item_count(),
                offset_in_item: px(0.),
            });
        }
    }

    pub fn scrolled_up(&self, key: ViewKey) -> bool {
        self.get(key)
            .is_some_and(|list| list.logical_scroll_top().item_ix < list.item_count())
    }
}
