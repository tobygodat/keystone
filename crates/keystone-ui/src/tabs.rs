// The bar's structure, a tracked horizontal scroll strip with a trailing empty
// slot, follows gpui-component 0.7.3 `tab/tab_bar.rs` and `tab/tab.rs`
// (Copyright 2024 - 2026 Longbridge, Apache-2.0). Modified: rewritten for Keystone's
// theme, with close, middle-click close, status dots, and drag reorder added.

use crate::button::IconButton;
use crate::icon::IconName;
use crate::label::Label;
use crate::theme::ActiveTheme;
use crate::tokens::{Elevation, TextSize, radius};
use crate::{Color, StyledExt, h_flex};
use gpui::{
    App, Context, ElementId, ExternalDragPayload, InteractiveElement, IntoElement, Pixels, Point,
    Render, RenderOnce, ScrollHandle, SharedString, Styled, Window, WindowId, div, prelude::*, px,
};
use std::rc::Rc;

/// The payload of a tab drag. Any drop target can accept it with
/// `on_drop::<DraggedTab>`; `window` and `bar_id` tell it which bar the tab left. A drag
/// leaving its window goes on as an app-private platform drag, so another window of the app
/// can take it and no other application can.
#[derive(Clone, Debug)]
pub struct DraggedTab {
    pub window: WindowId,
    pub bar_id: ElementId,
    pub index: usize,
}

/// A status mark in the close button's slot, hidden while the pointer is over the tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabDot {
    /// Unsaved changes, in the muted text color.
    Dirty,
    /// Work in progress, in the accent color.
    Working,
}

/// One tab of a [`TabBar`]. Its id keeps hover state with the tab when the bar reorders.
pub struct Tab {
    id: ElementId,
    label: SharedString,
    dot: Option<TabDot>,
}

impl Tab {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            dot: None,
        }
    }

    pub fn dot(mut self, dot: impl Into<Option<TabDot>>) -> Self {
        self.dot = dot.into();
        self
    }
}

type IndexHandler = Rc<dyn Fn(&usize, &mut Window, &mut App)>;
type MoveHandler = Rc<dyn Fn(&(usize, usize), &mut Window, &mut App)>;
type MenuHandler = Rc<dyn Fn(&(usize, Point<Pixels>), &mut Window, &mut App)>;
type InsertHandler = Rc<dyn Fn(&(DraggedTab, usize), &mut Window, &mut App)>;

/// A row of tabs that scrolls sideways when it overflows. The caller owns the
/// order and the selection, and changes them in the handlers.
///
/// Debug selectors are `{id}-tab-{n}` and `{id}-tab-{n}-close`, counting from 0.
#[derive(IntoElement)]
pub struct TabBar {
    id: ElementId,
    tabs: Vec<Tab>,
    selected: Option<usize>,
    on_select: Option<IndexHandler>,
    on_close: Option<IndexHandler>,
    on_menu: Option<MenuHandler>,
    drops: Drops,
}

const TAB_PAD_L: Pixels = px(12.);
const TAB_PAD_R: Pixels = px(4.);
const MARKER: f32 = 2.;

impl TabBar {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            tabs: Vec::new(),
            selected: None,
            on_select: None,
            on_close: None,
            on_menu: None,
            drops: Drops::default(),
        }
    }

    pub fn child(mut self, tab: Tab) -> Self {
        self.tabs.push(tab);
        self
    }

    pub fn children(mut self, tabs: impl IntoIterator<Item = Tab>) -> Self {
        self.tabs.extend(tabs);
        self
    }

    /// The selected tab, which the bar scrolls into view whenever it changes.
    pub fn selected(mut self, index: impl Into<Option<usize>>) -> Self {
        self.selected = index.into();
        self
    }

    /// A press on a tab.
    pub fn on_select(mut self, handler: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }

    /// The close button or a middle click. Tabs show a close button only when this is set.
    pub fn on_close(mut self, handler: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(handler));
        self
    }

    /// A right click on a tab, with where it was pressed, to open the tab's menu.
    pub fn on_menu(
        mut self,
        handler: impl Fn(&(usize, Point<Pixels>), &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_menu = Some(Rc::new(handler));
        self
    }

    /// A tab of this bar dropped on another slot, as `(from, to)`. Removing the
    /// tab at `from` and inserting it at `to` gives the new order. A drop on the
    /// empty end of the bar moves the tab last. Tabs are draggable only when this is set.
    pub fn on_move(
        mut self,
        handler: impl Fn(&(usize, usize), &mut Window, &mut App) + 'static,
    ) -> Self {
        self.drops.on_move = Some(Rc::new(handler));
        self
    }

    /// A tab of another bar dropped on this one, with the index to insert it at.
    /// A drop on a tab inserts before it, and a drop on the empty end appends.
    ///
    /// GPUI ends a drag at the first drop target under the pointer, so a
    /// `DraggedTab` released over a bar with drops enabled never reaches an
    /// enclosing target. A bar without this handler ignores such a drop.
    pub fn on_insert(
        mut self,
        handler: impl Fn(&(DraggedTab, usize), &mut Window, &mut App) + 'static,
    ) -> Self {
        self.drops.on_insert = Some(Rc::new(handler));
        self
    }
}

/// Where a dragged tab lands when it is dropped on a slot.
#[derive(Clone, Copy)]
enum Landing {
    /// A tab of this bar, as `(from, to)`.
    Move(usize, usize),
    /// A tab of another bar, inserted at this index.
    Insert(usize),
}

impl Landing {
    /// Whether the insertion marker goes on the slot's right edge.
    fn after(self) -> bool {
        matches!(self, Landing::Move(from, to) if from < to)
    }
}

#[derive(Clone, Default)]
struct Drops {
    on_move: Option<MoveHandler>,
    on_insert: Option<InsertHandler>,
}

impl Drops {
    fn enabled(&self) -> bool {
        self.on_move.is_some() || self.on_insert.is_some()
    }

    /// The landing of `drag` on the tab at `slot`, or on the empty end when
    /// `slot` is the tab count, if a handler takes it and it changes the order.
    fn landing(
        &self,
        (window, bar): (WindowId, &ElementId),
        drag: &DraggedTab,
        slot: usize,
        count: usize,
    ) -> Option<Landing> {
        if drag.window == window && drag.bar_id == *bar {
            let to = slot.min(count.saturating_sub(1));
            (self.on_move.is_some() && drag.index != to).then_some(Landing::Move(drag.index, to))
        } else {
            self.on_insert.is_some().then_some(Landing::Insert(slot))
        }
    }

    fn drop(&self, landing: Landing, drag: &DraggedTab, window: &mut Window, cx: &mut App) {
        match (landing, &self.on_move, &self.on_insert) {
            (Landing::Move(from, to), Some(on_move), _) => on_move(&(from, to), window, cx),
            (Landing::Insert(to), _, Some(on_insert)) => on_insert(&(drag.clone(), to), window, cx),
            _ => {}
        }
    }

    /// Makes `el`, the slot at `slot`, a drop target that marks where the tab will land.
    /// The marker takes its width from the padding, so the slot never shifts.
    fn target<E: InteractiveElement + 'static>(
        &self,
        el: E,
        window_id: WindowId,
        bar: &ElementId,
        slot: usize,
        count: usize,
        pad: (Pixels, Pixels),
    ) -> E {
        if !self.enabled() {
            return el;
        }
        let (over, drops, bar_over, bar_drop) =
            (self.clone(), self.clone(), bar.clone(), bar.clone());
        el.drag_over::<DraggedTab>(move |s, drag, _, cx| {
            let Some(landing) = over.landing((window_id, &bar_over), drag, slot, count) else {
                return s;
            };
            let accent = cx.theme().colors.accent;
            let s = s.border_color(accent);
            if landing.after() {
                s.border_r(px(MARKER)).pr(pad.1 - px(MARKER))
            } else {
                s.border_l(px(MARKER)).pl(pad.0 - px(MARKER))
            }
        })
        .on_drop::<DraggedTab>(move |drag, window, cx| {
            if let Some(landing) = drops.landing((window_id, &bar_drop), drag, slot, count) {
                drops.drop(landing, drag, window, cx)
            }
        })
    }
}

impl RenderOnce for TabBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id;
        let scroll = window
            .use_keyed_state((id.clone(), "scroll"), cx, |_, _| ScrollHandle::new())
            .read(cx)
            .clone();
        let revealed = window.use_keyed_state((id.clone(), "revealed"), cx, |_, _| None);
        if *revealed.read(cx) != self.selected {
            match self.selected {
                // A reveal asked for before the strip's first prepaint is dropped,
                // so the first frame lays out and the next one reveals.
                Some(index)
                    if index < self.tabs.len() && scroll.bounds_for_item(index).is_none() =>
                {
                    window.request_animation_frame()
                }
                selected => {
                    if let Some(index) = selected {
                        scroll.scroll_to_item(index);
                    }
                    revealed.update(cx, |revealed, _| *revealed = selected);
                }
            }
        }

        let theme = cx.theme();
        let colors = theme.colors;
        let height = theme.density.control(px(34.));
        let tab_height = theme.density.control(px(26.));
        let group = SharedString::from(format!("{id}-tab"));
        let count = self.tabs.len();
        let window_id = window.window_handle().window_id();

        let tabs = self.tabs.into_iter().enumerate().map(|(ix, tab)| {
            let selected = self.selected == Some(ix);
            let selector = format!("{id}-tab-{ix}");
            let slot = h_flex()
                .flex_shrink_0()
                .size(px(18.))
                .justify_center()
                .relative()
                .when_some(tab.dot, |d, dot| {
                    let color = match dot {
                        TabDot::Dirty => colors.text_muted,
                        TabDot::Working => colors.accent,
                    };
                    d.child(
                        div()
                            .size(px(7.))
                            .rounded_full()
                            .bg(color)
                            .when(self.on_close.is_some(), |d| {
                                d.group_hover(group.clone(), |s| s.invisible())
                            }),
                    )
                })
                .when_some(self.on_close.clone(), |d, close| {
                    d.child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .justify_center()
                            .items_center()
                            .when(!selected || tab.dot.is_some(), |d| {
                                d.invisible().group_hover(group.clone(), |s| s.visible())
                            })
                            .child(
                                IconButton::new((tab.id.clone(), "close"), IconName::Close)
                                    .size(crate::ButtonSize::Compact)
                                    .debug_selector(|| format!("{selector}-close"))
                                    .on_click(move |_, window, cx| close(&ix, window, cx)),
                            )
                            // A press here must not start a drag of the tab.
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation()
                            }),
                    )
                });
            let label = Label::new(tab.label.clone())
                .size(TextSize::Ui)
                .color(if selected {
                    Color::Strong
                } else {
                    Color::Muted
                })
                .truncate();
            let el = h_flex()
                .id(tab.id)
                .group(group.clone())
                .flex_shrink_0()
                .h(tab_height)
                .min_w(px(72.))
                .max_w(px(200.))
                .pl(TAB_PAD_L)
                .pr(TAB_PAD_R)
                .gap(px(4.))
                .rounded(radius::CONTROL)
                .cursor_pointer()
                .when(selected, |d| d.bg(colors.element_selected))
                .when(!selected, |d| d.hover(|s| s.bg(colors.element_hover)))
                .child(div().flex_1().min_w_0().child(label))
                .child(slot)
                .when_some(self.on_select.clone(), |d, select| {
                    d.on_click(move |_, window, cx| select(&ix, window, cx))
                })
                .when(self.on_close.is_some() || self.on_menu.is_some(), |d| {
                    let (close, menu) = (self.on_close.clone(), self.on_menu.clone());
                    d.on_aux_click(move |event, window, cx| {
                        if event.is_middle_click()
                            && let Some(close) = &close
                        {
                            close(&ix, window, cx)
                        } else if event.is_right_click()
                            && let Some(menu) = &menu
                        {
                            menu(&(ix, event.position()), window, cx)
                        }
                    })
                })
                .when(self.drops.on_move.is_some(), |d| {
                    let label = tab.label.clone();
                    d.on_drag(
                        DraggedTab {
                            window: window_id,
                            bar_id: id.clone(),
                            index: ix,
                        },
                        move |_, _, _, cx| {
                            let label = label.clone();
                            cx.new(|_| TabDragPreview { label })
                        },
                    )
                    .external_drag_payload(|_: &DraggedTab, _, _| Some(ExternalDragPayload::App))
                });
            let el = self
                .drops
                .target(el, window_id, &id, ix, count, (TAB_PAD_L, TAB_PAD_R));
            crate::automation::tag(el, selector)
        });
        let tabs: Vec<_> = tabs.collect();

        let end = div()
            .id((id.clone(), "end"))
            .flex_1()
            .min_w(px(24.))
            .h(tab_height)
            .pl(px(MARKER));
        let end = self
            .drops
            .target(end, window_id, &id, count, count, (px(MARKER), px(0.)));

        let bar = h_flex()
            .id(id.clone())
            .w_full()
            .h(height)
            .px(px(6.))
            .bg(colors.sidebar)
            .hairline_b(cx)
            .child(
                h_flex()
                    .id((id.clone(), "strip"))
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .gap(px(2.))
                    .overflow_x_scroll()
                    .track_scroll(&scroll)
                    .children(tabs)
                    .child(end),
            );
        crate::automation::tag(bar, id.to_string())
    }
}

struct TabDragPreview {
    label: SharedString,
}

impl Render for TabDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height = cx.theme().density.control(px(26.));
        h_flex()
            .h(height)
            .px(TAB_PAD_L)
            .surface(Elevation::Overlay, cx)
            .font_ui(cx)
            .child(Label::new(self.label.clone()).size(TextSize::Ui))
    }
}
