use crate::color::Color;
use crate::theme::ActiveTheme;
use crate::tokens::TextSize;
use gpui::{
    App, FontWeight, IntoElement, RenderOnce, SharedString, Styled, Window, div, prelude::*,
};

#[derive(IntoElement)]
pub struct Label {
    text: SharedString,
    size: TextSize,
    color: Color,
    weight: FontWeight,
    truncate: bool,
    mono: bool,
}

impl Label {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self {
            text: text.into(),
            size: TextSize::default(),
            color: Color::default(),
            weight: FontWeight::NORMAL,
            truncate: false,
            mono: false,
        }
    }

    pub fn size(mut self, size: TextSize) -> Self {
        self.size = size;
        self
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    pub fn weight(mut self, weight: FontWeight) -> Self {
        self.weight = weight;
        self
    }

    /// Clips to one line with an ellipsis. The parent must constrain the width.
    pub fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }
}

impl RenderOnce for Label {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .text_size(self.size.px())
            .text_color(self.color.to_hsla(cx))
            .font_weight(self.weight)
            .when(self.mono, |d| d.font_family(cx.theme().fonts.mono.clone()))
            .when(self.truncate, |d| d.w_full().min_w_0().truncate())
            .child(self.text)
    }
}
