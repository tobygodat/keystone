//! Keystone's component library: theme, tokens, icons, and the controls the
//! desktop app is built from. Components read the theme from the `App` global
//! that [`init`] installs, so call sites never pass colors or a theme.

mod color;
mod icon;
mod styled_ext;
mod theme;
mod tokens;

pub use color::Color;
pub use icon::{Icon, IconName, Icons};
pub use styled_ext::{StyledExt, h_flex, v_flex};
pub use theme::{ActiveTheme, Colors, Fonts, Theme};
pub use tokens::{Elevation, TextSize, radius, spacing};

/// Installs the dark theme. Call once before opening a window.
pub fn init(cx: &mut gpui::App) {
    cx.set_global(Theme::dark());
}

pub mod prelude {
    pub use crate::{
        ActiveTheme, Color, Elevation, Icon, IconName, StyledExt, TextSize, h_flex, radius,
        spacing, v_flex,
    };
}
