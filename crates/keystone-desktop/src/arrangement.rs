use crate::arrangement_editor::Action;
use crate::{Keystone, ViewKey, arch_mark, now_ms};
use gpui::{
    AnyElement, AvailableSpace, Bounds, Context, Empty, FontWeight, IntoElement, MouseButton,
    Pixels, Point, SharedString, Window, canvas, div, prelude::*, px, size,
};
use keystone_core::{Axis, Component, Composition, Edit, Node, NodePath, Place, Size, ViewId};
use keystone_ui::prelude::*;
use std::collections::HashMap;
use std::num::NonZeroU16;

const GRIP: f32 = 6.;
const CONVERSATION_MIN: f32 = 320.;
const COMPOSER_MIN: f32 = 420.;
const SESSION_MIN: f32 = 220.;

#[derive(Default)]
pub struct Arrangement {
    pub visible: Vec<ViewId>,
    pub fitted: bool,
    tab: Option<FittedTab>,
    tabs: HashMap<ViewId, ViewId>,
    pub sidebar_overlay: bool,
    geometry: HashMap<NodePath, Bounds<Pixels>>,
    toolbar_height: Pixels,
    composer_min_width: f32,
    resize: Option<Resize>,
}
struct FittedTab {
    source: Composition,
    view: ViewId,
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
struct ComponentDrag(ViewId);
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

fn first_displayed_conversation(node: &Node, c: &Composition) -> Option<ViewId> {
    match node {
        Node::View { id } => (c.component(*id) == Some(Component::Conversation)).then_some(*id),
        Node::Tabs { selected, views } => Some(views[*selected]),
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
        },
        Node::Tabs { .. } => [CONVERSATION_MIN, 154. + header],
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
    pub(crate) fn restore_tabs(&mut self) {
        self.tabs.clear();
        self.tab = None;
    }
}

impl Keystone {
    pub(crate) fn arrangement_has_sidebar(&self) -> bool {
        let saved = self.saved_composition();
        self.customization
            .shown(&saved)
            .views()
            .any(|(_, component)| component == Component::SessionList)
    }

    fn composer_minimum_width(&self, window: &mut Window, cx: &mut Context<Self>) -> f32 {
        let Some(session) = self.selected_session() else {
            return COMPOSER_MIN;
        };
        let project = self.project_dir.file_name().map_or_else(
            || self.project_dir.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let provider = h_flex()
            .gap(px(7.))
            .flex_none()
            .child(Icon::new(IconName::Diamond).size(px(12.)))
            .child(
                Label::new(crate::provider_name(session.provider()))
                    .size(TextSize::Ui)
                    .weight(FontWeight::MEDIUM),
            )
            .child(div().w_0().flex_none());
        let mut controls = h_flex()
            .gap(px(14.))
            .font_family(cx.theme().fonts.ui.clone())
            .text_size(TextSize::Base.px())
            .child(provider)
            .child(div().w_px().flex_none())
            .child(
                Button::new("measure-project", project)
                    .size(ButtonSize::Compact)
                    .icon(IconName::Folder),
            )
            .child(
                Button::new("measure-approval", session.approval_mode().label())
                    .size(ButtonSize::Compact)
                    .icon(IconName::Shield),
            )
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

    pub(crate) fn arrangement_body(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let saved = self.saved_composition();
        let c = self.customization.shown(&saved).clone();
        if self
            .arrangement
            .tab
            .as_ref()
            .is_some_and(|tab| tab.source != c)
        {
            self.arrangement.tab = None;
        }
        self.arrangement.composer_min_width = self.composer_minimum_width(window, cx);
        let required = minimum(
            c.root(),
            &c,
            self.customization.is_open(),
            self.sidebar,
            self.arrangement.composer_min_width,
        );
        let viewport = window.viewport_size();
        let toolbar = if self.customization.is_open() {
            f32::from(self.arrangement.toolbar_height)
        } else {
            0.
        };
        let fitted = f32::from(viewport.width) < required[0]
            || f32::from(viewport.height) - 44. - toolbar < required[1];
        if fitted != self.arrangement.fitted
            || !c
                .views()
                .any(|(_, component)| component == Component::SessionList)
        {
            self.arrangement.sidebar_overlay = false;
        }
        self.arrangement.fitted = fitted;
        self.arrangement.visible.clear();
        if fitted {
            let conversations: Vec<_> = ordered_views(c.root())
                .into_iter()
                .filter(|id| c.component(*id) == Some(Component::Conversation))
                .collect();
            let selected = self
                .arrangement
                .tab
                .as_ref()
                .map(|tab| tab.view)
                .unwrap_or_else(|| first_displayed_conversation(c.root(), &c).unwrap());
            let composer = c
                .views()
                .find(|(_, k)| *k == Component::Composer)
                .unwrap()
                .0;
            let mut body = v_flex().relative().flex_1().min_h_0().w_full()
                .when(f32::from(viewport.height)-44.-toolbar > 230., |d| d.child(div().flex_none().child(Label::new("Views use tabs and the composer stays below until this window has more room.").size(TextSize::Sm).color(Color::Muted))))
                .child(h_flex().flex_none().gap_1().children(conversations.iter().enumerate().map(|(i, &id)| {
                    let source = c.clone();
                    self.conversation_tab(format!("fitted-tab-{}", id.0), format!("View {}", i+1), selected == id, cx)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.arrangement.tab = Some(FittedTab {
                                source: source.clone(),
                                view: id,
                            });
                            cx.notify();
                        }))
                })))
                .child(div().flex_1().min_h_0().overflow_hidden().child(self.arrangement_view(selected, &c, false, cx)))
                .child(div().flex_none().w_full().child(self.arrangement_view(composer, &c, true, cx)));
            if self.arrangement.sidebar_overlay {
                if let Some((id, _)) = c.views().find(|(_, k)| *k == Component::SessionList) {
                    body = body.child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .w(px(
                                SESSION_MIN.max((f32::from(viewport.width) * 0.8).min(302.))
                            ))
                            .occlude()
                            .child(self.arrangement_view(id, &c, false, cx)),
                    );
                }
            }
            body.into_any_element()
        } else {
            self.arrangement_node(c.root(), &c, NodePath::default(), false, cx)
        }
    }

    fn arrangement_node(
        &mut self,
        node: &Node,
        c: &Composition,
        path: NodePath,
        auto: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            Node::View { id } => self.arrangement_view(*id, c, auto, cx),
            Node::Tabs { selected, views } => {
                let views_first = views[0];
                let selected = if self.customization.is_open() {
                    *selected
                } else {
                    self.arrangement
                        .tabs
                        .get(&views_first)
                        .and_then(|id| views.iter().position(|v| v == id))
                        .unwrap_or(*selected)
                };
                v_flex()
                    .size_full()
                    .min_h_0()
                    .min_w_0()
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .children(views.iter().enumerate().map(|(i, &id)| {
                                self.conversation_tab(
                                    format!("conversation-tab-{}", id.0),
                                    format!("View {}", id.0),
                                    i == selected,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.select_conversation_tab(views_first, id, window, cx)
                                    },
                                ))
                            })),
                    )
                    .child(div().flex_1().min_h_0().child(self.arrangement_view(
                        views[selected],
                        c,
                        false,
                        cx,
                    )))
                    .into_any_element()
            }
            Node::Split { axis, children } => {
                if *axis == Axis::Column
                    && children.len() == 2
                    && self.selected_session().is_none()
                    && !self.fixture
                    && !self.customization.is_open()
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
                                .child(self.arrangement_view(first, c, false, cx)),
                        )
                        .child(self.arrangement_view(second, c, true, cx))
                        .into_any_element();
                }
                let axis = *axis;
                let geometry_path = path.clone();
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
                                let _ = app.update(cx, |this, _| {
                                    this.arrangement
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
                        self.customization.is_open(),
                        self.sidebar,
                        self.arrangement.composer_min_width,
                    );
                    let hidden_sidebar = !self.sidebar
                        && matches!(&child.node, Node::View { id } if c.component(*id) == Some(Component::SessionList));
                    if hidden_sidebar {
                        continue;
                    }
                    if i > 0 && self.customization.is_open() {
                        split = split.child(self.split_grip(
                            path.clone(),
                            axis,
                            i - 1,
                            children.iter().map(|c| c.size).collect(),
                            cx,
                        ));
                    }
                    let app = cx.weak_entity();
                    let measure = child_path.clone();
                    let content = self.arrangement_node(
                        &child.node,
                        c,
                        child_path,
                        child.size == Size::Auto,
                        cx,
                    );
                    split = split.child(
                        div()
                            .relative()
                            .min_h_0()
                            .min_w_0()
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
                            .when(axis == Axis::Row, |d| d.h_full())
                            .when(axis == Axis::Column, |d| d.w_full())
                            .child(
                                canvas(
                                    move |bounds, _, cx| {
                                        let _ = app.update(cx, |this, _| {
                                            this.arrangement
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
            .child(Label::new(text).size(TextSize::Ui).color(if selected {
                Color::Strong
            } else {
                Color::Muted
            }))
    }

    pub(crate) fn measure_customize_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let app = cx.weak_entity();
        canvas(
            move |bounds, window, cx| {
                let app = app.clone();
                window.defer(cx, move |_, cx| {
                    let _ = app.update(cx, |this, cx| {
                        if this.arrangement.toolbar_height != bounds.size.height {
                            this.arrangement.toolbar_height = bounds.size.height;
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

    fn select_conversation_tab(
        &mut self,
        group: ViewId,
        id: ViewId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.customization.is_open() {
            self.arrangement.tabs.remove(&group);
            self.customize(Action::Edit(Edit::SelectTab(id)), window, cx);
        } else {
            self.arrangement.tabs.insert(group, id);
            cx.notify();
        }
    }

    fn arrangement_view(
        &mut self,
        id: ViewId,
        c: &Composition,
        auto: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let component = c.component(id).unwrap();
        let editing = self.customization.is_open();
        let colors = cx.theme().colors;
        let current = self.selected_session().map(|s| (s.id(), s.provider()));
        let mut pane = v_flex()
            .id(("occurrence", id.0 as usize))
            .relative()
            .min_h_0()
            .min_w_0()
            .w_full()
            .when(!auto, |d| d.h_full())
            .when(editing, |d| {
                d.border_1()
                    .border_color(if self.customization.selected() == Some(id) {
                        colors.selection_ring
                    } else {
                        colors.border
                    })
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
                                cx.listener(move |this, _, window, cx| {
                                    this.customize(Action::Select(id), window, cx)
                                }),
                            )
                            .on_drag(ComponentDrag(id), |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.new(|_| Empty)
                            }),
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
        if !editing
            && component == Component::Conversation
            && c.views()
                .filter(|(_, kind)| *kind == Component::Conversation)
                .count()
                > 1
        {
            let earlier = current
                .is_some_and(|(session, _)| self.views.scrolled_up(ViewKey { view: id, session }));
            pane = pane.child(
                h_flex().h(px(26.)).flex_none().px_2().child(
                    Label::new(format!(
                        "Conversation {} · {}",
                        id.0,
                        if earlier {
                            "Reading earlier"
                        } else {
                            "Following latest"
                        }
                    ))
                    .size(TextSize::Sm)
                    .color(Color::Muted),
                ),
            );
        }
        match component {
            Component::SessionList => {
                pane = pane.child(
                    self.sidebar_column(302., keystone_core::SidebarSide::Left, cx)
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
                    .child(self.composer(current, cx))
                    .children(self.error_line());
            }
            Component::Conversation => {
                self.arrangement.visible.push(id);
                if let Some((session, _)) = current {
                    let key = ViewKey { view: id, session };
                    let rows = self.conversations.get(&session).map_or(0, |c| c.rows.len());
                    self.views.list(key, rows);
                    let focus = self.views.focus(key, cx);
                    pane = pane
                        .track_focus(&focus)
                        .border_1()
                        .border_color(colors.border.opacity(0.))
                        .focus(move |d| d.border_color(colors.border_focused))
                        .tab_index(0)
                        .key_context("Conversation")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.views.focus(key, cx).focus(window, cx);
                                this.views.activate(key);
                                cx.notify();
                            }),
                        );
                    let revision = self.views.revision(key);
                    let app = cx.weak_entity();
                    let empty = self
                        .conversations
                        .get(&session)
                        .is_some_and(|c| !c.is_loading() && c.is_empty());
                    pane = pane.child(
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
        if editing {
            let tab = component == Component::Conversation;
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
                cx.listener(move |this, dragged: &ComponentDrag, window, cx| {
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
        div()
            .id(SharedString::from(name.clone()))
            .automation_id(name)
            .flex_none()
            .bg(colors.border)
            .map(|d| {
                if axis == Axis::Row {
                    d.w(px(GRIP)).h_full().cursor_col_resize()
                } else {
                    d.h(px(GRIP)).w_full().cursor_row_resize()
                }
            })
            .hover(move |d| d.bg(colors.selection_ring))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, _| {
                    let extent = |i| {
                        let mut path = split.0.clone();
                        path.push(i);
                        this.arrangement
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
                    this.arrangement.resize = Some(Resize {
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
        let Some(drag) = &self.arrangement.resize else {
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
        self.customize(Action::Edit(Edit::Resize { split, sizes }), window, cx);
    }

    pub(crate) fn on_arrangement_drag<E: gpui::InteractiveElement>(
        &self,
        root: E,
        cx: &mut Context<Self>,
    ) -> E {
        root.on_drag_move::<ResizeGrip>(cx.listener(
            |this, event: &gpui::DragMoveEvent<ResizeGrip>, window, cx| {
                this.resize_components(event.event.position, window, cx)
            },
        ))
    }

    fn observe_conversation(
        &mut self,
        key: ViewKey,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.views.revision(key) != revision
            || !window.is_window_active()
            || self.new_menu
            || self.settings_menu
            || self.approval_menu
            || self.renaming.is_some()
            || self.confirm_full_access.is_some()
            || self.customization.chooser().is_some()
            || (self.arrangement.sidebar_overlay
                && self.arrangement.fitted
                && self.arrangement_has_sidebar())
            || self.store.workspace().selected() != Some(key.session)
            || !self.arrangement.visible.contains(&key.view)
            || !self.views.focus(key, cx).contains_focused(window, cx)
        {
            return;
        }
        self.views.note_focus(key);
        if self.views.scrolled_up(key) || self.replies.borrow().pacing() {
            return;
        }
        let Some(conversation) = self.conversations.get(&key.session) else {
            return;
        };
        if conversation.running || conversation.is_loading() {
            return;
        }
        let Some(session) = self.selected_session() else {
            return;
        };
        let Some(updated) = session
            .updated_at()
            .filter(|updated| session.viewed_at().is_none_or(|v| *updated > v))
        else {
            return;
        };
        if !self.views.attempt_observation(key, updated) {
            return;
        }
        match self.store.mark_viewed(key.session, now_ms()) {
            Ok(applied) => self.error = crate::durability_warning(applied.durability),
            Err(error) => self.error = Some(format!("Could not save read status. {error}")),
        }
        self.refresh(cx);
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "arrangement_tests.rs"]
mod tests;
