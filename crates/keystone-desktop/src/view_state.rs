use gpui::{App, FocusHandle, ListAlignment, ListOffset, ListState, Window, px};
use keystone_core::ViewId;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewKey {
    pub view: ViewId,
    pub session: u64,
}

struct Transcript {
    list: ListState,
    remeasured_top: bool,
    focus: Option<FocusHandle>,
    focused_at: u64,
    revision: u64,
    observation_attempt: Option<u64>,
    keyboard: Option<TranscriptKeyboard>,
    visible_rows: Option<Range<usize>>,
    fully_visible_rows: Option<Range<usize>>,
    visible_anchor: Option<ListOffset>,
    collapsed_activity: HashSet<usize>,
}

pub struct TranscriptKeyboard {
    pub row: Option<usize>,
    pub action: Option<CopyTarget>,
    pub selection_from_pointer: bool,
    pub action_focus: FocusHandle,
    pub jump_focus: FocusHandle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyTarget {
    Row,
    Code(usize),
    DiagramToggle(usize),
    ActivityToggle,
    TextSelection,
}

#[derive(Default)]
pub struct ViewStates {
    transcripts: HashMap<ViewKey, Transcript>,
    focus_serial: u64,
}

impl ViewStates {
    pub fn suspend(&mut self) {
        for t in self.transcripts.values_mut() {
            t.revision += 1;
            t.observation_attempt = None;
            let top = t.list.logical_scroll_top();
            let count = t.list.item_count();
            t.list.splice(0..count, count);
            t.list.scroll_to(top);
            t.remeasured_top = true;
        }
    }

    pub fn list(&mut self, key: ViewKey, rows: usize) -> ListState {
        self.transcripts
            .entry(key)
            .or_insert_with(|| Transcript {
                list: ListState::new(rows + 1, ListAlignment::Bottom, px(600.)),
                remeasured_top: false,
                focus: None,
                focused_at: 0,
                revision: 0,
                observation_attempt: None,
                keyboard: None,
                visible_rows: None,
                fully_visible_rows: None,
                visible_anchor: None,
                collapsed_activity: HashSet::new(),
            })
            .list
            .clone()
    }

    pub fn focus(&mut self, key: ViewKey, cx: &mut App) -> FocusHandle {
        self.list(key, 0);
        self.transcripts
            .get_mut(&key)
            .unwrap()
            .focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone()
    }

    pub fn revision(&self, key: ViewKey) -> u64 {
        self.transcripts.get(&key).map_or(0, |t| t.revision)
    }

    pub fn keyboard(&mut self, key: ViewKey, cx: &mut App) -> &mut TranscriptKeyboard {
        self.list(key, 0);
        self.transcripts
            .get_mut(&key)
            .unwrap()
            .keyboard
            .get_or_insert_with(|| TranscriptKeyboard {
                row: None,
                action: None,
                selection_from_pointer: false,
                action_focus: cx.focus_handle(),
                jump_focus: cx.focus_handle(),
            })
    }

    pub fn keyboard_state(&self, key: ViewKey) -> Option<&TranscriptKeyboard> {
        self.transcripts.get(&key)?.keyboard.as_ref()
    }

    pub fn begin_frame(&mut self, key: ViewKey) {
        let t = self.transcripts.get_mut(&key).unwrap();
        t.visible_rows = None;
        t.fully_visible_rows = None;
        t.visible_anchor = None;
    }

    pub fn note_visible_row(
        &mut self,
        key: ViewKey,
        row: usize,
        fully: bool,
        offset: gpui::Pixels,
    ) {
        if let Some(t) = self.transcripts.get_mut(&key) {
            if t.visible_anchor.is_none_or(|top| row < top.item_ix) {
                t.visible_anchor = Some(ListOffset {
                    item_ix: row,
                    offset_in_item: offset,
                });
            }
            t.visible_rows = Some(t.visible_rows.as_ref().map_or(row..row + 1, |rows| {
                rows.start.min(row)..rows.end.max(row + 1)
            }));
            if fully {
                t.fully_visible_rows =
                    Some(t.fully_visible_rows.as_ref().map_or(row..row + 1, |rows| {
                        rows.start.min(row)..rows.end.max(row + 1)
                    }));
            }
        }
    }

    pub fn activity_expanded(&self, key: ViewKey, row: usize) -> bool {
        self.transcripts
            .get(&key)
            .is_none_or(|t| !t.collapsed_activity.contains(&row))
    }

    pub fn toggle_activity(&mut self, key: ViewKey, row: usize) {
        let Some(t) = self.transcripts.get_mut(&key) else {
            return;
        };
        let collapsed = t.collapsed_activity.insert(row);
        if !collapsed {
            t.collapsed_activity.remove(&row);
        }
        let mut top = t.list.logical_scroll_top();
        if top.item_ix == t.list.item_count() {
            top = t.visible_anchor.unwrap_or(ListOffset {
                item_ix: row,
                offset_in_item: px(0.),
            });
        }
        if collapsed && top.item_ix == row {
            top.offset_in_item = px(0.);
        }
        t.list.splice(row..row + 1, 1);
        t.list.scroll_to(top);
        t.revision += 1;
    }

    pub fn row_is_visible(&self, key: ViewKey, row: usize, fully: bool) -> bool {
        self.transcripts
            .get(&key)
            .and_then(|t| {
                if fully {
                    t.fully_visible_rows.as_ref()
                } else {
                    t.visible_rows.as_ref()
                }
            })
            .is_some_and(|rows| rows.contains(&row))
    }

    pub fn contains_focus(&mut self, key: ViewKey, window: &Window, cx: &mut App) -> bool {
        self.focus(key, cx).contains_focused(window, cx)
            || self.keyboard_state(key).is_some_and(|k| {
                k.action_focus.is_focused(window) || k.jump_focus.is_focused(window)
            })
    }

    pub fn focused_region(&self, key: ViewKey, window: &Window) -> bool {
        self.transcripts
            .get(&key)
            .and_then(|t| t.focus.as_ref())
            .is_some_and(|f| f.is_focused(window))
    }

    pub fn activate(&mut self, key: ViewKey) {
        self.transcripts.get_mut(&key).unwrap().observation_attempt = None;
        self.note_focus(key);
    }

    pub fn note_focus(&mut self, key: ViewKey) {
        self.focus_serial += 1;
        self.transcripts.get_mut(&key).unwrap().focused_at = self.focus_serial;
    }

    pub fn send_target(&self, visible: &[ViewId], session: u64) -> Option<ViewKey> {
        visible
            .iter()
            .map(|view| ViewKey {
                view: *view,
                session,
            })
            .max_by_key(|key| self.transcripts.get(key).map_or(0, |t| t.focused_at))
            .filter(|key| self.transcripts.get(key).is_some_and(|t| t.focused_at > 0))
            .or_else(|| {
                visible.first().map(|view| ViewKey {
                    view: *view,
                    session,
                })
            })
    }

    pub fn attempt_observation(&mut self, key: ViewKey, updated: u64) -> bool {
        let t = self.transcripts.get_mut(&key).unwrap();
        if t.observation_attempt == Some(updated) {
            return false;
        }
        t.observation_attempt = Some(updated);
        true
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
            transcript.revision += 1;
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
    pub fn scroll_to_end(&mut self, key: ViewKey) {
        if let Some(transcript) = self.transcripts.get_mut(&key) {
            if let Some(keyboard) = &mut transcript.keyboard {
                keyboard.row = None;
                keyboard.action = None;
                keyboard.selection_from_pointer = false;
            }
            transcript.list.scroll_to(ListOffset {
                item_ix: transcript.list.item_count(),
                offset_in_item: px(0.),
            });
        }
    }

    pub fn scrolled_up(&self, key: ViewKey) -> bool {
        self.get(key)
            .is_some_and(|list| list.logical_scroll_top().item_ix < list.item_count())
    }
}
