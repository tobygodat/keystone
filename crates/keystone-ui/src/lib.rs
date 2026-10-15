//! Keystone's component library: theme, tokens, icons, and the controls the
//! desktop app is built from. Components read the theme from the `App` global
//! that [`init`] installs, so call sites never pass colors or a theme.

mod button;
mod color;
mod icon;
mod kbd;
mod label;
mod list_item;
mod popover;
mod styled_ext;
mod theme;
mod tokens;
mod tooltip;

pub use button::{Button, ButtonSize, ButtonStyle, IconButton};
pub use color::Color;
pub use icon::{Icon, IconName, Icons};
pub use kbd::Kbd;
pub use label::Label;
pub use list_item::ListItem;
pub use popover::{MenuItem, MenuSeparator, PopoverMenu};
pub use styled_ext::{StyledExt, h_flex, v_flex};
pub use theme::{ActiveTheme, Colors, Fonts, Theme};
pub use tokens::{Elevation, TextSize, radius, spacing};
pub use tooltip::Tooltip;

/// Installs the dark theme. Call once before opening a window.
pub fn init(cx: &mut gpui::App) {
    cx.set_global(Theme::dark());
}

pub mod prelude {
    pub use crate::{
        ActiveTheme, Button, ButtonSize, ButtonStyle, Color, Elevation, Icon, IconButton, IconName,
        Kbd, Label, ListItem, MenuItem, MenuSeparator, PopoverMenu, StyledExt, TextSize, Tooltip,
        h_flex, radius, spacing, v_flex,
    };
}
