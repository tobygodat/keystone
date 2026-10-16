use crate::theme::ActiveTheme;
use gpui::{App, Hsla};

/// Semantic text and icon colors. Resolved against the active theme at render time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Color {
    #[default]
    Default,
    /// The brightest text, for selected titles, headings, and draft text.
    Strong,
    Muted,
    Faint,
    Placeholder,
    Accent,
    Danger,
    Success,
    Warning,
    Custom(Hsla),
}

impl Color {
    pub fn to_hsla(self, cx: &App) -> Hsla {
        let colors = &cx.theme().colors;
        match self {
            Color::Default => colors.text,
            Color::Strong => colors.text_strong,
            Color::Muted => colors.text_muted,
            Color::Faint => colors.text_faint,
            Color::Placeholder => colors.text_placeholder,
            Color::Accent => colors.accent,
            Color::Danger => colors.danger,
            Color::Success => colors.success,
            Color::Warning => colors.warning,
            Color::Custom(color) => color,
        }
    }
}
