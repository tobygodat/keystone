use crate::theme::ActiveTheme;
use crate::tokens::{Elevation, radius};
use gpui::{App, BoxShadow, Div, Styled, div, point, px};

pub fn h_flex() -> Div {
    div().flex().flex_row().items_center()
}

pub fn v_flex() -> Div {
    div().flex().flex_col()
}

pub trait StyledExt: Styled + Sized {
    /// Background, hairline border, radius, and (for overlays) shadow for one elevation level.
    fn surface(self, elevation: Elevation, cx: &App) -> Self {
        let colors = &cx.theme().colors;
        let (bg, rounding) = match elevation {
            Elevation::Surface => (colors.surface, radius::PANEL),
            Elevation::Raised => (colors.surface_raised, radius::PANEL),
            Elevation::Overlay => (colors.overlay, radius::POPOVER),
        };
        let el = self
            .bg(bg)
            .border_1()
            .border_color(colors.border)
            .rounded(rounding);
        if elevation == Elevation::Overlay {
            el.shadow(vec![BoxShadow {
                color: colors.shadow,
                offset: point(px(0.), px(8.)),
                blur_radius: px(24.),
                spread_radius: px(0.),
            }])
        } else {
            el
        }
    }

    fn hairline_t(self, cx: &App) -> Self {
        self.border_t_1().border_color(cx.theme().colors.border)
    }

    fn hairline_b(self, cx: &App) -> Self {
        self.border_b_1().border_color(cx.theme().colors.border)
    }

    fn hairline_l(self, cx: &App) -> Self {
        self.border_l_1().border_color(cx.theme().colors.border)
    }

    fn hairline_r(self, cx: &App) -> Self {
        self.border_r_1().border_color(cx.theme().colors.border)
    }
}

impl<T: Styled> StyledExt for T {}
