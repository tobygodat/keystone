//! Maps saved preferences onto the keystone-ui theme.

use gpui::{App, Window, WindowAppearance};
use keystone_core::{Density as SavedDensity, Preferences, ThemeMode};
use keystone_ui::{Appearance, Density, Fonts, Theme};

/// Interface fonts offered in the settings menu when they are installed. The default,
/// Inter, is listed separately as "Default".
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
const MONO_CANDIDATES: [&str; 7] = [
    "GeistMono Nerd Font Mono",
    "JetBrainsMono Nerd Font",
    "JetBrains Mono",
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "Adwaita Mono",
];

pub fn theme_for(preferences: &Preferences, window: &Window, cx: &App) -> Theme {
    let appearance = match preferences.theme {
        ThemeMode::Light => Appearance::Light,
        ThemeMode::Dark => Appearance::Dark,
        ThemeMode::System => match window.appearance() {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
        },
    };
    let density = match preferences.density {
        SavedDensity::Compact => Density::Compact,
        SavedDensity::Default => Density::Default,
        SavedDensity::Spacious => Density::Spacious,
    };
    let mut fonts = Fonts::default();
    if let Some(font) = &preferences.ui_font {
        fonts.ui = font.clone().into();
    }
    let installed = cx.text_system().all_font_names();
    if let Some(mono) = MONO_CANDIDATES
        .iter()
        .find(|font| installed.iter().any(|name| name == *font))
    {
        fonts.mono = (*mono).into();
    }
    Theme::new(appearance, density, fonts)
}

/// The candidate fonts that are installed, plus a saved choice that is no longer listed,
/// so the menu always shows what is in use.
pub fn font_choices(preferences: &Preferences, cx: &App) -> Vec<String> {
    let installed = cx.text_system().all_font_names();
    let mut choices: Vec<String> = FONT_CANDIDATES
        .iter()
        .filter(|font| installed.iter().any(|name| name == *font))
        .map(|font| font.to_string())
        .collect();
    if let Some(saved) = &preferences.ui_font
        && !choices.contains(saved)
    {
        choices.push(saved.clone());
    }
    choices
}
