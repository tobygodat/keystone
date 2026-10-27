//! Sessions in tabs and split panes.
//!
//! A conversation view either holds a session, as a session tab, or follows the
//! selected session as every view did before tabs. The active view is the one the user
//! last worked in. It always shows the selected session, which the composer sends to,
//! so a session chosen in the sidebar shows there, or in the tab that already holds it.
//! Working in another view selects the session it holds. The layout, the tabs and the
//! selection are saved together, so a relaunch or a crash restores them as one.

use crate::arrangement::{ordered_views, tabs_holding};
use crate::view_state::ViewKey;
use crate::{Keystone, durability_warning};
use gpui::{App, Context, IntoElement, Window};
use keystone_core::{Axis, Component, Composition, Edit, Place, SessionTabs, Size, ViewId};
use keystone_ui::prelude::*;
use std::collections::HashSet;
use std::num::NonZeroU16;

gpui::actions!(
    keystone,
    [NewTab, CloseTab, NextTab, PreviousTab, SplitRight]
);

/// A layout, when it changes, and the tabs, to save with a selection.
pub(crate) type Shown = (Option<Composition>, SessionTabs);

pub(crate) fn conversations(c: &Composition) -> Vec<ViewId> {
    ordered_views(c.root())
        .into_iter()
        .filter(|&view| c.component(view) == Some(Component::Conversation))
        .collect()
}

/// The conversation view that takes over from `closed` once `after` replaced `before`: the
/// rest of its tab group, else the view before it.
pub(crate) fn replacement(
    before: &Composition,
    after: &Composition,
    closed: ViewId,
) -> Option<ViewId> {
    let index = conversations(before)
        .iter()
        .position(|&v| v == closed)
        .unwrap_or(0);
    tabs_holding(before.root(), closed)
        .and_then(|(group, _)| group.iter().copied().find(|&v| v != closed))
        .map(|sibling| match tabs_holding(after.root(), sibling) {
            Some((group, shown)) => group[shown],
            None => sibling,
        })
        .filter(|&v| after.component(v) == Some(Component::Conversation))
        .or_else(|| conversations(after).get(index.saturating_sub(1)).copied())
}

/// A conversation view that holds no tab and so follows the selection, preferring one
/// its tab group shows.
fn free_view(c: &Composition, tabs: &SessionTabs, views: &[ViewId]) -> Option<ViewId> {
    let free = || {
        views
            .iter()
            .copied()
            .filter(|&view| tabs.get(view).is_none())
    };
    free()
        .find(|&view| {
            tabs_holding(c.root(), view).is_none_or(|(group, shown)| group[shown] == view)
        })
        .or_else(|| free().next())
}

/// The layout with `view` brought forward in its tab group, when another tab hides it.
pub(crate) fn bring_forward(c: &Composition, view: ViewId) -> Option<Composition> {
    tabs_holding(c.root(), view)
        .is_some_and(|(views, shown)| views[shown] != view)
        .then(|| c.edit(Edit::SelectTab(view)).ok())
        .flatten()
}

impl Keystone {
    /// Which session each conversation view holds, leaving out views and sessions that
    /// no longer exist, and sessions of another project, held from before projects.
    pub(crate) fn session_tabs(&self, cx: &App) -> SessionTabs {
        let Ok(project) = &self.presentation.project else {
            return SessionTabs::default();
        };
        let workspace = self.state.read(cx).store().workspace();
        let mut tabs = workspace
            .preferences()
            .session_tabs_in(project, self.window_id);
        if tabs.is_empty() {
            return tabs;
        }
        let held: HashSet<u64> = tabs.iter().map(|(_, session)| session).collect();
        let sessions: HashSet<u64> = workspace
            .sessions()
            .iter()
            .filter(|s| held.contains(&s.id()) && crate::projects::holds(project, s))
            .map(|s| s.id())
            .collect();
        let c = self.shown_composition(cx);
        tabs.retain(|view, session| {
            c.component(view) == Some(Component::Conversation) && sessions.contains(&session)
        });
        tabs
    }

    /// The session `view` shows: the one it holds, else the selected one.
    pub(crate) fn view_session(&self, view: ViewId, cx: &App) -> Option<u64> {
        let state = self.state.read(cx);
        let workspace = state.store().workspace();
        self.presentation
            .project
            .as_ref()
            .ok()
            .and_then(|project| {
                workspace
                    .preferences()
                    .held_in(project, self.window_id, view)
            })
            .filter(|&session| {
                state.session(session).is_some_and(|s| {
                    self.presentation
                        .project
                        .as_ref()
                        .is_ok_and(|project| crate::projects::holds(project, s))
                })
            })
            .or_else(|| self.selected(cx))
    }

    pub(crate) fn view_key(&self, view: ViewId, cx: &App) -> Option<ViewKey> {
        self.view_session(view, cx)
            .map(|session| ViewKey { view, session })
    }

    /// The visible conversation views that show `session`.
    pub(crate) fn showing(&self, session: u64, cx: &App) -> Vec<ViewId> {
        let tabs = self.session_tabs(cx);
        let selected = self.selected(cx);
        self.presentation
            .current
            .arrangement
            .visible
            .iter()
            .copied()
            .filter(|&view| tabs.get(view).or(selected) == Some(session))
            .collect()
    }

    /// The view the selected session shows in: the one the user last worked in, else
    /// the tab holding it, else a view that follows the selection.
    pub(crate) fn active_view(&self, cx: &App) -> Option<ViewId> {
        let c = self.shown_composition(cx);
        let views = conversations(&c);
        let tabs = self.session_tabs(cx);
        let selected = self.selected(cx);
        self.presentation
            .current
            .arrangement
            .active
            .filter(|view| {
                views.contains(view) && tabs.get(*view).is_none_or(|s| Some(s) == selected)
            })
            .or_else(|| selected.and_then(|s| tabs.view_of(s)))
            .or_else(|| free_view(&c, &tabs, &views))
            .or_else(|| views.first().copied())
    }

    /// Whether more than one conversation view is laid out, so each pane names its
    /// sessions in a tab strip.
    pub(crate) fn has_session_tabs(c: &Composition) -> bool {
        conversations(c).len() > 1
    }

    /// Shows a session selected anywhere (the sidebar, a new session, the palette) in
    /// the active view, or brings forward the tab that already holds it. Runs once per
    /// selection, before the frame is drawn.
    ///
    /// The first time after launch, a project switch or applying Customize, the saved
    /// tabs win: a selection no view can show selects the first tab's session instead,
    /// so a crash between two writes, or a selection made in another project, never
    /// replaces a tab.
    pub(crate) fn follow_selection(&mut self, cx: &mut Context<Self>) {
        if self.presentation.current.customization.is_open() {
            return;
        }
        let selected = self.selected(cx);
        let picked = std::mem::take(&mut self.presentation.current.arrangement.picked);
        let first = self.presentation.current.arrangement.seen.is_none() && !picked;
        if !picked && self.presentation.current.arrangement.seen == Some(selected) {
            return;
        }
        self.presentation.current.arrangement.seen = Some(selected);
        let Some(session) = selected else {
            return;
        };
        let tabs = self.session_tabs(cx);
        let c = self.saved_composition(cx);
        let views = conversations(&c);
        if first && tabs.view_of(session).is_none() {
            if let Some(free) = free_view(&c, &tabs, &views) {
                self.show_fitted(free, cx);
                if let Some(layout) = bring_forward(&c, free) {
                    self.save_view(Some(layout), tabs, cx);
                }
                return;
            }
            if let Some(&view) = views.first()
                && let Some(held) = tabs.get(view)
            {
                self.presentation.current.arrangement.active = Some(view);
                self.presentation.current.arrangement.seen = Some(Some(held));
                return self.select(held, cx);
            }
        }
        let Some((view, change)) = self.place(session, cx) else {
            return;
        };
        self.show_fitted(view, cx);
        if let Some((layout, tabs)) = change {
            self.save_view(layout, tabs, cx);
        }
    }

    /// Where selecting `session` shows it: the tab holding it, brought forward, else the
    /// active view, whose tab then holds it. Returns the view and, when they change, the
    /// layout and tabs to save with the selection. `None` while Customize is open.
    pub(crate) fn place(&self, session: u64, cx: &App) -> Option<(ViewId, Option<Shown>)> {
        if self.presentation.current.customization.is_open() {
            return None;
        }
        let c = self.saved_composition(cx);
        let mut tabs = self.session_tabs(cx);
        if let Some(view) = tabs.view_of(session) {
            return Some((
                view,
                bring_forward(&c, view).map(|layout| (Some(layout), tabs)),
            ));
        }
        let views = conversations(&c);
        let view = self
            .presentation
            .current
            .arrangement
            .active
            .filter(|view| views.contains(view))
            .or_else(|| free_view(&c, &tabs, &views))
            .or_else(|| views.first().copied())?;
        let layout = bring_forward(&c, view);
        if tabs.get(view).is_none() {
            return Some((view, layout.map(|layout| (Some(layout), tabs))));
        }
        tabs.hold(view, session);
        Some((view, Some((layout, tabs))))
    }

    /// Makes `view` active, and the fitted tab shown while the window is fitted unless
    /// the shown one already follows the selection with it, so the conversation on
    /// screen is always the one the composer sends to.
    pub(crate) fn show_fitted(&mut self, view: ViewId, cx: &App) {
        self.presentation.current.arrangement.active = Some(view);
        if !self.presentation.current.arrangement.fitted {
            return;
        }
        let c = self.shown_composition(cx);
        let shown = self.fitted_shown(&c, cx);
        let tabs = self.session_tabs(cx);
        if shown == Some(view)
            || shown.is_some_and(|shown| tabs.get(shown).is_none()) && tabs.get(view).is_none()
        {
            return;
        }
        self.presentation.current.arrangement.reveal(view);
    }

    /// Makes `view` the active one, which selects the session it holds.
    pub(crate) fn activate(&mut self, view: ViewId, cx: &mut Context<Self>) {
        if self.presentation.current.customization.is_open() {
            return;
        }
        self.show_fitted(view, cx);
        let held = self.session_tabs(cx).get(view);
        if let Some(session) = held
            && self.selected(cx) != Some(session)
        {
            self.select(session, cx);
            self.presentation.current.arrangement.seen = Some(Some(session));
        }
    }

    /// Activates the view keyboard focus moved into, by Tab or F6, so the composer
    /// sends where the user is reading.
    pub(crate) fn activate_focused_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active_view(cx);
        let visible = self.presentation.current.arrangement.visible.clone();
        let focused = visible.into_iter().find(|&view| {
            Some(view) != active
                && self.view_key(view, cx).is_some_and(|key| {
                    self.presentation
                        .current
                        .views
                        .contains_focus(key, window, cx)
                })
        });
        if std::mem::replace(&mut self.presentation.current.arrangement.focused, focused) == focused
        {
            return;
        }
        if let Some(view) = focused {
            cx.defer_in(window, move |this, _, cx| this.activate(view, cx));
        }
    }

    /// Saves `layout`, when given, the tabs and the selection in one write. Tabs of
    /// views the layout no longer has are dropped.
    pub(crate) fn save_view(
        &mut self,
        layout: Option<Composition>,
        mut tabs: SessionTabs,
        cx: &mut Context<Self>,
    ) -> bool {
        let project = match &self.presentation.project {
            Ok(project) => project.clone(),
            Err(error) => {
                self.error = Some(format!(
                    "Layout not changed. Cannot save this project's layout. {error}"
                ));
                self.refresh(cx);
                return false;
            }
        };
        let shown = layout.clone().unwrap_or_else(|| self.saved_composition(cx));
        tabs.retain(|view, _| shown.component(view) == Some(Component::Conversation));
        let selected = self.selected(cx);
        let saved = match self.state.update(cx, |state, cx| {
            state.show(&project, self.window_id, layout, tabs, selected, cx)
        }) {
            Ok(applied) => {
                self.error = durability_warning(applied.durability);
                true
            }
            Err(error) => {
                self.error = Some(format!("Layout not changed. {error}"));
                false
            }
        };
        self.refresh(cx);
        saved
    }

    /// Saves `layout`, when given, and the tabs, and makes `view` active, selecting the
    /// session it holds, all in one write.
    pub(crate) fn commit_view(
        &mut self,
        layout: Option<Composition>,
        mut tabs: SessionTabs,
        view: ViewId,
        cx: &mut Context<Self>,
    ) -> bool {
        let selected = self.selected(cx);
        let Some(session) = tabs.get(view).filter(|&s| Some(s) != selected) else {
            let saved = self.save_view(layout, tabs, cx);
            if saved {
                self.show_fitted(view, cx);
            }
            return saved;
        };
        let shown = layout.clone().unwrap_or_else(|| self.saved_composition(cx));
        tabs.retain(|v, _| shown.component(v) == Some(Component::Conversation));
        let saved = self.select_showing(session, Some((layout, tabs)), cx);
        if saved {
            self.show_fitted(view, cx);
            self.presentation.current.arrangement.seen = Some(Some(session));
        }
        saved
    }

    /// The saved layout and tabs, with the active view held to the session it shows so
    /// a tab or pane opened beside it leaves it in place. `None` while Customize is open.
    pub(crate) fn layout_for_change(&self, cx: &App) -> Option<(Composition, SessionTabs, ViewId)> {
        if self.presentation.current.customization.is_open() {
            return None;
        }
        let mut tabs = self.session_tabs(cx);
        let active = self.active_view(cx)?;
        if let Some(session) = self.selected(cx)
            && tabs.get(active).is_none()
            && tabs.view_of(session).is_none()
        {
            tabs.hold(active, session);
        }
        Some((self.saved_composition(cx), tabs, active))
    }

    pub(crate) fn open_view(
        &mut self,
        c: &Composition,
        at: Place,
        cx: &mut Context<Self>,
    ) -> Option<(ViewId, Composition)> {
        let opened = self
            .presentation
            .current
            .customization
            .allocate()
            .and_then(|view| {
                c.edit(Edit::Open {
                    view,
                    component: Component::Conversation,
                    at,
                    size: Size::Fill(NonZeroU16::MIN),
                })
                .map(|layout| (view, layout))
                .map_err(|problem| format!("Layout not changed. {problem}"))
            });
        match opened {
            Ok(opened) => Some(opened),
            Err(error) => {
                self.error = Some(error);
                self.refresh(cx);
                None
            }
        }
    }

    /// Opens `session` in a new tab beside the active one, or brings forward the tab
    /// that holds it.
    pub(crate) fn open_in_new_tab(&mut self, session: u64, cx: &mut Context<Self>) {
        if self.session_tabs(cx).view_of(session).is_some() {
            return self.select(session, cx);
        }
        let Some((c, mut tabs, active)) = self.layout_for_change(cx) else {
            return self.select(session, cx);
        };
        if tabs.get(active).is_none() {
            return self.select(session, cx);
        }
        let Some((view, layout)) = self.open_view(&c, Place::TabWith(active), cx) else {
            return;
        };
        tabs.hold(view, session);
        if self.commit_view(Some(layout), tabs, view, cx) {
            self.presentation.current.customization.forget_undo();
        }
    }

    /// Starts a session with the active session's provider in a new tab, saving the
    /// session, the tab and the selection in one write.
    pub(crate) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let provider = self
            .selected_session(cx)
            .map_or(keystone_core::Provider::Claude, |s| s.provider());
        let Some((c, tabs, active)) = self
            .layout_for_change(cx)
            .filter(|(_, tabs, active)| tabs.get(*active).is_some())
        else {
            return self.start(provider, window, cx);
        };
        let Some((view, layout)) = self.open_view(&c, Place::TabWith(active), cx) else {
            return;
        };
        let before = self.selected(cx);
        self.start_showing(provider, Some((layout, tabs, view)), window, cx);
        let selected = self.selected(cx);
        if selected == before {
            return;
        }
        if self.session_tabs(cx).get(view) != selected {
            self.error = Some("Layout not changed. It is read-only.".into());
            return self.refresh(cx);
        }
        self.presentation.current.customization.forget_undo();
        self.presentation.current.arrangement.seen = Some(selected);
        self.show_fitted(view, cx);
    }

    /// Closes the active tab. The pane closes with its last tab and its split
    /// collapses. The last conversation view stays and follows the selection again.
    pub(crate) fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.presentation.current.customization.is_open() {
            return;
        }
        if let Some(view) = self.active_view(cx) {
            self.close_tab(view, window, cx);
        }
    }

    pub(crate) fn close_tab(&mut self, view: ViewId, window: &mut Window, cx: &mut Context<Self>) {
        if self.presentation.current.customization.is_open() {
            return self.edit_surfaces(Edit::Close(view), window, cx);
        }
        let before = self.saved_composition(cx);
        let views = conversations(&before);
        if views.len() < 2 {
            let mut tabs = self.session_tabs(cx);
            if tabs.get(view).is_some() {
                tabs.release(view);
                self.save_view(None, tabs, cx);
            }
            return;
        }
        let was_active = self.active_view(cx) == Some(view);
        let focused = self.view_contains_focus(view, window, cx);
        let after = match before.edit(Edit::Close(view)) {
            Ok(after) => after,
            Err(problem) => {
                self.error = Some(format!("Layout not changed. {problem}"));
                return self.refresh(cx);
            }
        };
        let replacement = replacement(&before, &after, view).filter(|_| was_active);
        let tabs = self.session_tabs(cx);
        let saved = match replacement {
            Some(replacement) => self.commit_view(Some(after), tabs, replacement, cx),
            None => self.save_view(Some(after), tabs, cx),
        };
        if !saved {
            return;
        }
        self.presentation.current.customization.forget_undo();
        if focused {
            let shown = self.shown_in_place_of(view, &before, cx);
            self.focus_view(shown, window, cx);
        }
        self.refresh(cx);
    }

    /// Brings `view` forward in its tab group and makes it active. Focus moves with it
    /// when it was in the view it replaces, or nowhere in particular.
    pub(crate) fn show_view(&mut self, view: ViewId, window: &mut Window, cx: &mut Context<Self>) {
        let follows = self.focus.is_focused(window)
            || self
                .active_view(cx)
                .is_some_and(|active| self.view_contains_focus(active, window, cx));
        let c = self.saved_composition(cx);
        let tabs = self.session_tabs(cx);
        if !self.commit_view(bring_forward(&c, view), tabs, view, cx) {
            return;
        }
        if follows && let Some(key) = self.view_key(view, cx) {
            self.presentation
                .current
                .views
                .focus(key, cx)
                .focus(window, cx);
        }
        self.refresh(cx);
    }

    /// Moves to the next or previous conversation tab, across panes, and wraps.
    pub(crate) fn cycle_tab(
        &mut self,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.presentation.current.customization.is_open() {
            return;
        }
        let views = conversations(&self.saved_composition(cx));
        let Some(at) = self
            .active_view(cx)
            .and_then(|active| views.iter().position(|&v| v == active))
        else {
            return;
        };
        if views.len() < 2 {
            return;
        }
        let next = if backwards {
            (at + views.len() - 1) % views.len()
        } else {
            (at + 1) % views.len()
        };
        self.show_view(views[next], window, cx);
    }

    /// Splits the active pane to the right. A tab that shares its pane moves into the
    /// new pane; a pane of one opens the most recent session not on screen beside it.
    pub(crate) fn split_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((c, mut tabs, active)) = self.layout_for_change(cx) else {
            return;
        };
        let beside = |target| Place::Beside {
            target,
            axis: Axis::Row,
            after: true,
        };
        if let Some(other) = tabs_holding(c.root(), active)
            .and_then(|(group, _)| group.iter().copied().find(|&v| v != active))
        {
            let layout = match c.edit(Edit::Move {
                view: active,
                to: beside(other),
            }) {
                Ok(layout) => layout,
                Err(problem) => {
                    self.error = Some(format!("Layout not changed. {problem}"));
                    return self.refresh(cx);
                }
            };
            if self.save_view(Some(layout), tabs, cx) {
                self.presentation.current.customization.forget_undo();
            }
            return;
        }
        let selected = self.selected(cx);
        let shown = |id: u64| Some(id) == selected || tabs.view_of(id).is_some();
        let Ok(project) = &self.presentation.project else {
            return;
        };
        let workspace = self.state.read(cx).store().workspace();
        let recent = |s: &&keystone_core::Session| s.updated_at().or(s.created_at());
        let next = workspace
            .sessions()
            .iter()
            .filter(|s| !s.settled() && !shown(s.id()) && crate::projects::holds(project, s))
            .max_by_key(recent)
            .or_else(|| {
                workspace
                    .sessions()
                    .iter()
                    .filter(|s| !shown(s.id()) && crate::projects::holds(project, s))
                    .max_by_key(recent)
            })
            .map(|s| s.id());
        let Some(session) = next else {
            self.error = Some("Open another session first.".into());
            return self.refresh(cx);
        };
        let Some((view, layout)) = self.open_view(&c, beside(active), cx) else {
            return;
        };
        tabs.hold(view, session);
        let follows = self.focus.is_focused(window) || self.view_contains_focus(active, window, cx);
        if !self.commit_view(Some(layout), tabs, view, cx) {
            return;
        }
        self.presentation.current.customization.forget_undo();
        if follows && let Some(key) = self.view_key(view, cx) {
            self.presentation
                .current
                .views
                .focus(key, cx)
                .focus(window, cx);
        }
    }

    /// A tab dropped on an edge of a pane: it leaves its strip and opens beside that
    /// pane. A pane's only tab dropped on its own edge stays where it is.
    pub(crate) fn split_with_tab(
        &mut self,
        view: ViewId,
        target: ViewId,
        axis: Axis,
        after: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.presentation.current.customization.is_open() {
            return;
        }
        let c = self.saved_composition(cx);
        let target = match tabs_holding(c.root(), target) {
            Some((group, _)) if group.contains(&view) => {
                match group.iter().copied().find(|&v| v != view) {
                    Some(sibling) => sibling,
                    None => return,
                }
            }
            _ if target == view => return,
            _ => target,
        };
        let moved = c.edit(Edit::Move {
            view,
            to: Place::Beside {
                target,
                axis,
                after,
            },
        });
        match moved {
            Ok(layout) => {
                let tabs = self.session_tabs(cx);
                let follows = self.focus.is_focused(window)
                    || self
                        .active_view(cx)
                        .is_some_and(|active| self.view_contains_focus(active, window, cx));
                if !self.commit_view(Some(layout), tabs, view, cx) {
                    return;
                }
                self.presentation.current.customization.forget_undo();
                if follows && let Some(key) = self.view_key(view, cx) {
                    self.presentation
                        .current
                        .views
                        .focus(key, cx)
                        .focus(window, cx);
                }
            }
            Err(problem) => {
                self.error = Some(format!("Layout not changed. {problem}"));
                self.refresh(cx);
            }
        }
    }

    /// A tab dropped on another slot of its own strip.
    pub(crate) fn reorder_tab(
        &mut self,
        view: ViewId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_surfaces(Edit::Reorder { view, index }, window, cx);
    }

    /// A tab dropped on another pane's strip, before the tab at `index`. The pane it left
    /// closes when it was that pane's last tab.
    pub(crate) fn move_tab(
        &mut self,
        view: ViewId,
        anchor: ViewId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.presentation.current.customization.is_open() {
            return;
        }
        let c = self.saved_composition(cx);
        let moved = c
            .edit(Edit::Move {
                view,
                to: Place::TabWith(anchor),
            })
            .and_then(|c| c.edit(Edit::Reorder { view, index }))
            .and_then(|c| c.edit(Edit::SelectTab(view)));
        match moved {
            Ok(layout) => {
                let tabs = self.session_tabs(cx);
                if self.commit_view(Some(layout), tabs, view, cx) {
                    self.presentation.current.customization.forget_undo();
                }
            }
            Err(problem) => {
                self.error = Some(format!("Layout not changed. {problem}"));
                self.refresh(cx);
            }
        }
    }

    /// The top bar's Split right, while one conversation view shows the selection.
    /// Once there are panes, each pane's strip has its own.
    pub(crate) fn split_toggle(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.presentation.current.customization.is_open()
            || self.presentation.project.is_err()
            || Self::has_session_tabs(&self.shown_composition(cx))
            || self.selected(cx).is_none()
        {
            return None;
        }
        Some(
            IconButton::new("split-right", IconName::Split)
                .size(ButtonSize::Compact)
                .debug_selector(|| "split-right".into())
                .tooltip(Tooltip::with_key(
                    "Split right",
                    keystone_ui::shortcut_text("secondary-\\"),
                ))
                .on_click(
                    self.scoped_listener(cx, |this, _, window, cx| this.split_right(window, cx)),
                ),
        )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "session_tabs_tests.rs"]
mod tests;
