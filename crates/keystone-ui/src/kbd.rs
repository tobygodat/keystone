use crate::color::Color;
use crate::label::Label;
use crate::tokens::TextSize;
use gpui::{App, IntoElement, RenderOnce, SharedString, Window};

/// A keyboard shortcut hint, such as `Ctrl+N`.
#[derive(IntoElement)]
pub struct Kbd {
    keys: SharedString,
}

impl Kbd {
    pub fn new(keys: impl Into<SharedString>) -> Self {
        Self { keys: keys.into() }
    }
}

impl RenderOnce for Kbd {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        Label::new(self.keys).size(TextSize::Sm).color(Color::Faint)
    }
}
