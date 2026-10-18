// The control's structure, a pill track with a filled span up to a round thumb that
// is centred on the value, follows gpui-component 0.7.3 `slider.rs` (Copyright 2024 -
// 2026 Longbridge, Apache-2.0). Modified: rewritten for Keystone's theme and neutral
// selection tokens, single-valued and horizontal only, with a controlled value that
// snaps to a step, and its own drag and key handling instead of a base component.

use crate::switch::keyed_focus;
use crate::theme::ActiveTheme;
use gpui::{
    App, Empty, IntoElement, RenderOnce, SharedString, Window, div, prelude::*, px, relative,
};
use std::ops::RangeInclusive;
use std::rc::Rc;

type ChangeHandler = Rc<dyn Fn(&f32, &mut Window, &mut App) + 'static>;

/// The payload of a thumb drag, naming the slider it belongs to.
struct ThumbDrag(SharedString);

/// `value` clamped to `range` and rounded to the nearest whole number of `step`s from
/// its start. A step that does not divide the range leaves the end reachable.
pub fn snap(value: f32, range: &RangeInclusive<f32>, step: f32) -> f32 {
    let (min, max) = (*range.start(), *range.end());
    let value = value.clamp(min, max);
    if step <= 0. || value == max {
        return value;
    }
    (min + ((value - min) / step).round() * step).min(max)
}

/// The next stop from `value` in `direction`, where the stops are the whole steps from
/// the start of `range` and its end. A step that does not divide the range still
/// stops at every whole step on the way back from the end.
fn step_from(value: f32, range: &RangeInclusive<f32>, step: f32, direction: f32) -> f32 {
    let (min, max) = (*range.start(), *range.end());
    if step <= 0. {
        return value;
    }
    let steps = (value - min) / step;
    let on_step = (steps - steps.round()).abs() < 1e-4;
    let next = match (direction > 0., on_step) {
        (true, true) => steps.round() + 1.,
        (true, false) => steps.ceil(),
        (false, true) => steps.round() - 1.,
        (false, false) => steps.floor(),
    };
    (min + next * step).clamp(min, max)
}

/// A horizontal value picker. The caller owns `value` and stores the value
/// `on_change` asks for, which is always snapped to `step` within `range`. Dragging the
/// thumb or pressing the track picks a value under the pointer. While focused, Left and
/// Down step down, Right and Up step up, and Home and End jump to the ends. Named `{id}`
/// for the track and `{id}-thumb` for the thumb.
#[derive(IntoElement)]
pub struct Slider {
    id: SharedString,
    value: f32,
    range: RangeInclusive<f32>,
    step: f32,
    disabled: bool,
    on_change: Option<ChangeHandler>,
}

impl Slider {
    pub fn new(
        id: impl Into<SharedString>,
        value: f32,
        range: RangeInclusive<f32>,
        step: f32,
    ) -> Self {
        Self {
            id: id.into(),
            value,
            range,
            step,
            disabled: false,
            on_change: None,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_change(mut self, handler: impl Fn(&f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Slider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focus = keyed_focus(&self.id, window, cx);
        let focused = focus.is_focused(window);
        let theme = cx.theme();
        let colors = theme.colors;
        let thumb = theme.density.control(px(16.));
        let half = thumb / 2.;
        let (min, max) = (*self.range.start(), *self.range.end());
        let value = snap(self.value, &self.range, self.step);
        let fraction = if max > min {
            (value - min) / (max - min)
        } else {
            0.
        };
        let ring = if focused {
            colors.border_focused
        } else {
            colors.background
        };
        let thumb_el = crate::automation::tag(
            div()
                .id(SharedString::from(format!("{}-thumb", self.id)))
                .absolute()
                .top(px(2.) - half)
                .left(relative(fraction))
                .ml(-half)
                .size(thumb)
                .rounded_full()
                .border_2()
                .border_color(ring)
                .bg(colors.selection_edge),
            format!("{}-thumb", self.id),
        );
        let bar = div()
            .relative()
            .w_full()
            .h(px(4.))
            .rounded_full()
            .bg(colors.selection_fill)
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .h_full()
                    .w(relative(fraction))
                    .rounded_full()
                    .bg(colors.selection_edge),
            )
            .child(thumb_el);
        let el = crate::automation::tag(
            div()
                .id(self.id.clone())
                .flex()
                .items_center()
                .w_full()
                .h(thumb + px(8.))
                .px(half)
                .child(bar),
            self.id.to_string(),
        );
        let Some(on_change) = self.on_change.filter(|_| !self.disabled) else {
            return el
                .when(self.disabled, |d| d.opacity(0.4).cursor_not_allowed())
                .into_any_element();
        };
        let (id, range, step) = (self.id, self.range, self.step);
        let pick = {
            let (range, on_change) = (range.clone(), on_change.clone());
            move |x: gpui::Pixels,
                  bounds: gpui::Bounds<gpui::Pixels>,
                  window: &mut Window,
                  cx: &mut App| {
                let width = bounds.size.width - half * 2.;
                let at = if width > px(0.) {
                    ((x - bounds.origin.x - half) / width).clamp(0., 1.)
                } else {
                    0.
                };
                let next = snap(min + at * (max - min), &range, step);
                if next != value {
                    on_change(&next, window, cx);
                }
            }
        };
        let pick = Rc::new(pick);
        // The press handler needs the track's bounds, which only layout knows.
        let track =
            window.use_keyed_state((gpui::ElementId::from(id.clone()), "bounds"), cx, |_, _| {
                gpui::Bounds::<gpui::Pixels>::default()
            });
        el.track_focus(&focus)
            .cursor_pointer()
            .child(
                gpui::canvas(
                    {
                        let track = track.clone();
                        move |bounds, _, cx| track.update(cx, |track, _| *track = bounds)
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .on_mouse_down(gpui::MouseButton::Left, {
                let (pick, focus, track) = (pick.clone(), focus.clone(), track.clone());
                move |event, window, cx| {
                    cx.stop_propagation();
                    window.focus(&focus, cx);
                    let bounds = *track.read(cx);
                    pick(event.position.x, bounds, window, cx);
                }
            })
            .on_drag(ThumbDrag(id.clone()), {
                let (pick, track) = (pick.clone(), track.clone());
                // The move that starts a drag reaches no drag-move listener.
                move |_, offset, window, cx| {
                    cx.stop_propagation();
                    let bounds = *track.read(cx);
                    pick(bounds.origin.x + offset.x, bounds, window, cx);
                    cx.new(|_| Empty)
                }
            })
            .on_drag_move::<ThumbDrag>(move |event, window, cx| {
                if event.drag(cx).0 == id {
                    pick(event.event.position.x, event.bounds, window, cx);
                }
            })
            .on_key_down(move |event, window, cx| {
                let next = match event.keystroke.key.as_str() {
                    _ if event.keystroke.modifiers.modified() => return,
                    "left" | "down" => step_from(value, &range, step, -1.),
                    "right" | "up" => step_from(value, &range, step, 1.),
                    "home" => min,
                    "end" => max,
                    _ => return,
                };
                cx.stop_propagation();
                let next = snap(next, &range, step);
                if next != value {
                    on_change(&next, window, cx);
                }
            })
            .into_any_element()
    }
}
