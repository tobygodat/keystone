//! Keystone's component library: theme, tokens, icons, and the controls the
//! desktop app is built from. Components read the theme from the `App` global
//! that [`init`] installs, so call sites never pass colors or a theme.

pub mod automation;
mod button;
pub mod code_editor;
mod color;
pub mod command_palette;
pub mod diagram;
mod dialog;
pub mod fuzzy;
pub mod highlight;
mod icon;
mod kbd;
mod label;
mod list_item;
pub mod markdown;
mod monogram;
mod popover;
mod segmented;
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

pub use button::{Button, ButtonSize, ButtonStyle, IconButton, Swatch};
pub use code_editor::CodeEditor;
pub use color::Color;
pub use command_palette::{CommandPalette, CommandPaletteState, PaletteItem};
pub use dialog::{ConfirmDialog, DialogFocus};
pub use icon::{Icon, IconName, Icons};
pub use kbd::{Kbd, shortcut_text};
pub use label::Label;
pub use list_item::ListItem;
pub use markdown::Markdown;
pub use monogram::Monogram;
pub use popover::{MenuHeader, MenuItem, MenuSeparator, PopoverMenu};
pub use segmented::SegmentedControl;
pub use select::Select;
pub use slider::Slider;
pub use split::{Edge, PaneEdge, Split, SplitState};
pub use styled_ext::{StyledExt, h_flex, v_flex};
pub use switch::Switch;
pub use tabs::{DraggedTab, Tab, TabBar, TabDot};
pub use text_input::{InputMode, TextInput};
pub use theme::{
    ActionColor, ActiveTheme, Appearance, Colors, Density, Fonts, SyntaxColors, TerminalColors,
    Theme, Tint,
};
pub use tokens::{Elevation, TextSize, radius, spacing};
pub use tooltip::Tooltip;
pub use tree::{NodeId, NodeKind, Tree, TreeEntry, TreeModel, TreeState};
pub use virtual_list::{VirtualList, VirtualListState};

/// Inter 4.1, embedded so the default interface font is Inter whether or not it is
/// installed. Provenance and license are in `vendor/inter`.
const BUNDLED_FONTS: [&[u8]; 7] = [
    include_bytes!("../vendor/inter/Inter-Regular.ttf"),
    include_bytes!("../vendor/inter/Inter-Medium.ttf"),
    include_bytes!("../vendor/inter/Inter-SemiBold.ttf"),
    include_bytes!("../vendor/inter/Inter-Italic.ttf"),
    include_bytes!("../vendor/inter/Inter-SemiBoldItalic.ttf"),
    include_bytes!("../vendor/inter/InterDisplay-SemiBold.ttf"),
    include_bytes!("../vendor/inter/InterDisplay-SemiBoldItalic.ttf"),
];

/// Adds the bundled fonts to `text_system`, after the installed ones. Where an installed
/// Inter has a face of the same weight and style, that face wins; the bundle fills the
/// rest. The text system skips data it cannot parse without an error, so the unit test
/// below is what guards the bytes.
pub fn register_fonts(text_system: &gpui::TextSystem) -> anyhow::Result<()> {
    text_system.add_fonts(
        BUNDLED_FONTS
            .iter()
            .map(|font| std::borrow::Cow::Borrowed(*font))
            .collect(),
    )
}

/// Registers the bundled fonts and installs the dark theme and component key bindings.
/// Call once before opening a window.
pub fn init(cx: &mut gpui::App) {
    register_fonts(cx.text_system()).expect("the bundled fonts load");
    set_theme(Theme::dark(), cx);
    bind_keys(cx);
}

/// Binds the components' own keys. A host that clears the keymap to rebind it calls
/// this again.
pub fn bind_keys(cx: &mut gpui::App) {
    virtual_list::bind_keys(cx);
    command_palette::bind_keys(cx);
    select::bind_keys(cx);
    tree::bind_keys(cx);
    code_editor::bind_keys(cx);
    markdown::selection::bind_keys(cx);
}

/// Replaces the active theme and redraws every window, including cached views.
pub fn set_theme(theme: Theme, cx: &mut gpui::App) {
    tokens::set_text_scale(theme.text_scale);
    cx.set_global(theme);
    cx.refresh_windows();
}

pub mod prelude {
    pub use crate::automation::AutomationExt;
    pub use crate::text_input::{InputMode, TextInput};
    pub use crate::{
        ActiveTheme, Button, ButtonSize, ButtonStyle, CodeEditor, Color, ConfirmDialog,
        DialogFocus, Elevation, Icon, IconButton, IconName, Kbd, Label, ListItem, Markdown,
        MenuHeader, MenuItem, MenuSeparator, Monogram, PopoverMenu, StyledExt, Swatch, TextSize,
        Tooltip, h_flex, radius, spacing, v_flex,
    };
    pub use crate::{CommandPalette, CommandPaletteState, PaletteItem};
    pub use crate::{DraggedTab, Tab, TabBar, TabDot};
    pub use crate::{NodeId, NodeKind, Tree, TreeEntry, TreeModel, TreeState};
    pub use crate::{SegmentedControl, Select, Slider, Switch};
    pub use crate::{Split, SplitState};
    pub use crate::{VirtualList, VirtualListState};
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{FontStyle, FontWeight, PlatformTextSystem, font, px};
    use gpui_wgpu::CosmicTextSystem;
    use std::sync::Arc;

    #[test]
    fn bundled_inter_resolves_when_no_font_is_installed() {
        let platform = Arc::new(CosmicTextSystem::new_without_system_fonts("Inter"));
        let text_system = gpui::TextSystem::new(platform.clone());
        assert!(text_system.all_font_names().is_empty());
        register_fonts(&text_system).unwrap();
        let names = text_system.all_font_names();
        assert_eq!(names, ["Inter", "Inter Display"]);

        let fonts = Fonts::default();
        let (ui, display) = (&fonts.ui, &fonts.display);
        for (family, weight, style, expected) in [
            (ui, FontWeight::NORMAL, FontStyle::Normal, 400.),
            (ui, FontWeight::MEDIUM, FontStyle::Normal, 500.),
            (ui, FontWeight::SEMIBOLD, FontStyle::Normal, 600.),
            (ui, FontWeight::BOLD, FontStyle::Normal, 600.),
            (ui, FontWeight::NORMAL, FontStyle::Italic, 400.),
            (ui, FontWeight::SEMIBOLD, FontStyle::Italic, 600.),
            (display, FontWeight::SEMIBOLD, FontStyle::Normal, 600.),
            (display, FontWeight::SEMIBOLD, FontStyle::Italic, 600.),
        ] {
            let mut requested = font(family.clone());
            requested.weight = weight;
            requested.style = style;
            requested.features = fonts.features.clone();
            let id = platform.font_id(&requested).unwrap();
            let (found, found_style) = platform.font_weight_and_style(id).unwrap();
            assert_eq!(found, FontWeight(expected), "{family} {weight:?} {style:?}");
            assert_eq!(found_style, style, "{family} {weight:?} {style:?}");
        }

        let shape = |text: &str, features: gpui::FontFeatures| {
            let mut requested = font(fonts.ui.clone());
            requested.features = features;
            let id = platform.font_id(&requested).unwrap();
            let run = gpui::FontRun {
                len: text.len(),
                font_id: id,
            };
            let layout = platform.layout_line(text, px(13.), &[run]);
            let glyphs = &layout.runs[0].glyphs;
            let ids: Vec<_> = glyphs.iter().map(|glyph| glyph.id).collect();
            let advances: Vec<_> = glyphs
                .windows(2)
                .map(|pair| pair[1].position.x - pair[0].position.x)
                .collect();
            (ids, advances)
        };
        let plain = |text| shape(text, Default::default()).0;
        let inter = |text| shape(text, fonts.features.clone()).0;
        assert_ne!(inter("1"), plain("1"), "cv01 reached the shaper");
        assert_ne!(inter(","), plain(","), "ss03 reached the shaper");
        assert_ne!(
            shape("1", fonts.tabular()).0,
            inter("1"),
            "tnum reached the shaper"
        );
        let (_, proportional) = shape("1101", fonts.features.clone());
        let (_, tabular) = shape("1101", fonts.tabular());
        assert_ne!(proportional[1], proportional[2], "{proportional:?}");
        assert!(tabular.iter().all(|w| *w == tabular[0]), "{tabular:?}");
    }
}
