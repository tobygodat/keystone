// Structure adapted from gpui-component 0.7.3 (`resizable.rs` and gpui-base's
// `resizable/`), Copyright 2024 - 2026 Longbridge, licensed Apache-2.0.
// Modified: fractions instead of pixel sizes, neighbour-only drags, a flat
// absolute layout, and typed pane-edge drop zones.

//! Resizable panes. [`SplitState`] owns the axis and each pane's share of the
//! container, and [`Split`] renders any number of panes with a draggable
//! divider between each pair.

use crate::{ActiveTheme, automation::tag};
use gpui::{
    AnyElement, App, AppContext as _, Axis, Bounds, Div, Empty, Entity, EntityId,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Point, RenderOnce,
    SharedString, Size, Stateful, StatefulInteractiveElement, Styled, Window, canvas, div,
    prelude::FluentBuilder, px, relative,
};
use std::{cell::Cell, rc::Rc};

/// The pointer band around each divider's hairline.
const DIVIDER_HIT: Pixels = px(6.);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// The pane a drag hovers and the edge of it nearest the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneEdge {
    pub pane: usize,
    pub edge: Edge,
}

pub struct SplitState {
    axis: Axis,
    fractions: Vec<f32>,
    min_px: Pixels,
    drop_target: Option<PaneEdge>,
}

impl SplitState {
    /// `panes` equal panes along `axis`, each at least 120 px long.
    pub fn new(axis: Axis, panes: usize) -> Self {
        assert!(panes > 0, "a split needs at least one pane");
        Self {
            axis,
            fractions: vec![1. / panes as f32; panes],
            min_px: px(120.),
            drop_target: None,
        }
    }

    pub fn min_px(mut self, min_px: Pixels) -> Self {
        self.min_px = min_px.max(px(0.));
        self
    }

    pub fn axis(&self) -> Axis {
        self.axis
    }

    pub fn set_axis(&mut self, axis: Axis) {
        self.axis = axis;
    }

    /// Each pane's share of the container, summing to 1.
    pub fn fractions(&self) -> &[f32] {
        &self.fractions
    }

    /// Restores saved shares, scaled to sum to 1. Returns false, changing
    /// nothing, unless every share is finite and positive.
    pub fn set_fractions(&mut self, fractions: &[f32]) -> bool {
        let valid = !fractions.is_empty() && fractions.iter().all(|f| f.is_finite() && *f > 0.);
        let total: f32 = fractions.iter().sum();
        if !valid || !total.is_finite() {
            return false;
        }
        self.fractions = fractions.iter().map(|f| f / total).collect();
        true
    }

    /// Puts `divider` at `offset` from the container's start, moving only the two
    /// panes beside it. Both keep at least `min_px`, or half their combined
    /// length when that is shorter.
    pub fn move_divider(&mut self, divider: usize, offset: Pixels, extent: Pixels) {
        let finite = f32::from(offset).is_finite() && f32::from(extent).is_finite();
        if divider + 1 >= self.fractions.len() || !finite || extent <= px(0.) {
            return;
        }
        let start: f32 = self.fractions[..divider].iter().sum();
        let pair = self.fractions[divider] + self.fractions[divider + 1];
        let min = (self.min_px / extent).min(pair / 2.);
        let before = (offset / extent - start).clamp(min, pair - min);
        self.fractions[divider] = before;
        self.fractions[divider + 1] = pair - before;
    }

    /// Returns every pane to an equal share.
    pub fn reset(&mut self) {
        let share = 1. / self.fractions.len() as f32;
        self.fractions.fill(share);
    }

    /// The pane under `at`, relative to a container of `size`, and the edge of
    /// that pane nearest to it in pixels.
    fn pane_edge(&self, at: Point<Pixels>, size: Size<Pixels>) -> Option<PaneEdge> {
        let inside = at.x >= px(0.) && at.y >= px(0.) && at.x <= size.width && at.y <= size.height;
        let (along, extent) = match self.axis {
            Axis::Horizontal => (at.x, size.width),
            Axis::Vertical => (at.y, size.height),
        };
        if !inside || extent <= px(0.) {
            return None;
        }
        let last = self.fractions.len() - 1;
        let mut start = px(0.);
        let mut pane = last;
        for (index, fraction) in self.fractions.iter().enumerate() {
            if index == last || along < start + extent * *fraction {
                pane = index;
                break;
            }
            start += extent * *fraction;
        }
        let length = extent * self.fractions[pane];
        let (x, y, width, height) = match self.axis {
            Axis::Horizontal => (along - start, at.y, length, size.height),
            Axis::Vertical => (at.x, along - start, size.width, length),
        };
        let edge = [
            (x, Edge::Left),
            (width - x, Edge::Right),
            (y, Edge::Top),
            (height - y, Edge::Bottom),
        ]
        .into_iter()
        .min_by(|a, b| f32::from(a.0).total_cmp(&f32::from(b.0)))
        .map_or(Edge::Left, |(_, edge)| edge);
        Some(PaneEdge { pane, edge })
    }

    fn set_drop_target(&mut self, target: Option<PaneEdge>, cx: &mut gpui::Context<Self>) {
        if self.drop_target != target {
            self.drop_target = target;
            cx.notify();
        }
    }
}

/// The drag payload of a divider, so a split moves only its own dividers.
struct DividerDrag {
    split: EntityId,
    divider: usize,
}

type AttachDrop = Box<dyn FnOnce(Stateful<Div>, Entity<SplitState>) -> Stateful<Div>>;

/// Lays its children out as the panes of a [`SplitState`], in order. Debug
/// selectors are `{id}-pane-{n}` and `{id}-divider-{n}`, counting from 0, and
/// `{id}-drop-target` for the highlight.
#[derive(IntoElement)]
pub struct Split {
    id: SharedString,
    state: Entity<SplitState>,
    panes: Vec<AnyElement>,
    attach_drop: Option<AttachDrop>,
}

impl Split {
    pub fn new(id: impl Into<SharedString>, state: &Entity<SplitState>) -> Self {
        Self {
            id: id.into(),
            state: state.clone(),
            panes: Vec::new(),
            attach_drop: None,
        }
    }

    /// Accepts a dragged `T` on a pane edge. While a `T` hovers a pane, the half
    /// of the pane on the nearest edge highlights. Dropping calls `on_drop`.
    pub fn on_drop_edge<T: 'static>(
        mut self,
        on_drop: impl Fn(&T, PaneEdge, &mut Window, &mut App) + 'static,
    ) -> Self {
        let on_drop = Rc::new(on_drop);
        self.attach_drop = Some(Box::new(move |container, state| {
            // A drag can start and drop without a move reaching this split, so the
            // drop reads the pointer against the bounds of the last painted frame.
            let painted = Rc::new(Cell::new(Bounds::default()));
            let record = painted.clone();
            let (hover, inside, outside) = (state.clone(), state.clone(), state.clone());
            let clear = |state: &Entity<SplitState>, cx: &mut App| {
                state.update(cx, |state, cx| state.set_drop_target(None, cx))
            };
            container
                .child(
                    canvas(move |bounds, _, _| record.set(bounds), |_, _, _, _| {})
                        .absolute()
                        .inset_0(),
                )
                .on_drag_move::<T>(move |event, _, cx| {
                    let bounds = event.bounds;
                    let at = event.event.position - bounds.origin;
                    let target = hover.read(cx).pane_edge(at, bounds.size);
                    hover.update(cx, |state, cx| state.set_drop_target(target, cx));
                })
                .on_drop::<T>(move |payload, window, cx| {
                    let bounds = painted.get();
                    let at = window.mouse_position() - bounds.origin;
                    let target = state.read(cx).pane_edge(at, bounds.size);
                    clear(&state, cx);
                    if let Some(target) = target {
                        on_drop(payload, target, window, cx);
                    }
                })
                .on_mouse_up(MouseButton::Left, move |_, _, cx| clear(&inside, cx))
                .on_mouse_up_out(MouseButton::Left, move |_, _, cx| clear(&outside, cx))
        }));
        self
    }
}

impl ParentElement for Split {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.panes.extend(elements);
    }
}

impl RenderOnce for Split {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = &cx.theme().colors;
        let (hairline, hover, wash, ring) = (
            colors.border,
            colors.element_hover,
            colors.selection,
            colors.border_focused,
        );
        let state = self.state.read(cx);
        let (axis, fractions) = (state.axis, state.fractions.clone());
        let drop_target = state.drop_target.filter(|_| cx.has_active_drag());
        debug_assert_eq!(
            self.panes.len(),
            fractions.len(),
            "a split renders one child per pane"
        );
        let split = self.state.entity_id();
        let id = self.id;
        let starts: Vec<f32> = fractions
            .iter()
            .scan(0., |start, fraction| {
                let this = *start;
                *start += fraction;
                Some(this)
            })
            .collect();

        let panes = self
            .panes
            .into_iter()
            .zip(starts.iter().zip(&fractions))
            .enumerate()
            .map(|(index, (child, (&start, &fraction)))| {
                let highlight = drop_target
                    .filter(|target| target.pane == index)
                    .map(|target| {
                        let half = div().absolute().bg(wash).border_2().border_color(ring);
                        let half = tag(half, format!("{id}-drop-target"));
                        match target.edge {
                            Edge::Left => half.top_0().bottom_0().left_0().w(relative(0.5)),
                            Edge::Right => half.top_0().bottom_0().right_0().w(relative(0.5)),
                            Edge::Top => half.left_0().right_0().top_0().h(relative(0.5)),
                            Edge::Bottom => half.left_0().right_0().bottom_0().h(relative(0.5)),
                        }
                    });
                let pane = div().absolute().overflow_hidden().map(|pane| match axis {
                    Axis::Horizontal => pane
                        .top_0()
                        .bottom_0()
                        .left(relative(start))
                        .w(relative(fraction)),
                    Axis::Vertical => pane
                        .left_0()
                        .right_0()
                        .top(relative(start))
                        .h(relative(fraction)),
                });
                tag(pane.child(child), format!("{id}-pane-{index}"))
                    .children(highlight)
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let dividers = starts
            .iter()
            .skip(1)
            .enumerate()
            .map(|(divider, &at)| {
                let line = div().bg(hairline).map(|line| match axis {
                    Axis::Horizontal => line.w(px(1.)).h_full(),
                    Axis::Vertical => line.h(px(1.)).w_full(),
                });
                let band = div()
                    .id(SharedString::from(format!("{id}-divider-{divider}")))
                    .absolute()
                    .occlude()
                    .flex()
                    .justify_center()
                    .items_center()
                    .hover(|band| band.bg(hover))
                    .map(|band| match axis {
                        Axis::Horizontal => band
                            .top_0()
                            .bottom_0()
                            .left(relative(at))
                            .ml(-DIVIDER_HIT / 2.)
                            .w(DIVIDER_HIT)
                            .cursor_col_resize(),
                        Axis::Vertical => band
                            .left_0()
                            .right_0()
                            .top(relative(at))
                            .mt(-DIVIDER_HIT / 2.)
                            .h(DIVIDER_HIT)
                            .cursor_row_resize(),
                    })
                    .on_drag(DividerDrag { split, divider }, |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| Empty)
                    })
                    .on_click({
                        let state = self.state.clone();
                        move |event, _, cx| {
                            if event.click_count() == 2 {
                                state.update(cx, |state, cx| {
                                    state.reset();
                                    cx.notify();
                                });
                            }
                        }
                    })
                    .child(line);
                tag(band, format!("{id}-divider-{divider}")).into_any_element()
            })
            .collect::<Vec<_>>();

        let state = self.state.clone();
        div()
            .id(id.clone())
            .relative()
            .size_full()
            .overflow_hidden()
            .on_drag_move::<DividerDrag>(move |event, _, cx| {
                let drag = event.drag(cx);
                if drag.split != split {
                    return;
                }
                let divider = drag.divider;
                let (offset, extent) = match axis {
                    Axis::Horizontal => (
                        event.event.position.x - event.bounds.origin.x,
                        event.bounds.size.width,
                    ),
                    Axis::Vertical => (
                        event.event.position.y - event.bounds.origin.y,
                        event.bounds.size.height,
                    ),
                };
                state.update(cx, |state, cx| {
                    state.drop_target = None;
                    state.move_divider(divider, offset, extent);
                    cx.notify();
                });
            })
            .children(panes)
            .children(dividers)
            .map(|container| match self.attach_drop {
                Some(attach) => attach(container, self.state),
                None => container,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three() -> SplitState {
        SplitState::new(Axis::Horizontal, 3).min_px(px(100.))
    }

    fn assert_close(actual: &[f32], expected: &[f32]) {
        let close = actual
            .iter()
            .zip(expected)
            .all(|(a, e)| (a - e).abs() < 1e-4);
        assert!(close, "{actual:?} != {expected:?}");
    }

    #[test]
    fn drag_moves_only_neighbours() {
        let mut state = three();
        state.move_divider(0, px(500.), px(1200.));
        assert_close(state.fractions(), &[500. / 1200., 300. / 1200., 1. / 3.]);
        state.move_divider(1, px(1000.), px(1200.));
        assert_close(
            state.fractions(),
            &[500. / 1200., 500. / 1200., 200. / 1200.],
        );
    }

    #[test]
    fn drag_clamps_to_min() {
        let mut state = three();
        state.move_divider(0, px(2000.), px(1200.));
        assert_close(state.fractions(), &[700. / 1200., 100. / 1200., 1. / 3.]);
        state.move_divider(0, px(-50.), px(1200.));
        assert_close(state.fractions(), &[100. / 1200., 700. / 1200., 1. / 3.]);
        // Two panes shorter than twice the minimum split their length evenly.
        state.move_divider(1, px(0.), px(200.));
        let pair = state.fractions()[1] + state.fractions()[2];
        assert_close(&state.fractions()[1..], &[pair / 2., pair / 2.]);
    }

    #[test]
    fn double_click_resets() {
        let mut state = three();
        state.move_divider(0, px(900.), px(1200.));
        state.reset();
        assert_close(state.fractions(), &[1. / 3.; 3]);
    }

    #[test]
    fn pane_edge_picks_the_nearest_edge_in_pixels() {
        let state = three();
        let size = gpui::size(px(1200.), px(900.));
        let at = |x, y| state.pane_edge(gpui::point(px(x), px(y)), size);
        let edge = |pane, edge| Some(PaneEdge { pane, edge });
        assert_eq!(at(750., 450.), edge(1, Edge::Right));
        assert_eq!(at(450., 450.), edge(1, Edge::Left));
        assert_eq!(at(600., 40.), edge(1, Edge::Top));
        assert_eq!(at(1100., 880.), edge(2, Edge::Bottom));
        assert_eq!(at(1200., 450.), edge(2, Edge::Right));
        assert_eq!(at(1201., 450.), None);
        // A tall, narrow pane: 40 px from the left beats 100 px from the top.
        let narrow = SplitState::new(Axis::Horizontal, 1);
        let tall = gpui::size(px(200.), px(1000.));
        assert_eq!(
            narrow.pane_edge(gpui::point(px(40.), px(100.)), tall),
            edge(0, Edge::Left)
        );
        let vertical = SplitState::new(Axis::Vertical, 2);
        assert_eq!(
            vertical.pane_edge(gpui::point(px(600.), px(850.)), size),
            edge(1, Edge::Bottom)
        );
    }

    #[test]
    fn bad_input_leaves_the_split_unchanged() {
        let mut state = three().min_px(px(-50.));
        state.move_divider(0, px(f32::NAN), px(1200.));
        state.move_divider(0, px(100.), px(f32::INFINITY));
        assert_close(state.fractions(), &[1. / 3.; 3]);
        state.move_divider(0, px(-500.), px(1200.));
        assert_close(state.fractions(), &[0., 2. / 3., 1. / 3.]);
        assert!(!state.set_fractions(&[1., f32::NAN, 1.]));
        assert!(!state.set_fractions(&[]));
        assert!(state.set_fractions(&[1., 1., 2.]));
        assert_close(state.fractions(), &[0.25, 0.25, 0.5]);
    }
}
