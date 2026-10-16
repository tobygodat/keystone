use gpui::{Pixels, px};

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

    pub fn px(self) -> Pixels {
        px(match self {
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
