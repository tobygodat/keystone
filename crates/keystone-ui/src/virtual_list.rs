use gpui::{
    Action, AnyElement, App, Entity, EntityId, FocusHandle, Focusable, IntoElement, KeyBinding,
    ParentElement, RenderOnce, ScrollStrategy, SharedString, StyleRefinement, Styled,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use std::rc::Rc;

gpui::actions!(
    virtual_list,
    [
        SelectPrevious,
        SelectNext,
        SelectPageUp,
        SelectPageDown,
        SelectFirst,
        SelectLast,
        Confirm
    ]
);

const CONTEXT: &str = "VirtualList";
const PAGE_BEFORE_LAYOUT: usize = 10;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, Some(CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(CONTEXT)),
        KeyBinding::new("pageup", SelectPageUp, Some(CONTEXT)),
        KeyBinding::new("pagedown", SelectPageDown, Some(CONTEXT)),
        KeyBinding::new("home", SelectFirst, Some(CONTEXT)),
        KeyBinding::new("end", SelectLast, Some(CONTEXT)),
        KeyBinding::new("enter", Confirm, Some(CONTEXT)),
    ]);
}

/// The rows, highlight, and scroll position behind a [`VirtualList`]. The highlight is
/// `None` exactly when the list is empty, and otherwise always names a row in range.
pub struct VirtualListState {
    count: usize,
    highlighted: Option<usize>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
}

impl VirtualListState {
    pub fn new(count: usize, cx: &mut App) -> Self {
        Self {
            count,
            highlighted: (count > 0).then_some(0),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    pub fn scroll_handle(&self) -> &UniformListScrollHandle {
        &self.scroll
    }

    /// Highlights `index`, clamped to the last row, and scrolls it into view.
    pub fn move_to(&mut self, index: usize) {
        if self.count == 0 {
            return;
        }
        let index = index.min(self.count - 1);
        self.highlighted = Some(index);
        self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
    }

    pub fn move_by(&mut self, delta: isize) {
        if let Some(current) = self.highlighted {
            self.move_to(current.saturating_add_signed(delta));
        }
    }

    /// Moves by `delta` viewports' worth of rows.
    pub fn page(&mut self, delta: isize) {
        self.move_by(delta.saturating_mul(self.page_size() as isize));
    }

    pub fn set_count(&mut self, count: usize) {
        self.count = count;
        match self.highlighted {
            _ if count == 0 => self.highlighted = None,
            // The list dropped any scroll request made while it was empty.
            current => self.move_to(current.unwrap_or(0)),
        }
    }

    fn page_size(&self) -> usize {
        match self.scroll.0.borrow().last_item_size {
            Some(size) if self.count > 0 && size.contents.height > px(0.) => {
                let row = size.contents.height / self.count as f32;
                ((size.item.height / row).floor() as usize).max(1)
            }
            _ => PAGE_BEFORE_LAYOUT,
        }
    }

    /// Pointer hover highlights without scrolling. Scrolling a partly visible row into
    /// view would slide the next row under a still pointer and chain hover to the end.
    fn hover(&mut self, index: usize) -> bool {
        if index >= self.count || self.highlighted == Some(index) {
            return false;
        }
        self.highlighted = Some(index);
        true
    }
}

impl Focusable for VirtualListState {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

type RenderRow = Rc<dyn Fn(usize, bool, &mut Window, &mut App) -> AnyElement + 'static>;
type ConfirmHandler = Rc<dyn Fn(&usize, &mut Window, &mut App) + 'static>;

/// A keyboard-driven list that renders only its visible rows. Every row must have the
/// same fixed height, since the list measures row 0 and lays out the rest from it.
/// Rows are named `{id}-row-{index}` for tests and native verification.
#[derive(IntoElement)]
pub struct VirtualList {
    id: SharedString,
    state: Entity<VirtualListState>,
    render_row: RenderRow,
    on_confirm: Option<ConfirmHandler>,
    focusable: bool,
    style: StyleRefinement,
}

impl VirtualList {
    pub fn new(
        id: impl Into<SharedString>,
        state: &Entity<VirtualListState>,
        render_row: impl Fn(usize, bool, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            state: state.clone(),
            render_row: Rc::new(render_row),
            on_confirm: None,
            focusable: true,
            style: StyleRefinement::default(),
        }
    }

    /// Called with the row chosen by Enter or a click.
    pub fn on_confirm(mut self, handler: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_confirm = Some(Rc::new(handler));
        self
    }

    /// Leaves focus with the host, such as a palette whose input forwards its keys to
    /// the state. A focusable list takes focus when clicked.
    pub fn focusable(mut self, focusable: bool) -> Self {
        self.focusable = focusable;
        self
    }
}

impl Styled for VirtualList {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

/// Notifies the view that drew the list as well as the state. A cached host view
/// only redraws for its own notifications, and the state is not a view.
pub(crate) fn change(
    state: &Entity<VirtualListState>,
    host: EntityId,
    cx: &mut App,
    change: impl FnOnce(&mut VirtualListState) -> bool,
) {
    if state.update(cx, |state, _| change(state)) {
        cx.notify(state.entity_id());
        cx.notify(host);
    }
}

fn update<A: Action>(
    state: &Entity<VirtualListState>,
    host: EntityId,
    edit: impl Fn(&mut VirtualListState) + 'static,
) -> impl Fn(&A, &mut Window, &mut App) + 'static {
    let state = state.clone();
    move |_, _, cx| {
        change(&state, host, cx, |state| {
            edit(state);
            true
        })
    }
}

impl RenderOnce for VirtualList {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let host = window.current_view();
        let state = self.state.read(cx);
        let (count, focus, scroll) = (state.count, state.focus.clone(), state.scroll.clone());
        let confirm = {
            let (state, on_confirm) = (self.state.clone(), self.on_confirm.clone());
            move |_: &Confirm, window: &mut Window, cx: &mut App| {
                if let (Some(index), Some(on_confirm)) = (state.read(cx).highlighted, &on_confirm) {
                    on_confirm(&index, window, cx);
                }
            }
        };
        let rows = {
            let (id, state, render_row, on_confirm) = (
                self.id.clone(),
                self.state.clone(),
                self.render_row,
                self.on_confirm,
            );
            move |range: std::ops::Range<usize>, window: &mut Window, cx: &mut App| {
                let highlighted = state.read(cx).highlighted;
                range
                    .map(|ix| {
                        let row = div()
                            .id(ix)
                            .on_hover({
                                let state = state.clone();
                                move |hovered, _, cx| {
                                    if *hovered {
                                        change(&state, host, cx, |state| state.hover(ix));
                                    }
                                }
                            })
                            .on_click({
                                let (state, on_confirm) = (state.clone(), on_confirm.clone());
                                move |_, window, cx| {
                                    change(&state, host, cx, |state| {
                                        state.move_to(ix);
                                        true
                                    });
                                    if let Some(on_confirm) = &on_confirm {
                                        on_confirm(&ix, window, cx);
                                    }
                                }
                            })
                            .child(render_row(ix, highlighted == Some(ix), window, cx));
                        crate::automation::tag(row, format!("{id}-row-{ix}"))
                    })
                    .collect::<Vec<_>>()
            }
        };
        let mut root = div()
            .flex()
            .flex_col()
            .when(self.focusable, |d| d.track_focus(&focus))
            .key_context(CONTEXT)
            .on_action(update::<SelectPrevious>(&self.state, host, |s| {
                s.move_by(-1)
            }))
            .on_action(update::<SelectNext>(&self.state, host, |s| s.move_by(1)))
            .on_action(update::<SelectPageUp>(&self.state, host, |s| s.page(-1)))
            .on_action(update::<SelectPageDown>(&self.state, host, |s| s.page(1)))
            .on_action(update::<SelectFirst>(&self.state, host, |s| s.move_to(0)))
            .on_action(update::<SelectLast>(&self.state, host, |s| {
                s.move_to(s.count.saturating_sub(1))
            }))
            .on_action(confirm);
        root.style().refine(&self.style);
        root.child(
            uniform_list(self.id, count, rows)
                .track_scroll(&scroll)
                .flex_1()
                .min_h_0(),
        )
    }
}
