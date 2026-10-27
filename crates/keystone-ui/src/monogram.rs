use crate::color::Color;
use crate::label::Label;
use crate::theme::ActiveTheme;
use crate::tokens::TextSize;
use gpui::{
    App, FontWeight, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div, px,
};
use unicode_segmentation::UnicodeSegmentation;

/// A name's first letter or digit on one of the theme's tile colors.
#[derive(Clone, Debug, IntoElement, PartialEq)]
pub struct Monogram {
    letter: SharedString,
    tint: usize,
}

impl Monogram {
    /// `tint` indexes [`crate::Colors::tiles`].
    pub fn new(name: &str, tint: usize) -> Self {
        let letter = name
            .graphemes(true)
            .find(|g| g.starts_with(char::is_alphanumeric))
            .or_else(|| name.graphemes(true).next())
            .unwrap_or_default();
        let mut chars = letter.chars();
        let letter: String = chars
            .next()
            .map(|first| first.to_uppercase().next().unwrap_or(first))
            .into_iter()
            .chain(chars)
            .collect();
        Self {
            letter: letter.into(),
            tint,
        }
    }
}

impl RenderOnce for Monogram {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let tiles = cx.theme().colors.tiles;
        let tint = tiles[self.tint % tiles.len()];
        div()
            .flex_none()
            .size(px(18.))
            .rounded(px(5.))
            .bg(tint.fill)
            .flex()
            .items_center()
            .justify_center()
            .child(
                Label::new(self.letter)
                    .size(TextSize::Xs)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Custom(tint.text)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::Monogram;

    #[test]
    fn the_letter_is_the_first_letter_or_digit() {
        let letter = |name: &str| Monogram::new(name, 0).letter.to_string();
        assert_eq!(letter("keystone"), "K");
        assert_eq!(letter(".dotfiles"), "D");
        assert_eq!(letter("_2026-plans"), "2");
        assert_eq!(letter("élan"), "É");
        assert_eq!(
            letter("e\u{301}lan"),
            "E\u{301}",
            "a decomposed name keeps its accent"
        );
        assert_eq!(letter("ßeta"), "S", "one letter on the tile");
        assert_eq!(letter("/"), "/");
    }
}
