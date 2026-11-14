//! The sidebar's search across sessions: as the user types, every session's saved
//! messages are scanned off the UI thread, and the newest match in each session shows
//! under the sessions whose titles match. Opening a result opens its session with the
//! find bar on the same words, at that message.

use crate::Keystone;
use gpui::{AppContext, Context, Focusable, Task, Window};
use keystone_agent::search::MessageHit;
use keystone_agent::{Item, ItemKind};
use keystone_core::search::Query;
use keystone_ui::markdown::Document;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct MessageSearch {
    /// The query `hits` were found for.
    pub searched: Option<String>,
    pub hits: Vec<MessageHit>,
    /// The result Up and Down moved to, by its place among the results shown.
    pub highlighted: Option<usize>,
    /// Enter was pressed while the scan for the typed query ran, with this session
    /// selected: open the first result once it lands, unless the user moved on first.
    open_when_found: Option<Option<u64>>,
    /// Counts searches, so an older scan stops when a newer one starts.
    generation: Arc<AtomicU64>,
    task: Option<Task<()>>,
}

impl MessageSearch {
    /// Whether a scan for the typed query is still running. The results shown meanwhile
    /// are an older query's: they are dimmed and open nothing.
    pub fn pending(&self, typed: &str) -> bool {
        !typed.is_empty() && self.searched.as_deref() != Some(typed)
    }

    /// The user moved on from the Enter they pressed mid-scan: another key, a click, or
    /// focus leaving the field. Nothing opens when the scan lands.
    pub fn forget_open(&mut self) {
        self.open_when_found = None;
    }
}

impl Keystone {
    /// Searches every session's saved messages for the sidebar's query.
    pub(crate) fn search_messages(&mut self, cx: &mut Context<Self>) {
        let typed = self.search.read(cx).value().to_owned();
        let search = &mut self.messages;
        let generation = search.generation.fetch_add(1, Ordering::SeqCst) + 1;
        search.highlighted = None;
        search.open_when_found = None;
        if typed.is_empty() {
            search.hits.clear();
            search.searched = None;
            search.task = None;
            return;
        }
        let state = self.state.read(cx);
        let database = state.store().database();
        let results = state.delivered_results();
        let current = search.generation.clone();
        let query = typed.clone();
        let task = cx.background_spawn(async move {
            let query = Query::new(&query).expect("not empty");
            keystone_agent::search::messages(
                &database,
                &query,
                // Each message is searched as the texts its row draws, as find searches
                // it: a reply's Markdown, and a delivered result as the reply it holds.
                |session, item: &Item| match item.kind {
                    ItemKind::UserMessage => match crate::app_state::delivery::parse(&item.text)
                        .filter(|_| results.source(session, &item.text).is_some())
                    {
                        Some(read) => Document::parse(read.reply).texts(),
                        None => vec![item.text.clone().into()],
                    },
                    _ => Document::parse(&item.text).texts(),
                },
                || current.load(Ordering::SeqCst) != generation,
            )
        });
        search.task = Some(cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.messages.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                this.messages.task = None;
                this.messages.searched = Some(typed);
                match found {
                    Ok(Some(found)) => this.messages.hits = found.hits,
                    Ok(None) => return,
                    Err(error) => {
                        this.messages.hits.clear();
                        this.error = Some(format!("Couldn't search messages. {error}"));
                    }
                }
                this.refresh(cx);
                // Enter belongs to its moment: it opens only if the same session is still
                // selected and the field still has focus. A key or a click since dropped it.
                if let Some(selected) = this.messages.open_when_found.take()
                    && selected == this.selected(cx)
                {
                    let (handle, this) = (this.handle, cx.weak_entity());
                    cx.defer(move |cx| {
                        let _ = handle.update(cx, |_, window, cx| {
                            let _ = this.update(cx, |this, cx| {
                                if this.search.focus_handle(cx).is_focused(window) {
                                    this.open_highlighted_message(window, cx)
                                }
                            });
                        });
                    });
                }
            });
        }));
    }

    /// The message results the sidebar shows: hits in sessions of this project that the
    /// provider filter keeps and whose titles did not match already.
    pub(crate) fn shown_hits(&self, title_matches: &[u64], cx: &gpui::App) -> Vec<MessageHit> {
        let workspace = self.state.read(cx).store().workspace();
        let project = self.presentation.project.as_ref().ok();
        self.messages
            .hits
            .iter()
            .filter(|hit| !title_matches.contains(&hit.session))
            .filter(|hit| {
                workspace
                    .sessions()
                    .iter()
                    .find(|s| s.id() == hit.session)
                    .is_some_and(|s| {
                        project.is_none_or(|project| crate::projects::holds(project, s))
                            && self.provider_filter.is_none_or(|p| s.provider() == p)
                    })
            })
            .cloned()
            .collect()
    }

    /// Up and Down in the search field: moves the highlight among the message results.
    pub(crate) fn move_message_highlight(&mut self, down: bool, cx: &mut Context<Self>) {
        let count = self.message_rows(cx).len();
        if count == 0 {
            return;
        }
        self.messages.highlighted = Some(match (self.messages.highlighted, down) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(at), true) => (at + 1).min(count - 1),
            (Some(at), false) => at.saturating_sub(1),
        });
        self.refresh(cx);
    }

    /// Enter in the search field: opens the highlighted message result, or the first.
    /// While the typed query's scan runs, the first of its results once they land.
    pub(crate) fn open_highlighted_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.search.read(cx).value();
        if self.messages.pending(typed) {
            self.messages.open_when_found = Some(self.selected(cx));
            return;
        }
        let rows = self.message_rows(cx);
        if let Some(hit) = rows.get(self.messages.highlighted.unwrap_or(0)).cloned() {
            self.open_message(hit, window, cx);
        }
    }

    /// The message results Up, Down and Enter move through: none while they are an
    /// older query's.
    pub(crate) fn message_rows(&self, cx: &gpui::App) -> Vec<MessageHit> {
        if self.messages.pending(self.search.read(cx).value()) {
            return Vec::new();
        }
        let titles = self.title_matches(cx);
        self.shown_hits(&titles, cx)
    }

    /// Opens `hit`'s session with the find bar on the sidebar's query, at the message.
    pub(crate) fn open_message(
        &mut self,
        hit: MessageHit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = self.search.read(cx).value().to_owned();
        self.select(hit.session, cx);
        let Some(key) = self.active_view(cx).map(|view| crate::view_state::ViewKey {
            view,
            session: hit.session,
        }) else {
            return;
        };
        self.state
            .update(cx, |state, _| state.ensure_history(hit.session));
        self.show_find(key, Some((query, hit.item)), window, cx);
        self.refresh(cx);
    }
}
