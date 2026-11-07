use crate::color::Color;
use crate::icon::{Icon, IconName};
use crate::label::Label;
use crate::theme::ActiveTheme;
use crate::tokens::{TextSize, radius};
use crate::tooltip::Tooltip;
use gpui::{
    App, ClickEvent, ElementId, FocusHandle, Hsla, IntoElement, Pixels, RenderOnce, SharedString,
    Stateful, Styled, Window, div, prelude::*, px,
};

/// How opaque the primary fill is while hovered and pressed. The theme's contrast test
/// checks every action color's label at both.
pub(crate) const PRIMARY_HOVER_OPACITY: f32 = 0.94;
pub(crate) const PRIMARY_PRESSED_OPACITY: f32 = 0.88;

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonStyle {
    /// No resting background. The default for toolbars and inline actions.
    #[default]
    Ghost,
    /// A faint resting wash, for secondary actions that need an edge.
    Subtle,
    /// The strongest fill, for the primary action in a group.
    Filled,
    Primary,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonSize {
    Compact,
    #[default]
    Default,
    Large,
}

impl ButtonSize {
    fn height(self, cx: &App) -> Pixels {
        cx.theme().density.control(px(match self {
            ButtonSize::Compact => 24.,
            ButtonSize::Default => 28.,
            ButtonSize::Large => 32.,
        }))
    }
}

/// Interaction state shared by `Button` and `IconButton`.
struct Common {
    id: ElementId,
    style: ButtonStyle,
    size: ButtonSize,
    selected: bool,
    disabled: bool,
    tooltip: Option<Tooltip>,
    on_click: Option<ClickHandler>,
    debug_selector: Option<String>,
    focus: Option<FocusHandle>,
    keyboard_ring: bool,
    reveal_on_focus: bool,
    reserve_focus_ring: bool,
    aria_label: Option<SharedString>,
    aria_expanded: Option<bool>,
}

impl Common {
    fn new(id: ElementId) -> Self {
        Self {
            id,
            style: ButtonStyle::default(),
            size: ButtonSize::default(),
            selected: false,
            disabled: false,
            tooltip: None,
            on_click: None,
            debug_selector: None,
            focus: None,
            keyboard_ring: false,
            reveal_on_focus: false,
            reserve_focus_ring: false,
            aria_label: None,
            aria_expanded: None,
        }
    }

    /// The interactive shell: fill per style and state, hover and press washes, click and tooltip.
    fn base(self, cx: &App) -> Stateful<gpui::Div> {
        let colors = &cx.theme().colors;
        let (rest, hover) = match (self.style, self.selected) {
            (ButtonStyle::Primary, _) => (
                Some(colors.element_inverse),
                colors.element_inverse.opacity(PRIMARY_HOVER_OPACITY),
            ),
            (ButtonStyle::Filled, _) => (Some(colors.element_filled), colors.element_filled_hover),
            (_, true) => (Some(colors.selection_fill), colors.selection_fill),
            (ButtonStyle::Subtle, false) => (Some(colors.element_hover), colors.element_active),
            (ButtonStyle::Ghost, false) => (None, colors.element_hover),
        };
        let active = match (self.style, self.selected) {
            (ButtonStyle::Primary, _) => colors.element_inverse.opacity(PRIMARY_PRESSED_OPACITY),
            (ButtonStyle::Ghost | ButtonStyle::Subtle, true) => colors.selection_fill,
            _ => colors.element_active,
        };
        let mut el = div()
            .id(self.id)
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .h(self.size.height(cx))
            .rounded(radius::CONTROL)
            .when_some(rest, |d, bg| d.bg(bg));
        if let Some(selector) = self.debug_selector {
            el = crate::automation::tag(el, selector);
        }
        if let Some(label) = self.aria_label {
            el = el.role(gpui::Role::Button).aria_label(label);
        }
        if let Some(expanded) = self.aria_expanded {
            el = el.aria_expanded(expanded);
        }
        if self.disabled {
            return el.opacity(0.4).cursor_not_allowed();
        }
        // The ring is a border that is always there, so focus never changes the size.
        let ring = focus_ring(self.style, colors);
        let reveal = self.reveal_on_focus;
        let keyboard_ring = self.keyboard_ring;
        el.when(self.reserve_focus_ring || self.focus.is_some(), |d| {
            d.border_2().border_color(ring.opacity(0.))
        })
        .when_some(self.focus, |d, focus| {
            d.track_focus(&focus)
                .map(|d| {
                    if keyboard_ring {
                        d.focus_visible(move |s| s.border_color(ring))
                    } else {
                        d.focus(move |s| s.border_color(ring))
                    }
                })
                .when(reveal, |d| {
                    d.relative().child(
                        gpui::canvas(
                            move |bounds, window, _| {
                                if focus.is_focused(window) && window.last_input_was_keyboard() {
                                    window.request_autoscroll(bounds);
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                })
        })
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(active))
        .when_some(self.tooltip, |d, tip| d.tooltip(tip.builder()))
        .when_some(self.on_click, |d, handler| {
            d.on_click(move |event, window, cx| {
                cx.stop_propagation();
                handler(event, window, cx)
            })
        })
    }
}

macro_rules! common_builders {
    () => {
        pub fn style(mut self, style: ButtonStyle) -> Self {
            self.common.style = style;
            self
        }

        pub fn size(mut self, size: ButtonSize) -> Self {
            self.common.size = size;
            self
        }

        pub fn selected(mut self, selected: bool) -> Self {
            self.common.selected = selected;
            self
        }

        pub fn disabled(mut self, disabled: bool) -> Self {
            self.common.disabled = disabled;
            self
        }

        pub fn tooltip(mut self, tooltip: Tooltip) -> Self {
            self.common.tooltip = Some(tooltip);
            self
        }

        pub fn on_click(
            mut self,
            handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        ) -> Self {
            self.common.on_click = Some(Box::new(handler));
            self
        }

        /// Makes the button focusable with a visible ring. Enter or Space clicks it while focused.
        pub fn track_focus(mut self, focus: &FocusHandle) -> Self {
            self.common.focus = Some(focus.clone());
            self
        }

        /// Shows the focus ring only when focus came from the keyboard, not a click.
        pub fn keyboard_focus_ring(mut self) -> Self {
            self.common.keyboard_ring = true;
            self
        }

        pub fn reveal_on_focus(mut self) -> Self {
            self.common.reveal_on_focus = true;
            self
        }

        pub fn reserve_focus_ring(mut self) -> Self {
            self.common.reserve_focus_ring = true;
            self
        }

        /// Names the element for simulated-input tests and native verification.
        pub fn debug_selector(mut self, selector: impl FnOnce() -> String) -> Self {
            self.common.debug_selector = Some(selector());
            self
        }

        /// Exposes the control to assistive technology as a button with this name. The
        /// name, role, click action and focus all sit on the same element.
        pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
            self.common.aria_label = Some(label.into());
            self
        }

        pub fn aria_expanded(mut self, expanded: bool) -> Self {
            self.common.aria_expanded = Some(expanded);
            self
        }
    };
}

#[derive(IntoElement)]
pub struct Button {
    common: Common,
    label: SharedString,
    icon: Option<IconName>,
    color: Option<Color>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            common: Common::new(id.into()),
            label: label.into(),
            icon: None,
            color: None,
        }
    }

    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Overrides the label and icon color, for a state that needs attention.
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    common_builders!();
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let size = self.common.size;
        let color = if let Some(color) = self.color {
            color
        } else if self.common.style == ButtonStyle::Primary {
            Color::Custom(cx.theme().colors.text_inverse)
        } else if self.common.selected {
            Color::Custom(cx.theme().colors.selection_edge)
        } else if self.common.style == ButtonStyle::Filled {
            Color::Default
        } else {
            Color::Muted
        };
        let text = if size == ButtonSize::Large {
            TextSize::Base
        } else {
            TextSize::Ui
        };
        self.common
            .base(cx)
            .gap_1p5()
            .px(match size {
                ButtonSize::Compact => px(6.),
                ButtonSize::Default => px(10.),
                ButtonSize::Large => px(12.),
            })
            .when_some(self.icon, |d, icon| {
                d.child(Icon::new(icon).size(px(16.)).color(color))
            })
            .child(Label::new(self.label).size(text).color(color))
    }
}

#[derive(IntoElement)]
pub struct IconButton {
    common: Common,
    icon: IconName,
    color: Option<Color>,
}

impl IconButton {
    pub fn new(id: impl Into<ElementId>, icon: IconName) -> Self {
        Self {
            common: Common::new(id.into()),
            icon,
            color: None,
        }
    }

    /// Overrides the icon color, for a state that needs attention.
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    common_builders!();
}

impl RenderOnce for IconButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let side = self.common.size.height(cx);
        let color = if let Some(color) = self.color {
            color
        } else if self.common.style == ButtonStyle::Primary {
            Color::Custom(cx.theme().colors.text_inverse)
        } else if self.common.selected {
            Color::Custom(cx.theme().colors.selection_edge)
        } else {
            Color::Muted
        };
        self.common
            .base(cx)
            .w(side)
            .child(Icon::new(self.icon).color(color))
    }
}

/// A color choice: a dot on the button shell, ringed in text color while selected.
#[derive(IntoElement)]
pub struct Swatch {
    common: Common,
    fill: Hsla,
}

impl Swatch {
    pub fn new(id: impl Into<ElementId>, fill: Hsla) -> Self {
        Self {
            common: Common::new(id.into()),
            fill,
        }
    }

    common_builders!();
}

impl RenderOnce for Swatch {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let side = self.common.size.height(cx);
        let text = cx.theme().colors.text;
        let ring = if self.common.selected {
            text
        } else {
            text.opacity(0.)
        };
        let fill = self.fill;
        self.common.base(cx).w(side).child(
            div()
                .size(px(20.))
                .p(px(2.))
                .rounded_full()
                .border_2()
                .border_color(ring)
                .child(div().size_full().rounded_full().bg(fill)),
        )
    }
}

/// The focus ring's colour. The focus blue would vanish on a primary button's blue
/// fill, so a primary button's ring takes its label's colour.
fn focus_ring(style: ButtonStyle, colors: &crate::theme::Colors) -> Hsla {
    if style == ButtonStyle::Primary {
        colors.text_inverse
    } else {
        colors.border_focused
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Colors;

    fn luminance(color: Hsla) -> f32 {
        let rgb = color.to_rgb();
        let channel = |c: f32| {
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
    }

    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// A focus ring must stand out from the button it marks (WCAG's 3:1 for
    /// non-text contrast), on the primary fill as on the subtle one.
    #[test]
    fn focus_rings_stand_out_from_their_buttons_in_both_themes() {
        for colors in [Colors::dark(), Colors::light()] {
            for (style, fill) in [
                (ButtonStyle::Primary, colors.element_inverse),
                (ButtonStyle::Subtle, colors.element_hover),
            ] {
                let ratio = contrast(focus_ring(style, &colors), fill);
                assert!(ratio >= 3.0, "{style:?}: {ratio:.2}");
            }
        }
    }
}
