use crate::color::Color;
use crate::label::Label;
use crate::styled_ext::{StyledExt, h_flex};
use crate::tokens::{Elevation, TextSize, radius};
use gpui::{
    AnyView, App, Context, IntoElement, Render, SharedString, Styled, Window, div, prelude::*, px,
};

/// A small hover card with a title and an optional shortcut.
#[derive(Clone)]
pub struct Tooltip {
    title: SharedString,
    key: Option<SharedString>,
}

impl Tooltip {
    pub fn text(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            key: None,
        }
    }

    pub fn with_key(title: impl Into<SharedString>, key: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            key: Some(key.into()),
        }
    }

    /// The builder `InteractiveElement::tooltip` expects.
    pub fn builder(self) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        move |_, cx| cx.new(|_| self.clone()).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The margin keeps the card clear of the pointer. GPUI draws tooltips as their own
        // root, so they do not inherit the window root's font.
        div().pl_2().pt_2().font_ui(cx).child(
            h_flex()
                .max_w(px(320.))
                .gap_2()
                .px(px(8.))
                .py(px(5.))
                .surface(Elevation::Overlay, cx)
                .rounded(radius::CONTROL)
                .child(
                    div()
                        .min_w_0()
                        .whitespace_normal()
                        .child(Label::new(self.title.clone()).size(TextSize::Sm)),
                )
                .when_some(self.key.clone(), |d, key| {
                    d.child(Label::new(key).size(TextSize::Sm).color(Color::Faint))
                }),
        )
    }
}
