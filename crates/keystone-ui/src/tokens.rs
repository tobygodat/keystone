use gpui::{Pixels, px};
use std::cell::Cell;

pub mod spacing {
    use gpui::{Pixels, px};

    pub const XS: Pixels = px(4.);
    pub const SM: Pixels = px(8.);
    pub const MD: Pixels = px(12.);
    pub const LG: Pixels = px(16.);
    pub const XL: Pixels = px(24.);
}

pub mod radius {
    use gpui::{Pixels, px};

    pub const CONTROL: Pixels = px(6.);
    pub const MENU_ROW: Pixels = px(7.);
    pub const ROW: Pixels = px(8.);
    pub const PANEL: Pixels = px(10.);
    pub const POPOVER: Pixels = px(12.);
    pub const COMPOSER: Pixels = px(18.);
}

thread_local! {
    // Per thread rather than global, so GPUI tests running in parallel keep their own.
    static TEXT_SCALE: Cell<f32> = const { Cell::new(1.) };
}

pub(crate) fn set_text_scale(scale: f32) {
    TEXT_SCALE.set(scale);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextSize {
    Xs,
    Sm,
    Ui,
    #[default]
    Base,
    Body,
    Title,
}

impl TextSize {
    pub const ALL: [TextSize; 6] = [
        TextSize::Xs,
        TextSize::Sm,
        TextSize::Ui,
        TextSize::Base,
        TextSize::Body,
        TextSize::Title,
    ];

    /// The size under the active theme's [`crate::Theme::text_scale`].
    pub fn px(self) -> Pixels {
        TEXT_SCALE.get()
            * px(match self {
                TextSize::Xs => 10.,
                TextSize::Sm => 11.,
                TextSize::Ui => 12.,
                TextSize::Base => 13.,
                TextSize::Body => 14.,
                TextSize::Title => 24.,
            })
    }
}

/// Back-to-front surface levels. Each maps to a background, a hairline border,
/// and a corner radius. Only `Overlay` casts a shadow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    Surface,
    Raised,
    Overlay,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_sizes_follow_the_text_scale() {
        set_text_scale(15. / 13.);
        assert_eq!(TextSize::Base.px(), px(15.));
        set_text_scale(1.);
        assert_eq!(TextSize::Base.px(), px(13.));
    }
}
