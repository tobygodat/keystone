//! Maps saved preferences onto the keystone-ui theme.

use gpui::{App, Window, WindowAppearance};
use keystone_core::{
    ActionColor as SavedActionColor, Appearance as SavedAppearance, Density as SavedDensity,
    ThemeMode,
};
use keystone_ui::{ActionColor, Appearance, Density, Fonts, Theme};

/// The interface's base text size in pixels when none is saved, `TextSize::Base` at a
/// text scale of 1.
pub const DEFAULT_TEXT_SIZE: u8 = 13;

/// The text sizes the settings page offers, in pixels.
pub const TEXT_SIZES: std::ops::RangeInclusive<u8> = 11..=16;

/// The transcript text sizes the settings page offers, in pixels.
pub const TRANSCRIPT_SIZES: std::ops::RangeInclusive<u8> = 12..=20;

/// The action colors the settings page offers, saved and shown, in display order.
pub const ACTION_COLORS: [(SavedActionColor, ActionColor); 6] = [
    (SavedActionColor::Ink, ActionColor::Ink),
    (SavedActionColor::Blue, ActionColor::Blue),
    (SavedActionColor::Green, ActionColor::Green),
    (SavedActionColor::Orange, ActionColor::Orange),
    (SavedActionColor::Purple, ActionColor::Purple),
    (SavedActionColor::Pink, ActionColor::Pink),
];

/// Interface fonts offered in the settings page when they are installed. The default,
/// the bundled Inter, is listed separately as "Default".
const FONT_CANDIDATES: [&str; 8] = [
    "Adwaita Sans",
    "Cantarell",
    "DejaVu Sans",
    "IBM Plex Sans",
    "Liberation Sans",
    "Noto Sans",
    "Roboto",
    "Ubuntu",
];

/// Monospace fonts for code, in order of preference. The first installed one is used, so
/// code stays monospace when the default is missing; a proportional fallback misaligns it.
const MONO_CANDIDATES: [&str; 8] = [
    "GeistMono Nerd Font Mono",
    "JetBrainsMono Nerd Font",
    "JetBrains Mono",
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "Adwaita Mono",
    // Every Mac has it.
    "Menlo",
];

pub fn theme_for(saved: &SavedAppearance, window: &Window, cx: &App) -> Theme {
    let appearance = match saved.theme {
        ThemeMode::Light => Appearance::Light,
        ThemeMode::Dark => Appearance::Dark,
        ThemeMode::System => match window.appearance() {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
        },
    };
    let density = match saved.density {
        SavedDensity::Compact => Density::Compact,
        SavedDensity::Default => Density::Default,
        SavedDensity::Spacious => Density::Spacious,
    };
    let mut fonts = Fonts::default();
    if let Some(font) = &saved.ui_font {
        fonts = fonts.with_ui(font.clone());
    }
    if let Some(font) = &saved.mono_font {
        fonts.mono = font.clone().into();
    } else {
        let installed = cx.text_system().all_font_names();
        if let Some(mono) = MONO_CANDIDATES
            .iter()
            .find(|font| installed.iter().any(|name| name == *font))
        {
            fonts.mono = (*mono).into();
        }
    }
    let action = ACTION_COLORS
        .iter()
        .find(|(saved_color, _)| *saved_color == saved.action_color)
        .map_or(ActionColor::Ink, |&(_, shown)| shown);
    let mut theme = Theme::new(appearance, density, fonts).with_action(action);
    theme.text_scale =
        f32::from(saved.text_size.unwrap_or(DEFAULT_TEXT_SIZE)) / f32::from(DEFAULT_TEXT_SIZE);
    theme.transcript_size = saved.transcript_size.map(|size| gpui::px(f32::from(size)));
    theme
}

/// The candidate fonts that are installed, plus a saved choice that is no longer listed,
/// so the pane always shows what is in use.
pub fn font_choices(saved: &SavedAppearance, cx: &App) -> Vec<String> {
    installed_choices(&FONT_CANDIDATES, saved.ui_font.as_ref(), cx)
}

/// [`font_choices`] for the code font.
pub fn mono_choices(saved: &SavedAppearance, cx: &App) -> Vec<String> {
    installed_choices(&MONO_CANDIDATES, saved.mono_font.as_ref(), cx)
}

fn installed_choices(candidates: &[&str], saved: Option<&String>, cx: &App) -> Vec<String> {
    let installed = cx.text_system().all_font_names();
    let mut choices: Vec<String> = candidates
        .iter()
        .filter(|font| installed.iter().any(|name| name == *font))
        .map(|font| font.to_string())
        .collect();
    if let Some(font) = saved
        && !choices.contains(font)
    {
        choices.push(font.clone());
    }
    choices
}
