//! Keystone's component library: theme, tokens, icons, and the controls the
//! desktop app is built from. Components read the theme from the `App` global
//! that [`init`] installs, so call sites never pass colors or a theme.

pub mod automation;
mod button;
mod color;
pub mod command_palette;
mod dialog;
pub mod fuzzy;
mod icon;
mod kbd;
mod label;
mod list_item;
pub mod markdown;
mod popover;
mod select;
mod slider;
mod split;
mod styled_ext;
mod switch;
mod tabs;
pub mod text_input;
mod theme;
mod tokens;
mod tooltip;
pub mod tree;
pub mod virtual_list;

pub use button::{Button, ButtonSize, ButtonStyle, IconButton};
pub use color::Color;
pub use command_palette::{CommandPalette, CommandPaletteState, PaletteItem};
pub use dialog::{ConfirmDialog, DialogFocus};
pub use icon::{Icon, IconName, Icons};
pub use kbd::Kbd;
pub use label::Label;
pub use list_item::ListItem;
pub use markdown::Markdown;
pub use popover::{MenuHeader, MenuItem, MenuSeparator, PopoverMenu};
pub use select::Select;
pub use slider::Slider;
pub use split::{Edge, PaneEdge, Split, SplitState};
pub use styled_ext::{StyledExt, h_flex, v_flex};
pub use switch::Switch;
pub use tabs::{DraggedTab, Tab, TabBar, TabDot};
pub use text_input::{InputMode, TextInput};
pub use theme::{ActiveTheme, Appearance, Colors, Density, Fonts, Theme};
pub use tokens::{Elevation, TextSize, radius, spacing};
pub use tooltip::Tooltip;
pub use tree::{NodeId, NodeKind, Tree, TreeEntry, TreeModel, TreeState};
pub use virtual_list::{VirtualList, VirtualListState};

/// Installs the dark theme and component key bindings. Call once before opening a window.
pub fn init(cx: &mut gpui::App) {
    cx.set_global(Theme::dark());
    virtual_list::bind_keys(cx);
    command_palette::bind_keys(cx);
    tree::bind_keys(cx);
}

/// Replaces the active theme and redraws every window, including cached views.
pub fn set_theme(theme: Theme, cx: &mut gpui::App) {
    cx.set_global(theme);
    cx.refresh_windows();
}

pub mod prelude {
    pub use crate::automation::AutomationExt;
    pub use crate::text_input::{InputMode, TextInput};
    pub use crate::{
        ActiveTheme, Button, ButtonSize, ButtonStyle, Color, ConfirmDialog, DialogFocus, Elevation,
        Icon, IconButton, IconName, Kbd, Label, ListItem, Markdown, MenuHeader, MenuItem,
        MenuSeparator, PopoverMenu, StyledExt, TextSize, Tooltip, h_flex, radius, spacing, v_flex,
    };
    pub use crate::{CommandPalette, CommandPaletteState, PaletteItem};
    pub use crate::{DraggedTab, Tab, TabBar, TabDot};
    pub use crate::{NodeId, NodeKind, Tree, TreeEntry, TreeModel, TreeState};
    pub use crate::{Select, Slider, Switch};
    pub use crate::{Split, SplitState};
    pub use crate::{VirtualList, VirtualListState};
}
