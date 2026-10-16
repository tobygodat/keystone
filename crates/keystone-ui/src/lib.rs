//! Keystone's component library: theme, tokens, icons, and the controls the
//! desktop app is built from. Components read the theme from the `App` global
//! that [`init`] installs, so call sites never pass colors or a theme.

pub mod automation;
mod button;
mod color;
mod icon;
mod kbd;
mod label;
mod list_item;
pub mod markdown;
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
pub use markdown::Markdown;
pub use popover::{MenuHeader, MenuItem, MenuSeparator, PopoverMenu};
pub use styled_ext::{StyledExt, h_flex, v_flex};
pub use theme::{ActiveTheme, Appearance, Colors, Density, Fonts, Theme};
pub use tokens::{Elevation, TextSize, radius, spacing};
pub use tooltip::Tooltip;

/// Installs the dark theme. Call once before opening a window.
pub fn init(cx: &mut gpui::App) {
    cx.set_global(Theme::dark());
}

/// Replaces the active theme and redraws every window, including cached views.
pub fn set_theme(theme: Theme, cx: &mut gpui::App) {
    cx.set_global(theme);
    cx.refresh_windows();
}

pub mod prelude {
    pub use crate::automation::AutomationExt;
    pub use crate::{
        ActiveTheme, Button, ButtonSize, ButtonStyle, Color, Elevation, Icon, IconButton, IconName,
        Kbd, Label, ListItem, Markdown, MenuHeader, MenuItem, MenuSeparator, PopoverMenu,
        StyledExt, TextSize, Tooltip, h_flex, radius, spacing, v_flex,
    };
}
