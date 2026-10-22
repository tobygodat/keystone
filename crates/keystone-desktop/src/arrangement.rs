use crate::arrangement_editor::Action;
use crate::customization::label;
use crate::windows::Landing;
use crate::{Keystone, ViewKey, arch_mark, model_selector, now_ms};
use gpui::{
    Anchor, AnyElement, App, AvailableSpace, Bounds, Context, Empty, Focusable, FontWeight,
    IntoElement, MouseButton, Pixels, Point, SharedString, Window, canvas, div, prelude::*, px,
    relative, size,
};
use keystone_core::{
    ApprovalMode, Axis, Component, Composition, Edit, Node, NodePath, Place, Provider, Size, ViewId,
};
use keystone_ui::prelude::*;
use std::collections::HashMap;
use std::num::NonZeroU16;

const GRIP: f32 = 6.;
const CONVERSATION_MIN: f32 = 320.;
const COMPOSER_MIN: f32 = 420.;
const SESSION_MIN: f32 = 220.;
const FILES_MIN: f32 = 220.;

#[derive(Default)]
pub struct Arrangement {
    pub visible: Vec<ViewId>,
    pub fitted: bool,
    /// The surface the fitted tabs show, chosen while the window was fitted.
    tab: Option<ViewId>,
    pub sidebar_overlay: bool,
    /// The surface group whose "+" menu is open, by the view it would tab with. The
    /// menu counts as open only while that view's strip is drawn.
    pub surface_menu: Option<ViewId>,
    geometry: HashMap<NodePath, Bounds<Pixels>>,
    toolbar_height: Pixels,
    composer_min_width: f32,
    resize: Option<Resize>,
    /// The conversation view the user last worked in. See `session_tabs`.
    pub active: Option<ViewId>,
    /// The selection `follow_selection` last placed, so it places each one once.
    pub seen: Option<Option<u64>>,
    /// The inactive view keyboard focus is in, so focus moving there activates it once.
    pub focused: Option<ViewId>,
    /// Each drawn tab strip's id and its views, so a tab dropped from another strip can
    /// be found.
    pub(crate) bars: Vec<(gpui::ElementId, Vec<ViewId>)>,
    /// Pane sizes being dragged outside Customize, shown until the drop saves them.
    pane_resize: Option<(NodePath, Vec<Size>)>,
    /// The fitted tab row's scroll, and the tab it last scrolled into view.
    fitted_scroll: gpui::ScrollHandle,
    fitted_revealed: Option<ViewId>,
}
struct Resize {
    start: Point<Pixels>,
    split: NodePath,
    axis: Axis,
    index: usize,
    sizes: Vec<Size>,
    extents: [f32; 2],
}
#[derive(Clone)]
struct ComponentDrag(ViewId, crate::presentation::Activation);
struct ResizeGrip;

pub fn selector(base: &str, view: ViewId) -> SharedString {
    if view == ViewId::MAIN {
        base.to_owned().into()
    } else {
        format!("{base}-view-{}", view.0).into()
    }
}

pub fn ordered_views(node: &Node) -> Vec<ViewId> {
    match node {
        Node::View { id } => vec![*id],
        Node::Tabs { views, .. } => views.clone(),
        Node::Split { children, .. } => children
            .iter()
            .flat_map(|c| ordered_views(&c.node))
            .collect(),
    }
}

pub(crate) fn first_displayed_conversation(node: &Node, c: &Composition) -> Option<ViewId> {
    let conversation = |id: &ViewId| c.component(*id) == Some(Component::Conversation);
    match node {
        Node::View { id } => conversation(id).then_some(*id),
        Node::Tabs { selected, views } => views
            .get(*selected)
            .filter(|id| conversation(id))
            .or_else(|| views.iter().find(|id| conversation(id)))
            .copied(),
        Node::Split { children, .. } => children
            .iter()
            .find_map(|child| first_displayed_conversation(&child.node, c)),
    }
}

fn minimum(
    node: &Node,
    c: &Composition,
    chrome: bool,
    sidebar: bool,
    composer_width: f32,
) -> [f32; 2] {
    let header = if chrome { 30. } else { 0. };
    match node {
        Node::View { id } => match c.component(*id).unwrap() {
            Component::Conversation => [CONVERSATION_MIN, 120. + header],
            Component::Composer => [composer_width, 142. + header],
            Component::SessionList => {
                if sidebar {
                    [SESSION_MIN, 160. + header]
                } else {
                    [0., 0.]
                }
            }
            Component::Files | Component::Notes => [FILES_MIN, 160. + header],
            // The message box and Commit button sit above the list.
            Component::Changes => [FILES_MIN, 260. + header],
        },
        Node::Tabs { views, .. } => {
            let changes = views
                .iter()
                .any(|view| c.component(*view) == Some(Component::Changes));
            [CONVERSATION_MIN, if changes { 260. } else { 154. } + header]
        }
        Node::Split { axis, children } => {
            let values: Vec<_> = children
                .iter()
                .map(|child| minimum(&child.node, c, chrome, sidebar, composer_width))
                .collect();
            let along = if *axis == Axis::Row { 0 } else { 1 };
            let mut result = [0., 0.];
            result[along] = values.iter().map(|s| s[along]).sum::<f32>()
                + if chrome {
                    GRIP * (values.len() - 1) as f32
                } else {
                    0.
                };
            result[1 - along] = values.iter().map(|s| s[1 - along]).fold(0., f32::max);
            result
        }
    }
}

impl Arrangement {
    pub(crate) fn suspend(&mut self) {
        self.visible.clear();
        self.geometry.clear();
        self.resize = None;
        self.sidebar_overlay = false;
        self.surface_menu = None;
        self.toolbar_height = px(0.);
        self.seen = None;
        self.focused = None;
        self.bars.clear();
        self.pane_resize = None;
    }

    pub(crate) fn restore_tabs(&mut self) {
        self.tab = None;
    }

    /// Shows `view` as the fitted tab while the window is fitted.
    pub(crate) fn reveal(&mut self, view: ViewId) {
        self.tab = Some(view);
    }

    /// Whether a window this wide is too narrow for the composer's full controls.
    pub(crate) fn narrows_composer(&self, window_width: Pixels) -> bool {
        self.fitted && f32::from(window_width) < self.composer_min_width
    }
}

/// The tab group holding `view` and its selected index.
pub(crate) fn tabs_holding(node: &Node, view: ViewId) -> Option<(&[ViewId], usize)> {
    match node {
        Node::View { .. } => None,
        Node::Tabs { selected, views } => views.contains(&view).then_some((views, *selected)),
        Node::Split { children, .. } => children
            .iter()
            .find_map(|child| tabs_holding(&child.node, view)),
    }
}

fn displays(node: &Node, view: ViewId) -> bool {
    match node {
        Node::View { id } => *id == view,
        Node::Tabs { selected, views } => views[*selected] == view,
        Node::Split { children, .. } => children.iter().any(|child| displays(&child.node, view)),
    }
}

/// Files, Changes and Notes: one-of-a-kind surfaces that get a tab strip even alone.
fn is_side_surface(component: Option<Component>) -> bool {
    matches!(
        component,
        Some(Component::Files | Component::Changes | Component::Notes)
    )
}

/// Whether a strip drawn for `node` has `anchor` in front, so its "+" is on screen.
fn strip_shows(node: &Node, c: &Composition, anchor: ViewId) -> bool {
    match node {
        Node::View { id } => {
            *id == anchor && (is_side_surface(c.component(anchor)) || Keystone::has_session_tabs(c))
        }
        Node::Tabs { selected, views } => views[*selected] == anchor,
        Node::Split { children, .. } => children
            .iter()
            .any(|child| strip_shows(&child.node, c, anchor)),
    }
}

impl Keystone {
    /// A tab's title: the session a conversation shows, else the component and its
    /// occurrence id.
    fn tab_label(&self, id: ViewId, c: &Composition, cx: &App) -> String {
        match c.component(id).unwrap() {
            component @ (Component::Files | Component::Notes) => label(component).to_owned(),
            Component::Changes => self.changes.label(self.files_root(cx).ok().as_deref(), cx),
            Component::Conversation if !self.presentation.current.customization.is_open() => self
                .view_session(id, cx)
                .and_then(|session| self.state.read(cx).session(session))
                .map_or_else(|| "New session".to_owned(), |s| s.title().to_owned()),
            component => format!("{} {}", label(component), id.0),
        }
    }

    /// The composition on screen: the Customize draft while it is open, else the saved one.
    pub(crate) fn shown_composition(&self, cx: &App) -> Composition {
        let saved = self.saved_composition(cx);
        if let Some((split, sizes)) = &self.presentation.current.arrangement.pane_resize
            && !self.presentation.current.customization.is_open()
        {
            return saved
                .edit(Edit::Resize {
                    split: split.clone(),
                    sizes: sizes.clone(),
                })
                .unwrap_or(saved);
        }
        self.presentation
            .current
            .customization
            .shown(&saved)
            .clone()
    }

    pub(crate) fn arrangement_has_sidebar(&self, cx: &App) -> bool {
        self.shown_composition(cx)
            .views()
            .any(|(_, component)| component == Component::SessionList)
    }

    /// Applies `edit` to the project's layout right away, or to the Customize draft while
    /// it is open, and reports a refused edit or a failed save in the error line. A
    /// structural edit withdraws Customize Undo; choosing a tab does not.
    pub(crate) fn edit_surfaces(
        &mut self,
        edit: Edit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = self.shown_composition(cx);
        let closing = match edit {
            Edit::Close(view) if self.view_contains_focus(view, window, cx) => Some(view),
            _ => None,
        };
        let selecting = matches!(edit, Edit::SelectTab(_));
        if self.presentation.current.customization.is_open() {
            self.customize(Action::Edit(edit), window, cx);
        } else {
            match before.edit(edit) {
                Err(problem) => self.error = Some(format!("Layout not changed. {problem}")),
                Ok(layout) => {
                    let tabs = self.session_tabs(cx);
                    if self.save_view(Some(layout), tabs, cx) && !selecting {
                        self.presentation.current.customization.forget_undo();
                    }
                }
            }
        }
        // A closed view's focus handle outlives it, and a handle that is no longer drawn
        // swallows every shortcut, so focus moves to what took the view's place.
        if let Some(closed) = closing
            && self.shown_composition(cx).component(closed).is_none()
        {
            let shown = self.shown_in_place_of(closed, &before, cx);
            self.focus_view(shown, window, cx);
        }
        self.refresh(cx);
    }

    /// The view the tab `drag` left holds, when it left this window.
    pub(crate) fn dragged_view(&self, drag: &keystone_ui::DraggedTab) -> Option<ViewId> {
        if drag.window != self.handle.window_id() {
            return None;
        }
        self.presentation
            .current
            .arrangement
            .bars
            .iter()
            .find(|(id, _)| *id == drag.bar_id)
            .and_then(|(_, views)| views.get(drag.index).copied())
    }

    /// The view on screen where `closed` was: the rest of its tab group, or whatever
    /// the fitted tabs fall back to.
    pub(crate) fn shown_in_place_of(
        &self,
        closed: ViewId,
        before: &Composition,
        cx: &App,
    ) -> Option<ViewId> {
        let after = self.shown_composition(cx);
        if self.presentation.current.arrangement.fitted {
            return self
                .fitted_tab(&after)
                .or_else(|| first_displayed_conversation(after.root(), &after));
        }
        let (group, _) = tabs_holding(before.root(), closed)?;
        let sibling = group.iter().copied().find(|&view| view != closed)?;
        Some(match tabs_holding(after.root(), sibling) {
            Some((views, selected)) => views[selected],
            None => sibling,
        })
    }

    /// Focuses the content of `view`, or the window when there is none to focus.
    pub(crate) fn focus_view(
        &mut self,
        view: Option<ViewId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let component = view.and_then(|view| self.shown_composition(cx).component(view));
        let session = view.and_then(|view| self.view_session(view, cx));
        match (component, view, session) {
            (Some(Component::Files), _, _) => self.focus_files(window, cx),
            (Some(Component::Notes), _, _) => self.focus_notes(window, cx),
            (Some(Component::Changes), _, _) => self.focus_changes(window, cx),
            (Some(Component::Conversation), Some(view), Some(session)) => self
                .presentation
                .current
                .views
                .focus(ViewKey { view, session }, cx)
                .focus(window, cx),
            _ => self.focus.focus(window, cx),
        }
    }

    /// Moves focus to `shown` when one of the `hidden` views it covers held it, or when
    /// nothing in particular did. A click on a tab lands focus on the window first, so
    /// the tab brought forward takes it from there.
    fn focus_replacing(
        &mut self,
        hidden: &[ViewId],
        shown: ViewId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focus.is_focused(window)
            || hidden
                .iter()
                .any(|&view| view != shown && self.view_contains_focus(view, window, cx))
        {
            self.focus_view(Some(shown), window, cx);
        }
    }

    pub(crate) fn view_contains_focus(
        &mut self,
        view: ViewId,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.shown_composition(cx).component(view) == Some(Component::Conversation) {
            return self.view_session(view, cx).is_some_and(|session| {
                self.presentation.current.views.contains_focus(
                    ViewKey { view, session },
                    window,
                    cx,
                )
            });
        }
        if self.shown_composition(cx).component(view) == Some(Component::Notes) {
            return self.notes_contain_focus(window, cx);
        }
        let handle = match self.shown_composition(cx).component(view) {
            Some(Component::Files) => self
                .files_root(cx)
                .ok()
                .map(|root| self.files.browser(&root, cx).focus_handle(cx)),
            Some(Component::Changes) => self.changes_panel(cx).map(|panel| panel.focus_handle(cx)),
            Some(Component::Conversation) => self.view_session(view, cx).map(|session| {
                self.presentation
                    .current
                    .views
                    .focus(ViewKey { view, session }, cx)
            }),
            _ => None,
        };
        handle.is_some_and(|handle| handle.contains_focused(window, cx))
    }

    /// The fitted tab's choice while `c` still shows that surface.
    /// The surface a fitted window shows: the one chosen there, else the layout's first
    /// displayed conversation while it shows the selected session, else the active view.
    pub(crate) fn fitted_shown(&self, c: &Composition, cx: &App) -> Option<ViewId> {
        let first = first_displayed_conversation(c.root(), c);
        self.fitted_tab(c).or_else(|| {
            let selected = self.selected(cx);
            if self.presentation.current.customization.is_open()
                || first.is_some_and(|view| self.view_session(view, cx) == selected)
            {
                return first;
            }
            self.active_view(cx).or(first)
        })
    }

    pub(crate) fn fitted_tab(&self, c: &Composition) -> Option<ViewId> {
        self.presentation
            .current
            .arrangement
            .tab
            .filter(|&view| c.component(view).is_some_and(Component::is_surface))
    }

    /// Brings `view` to the front: of the fitted tabs in a fitted window, else of its
    /// tab group.
    pub(crate) fn reveal_view(
        &mut self,
        view: ViewId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A view that was just opened can leave too little room, so the window fits its
        // views as tabs from the next frame. The choice keeps the view in front then; a
        // frame that is not fitted clears it.
        self.presentation.current.arrangement.tab = Some(view);
        if !self.presentation.current.arrangement.fitted {
            return self.select_tab(view, window, cx);
        }
        let surfaces: Vec<_> = self
            .shown_composition(cx)
            .views()
            .filter(|(_, component)| component.is_surface())
            .map(|(id, _)| id)
            .collect();
        self.focus_replacing(&surfaces, view, window, cx);
        cx.notify();
    }

    /// Closes the `component` view (Files or Notes, of which a layout holds at most
    /// one) when it has focus, brings an open one forward and focuses it, or opens one
    /// `width` wide to the right of the side surface target, and focuses it.
    pub(crate) fn toggle_surface(
        &mut self,
        component: Component,
        width: Size,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.shown_composition(cx);
        if let Some((open, _)) = shown.views().find(|(_, c)| *c == component) {
            if self.view_contains_focus(open, window, cx) {
                return self.edit_surfaces(Edit::Close(open), window, cx);
            }
            self.reveal_view(open, window, cx);
            return self.focus_view(Some(open), window, cx);
        }
        let at = Place::Beside {
            target: self.side_surface_target(cx),
            axis: Axis::Row,
            after: true,
        };
        self.open_surface(component, at, width, window, cx);
        if let Some((open, _)) = self
            .shown_composition(cx)
            .views()
            .find(|(_, c)| *c == component)
        {
            self.reveal_view(open, window, cx);
            self.focus_view(Some(open), window, cx);
        }
    }

    /// Opens a new occurrence of `component` at `at` with a fresh view id.
    pub(crate) fn open_surface(
        &mut self,
        component: Component,
        at: Place,
        size: Size,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.presentation.current.customization.allocate() {
            Ok(view) => self.edit_surfaces(
                Edit::Open {
                    view,
                    component,
                    at,
                    size,
                },
                window,
                cx,
            ),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    /// The composer's minimum holds the widest of each control any session shows, so a
    /// Send, a provider's report or another selection never refits the arrangement.
    fn composer_minimum_width(&self, window: &mut Window, cx: &mut Context<Self>) -> f32 {
        let Some(session) = self.selected_session(cx) else {
            return COMPOSER_MIN;
        };
        let folder = self.conversation_folder(session.id(), cx).unwrap();
        let models = [Provider::Claude, Provider::Codex].map(|provider| {
            let selection = model_selector::Selection {
                provider,
                account: None,
                accounts: &[],
                model: None,
                reported_model: None,
                started: false,
                choices: model_selector::Choices::Loading,
            };
            model_selector::trigger(selection, false, cx).0
        });
        let provider = h_flex()
            .gap(px(7.))
            .flex_none()
            .child(v_flex().children(models))
            .child(div().w_0().flex_none());
        let approvals = ApprovalMode::ALL.map(|mode| {
            Button::new(("measure-approval", mode as usize), mode.label())
                .size(ButtonSize::Compact)
                .icon(IconName::Shield)
        });
        let mut controls = h_flex()
            .gap(px(14.))
            .font_ui(cx)
            .text_size(TextSize::Base.px())
            .child(provider)
            .child(div().w_px().flex_none())
            .child(folder.button("measure-project"))
            .child(v_flex().children(approvals))
            .child(div().w_0().flex_none())
            .child(
                Button::new("measure-stop", "Stop")
                    .style(ButtonStyle::Subtle)
                    .icon(IconName::Square),
            )
            .child(div().w(px(32.)).flex_none())
            .into_any_element();
        let measured = controls
            .layout_as_root(
                size(AvailableSpace::MaxContent, AvailableSpace::MinContent),
                window,
                cx,
            )
            .width;
        (f32::from(measured) + 56.).max(COMPOSER_MIN)
    }

    pub(crate) fn update_arrangement_fitting(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let c = self.shown_composition(cx);
        self.presentation.current.arrangement.composer_min_width =
            self.composer_minimum_width(window, cx);
        let required = minimum(
            c.root(),
            &c,
            self.presentation.current.customization.is_open(),
            self.presentation.current.sidebar,
            self.presentation.current.arrangement.composer_min_width,
        );
        let viewport = window.viewport_size();
        let toolbar = if self.presentation.current.customization.is_open() {
            f32::from(self.presentation.current.arrangement.toolbar_height)
        } else {
            0.
        };
        let fitted = f32::from(viewport.width) < required[0]
            || f32::from(viewport.height) - 44. - toolbar < required[1];
        if fitted != self.presentation.current.arrangement.fitted
            || !c
                .views()
                .any(|(_, component)| component == Component::SessionList)
        {
            self.presentation.current.arrangement.sidebar_overlay = false;
        }
        if !fitted {
            self.presentation.current.arrangement.tab = None;
        }
        self.presentation.current.arrangement.fitted = fitted;
    }

    pub(crate) fn arrangement_body(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = self.shown_composition(cx);
        let fitted = self.presentation.current.arrangement.fitted;
        let viewport = window.viewport_size();
        let toolbar = if self.presentation.current.customization.is_open() {
            f32::from(self.presentation.current.arrangement.toolbar_height)
        } else {
            0.
        };
        self.presentation.current.arrangement.visible.clear();
        self.presentation.current.arrangement.bars.clear();
        if self
            .presentation
            .current
            .arrangement
            .surface_menu
            .is_some_and(|anchor| fitted || !strip_shows(c.root(), &c, anchor))
        {
            self.presentation.current.arrangement.surface_menu = None;
        }
        let body = if fitted {
            let surfaces: Vec<_> = ordered_views(c.root())
                .into_iter()
                .filter(|id| c.component(*id).is_some_and(Component::is_surface))
                .collect();
            let selected = self.fitted_shown(&c, cx).unwrap();
            let arrangement = &mut self.presentation.current.arrangement;
            if arrangement.fitted_revealed != Some(selected)
                && let Some(index) = surfaces.iter().position(|&id| id == selected)
            {
                if arrangement.fitted_scroll.bounds_for_item(index).is_some() {
                    arrangement.fitted_scroll.scroll_to_item(index);
                    arrangement.fitted_revealed = Some(selected);
                } else {
                    window.request_animation_frame();
                }
            }
            let composer = c
                .views()
                .find(|(_, k)| *k == Component::Composer)
                .unwrap()
                .0;
            let mut body = v_flex().relative().flex_1().min_h_0().w_full()
                .when(f32::from(viewport.height)-44.-toolbar > 230., |d| d.child(div().flex_none().px_2().py_1().child(div().debug_selector(|| "fitted-hint".into()).child(Label::new("Views use tabs and the composer stays below until this window has more room.").size(TextSize::Sm).color(Color::Muted)))))
                .child(h_flex().id("fitted-tabs").debug_selector(|| "fitted-tabs".into()).flex_none().w_full().min_w_0().gap_1().overflow_x_scroll().track_scroll(&self.presentation.current.arrangement.fitted_scroll).children(surfaces.iter().map(|&id| {
                    self.conversation_tab(format!("fitted-tab-{}", id.0), self.tab_label(id, &c, cx), selected == id, cx)
                        .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                            this.reveal_view(id, window, cx);
                            if this.shown_composition(cx).component(id) == Some(Component::Conversation) {
                                this.activate(id, cx);
                            }
                        }))
                        .on_mouse_down(gpui::MouseButton::Right, self.scoped_listener(cx, move |this, event: &gpui::MouseDownEvent, _, cx| {
                            if this.shown_composition(cx).component(id) == Some(Component::Conversation)
                                && let Some(session) = this.view_session(id, cx)
                            {
                                this.open_context_menu(session, event.position, cx);
                            }
                        }))
                })))
                .child(div().flex_1().min_h_0().overflow_hidden().child(self.arrangement_view(selected, &c, false, viewport, cx)))
                .child(div().flex_none().w_full().child(self.arrangement_view(composer, &c, true, viewport, cx)));
            if self.presentation.current.arrangement.sidebar_overlay {
                if let Some((id, _)) = c.views().find(|(_, k)| *k == Component::SessionList) {
                    body = body.child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .w(px(SESSION_MIN.max((f32::from(viewport.width) * 0.8).min(
                                f32::from(keystone_core::SidebarWidth::DEFAULT.px()),
                            ))))
                            .occlude()
                            .child(self.arrangement_view(id, &c, false, viewport, cx)),
                    );
                }
            }
            body.into_any_element()
        } else {
            self.arrangement_node(c.root(), &c, NodePath::default(), false, viewport, cx)
        };
        let displayed: Vec<_> = c
            .views()
            .filter(|(view, component)| {
                component.is_surface()
                    && if fitted {
                        self.fitted_shown(&c, cx) == Some(*view)
                    } else {
                        displays(c.root(), *view)
                    }
            })
            .map(|(view, _)| view)
            .collect();
        for (view, _) in c
            .views()
            .filter(|(view, component)| component.is_surface() && !displayed.contains(view))
        {
            if self.view_contains_focus(view, window, cx) {
                let replacement = tabs_holding(c.root(), view)
                    .map(|(views, selected)| views[selected])
                    .filter(|view| displayed.contains(view))
                    .or_else(|| displayed.first().copied());
                self.focus_view(replacement, window, cx);
                break;
            }
        }
        body
    }

    fn arrangement_node(
        &mut self,
        node: &Node,
        c: &Composition,
        path: NodePath,
        auto: bool,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            Node::View { id }
                if is_side_surface(c.component(*id))
                    || (c.component(*id) == Some(Component::Conversation)
                        && Self::has_session_tabs(c)
                        && !self.presentation.current.customization.is_open()) =>
            {
                self.surface_group(std::slice::from_ref(id), 0, c, viewport, cx)
            }
            Node::View { id } => self.arrangement_view(*id, c, auto, viewport, cx),
            Node::Tabs { selected, views } => self.surface_group(views, *selected, c, viewport, cx),
            Node::Split { axis, children } => {
                if *axis == Axis::Column
                    && children.len() == 2
                    && self.selected_session(cx).is_none()
                    && !self.fixture
                    && !self.presentation.current.customization.is_open()
                    && matches!(children[0].node, Node::View { id } if c.component(id) == Some(Component::Conversation))
                    && matches!(children[1].node, Node::View { id } if c.component(id) == Some(Component::Composer))
                {
                    let first = ordered_views(&children[0].node)[0];
                    let second = ordered_views(&children[1].node)[0];
                    return v_flex()
                        .size_full()
                        .min_w_0()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .pb(px(60.))
                        .child(
                            div()
                                .h(px(110.))
                                .w_full()
                                .child(self.arrangement_view(first, c, false, viewport, cx)),
                        )
                        .child(self.arrangement_view(second, c, true, viewport, cx))
                        .into_any_element();
                }
                let axis = *axis;
                let geometry_path = path.clone();
                let activation = self.presentation.activation();
                let app = cx.weak_entity();
                let mut split = div()
                    .id(SharedString::from(format!("split-{:?}", path.0)))
                    .relative()
                    .flex()
                    .min_h_0()
                    .min_w_0()
                    .when(axis == Axis::Column, |d| d.flex_col())
                    .when(!auto, |d| d.size_full())
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                let _ = app.update(cx, |this, _cx| {
                                    if !this.presentation.matches(activation) {
                                        return;
                                    }
                                    this.presentation
                                        .current
                                        .arrangement
                                        .geometry
                                        .insert(geometry_path.clone(), bounds);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    );
                for (i, child) in children.iter().enumerate() {
                    let mut next = path.0.clone();
                    next.push(i);
                    let child_path = NodePath(next);
                    let child_min = minimum(
                        &child.node,
                        c,
                        self.presentation.current.customization.is_open(),
                        self.presentation.current.sidebar,
                        self.presentation.current.arrangement.composer_min_width,
                    );
                    let hidden_sidebar = !self.presentation.current.sidebar
                        && matches!(&child.node, Node::View { id } if c.component(*id) == Some(Component::SessionList));
                    if hidden_sidebar {
                        continue;
                    }
                    let editing = self.presentation.current.customization.is_open();
                    let panes = !editing
                        && i > 0
                        && [&children[i - 1].node, &child.node].iter().all(|node| {
                            ordered_views(node)
                                .iter()
                                .all(|&v| c.component(v) == Some(Component::Conversation))
                        });
                    if i > 0 && (editing || panes) {
                        split = split.child(self.split_grip(
                            path.clone(),
                            axis,
                            i - 1,
                            children.iter().map(|c| c.size).collect(),
                            cx,
                        ));
                    }
                    let activation = self.presentation.activation();
                    let app = cx.weak_entity();
                    let measure = child_path.clone();
                    let content = self.arrangement_node(
                        &child.node,
                        c,
                        child_path,
                        child.size == Size::Auto,
                        viewport,
                        cx,
                    );
                    split = split.child(
                        div()
                            .relative()
                            .map(|d| match child.size {
                                Size::Auto => d
                                    .flex_shrink(1.)
                                    .when(axis == Axis::Row, |d| d.min_w(px(child_min[0])))
                                    .when(axis == Axis::Column, |d| d.min_h(px(child_min[1]))),
                                Size::Fill(weight) => d
                                    .when(axis == Axis::Row, |d| d.min_w(px(child_min[0])))
                                    .when(axis == Axis::Column, |d| d.min_h(px(child_min[1])))
                                    .flex_basis(px(0.))
                                    .flex_grow(f32::from(weight.get()))
                                    .flex_shrink(1.),
                                Size::Px(extent) => {
                                    if axis == Axis::Row {
                                        d.w(px(f32::from(extent.get())))
                                            .min_w(px(child_min[0]))
                                            .flex_shrink(1.)
                                    } else {
                                        d.h(px(f32::from(extent.get())))
                                            .min_h(px(child_min[1]))
                                            .flex_shrink(1.)
                                    }
                                }
                            })
                            .map(|d| match axis {
                                Axis::Column => d.min_w_0().w_full(),
                                Axis::Row if !auto => d.min_h_0().h_full(),
                                Axis::Row => d,
                            })
                            .child(
                                canvas(
                                    move |bounds, _, cx| {
                                        let _ = app.update(cx, |this, _cx| {
                                            if !this.presentation.matches(activation) {
                                                return;
                                            }
                                            this.presentation
                                                .current
                                                .arrangement
                                                .geometry
                                                .insert(measure.clone(), bounds);
                                        });
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .inset_0(),
                            )
                            .child(content),
                    );
                }
                split.into_any_element()
            }
        }
    }

    /// Surfaces sharing one area under a tab strip: the tabs, each with a close button,
    /// and a "+" that opens another surface there. A lone Files view is a group of one,
    /// so it can be closed and joined; so is a lone conversation once there are several,
    /// each pane naming the sessions it shows. Session tabs drag to reorder and to move
    /// between panes, and the active pane's strip is underlined.
    fn surface_group(
        &mut self,
        views: &[ViewId],
        selected: usize,
        c: &Composition,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (select, close, order, insert) =
            (views.to_vec(), views.to_vec(), views.to_vec(), views[0]);
        let colors = cx.theme().colors;
        let editing = self.presentation.current.customization.is_open();
        let lone = (views.len() == 1)
            .then(|| c.component(views[0]))
            .flatten()
            .filter(|component| is_side_surface(Some(*component)));
        let bar_id = SharedString::from(if let Some(component) = lone {
            label(component).to_lowercase()
        } else {
            format!("surface-tabs-{}", views[0].0)
        });
        self.presentation
            .current
            .arrangement
            .bars
            .push((bar_id.clone().into(), views.to_vec()));
        let active = self.active_view(cx);
        let sessions = Self::has_session_tabs(c) && !editing;
        let running = |this: &Self, id: ViewId, cx: &App| {
            c.component(id) == Some(Component::Conversation)
                && this.view_session(id, cx).is_some_and(|session| {
                    this.state
                        .read(cx)
                        .conversations()
                        .get(&session)
                        .is_some_and(|c| c.running)
                })
        };
        let tabs: Vec<_> = views
            .iter()
            .map(|&id| {
                Tab::new(("surface-tab", id.0 as usize), self.tab_label(id, c, cx))
                    .dot(running(self, id, cx).then_some(TabDot::Working))
            })
            .collect();
        let mut bar = TabBar::new(bar_id)
            .children(tabs)
            .selected(selected)
            .on_select(
                self.scoped_listener(cx, move |this, ix: &usize, window, cx| {
                    let view = select[*ix];
                    if this.presentation.current.customization.is_open()
                        || this.shown_composition(cx).component(view)
                            != Some(Component::Conversation)
                    {
                        return this.select_tab(view, window, cx);
                    }
                    this.show_view(view, window, cx);
                }),
            )
            .on_close(
                self.scoped_listener(cx, move |this, ix: &usize, window, cx| {
                    let view = close[*ix];
                    if this.shown_composition(cx).component(view) == Some(Component::Conversation) {
                        this.close_tab(view, window, cx)
                    } else {
                        this.edit_surfaces(Edit::Close(view), window, cx)
                    }
                }),
            );
        if sessions {
            let menu = views.to_vec();
            bar = bar
                .on_menu(self.scoped_listener(
                    cx,
                    move |this, &(ix, position): &(usize, gpui::Point<Pixels>), _, cx| {
                        if let Some(session) = this.view_session(menu[ix], cx) {
                            this.open_context_menu(session, position, cx);
                        }
                    },
                ))
                .on_move(self.scoped_listener(
                    cx,
                    move |this, &(from, to): &(usize, usize), window, cx| {
                        this.reorder_tab(order[from], to, window, cx)
                    },
                ))
                .on_insert(self.scoped_listener(
                    cx,
                    move |this, (drag, at): &(DraggedTab, usize), _, cx| {
                        if let Some(view) = this.dragged_view(drag) {
                            this.move_tab(view, insert, *at, cx);
                        } else {
                            let landing = Landing::Strip {
                                anchor: insert,
                                index: *at,
                            };
                            this.receive_tab(drag, landing, cx);
                        }
                    },
                ));
        }
        let in_front = views.iter().any(|&view| Some(view) == active);
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .pr_1()
                    .bg(colors.sidebar)
                    .map(|d| {
                        if sessions && in_front {
                            d.border_b_2().border_color(colors.accent)
                        } else {
                            d.hairline_b(cx)
                        }
                    })
                    .child(div().flex_1().min_w_0().child(bar))
                    .when(sessions, |d| {
                        let view = views[selected];
                        d.child(
                            IconButton::new(("pane-split", view.0 as usize), IconName::Split)
                                .size(ButtonSize::Compact)
                                .debug_selector(move || format!("pane-split-{}", view.0))
                                .tooltip(Tooltip::text("Split right · Ctrl+\\"))
                                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                                    this.activate(view, cx);
                                    this.split_right(window, cx)
                                })),
                        )
                    })
                    .child(self.surface_menu(views[selected], c, cx)),
            )
            .child(div().flex_1().min_h_0().child(self.arrangement_view(
                views[selected],
                c,
                false,
                viewport,
                cx,
            )))
            .into_any_element()
    }

    /// The "+" of a surface group, listing the surfaces the composition still allows.
    /// A chosen one opens as a tab with `anchor`.
    fn surface_menu(
        &self,
        anchor: ViewId,
        c: &Composition,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.presentation.current.arrangement.surface_menu == Some(anchor);
        let mut menu = PopoverMenu::new(
            ("surface-menu", anchor.0 as usize),
            IconButton::new(("surface-add", anchor.0 as usize), IconName::Plus)
                .size(ButtonSize::Compact)
                .selected(open)
                .debug_selector(|| "surface-add".into())
                .tooltip(Tooltip::text("Open a surface")),
        )
        .anchor(Anchor::TopRight)
        .open(open)
        .on_open_change(self.scoped_listener(cx, move |this, open: &bool, _, cx| {
            this.presentation.current.arrangement.surface_menu = open.then_some(anchor);
            cx.notify();
        }))
        .child(MenuHeader::new("Open a surface"));
        for (component, icon, id) in [
            (Component::Files, IconName::Folder, "surface-add-files"),
            (Component::Notes, IconName::Note, "surface-add-notes"),
            (Component::Changes, IconName::Branch, "surface-add-changes"),
            (
                Component::Conversation,
                IconName::Chat,
                "surface-add-conversation",
            ),
        ] {
            let count = c.views().filter(|(_, k)| *k == component).count();
            if !component.allows(count + 1) {
                continue;
            }
            menu = menu.child(
                MenuItem::new(id, label(component))
                    .icon(icon)
                    .debug_selector(|| id.into())
                    .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                        this.open_surface(
                            component,
                            Place::TabWith(anchor),
                            Size::Fill(NonZeroU16::MIN),
                            window,
                            cx,
                        );
                        match component {
                            Component::Files => this.focus_files(window, cx),
                            Component::Notes => this.focus_notes(window, cx),
                            Component::Changes => this.focus_changes(window, cx),
                            _ => {}
                        }
                    })),
            );
        }
        menu
    }

    fn conversation_tab(
        &self,
        id: String,
        text: String,
        selected: bool,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = cx.theme().colors;
        h_flex()
            .id(SharedString::from(id.clone()))
            .automation_id(id)
            .h(px(26.))
            .flex_shrink_0()
            .max_w(px(200.))
            .px_2()
            .rounded(radius::CONTROL)
            .bg(colors
                .selection_fill
                .opacity(if selected { 1. } else { 0. }))
            .border_1()
            .border_color(
                colors
                    .selection_ring
                    .opacity(if selected { 1. } else { 0. }),
            )
            .cursor_pointer()
            .hover(move |d| d.bg(colors.selection_fill))
            .child(
                Label::new(text)
                    .size(TextSize::Ui)
                    .color(if selected {
                        Color::Strong
                    } else {
                        Color::Muted
                    })
                    .truncate(),
            )
    }

    pub(crate) fn measure_customize_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let activation = self.presentation.activation();
        let app = cx.weak_entity();
        canvas(
            move |bounds, window, cx| {
                let app = app.clone();
                window.defer(cx, move |_, cx| {
                    let _ = app.update(cx, |this, cx| {
                        if !this.presentation.matches(activation) {
                            return;
                        }
                        if this.presentation.current.arrangement.toolbar_height
                            != bounds.size.height
                        {
                            this.presentation.current.arrangement.toolbar_height =
                                bounds.size.height;
                            cx.notify();
                        }
                    });
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
    }

    /// Brings a tabbed `view` to the front of its group, in the layout or the Customize
    /// draft, and gives it focus when the tab it hides held focus.
    fn select_tab(&mut self, view: ViewId, window: &mut Window, cx: &mut Context<Self>) {
        let c = self.shown_composition(cx);
        let Some((views, selected)) = tabs_holding(c.root(), view) else {
            return;
        };
        let views = views.to_vec();
        if views[selected] != view {
            self.edit_surfaces(Edit::SelectTab(view), window, cx);
            let shown = self.shown_composition(cx);
            if tabs_holding(shown.root(), view)
                .is_none_or(|(views, selected)| views[selected] != view)
            {
                return;
            }
        }
        self.focus_replacing(&views, view, window, cx);
    }

    fn arrangement_view(
        &mut self,
        id: ViewId,
        c: &Composition,
        auto: bool,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let component = c.component(id).unwrap();
        let editing = self.presentation.current.customization.is_open();
        let colors = cx.theme().colors;
        let current = self.selected_session(cx).map(|s| (s.id(), s.provider()));
        let mut pane = v_flex()
            .id(("occurrence", id.0 as usize))
            .relative()
            .min_h_0()
            .min_w_0()
            .w_full()
            .when(!auto, |d| d.h_full())
            .when(editing, |d| {
                d.border_1().border_color(
                    if self.presentation.current.customization.selected() == Some(id) {
                        colors.selection_ring
                    } else {
                        colors.border
                    },
                )
            });
        if editing {
            pane = pane.child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .px_1()
                    .h(px(28.))
                    .child(
                        div()
                            .id(("grip", id.0 as usize))
                            .automation_id(format!("component-grip-{}", id.0))
                            .cursor_grab()
                            .px_2()
                            .child(Label::new("⠿").color(Color::Muted))
                            .on_mouse_down(
                                MouseButton::Left,
                                self.scoped_listener(cx, move |this, _, window, cx| {
                                    this.customize(Action::Select(id), window, cx)
                                }),
                            )
                            .on_drag(
                                ComponentDrag(id, self.presentation.activation()),
                                |_, _, _, cx| {
                                    cx.stop_propagation();
                                    cx.new(|_| Empty)
                                },
                            ),
                    )
                    .child(
                        Label::new(format!(
                            "{} {}",
                            crate::customization::label(component),
                            id.0
                        ))
                        .size(TextSize::Sm)
                        .color(Color::Muted),
                    ),
            );
        }
        match component {
            Component::SessionList => {
                pane = pane.child(
                    self.sidebar_column(
                        f32::from(keystone_core::SidebarWidth::DEFAULT.px()),
                        keystone_core::SidebarSide::Left,
                        cx,
                    )
                    .w_full()
                    .flex_1()
                    .min_h_0(),
                );
            }
            Component::Composer => {
                pane = pane
                    .items_center()
                    .justify_end()
                    .p_3()
                    .child(self.composer(current, viewport, cx))
                    .children(self.error_line(cx));
            }
            Component::Files => {
                pane = pane.child(self.file_list(cx));
            }
            Component::Notes => {
                pane = pane.child(self.note_pane(cx));
            }
            Component::Changes => {
                pane = pane.child(self.changes_list(cx));
            }
            Component::Conversation => {
                self.presentation.current.arrangement.visible.push(id);
                if !editing {
                    pane = pane.capture_any_mouse_down(
                        self.scoped_listener(cx, move |this, _, _, cx| this.activate(id, cx)),
                    );
                }
                if let Some(session) = self.view_session(id, cx) {
                    self.state
                        .update(cx, |state, _| state.ensure_history(session));
                    let key = ViewKey { view: id, session };
                    let rows = self
                        .state
                        .read(cx)
                        .conversations()
                        .get(&session)
                        .map_or(0, |c| c.rows.len());
                    self.presentation.current.views.list(key, rows);
                    let focus = self.presentation.current.views.focus(key, cx);
                    pane = pane
                        .track_focus(&focus)
                        .border_1()
                        .border_color(colors.border.opacity(0.))
                        .focus(move |d| d.border_color(colors.border_focused))
                        .tab_index(0)
                        .key_context("Conversation")
                        .on_action(self.scoped_listener(
                            cx,
                            move |this,
                                  _: &crate::transcript_keyboard::LeaveTranscript,
                                  window,
                                  cx| this.leave_transcript(window, cx),
                        ))
                        .on_action(self.scoped_listener(
                            cx,
                            move |this, _: &crate::transcript_keyboard::PreviousRow, window, cx| {
                                this.transcript_row(key, -1, window, cx)
                            },
                        ))
                        .on_action(self.scoped_listener(
                            cx,
                            move |this, _: &crate::transcript_keyboard::NextRow, window, cx| {
                                this.transcript_row(key, 1, window, cx)
                            },
                        ))
                        .on_action(self.scoped_listener(
                            cx,
                            move |this, _: &crate::transcript_keyboard::FirstRow, window, cx| {
                                this.transcript_row(key, -2, window, cx)
                            },
                        ))
                        .on_action(self.scoped_listener(
                            cx,
                            move |this, _: &crate::transcript_keyboard::LastRow, window, cx| {
                                this.transcript_row(key, 2, window, cx)
                            },
                        ))
                        .on_action(self.scoped_listener(
                            cx,
                            move |this, _: &crate::transcript_keyboard::NextAction, window, cx| {
                                this.transcript_action(key, false, window, cx)
                            },
                        ))
                        .on_action(self.scoped_listener(
                            cx,
                            move |this,
                                  _: &crate::transcript_keyboard::PreviousAction,
                                  window,
                                  cx| {
                                this.transcript_action(key, true, window, cx)
                            },
                        ))
                        .on_action(self.scoped_listener(
                            cx,
                            move |this,
                                  _: &crate::transcript_keyboard::CopyTranscript,
                                  window,
                                  cx| {
                                this.copy_transcript(key, window, cx)
                            },
                        ))
                        .on_action(self.scoped_listener(
                            cx,
                            move |this, _: &crate::transcript_keyboard::FocusLatest, window, cx| {
                                this.focus_latest(key, window, cx)
                            },
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            self.scoped_listener(cx, move |this, _, window, cx| {
                                let keyboard = this.presentation.current.views.keyboard(key, cx);
                                keyboard.row = None;
                                keyboard.action = None;
                                keyboard.selection_from_pointer = false;
                                this.presentation
                                    .current
                                    .views
                                    .focus(key, cx)
                                    .focus(window, cx);
                                this.presentation.current.views.activate(key);
                                cx.notify();
                            }),
                        );
                    let revision = self.presentation.current.views.revision(key);
                    let activation = self.presentation.activation();
                    let app = cx.weak_entity();
                    let empty = self
                        .state
                        .read(cx)
                        .conversations()
                        .get(&session)
                        .is_some_and(|c| !c.is_loading() && c.is_empty());
                    pane = pane.children(self.handoff_links(key, cx)).child(
                        div()
                            .id(selector("conversation", id))
                            .automation_id(selector("conversation", id))
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .px_3()
                            .child(self.transcript_top_check(key, cx))
                            .child(self.transcript_list(key, cx))
                            .child(
                                canvas(
                                    |_, _, _| {},
                                    move |bounds, _, window, cx| {
                                        let bounds =
                                            bounds.intersect(&window.content_mask().bounds);
                                        if bounds.size.width > px(0.) && bounds.size.height > px(0.)
                                        {
                                            let app = app.clone();
                                            window.defer(cx, move |window, cx| {
                                                let _ = app.update(cx, |this, cx| {
                                                    if !this.presentation.matches(activation) {
                                                        return;
                                                    }
                                                    let hidden_selection = this.selected(cx) == Some(key.session)
                                                        && this.presentation.current.arrangement.visible.contains(&key.view)
                                                        && this.presentation.current.views.keyboard_state(key).is_some_and(|keyboard| {
                                                            keyboard.action == Some(crate::view_state::CopyTarget::TextSelection)
                                                                && keyboard.action_focus.is_focused(window)
                                                                && keyboard.row.is_some_and(|row| !this.presentation.current.views.row_is_visible(key, row, false))
                                                        });
                                                    if hidden_selection {
                                                        let keyboard = this.presentation.current.views.keyboard(key, cx);
                                                        keyboard.row = None;
                                                        keyboard.action = None;
                                                        keyboard.selection_from_pointer = false;
                                                        this.presentation.current.views.focus(key, cx).focus(window, cx);
                                                        cx.notify();
                                                    }
                                                    this.observe_conversation(
                                                        key, revision, window, cx,
                                                    )
                                                });
                                            });
                                        }
                                    },
                                )
                                .absolute()
                                .inset_0(),
                            )
                            .children(self.jump_to_latest(key, cx))
                            .when(empty, |d| {
                                d.child(
                                    div()
                                        .id(selector("empty-conversation", id))
                                        .absolute()
                                        .inset_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            Label::new(
                                                "No messages yet. Your draft is saved here.",
                                            )
                                            .color(Color::Muted),
                                        ),
                                )
                            }),
                    );
                } else if self.fixture {
                    pane = pane.child(
                        div()
                            .id(selector("conversation", id))
                            .automation_id(selector("conversation", id))
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.transcript_scroll)
                            .child(self.transcript()),
                    );
                } else {
                    pane = pane
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .child(arch_mark(px(40.)))
                        .child(
                            div()
                                .id(selector("welcome-heading", id))
                                .automation_id(selector("welcome-heading", id))
                                .child(
                                    Label::new("What should we work on?")
                                        .size(TextSize::Title)
                                        .weight(FontWeight::SEMIBOLD),
                                ),
                        );
                }
            }
        }
        let tab_from_elsewhere = cx
            .active_drag_value::<DraggedTab>()
            .is_some_and(|drag| drag.window != self.handle.window_id());
        if !editing
            && component == Component::Conversation
            && (Self::has_session_tabs(c) || tab_from_elsewhere)
            && cx.has_active_drag()
        {
            pane = pane.children(
                [
                    ("left", Axis::Row, false),
                    ("right", Axis::Row, true),
                    ("above", Axis::Column, false),
                    ("below", Axis::Column, true),
                ]
                .map(|(side, axis, after)| self.tab_drop_edge(id, side, axis, after, cx)),
            );
        }
        if editing {
            let tab = component.is_surface();
            pane = pane
                .children([
                    self.drop_target(
                        id,
                        "left",
                        Place::Beside {
                            target: id,
                            axis: Axis::Row,
                            after: false,
                        },
                        cx,
                    ),
                    self.drop_target(
                        id,
                        "right",
                        Place::Beside {
                            target: id,
                            axis: Axis::Row,
                            after: true,
                        },
                        cx,
                    ),
                    self.drop_target(
                        id,
                        "above",
                        Place::Beside {
                            target: id,
                            axis: Axis::Column,
                            after: false,
                        },
                        cx,
                    ),
                    self.drop_target(
                        id,
                        "below",
                        Place::Beside {
                            target: id,
                            axis: Axis::Column,
                            after: true,
                        },
                        cx,
                    ),
                ])
                .when(tab, |d| {
                    d.child(self.drop_target(id, "tabs", Place::TabWith(id), cx))
                });
        }
        pane.into_any_element()
    }

    /// A band along one edge of a pane that takes a dragged session tab and splits the
    /// pane there. It is drawn only while something is being dragged.
    fn tab_drop_edge(
        &self,
        target: ViewId,
        side: &'static str,
        axis: Axis,
        after: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let id = format!("pane-drop-{}-{side}", target.0);
        div()
            .id(SharedString::from(id.clone()))
            .automation_id(id)
            .absolute()
            .map(|d| match side {
                "left" => d.left_0().top_0().bottom_0().w(relative(0.25)),
                "right" => d.right_0().top_0().bottom_0().w(relative(0.25)),
                "above" => d
                    .top_0()
                    .left(relative(0.25))
                    .right(relative(0.25))
                    .h(relative(0.3)),
                _ => d
                    .bottom_0()
                    .left(relative(0.25))
                    .right(relative(0.25))
                    .h(relative(0.3)),
            })
            .drag_over::<DraggedTab>(move |d, _, _, _| {
                d.bg(colors.selection_fill.opacity(0.6))
                    .border_2()
                    .border_color(colors.selection_ring)
            })
            .on_drop(
                self.scoped_listener(cx, move |this, drag: &DraggedTab, window, cx| {
                    if let Some(view) = this.dragged_view(drag) {
                        this.split_with_tab(view, target, axis, after, window, cx);
                    } else {
                        let landing = Landing::Beside {
                            target,
                            axis,
                            after,
                        };
                        this.receive_tab(drag, landing, cx);
                    }
                }),
            )
            .into_any_element()
    }

    fn drop_target(
        &self,
        target: ViewId,
        side: &'static str,
        place: Place,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        div()
            .id(SharedString::from(format!("drop-{}-{side}", target.0)))
            .automation_id(format!("drop-{}-{side}", target.0))
            .absolute()
            .map(|d| match side {
                "left" => d.left_0().top(px(30.)).bottom(px(20.)).w(px(16.)),
                "right" => d.right_0().top(px(30.)).bottom(px(20.)).w(px(16.)),
                "above" => d.top_0().left(px(20.)).right(px(20.)).h(px(8.)),
                "below" => d.bottom_0().left(px(20.)).right(px(20.)).h(px(12.)),
                _ => d
                    .top_0()
                    .right_0()
                    .w(px(48.))
                    .h(px(26.))
                    .child(Label::new("Tabs").size(TextSize::Xs).color(Color::Faint)),
            })
            .drag_over::<ComponentDrag>(move |d, drag, _, _| {
                if drag.0 != target {
                    d.bg(colors.selection_fill)
                        .border_1()
                        .border_color(colors.selection_ring)
                } else {
                    d
                }
            })
            .on_drop(
                self.scoped_listener(cx, move |this, dragged: &ComponentDrag, window, cx| {
                    if !this.presentation.matches(dragged.1) {
                        return;
                    }
                    this.customize(
                        Action::Edit(Edit::Move {
                            view: dragged.0,
                            to: place,
                        }),
                        window,
                        cx,
                    )
                }),
            )
            .into_any_element()
    }

    fn split_grip(
        &self,
        split: NodePath,
        axis: Axis,
        index: usize,
        sizes: Vec<Size>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let name = format!("split-grip-{:?}-{index}", split.0);
        let quiet = !self.presentation.current.customization.is_open();
        div()
            .id(SharedString::from(name.clone()))
            .automation_id(name)
            .flex_none()
            .map(|d| {
                if quiet {
                    d.bg(colors.border.opacity(0.5))
                } else {
                    d.bg(colors.border)
                }
            })
            .map(|d| {
                let grip = if quiet { 3. } else { GRIP };
                if axis == Axis::Row {
                    d.w(px(grip)).h_full().cursor_col_resize()
                } else {
                    d.h(px(grip)).w_full().cursor_row_resize()
                }
            })
            .hover(move |d| d.bg(colors.selection_ring))
            .on_mouse_down(
                MouseButton::Left,
                self.scoped_listener(cx, move |this, event: &gpui::MouseDownEvent, _, _| {
                    let extent = |i| {
                        let mut path = split.0.clone();
                        path.push(i);
                        this.presentation
                            .current
                            .arrangement
                            .geometry
                            .get(&NodePath(path))
                            .map_or(1., |b| {
                                f32::from(if axis == Axis::Row {
                                    b.size.width
                                } else {
                                    b.size.height
                                })
                            })
                    };
                    this.presentation.current.arrangement.resize = Some(Resize {
                        start: event.position,
                        split: split.clone(),
                        axis,
                        index,
                        sizes: sizes.clone(),
                        extents: [extent(index), extent(index + 1)],
                    });
                }),
            )
            .on_drag(ResizeGrip, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| Empty)
            })
            .into_any_element()
    }

    pub(crate) fn resize_components(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = &self.presentation.current.arrangement.resize else {
            return;
        };
        let delta = f32::from(if drag.axis == Axis::Row {
            position.x - drag.start.x
        } else {
            position.y - drag.start.y
        });
        let total = drag.extents[0] + drag.extents[1];
        let a = (drag.extents[0] + delta).clamp(40., (total - 40.).max(40.));
        let extents = [a, (total - a).max(1.)];
        let mut sizes = drag.sizes.clone();
        if let (Size::Fill(left), Size::Fill(right)) = (sizes[drag.index], sizes[drag.index + 1]) {
            let max_weight = sizes
                .iter()
                .filter_map(|s| {
                    if let Size::Fill(w) = s {
                        Some(w.get())
                    } else {
                        None
                    }
                })
                .max()
                .unwrap();
            let pair = u32::from(left.get()) + u32::from(right.get());
            let scale = (1000 / pair).max(1).min(u32::from(u16::MAX / max_weight));
            for size in &mut sizes {
                if let Size::Fill(w) = size {
                    *w = NonZeroU16::new((u32::from(w.get()) * scale) as u16).unwrap();
                }
            }
            let pair = pair * scale;
            let left = (extents[0] / total * pair as f32).round().clamp(
                pair.saturating_sub(u32::from(u16::MAX)).max(1) as f32,
                pair.saturating_sub(1).min(u32::from(u16::MAX)) as f32,
            ) as u32;
            sizes[drag.index] = Size::Fill(NonZeroU16::new(left as u16).unwrap());
            sizes[drag.index + 1] = Size::Fill(NonZeroU16::new((pair - left) as u16).unwrap());
        } else {
            for i in 0..2 {
                if matches!(sizes[drag.index + i], Size::Px(_)) {
                    sizes[drag.index + i] = Size::Px(
                        NonZeroU16::new(extents[i].round().clamp(1., 65535.) as u16).unwrap(),
                    );
                }
            }
        }
        let split = drag.split.clone();
        if self.presentation.current.customization.is_open() {
            self.customize(Action::Edit(Edit::Resize { split, sizes }), window, cx);
        } else {
            self.presentation.current.arrangement.pane_resize = Some((split, sizes));
            cx.notify();
        }
    }

    /// Saves pane sizes dragged outside Customize once the divider is released.
    fn finish_pane_resize(&mut self, cx: &mut Context<Self>) {
        self.presentation.current.arrangement.resize = None;
        let Some((split, sizes)) = self.presentation.current.arrangement.pane_resize.take() else {
            return;
        };
        match self
            .saved_composition(cx)
            .edit(Edit::Resize { split, sizes })
        {
            Ok(layout) => {
                let tabs = self.session_tabs(cx);
                self.save_view(Some(layout), tabs, cx);
            }
            Err(problem) => {
                self.error = Some(format!("Layout not changed. {problem}"));
                self.refresh(cx);
            }
        }
    }

    pub(crate) fn on_arrangement_drag<E: gpui::InteractiveElement>(
        &self,
        root: E,
        cx: &mut Context<Self>,
    ) -> E {
        root.on_drag_move::<ResizeGrip>(self.scoped_listener(
            cx,
            |this, event: &gpui::DragMoveEvent<ResizeGrip>, window, cx| {
                this.resize_components(event.event.position, window, cx)
            },
        ))
        .on_drop::<ResizeGrip>(self.scoped_listener(cx, |this, _: &ResizeGrip, _, cx| {
            this.finish_pane_resize(cx)
        }))
    }

    fn observe_conversation(
        &mut self,
        key: ViewKey,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.presentation.current.views.revision(key) != revision
            || !window.is_window_active()
            || self.filter_menu
            || self.new_menu
            || self.settings.is_some()
            || self.approval_menu
            || self.renaming.is_some()
            || self.worktree_dialog.is_some()
            || self.confirm_full_access.is_some()
            || self.presentation.current.customization.chooser().is_some()
            || self.presentation.current.arrangement.surface_menu.is_some()
            || (self.presentation.current.arrangement.sidebar_overlay
                && self.presentation.current.arrangement.fitted
                && self.arrangement_has_sidebar(cx))
            || self.selected(cx) != Some(key.session)
            || !self
                .presentation
                .current
                .arrangement
                .visible
                .contains(&key.view)
            || !self
                .presentation
                .current
                .views
                .focus(key, cx)
                .contains_focused(window, cx)
        {
            return;
        }
        self.presentation.current.views.note_focus(key);
        if self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .filter(|k| !k.selection_from_pointer)
            .and_then(|k| k.row)
            .is_some_and(|row| {
                self.state
                    .read(cx)
                    .conversations()
                    .get(&key.session)
                    .is_some_and(|c| {
                        c.rows
                            .iter()
                            .enumerate()
                            .rev()
                            .find(|(_, r)| crate::conversation::visible(r))
                            .is_some_and(|(last, _)| row < last)
                    })
            })
        {
            return;
        }
        if self.presentation.current.views.scrolled_up(key)
            || self.replies.borrow().pacing_in(key.session)
        {
            return;
        }
        let Some(conversation) = self.state.read(cx).conversations().get(&key.session) else {
            return;
        };
        if conversation.running || conversation.is_loading() {
            return;
        }
        let Some(session) = self.selected_session(cx) else {
            return;
        };
        let Some(updated) = session
            .updated_at()
            .filter(|updated| session.viewed_at().is_none_or(|v| *updated > v))
        else {
            return;
        };
        if !self
            .presentation
            .current
            .views
            .attempt_observation(key, updated)
        {
            return;
        }
        match self
            .state
            .update(cx, |state, cx| state.mark_viewed(key.session, now_ms(), cx))
        {
            Ok(applied) => self.error = crate::durability_warning(applied.durability),
            Err(error) => self.error = Some(format!("Could not save read status. {error}")),
        }
        self.refresh(cx);
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "arrangement_tests.rs"]
mod tests;
