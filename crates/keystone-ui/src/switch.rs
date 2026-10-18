// The control's structure, a focusable row holding a pill track whose thumb sits at
// one end, follows gpui-component 0.7.3 `switch.rs` (Copyright 2024 - 2026 Longbridge,
// Apache-2.0). Modified: rewritten for Keystone's theme and neutral selection tokens,
// with no animation, and a controlled `on` value instead of a base component.

use crate::label::Label;
use crate::theme::ActiveTheme;
use crate::tokens::{TextSize, radius};
use gpui::{App, FocusHandle, IntoElement, RenderOnce, SharedString, Window, div, prelude::*, px};
use std::rc::Rc;

type ChangeHandler = Rc<dyn Fn(&bool, &mut Window, &mut App) + 'static>;

/// An on/off toggle. The caller owns `on` and stores the value `on_change` asks for.
/// Clicking, or Space or Enter while focused, asks for the opposite value. Named
/// `{id}` for tests and native verification.
#[derive(IntoElement)]
pub struct Switch {
    id: SharedString,
    on: bool,
    disabled: bool,
    label: Option<SharedString>,
    on_change: Option<ChangeHandler>,
}

impl Switch {
    pub fn new(id: impl Into<SharedString>, on: bool) -> Self {
        Self {
            id: id.into(),
            on,
            disabled: false,
            label: None,
            on_change: None,
        }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_change(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(handler));
        self
    }
}

/// A tab-stop focus handle that lives as long as the element keeps rendering.
pub(crate) fn keyed_focus(id: &SharedString, window: &mut Window, cx: &mut App) -> FocusHandle {
    window
        .use_keyed_state((gpui::ElementId::from(id.clone()), "focus"), cx, |_, cx| {
            cx.focus_handle().tab_stop(true)
        })
        .read(cx)
        .clone()
}

impl RenderOnce for Switch {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focus = keyed_focus(&self.id, window, cx);
        let theme = cx.theme();
        let colors = theme.colors;
        let height = theme.density.control(px(20.));
        let inset = px(2.);
        let (track, thumb) = if self.on {
            (colors.selection_edge, colors.background)
        } else {
            (colors.selection_fill, colors.text_muted)
        };
        let ring = colors.border_focused;
        let mut el = div()
            .id(self.id.clone())
            .flex()
            .items_center()
            .gap_2()
            .p(px(1.))
            .border_2()
            .border_color(ring.opacity(0.))
            .rounded(radius::CONTROL)
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .when(self.on, |d| d.justify_end())
                    .w(height * 1.8)
                    .h(height)
                    .p(inset)
                    .rounded_full()
                    .bg(track)
                    .child(div().size(height - inset * 2.).rounded_full().bg(thumb)),
            )
            .when_some(self.label, |d, label| {
                d.child(Label::new(label).size(TextSize::Base))
            });
        el = crate::automation::tag(el, self.id.to_string());
        if self.disabled {
            return el.opacity(0.4).cursor_not_allowed();
        }
        let on = self.on;
        el.track_focus(&focus)
            .focus(move |s| s.border_color(ring))
            .cursor_pointer()
            .when_some(self.on_change, |d, handler| {
                d.on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    handler(&!on, window, cx)
                })
            })
    }
}
