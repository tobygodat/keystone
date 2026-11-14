//! Find in a session: one bar over the transcript that marks every match of a plain-text
//! query in the session's whole conversation, and steps through them.
//!
//! The transcript is virtualized, so find reads rows, not what is on screen. Each row's
//! searched texts are the ones it draws, in drawing order ([`row_texts`]), cached per
//! session until the row changes. The search runs on a background thread; the UI only
//! stamps rows and hands over the texts.

use crate::Keystone;
use crate::conversation::Row;
use crate::transcript::{EditFolder, edit_label, preview_texts};
use crate::view_state::ViewKey;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, SharedString, Styled, Task, Window, div, px,
};
use keystone_agent::{ItemKind, ItemStatus};
use keystone_core::search::Query;
use keystone_ui::markdown::Document;
use keystone_ui::prelude::*;
use keystone_ui::{Mark, MarkSlot, text_input};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

gpui::actions!(find, [FindInSession, CloseFind]);

pub(crate) const CONTEXT: &str = "FindBar";

/// How long rows that keep changing, such as a streaming reply, wait before find reads
/// them again.
const ROWS_SETTLE: Duration = Duration::from_millis(150);

/// One match: the row, which of the row's texts, and where in that text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub row: usize,
    pub text: usize,
    pub range: Range<usize>,
    /// The text shows only while the row's disclosure is open.
    pub hidden: bool,
}

/// The texts a row draws that find searches, in the order it draws them.
#[derive(Debug, Default, PartialEq)]
pub struct RowTexts {
    pub texts: Vec<SharedString>,
    /// Texts from this one on show only while the row's disclosure is open.
    pub hidden_from: usize,
}

/// What a row is searched as. A delivered result is a user message the transcript draws
/// as its reply's Markdown, which only the app state can tell, and an edit row shows its
/// paths from the session's folder.
pub fn row_texts(row: &Row, result: bool, folder: Option<&EditFolder>) -> Option<RowTexts> {
    let Row::Item(item) = row else {
        return None;
    };
    let shown = |texts: Vec<SharedString>| {
        let hidden_from = texts.len();
        Some(RowTexts { texts, hidden_from })
    };
    match &item.kind {
        ItemKind::UserMessage if result => {
            let read = crate::app_state::delivery::parse(&item.text)?;
            shown(Document::parse(read.reply).texts())
        }
        ItemKind::UserMessage if !item.text.is_empty() => shown(vec![item.text.clone().into()]),
        ItemKind::AgentMessage => shown(Document::parse(&item.text).texts()),
        ItemKind::Reasoning if !item.text.is_empty() => Some(RowTexts {
            texts: vec![item.text.clone().into()],
            hidden_from: 0,
        }),
        ItemKind::Command { command, .. } => Some(RowTexts {
            texts: vec![
                format!("$ {command}").into(),
                item.text.trim_end().to_owned().into(),
            ],
            hidden_from: 1,
        }),
        ItemKind::Other { .. } if item.id.starts_with(keystone_agent::log::LOSS_MARKER_PREFIX) => {
            None
        }
        ItemKind::Tool { name } if name == keystone_agent::surface::TOOL => None,
        ItemKind::Tool { .. } | ItemKind::Other { .. } => Some(RowTexts {
            texts: vec![item.text.clone().into()],
            hidden_from: 0,
        }),
        // Its label, then, while open, its preview's file names and lines.
        ItemKind::FileChange { paths, patch } => {
            let folder = folder.cloned().unwrap_or_default();
            let mut texts = vec![edit_label(&folder, paths, item.status).into()];
            if let Some(patch) = patch {
                texts.extend(preview_texts(patch, &folder).into_iter().map(Into::into));
            }
            Some(RowTexts {
                texts,
                hidden_from: 1,
            })
        }
        _ => None,
    }
}

/// What a row held when its texts were read, so a row that changed is read again.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    id: String,
    len: usize,
    status: ItemStatus,
    result: bool,
    /// For an edit row, the folder its paths are shown from.
    folder: Option<EditFolder>,
}

fn stamp(row: &Row, result: bool, folder: &EditFolder) -> Option<Stamp> {
    match row {
        Row::Item(item) => Some(Stamp {
            id: item.id.clone(),
            len: item.text.len(),
            status: item.status,
            result,
            folder: matches!(item.kind, ItemKind::FileChange { .. }).then(|| folder.clone()),
        }),
        _ => None,
    }
}

/// One row's part of a search: texts already read, or a row to read.
enum Job {
    Skip,
    Read(Arc<RowTexts>),
    Fresh(Stamp, Box<Row>),
}

/// What a background search returns.
pub struct Outcome {
    pub matches: Vec<Match>,
    fresh: Vec<(usize, Stamp, Arc<RowTexts>)>,
    /// Rows read again because they were new or changed.
    pub rows_read: usize,
}

fn search(query: Query, jobs: Vec<Job>) -> Outcome {
    let mut matches = Vec::new();
    let mut fresh = Vec::new();
    for (row, job) in jobs.into_iter().enumerate() {
        let texts = match job {
            Job::Skip => continue,
            Job::Read(texts) => texts,
            Job::Fresh(stamp, content) => {
                let texts = Arc::new(
                    row_texts(&content, stamp.result, stamp.folder.as_ref()).unwrap_or_default(),
                );
                fresh.push((row, stamp, texts.clone()));
                texts
            }
        };
        for (text, value) in texts.texts.iter().enumerate() {
            for range in query.find(value) {
                matches.push(Match {
                    row,
                    text,
                    range,
                    hidden: text >= texts.hidden_from,
                });
            }
        }
    }
    let rows_read = fresh.len();
    Outcome {
        matches,
        fresh,
        rows_read,
    }
}

/// One row's texts as last read, and what the row held then.
type Read = Option<(Stamp, Arc<RowTexts>)>;

/// Each session's rows' texts, as last read.
#[derive(Default)]
pub struct Index {
    sessions: HashMap<u64, Vec<Read>>,
}

/// Why a search runs, which decides where the current match goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    /// The query changed: go to the first match from the top of the view, and show it.
    Typed,
    /// The rows changed: keep the current match, and don't move the reader.
    Rows,
}

/// One view's find bar.
pub struct FindBar {
    pub input: Entity<keystone_ui::TextInput>,
    /// Where focus was when the bar opened, given back when it closes.
    back: Option<FocusHandle>,
    pub matches: Vec<Match>,
    pub current: Option<usize>,
    /// The query `matches` were found for.
    searched: Option<String>,
    generation: u64,
    task: Option<Task<()>>,
    /// Rows changed while a search ran: search again when it lands.
    stale: bool,
    /// A delayed search for changed rows is waiting.
    settling: bool,
    /// A message to land on, by item ID, once a search finds it: a sidebar result.
    target: Option<String>,
    /// Where the current match was last drawn.
    pub slot: MarkSlot,
    /// The current match should be scrolled into view once it is drawn.
    reveal: bool,
    /// The field has focus, so it shows its ring.
    focused: bool,
    /// Rows the last search read afresh; the rest came from the index.
    pub rows_read: usize,
    _subscriptions: Vec<gpui::Subscription>,
}

impl FindBar {
    /// Whether `matches` were found for the query in the field. While a newer query's
    /// search runs they are an older one's, so Enter and the arrows wait for it.
    fn current_query(&self, cx: &App) -> bool {
        let typed = self.input.read(cx).value();
        !typed.is_empty() && self.searched.as_deref() == Some(typed)
    }

    /// The count the bar shows: "3 of 12", or "No matches" once a search found none.
    pub fn count(&self, cx: &App) -> Option<String> {
        if !self.current_query(cx) {
            return None;
        }
        Some(match (self.current, self.matches.len()) {
            (_, 0) => "No matches".to_owned(),
            (Some(current), total) => format!("{} of {total}", current + 1),
            (None, total) => format!("{total} found"),
        })
    }

    /// The marks of row `row`, the current one among them.
    pub fn marks(&self, row: usize) -> Vec<Mark> {
        let start = self.matches.partition_point(|m| m.row < row);
        self.matches[start..]
            .iter()
            .take_while(|m| m.row == row)
            .enumerate()
            .map(|(n, m)| Mark {
                text: m.text,
                range: m.range.clone(),
                current: self.current == Some(start + n),
            })
            .collect()
    }
}

#[derive(Default)]
pub struct Finds {
    pub bars: HashMap<ViewKey, FindBar>,
    pub index: Index,
}

impl Keystone {
    /// Opens the find bar over the active view's transcript, or selects its query when
    /// it is already open.
    pub(crate) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self
            .active_view(cx)
            .and_then(|view| self.view_key(view, cx))
        else {
            return;
        };
        self.show_find(key, None, window, cx);
    }

    /// Shows `key`'s find bar with focus in it. With `query`, the bar searches it and
    /// lands on the message `target` names, as opening a sidebar result does.
    pub(crate) fn show_find(
        &mut self,
        key: ViewKey,
        query: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = window.focused(cx);
        if let std::collections::hash_map::Entry::Vacant(vacant) = self.finds.bars.entry(key) {
            let input = cx.new(|cx| {
                keystone_ui::TextInput::new("Find in session", InputMode::SingleLine, cx)
            });
            let handle = input.focus_handle(cx);
            let subscriptions = vec![
                cx.subscribe(&input, move |this, _, _: &text_input::Changed, cx| {
                    this.search_find(key, Reason::Typed, cx)
                }),
                cx.on_focus(&handle, window, move |this, _, cx| {
                    this.find_focus(key, true, cx)
                }),
                cx.on_blur(&handle, window, move |this, _, cx| {
                    this.find_focus(key, false, cx)
                }),
            ];
            vacant.insert(FindBar {
                input,
                back: None,
                matches: Vec::new(),
                current: None,
                searched: None,
                generation: 0,
                task: None,
                stale: false,
                settling: false,
                target: None,
                slot: MarkSlot::default(),
                reveal: false,
                focused: false,
                rows_read: 0,
                _subscriptions: subscriptions,
            });
        }
        let bar = self.finds.bars.get_mut(&key).expect("bar just made");
        let input = bar.input.clone();
        let inside = focused
            .as_ref()
            .is_some_and(|focused| *focused == input.focus_handle(cx));
        if !inside {
            bar.back = focused;
        }
        match query {
            Some((query, target)) => {
                bar.target = Some(target);
                input.update(cx, |input, cx| input.set_value(query, cx));
                self.search_find(key, Reason::Typed, cx);
            }
            None => input.update(cx, |input, cx| {
                input.select_all(&text_input::SelectAll, window, cx)
            }),
        }
        input.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn focus_find(&mut self, key: ViewKey, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bar) = self.finds.bars.get(&key) {
            bar.input.focus_handle(cx).focus(window, cx);
        }
    }

    fn find_focus(&mut self, key: ViewKey, focused: bool, cx: &mut Context<Self>) {
        if let Some(bar) = self.finds.bars.get_mut(&key) {
            bar.focused = focused;
            cx.notify();
        }
    }

    /// Closes `key`'s find bar and gives focus back to where it was.
    pub(crate) fn close_find(&mut self, key: ViewKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.finds.bars.remove(&key) else {
            return;
        };
        match bar.back {
            Some(back) => back.focus(window, cx),
            None => self.focus_composer(window, cx),
        }
        cx.notify();
    }

    /// Moves to the next match, or with `backwards` the previous one, wrapping at the
    /// ends, and shows it. While a new query's search runs it does nothing: the search
    /// shows its first match when it lands.
    pub(crate) fn step_find(&mut self, key: ViewKey, backwards: bool, cx: &mut Context<Self>) {
        let Some(bar) = self.finds.bars.get_mut(&key) else {
            return;
        };
        if !bar.current_query(cx) {
            return;
        }
        let total = bar.matches.len();
        if total == 0 {
            return;
        }
        bar.current = Some(match (bar.current, backwards) {
            (None, false) => 0,
            (None, true) => total - 1,
            (Some(at), false) => (at + 1) % total,
            (Some(at), true) => (at + total - 1) % total,
        });
        self.show_current_match(key, cx);
    }

    /// The rows of `session` changed: open bars on it search again, after the rows
    /// settle, without moving the reader.
    pub(crate) fn find_rows_changed(&mut self, session: u64, cx: &mut Context<Self>) {
        let keys: Vec<ViewKey> = self
            .finds
            .bars
            .keys()
            .copied()
            .filter(|key| key.session == session)
            .collect();
        for key in keys {
            let bar = self.finds.bars.get_mut(&key).expect("listed");
            if bar.task.is_some() {
                bar.stale = true;
                continue;
            }
            if std::mem::replace(&mut bar.settling, true) {
                continue;
            }
            let wait = cx.background_executor().timer(ROWS_SETTLE);
            cx.spawn(async move |this, cx| {
                wait.await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(bar) = this.finds.bars.get_mut(&key) {
                        bar.settling = false;
                        this.search_find(key, Reason::Rows, cx);
                    }
                });
            })
            .detach();
        }
    }

    /// Searches `key`'s session for the bar's query on a background thread.
    pub(crate) fn search_find(&mut self, key: ViewKey, reason: Reason, cx: &mut Context<Self>) {
        let folder = self.edit_folder(key.session, cx);
        let Some(bar) = self.finds.bars.get_mut(&key) else {
            return;
        };
        bar.generation += 1;
        let generation = bar.generation;
        let typed = bar.input.read(cx).value().to_owned();
        if typed.is_empty() {
            bar.matches.clear();
            bar.current = None;
            bar.searched = None;
            bar.task = None;
            cx.notify();
            return;
        };
        let state = self.state.read(cx);
        let rows: &[Row] = state
            .conversations()
            .get(&key.session)
            .map_or(&[], |c| c.rows.as_slice());
        let cached = self.finds.index.sessions.entry(key.session).or_default();
        cached.resize(rows.len(), None);
        let jobs: Vec<Job> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let result = matches!(row, Row::Item(item) if item.kind == ItemKind::UserMessage
                    && state.result_source(key.session, &item.text).and_then(|child| state.session(child)).is_some());
                let Some(stamp) = stamp(row, result, &folder) else {
                    return Job::Skip;
                };
                match &cached[index] {
                    Some((saved, texts)) if *saved == stamp => Job::Read(texts.clone()),
                    _ => Job::Fresh(stamp, Box::new(row.clone())),
                }
            })
            .collect();
        // A first query builds the matcher's case table, so even that is off this thread.
        let query = typed.clone();
        let task = cx
            .background_spawn(async move { search(Query::new(&query).expect("not empty"), jobs) });
        bar.task = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |this, cx| {
                this.found(key, generation, typed, reason, outcome, cx)
            });
        }));
    }

    fn found(
        &mut self,
        key: ViewKey,
        generation: u64,
        typed: String,
        reason: Reason,
        outcome: Outcome,
        cx: &mut Context<Self>,
    ) {
        let cached = self.finds.index.sessions.entry(key.session).or_default();
        for (row, stamp, texts) in outcome.fresh {
            if row >= cached.len() {
                cached.resize(row + 1, None);
            }
            cached[row] = Some((stamp, texts));
        }
        let top = self
            .presentation
            .current
            .views
            .get(key)
            .map(|list| list.logical_scroll_top().item_ix);
        let rows: Vec<Option<String>> = self
            .state
            .read(cx)
            .conversations()
            .get(&key.session)
            .map(|c| {
                c.rows
                    .iter()
                    .map(|row| match row {
                        Row::Item(item) => Some(item.id.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let Some(bar) = self.finds.bars.get_mut(&key) else {
            return;
        };
        if bar.generation != generation {
            return;
        }
        bar.task = None;
        bar.rows_read = outcome.rows_read;
        let before = bar.current.and_then(|at| bar.matches.get(at).cloned());
        bar.matches = outcome.matches;
        bar.searched = Some(typed);
        let landing = bar.target.as_ref().and_then(|target| {
            bar.matches
                .iter()
                .position(|m| rows.get(m.row).and_then(Option::as_ref) == Some(target))
        });
        let mut show = false;
        bar.current = if let Some(at) = landing {
            bar.target = None;
            show = true;
            Some(at)
        } else if bar.matches.is_empty() {
            None
        } else {
            match (reason, before) {
                // The same match where it was, else the next one after it.
                (Reason::Rows, Some(before)) => Some(
                    bar.matches
                        .iter()
                        .position(|m| {
                            (m.row, m.text, m.range.start)
                                >= (before.row, before.text, before.range.start)
                        })
                        .unwrap_or(0),
                ),
                (Reason::Rows, None) => None,
                (Reason::Typed, _) => {
                    show = true;
                    let from = top.unwrap_or(0);
                    Some(bar.matches.iter().position(|m| m.row >= from).unwrap_or(0))
                }
            }
        };
        let stale = std::mem::take(&mut bar.stale);
        if show {
            self.show_current_match(key, cx);
        } else {
            cx.notify();
        }
        if stale {
            self.find_rows_changed(key.session, cx);
        }
    }

    /// Opens the current match's row if it is collapsed, and scrolls it into view.
    fn show_current_match(&mut self, key: ViewKey, cx: &mut Context<Self>) {
        let Some(found) = self.finds.bars.get_mut(&key).and_then(|bar| {
            bar.reveal = true;
            *bar.slot.borrow_mut() = None;
            bar.matches.get(bar.current?).cloned()
        }) else {
            return;
        };
        if found.hidden {
            let opens = self
                .state
                .read(cx)
                .conversations()
                .get(&key.session)
                .and_then(|c| match c.rows.get(found.row) {
                    Some(Row::Item(item)) => Some(crate::transcript::opens_expanded(item)),
                    _ => None,
                });
            if let Some(opens) = opens
                && !self
                    .presentation
                    .current
                    .views
                    .activity_expanded(key, found.row, opens)
            {
                self.presentation
                    .current
                    .views
                    .toggle_activity(key, found.row, opens);
            }
        }
        if !self
            .presentation
            .current
            .views
            .row_is_visible(key, found.row, true)
            && let Some(list) = self.presentation.current.views.get(key)
        {
            // Rows between here and there may never have been measured, so the row is
            // put at the top; once drawn, `reveal_found` places the match itself.
            list.scroll_to(gpui::ListOffset {
                item_ix: found.row,
                offset_in_item: px(0.),
            });
        }
        cx.notify();
    }

    /// After a frame that drew the current match, scrolls the list so the match itself,
    /// not only its row, is in view.
    pub(crate) fn reveal_found(&mut self, key: ViewKey, cx: &mut Context<Self>) {
        let Some(bar) = self.finds.bars.get_mut(&key) else {
            return;
        };
        if !bar.reveal {
            return;
        }
        let Some((layout, at)) = bar.slot.borrow().clone() else {
            return;
        };
        bar.reveal = false;
        let Some(list) = self.presentation.current.views.get(key) else {
            return;
        };
        let Some(position) = layout.position_for_index(at) else {
            return;
        };
        let line = layout.line_height();
        let viewport = list.viewport_bounds();
        // Keep the match a third of the way down, clear of the bar over the top.
        let goal = viewport.top() + viewport.size.height / 3.;
        if position.y < viewport.top() + px(48.) || position.y + line > viewport.bottom() {
            list.scroll_by(position.y - goal);
            cx.notify();
        }
    }

    /// The find bar over `key`'s transcript, while it is open.
    pub(crate) fn find_bar(&self, key: ViewKey, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = self.finds.bars.get(&key)?;
        let selector = |name: &str| crate::arrangement::selector(name, key.view).to_string();
        let colors = cx.theme().colors;
        let focused = bar.focused;
        let any = !bar.matches.is_empty() && bar.current_query(cx);
        let count = bar.count(cx);
        let button = |id: &str, icon: IconName, label: &'static str, keys: &str| {
            let id = selector(id);
            IconButton::new(SharedString::from(id.clone()), icon)
                .size(ButtonSize::Compact)
                .aria_label(label)
                .tooltip(Tooltip::with_key(label, keystone_ui::shortcut_text(keys)))
                .debug_selector(move || id.clone())
        };
        // The count's slot is always as wide as "9999 of 9999", so neither stepping nor
        // typing moves the field.
        let count_width = TextSize::Sm.px() * 6.5;
        Some(
            // Spans the column, so the bar narrows with it rather than spilling over the
            // sidebar when side panels squeeze the column to its narrowest.
            h_flex()
                .absolute()
                .top_2()
                .left_4()
                .right_4()
                .justify_end()
                .child(gpui::deferred(
                    h_flex()
                        .id(SharedString::from(selector("find-bar")))
                        .automation_id(selector("find-bar"))
                        .key_context(CONTEXT)
                        .occlude()
                        .w(px(340.))
                        .max_w_full()
                        .h(px(36.))
                        .pl_3()
                        .pr_1()
                        .gap_1()
                        .rounded(radius::PANEL)
                        .surface(Elevation::Overlay, cx)
                        .on_action(
                            self.scoped_listener(cx, move |this, _: &CloseFind, window, cx| {
                                this.close_find(key, window, cx)
                            }),
                        )
                        // A press in the bar keeps focus in it, not the transcript.
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        // Enter and Shift+Enter step before the field's own
                        // binding turns either into a newline it would refuse.
                        .capture_key_down(self.scoped_listener(
                            cx,
                            move |this, event: &gpui::KeyDownEvent, _, cx| {
                                let keys = &event.keystroke;
                                match keys.key.as_str() {
                                    "enter" if !keys.modifiers.secondary() => {
                                        this.step_find(key, keys.modifiers.shift, cx)
                                    }
                                    _ => return,
                                }
                                cx.stop_propagation();
                            },
                        ))
                        .child(
                            Icon::new(IconName::Search)
                                .size(px(14.))
                                .color(Color::Muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px_1()
                                .rounded(radius::CONTROL)
                                .border_1()
                                .border_color(colors.border_focused.opacity(0.))
                                .when(focused, |d| d.border_color(colors.border_focused))
                                .automation_id(selector("find-field"))
                                .child(bar.input.clone()),
                        )
                        .child(
                            h_flex()
                                .flex_none()
                                .min_w(count_width)
                                .justify_end()
                                .children(count.map(|count| {
                                    div().automation_id(selector("find-count")).child(
                                        Label::new(count)
                                            .size(TextSize::Sm)
                                            .color(Color::Muted)
                                            .tabular(),
                                    )
                                })),
                        )
                        .child(
                            button(
                                "find-previous",
                                IconName::Up,
                                "Previous match",
                                "shift-enter",
                            )
                            .disabled(!any)
                            .on_click(self.scoped_listener(
                                cx,
                                move |this, _, window, cx| {
                                    this.step_find(key, true, cx);
                                    this.focus_find(key, window, cx);
                                },
                            )),
                        )
                        .child(
                            button("find-next", IconName::Down, "Next match", "enter")
                                .disabled(!any)
                                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                                    this.step_find(key, false, cx);
                                    this.focus_find(key, window, cx);
                                })),
                        )
                        .child(
                            button("find-close", IconName::Close, "Close find", "escape").on_click(
                                self.scoped_listener(cx, move |this, _, window, cx| {
                                    this.close_find(key, window, cx)
                                }),
                            ),
                        ),
                ))
                .into_any_element(),
        )
    }

    /// The marks `key`'s find bar puts on row `row`, and where to record the current one.
    pub(crate) fn find_marks(&self, key: ViewKey, row: usize) -> (Vec<Mark>, Option<MarkSlot>) {
        match self.finds.bars.get(&key) {
            Some(bar) => (bar.marks(row), Some(bar.slot.clone())),
            None => (Vec::new(), None),
        }
    }
}
