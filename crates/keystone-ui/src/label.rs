use crate::color::Color;
use crate::styled_ext::StyledExt;
use crate::theme::ActiveTheme;
use crate::tokens::TextSize;
use gpui::{
    App, AvailableSpace, Bounds, Element, ElementId, FontWeight, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, RenderOnce, ShapedLine, SharedString, Style,
    Styled, TextAlign, TextRun, Window, div, prelude::*, px, size,
};

#[derive(IntoElement)]
pub struct Label {
    text: SharedString,
    size: TextSize,
    color: Color,
    weight: FontWeight,
    truncate: bool,
    mono: bool,
    tabular: bool,
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
            tabular: false,
        }
    }

    /// A group's small-caps label with its count, such as "NEEDS ACTION · 3", for the
    /// sidebar's sections and the side panels' groups.
    pub fn group(name: &str, count: usize) -> Self {
        Self::new(format!("{name} · {count}").to_uppercase())
            .size(TextSize::Xs)
            .weight(FontWeight::MEDIUM)
            .color(Color::Faint)
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

    /// Keeps to one line and ends text that does not fit with "…".
    pub fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    /// Equal-width figures, for numbers that line up in a column.
    pub fn tabular(mut self) -> Self {
        self.tabular = true;
        self
    }
}

impl RenderOnce for Label {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .text_size(self.size.px())
            .line_height(self.size.line_height())
            .text_color(self.color.to_hsla(cx))
            .font_weight(self.weight)
            .when(
                self.size == TextSize::Title && self.weight >= FontWeight::SEMIBOLD,
                |d| d.font_family(cx.theme().fonts.display.clone()),
            )
            .when(self.tabular, |d| {
                d.font_features(cx.theme().fonts.tabular())
            })
            .when(self.mono, |d| d.font_mono(cx))
            .map(|d| {
                if self.truncate {
                    d.min_w_0()
                        .overflow_hidden()
                        .child(Truncated { text: self.text })
                        .into_any_element()
                } else {
                    d.child(self.text).into_any_element()
                }
            })
    }
}

/// One line of text cut to its final width with a trailing "…". GPUI 0.2.2's
/// `text_ellipsis` reuses the first width it measured whatever width layout settles on
/// (KS-010), so this cuts in prepaint, against the bounds layout actually gave it.
struct Truncated {
    text: SharedString,
}

impl IntoElement for Truncated {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

fn shape(text: SharedString, window: &Window) -> ShapedLine {
    let style = window.text_style();
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color: style.color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let font_size = style.font_size.to_pixels(window.rem_size());
    window
        .text_system()
        .shape_line(text, font_size, &[run], None)
}

/// `line` shortened to end in "…" within `width`, or `line` itself when it fits.
fn fit(line: ShapedLine, width: Pixels, window: &Window) -> ShapedLine {
    if line.width <= width {
        return line;
    }
    let ellipsis = shape("…".into(), window);
    let cut = line
        .index_for_x((width - ellipsis.width).max(px(0.)))
        .unwrap_or(line.len());
    let kept = line.text[..cut].trim_end();
    shape(format!("{kept}…").into(), window)
}

impl Element for Truncated {
    type RequestLayoutState = ();
    type PrepaintState = ShapedLine;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, ()) {
        // Whole pixels, so layout's rounding never leaves text that fits a hair too narrow.
        let full = shape(self.text.clone(), window).width.ceil();
        let height = window.line_height();
        let mut style = Style::default();
        style.min_size.width = px(0.).into();
        let layout = window.request_measured_layout(style, move |known, available, _, _| {
            let width = known.width.unwrap_or(match available.width {
                AvailableSpace::Definite(space) => full.min(space),
                _ => full,
            });
            size(width, height)
        });
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) -> ShapedLine {
        fit(shape(self.text.clone(), window), bounds.size.width, window)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        line: &mut ShapedLine,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _ = line.paint(
            bounds.origin,
            window.line_height(),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod tests {
    use super::*;

    #[gpui::test]
    fn text_that_does_not_fit_ends_in_an_ellipsis(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let window = cx.add_empty_window();
        window.update(|window, _| {
            let title = "Fix persistence durability first";
            let full = shape(title.into(), window);
            assert_eq!(fit(full.clone(), full.width, window).text.as_ref(), title);

            let width = full.width / 2.;
            let cut = fit(full, width, window);
            let kept = cut.text.strip_suffix('…').expect("ends in an ellipsis");
            assert!(!kept.is_empty() && title.starts_with(kept), "{}", cut.text);
            assert!(!kept.ends_with(' '));
            assert!(cut.width <= width, "{:?} > {width:?}", cut.width);

            assert_eq!(
                fit(shape(title.into(), window), px(0.), window)
                    .text
                    .as_ref(),
                "…"
            );
        });
    }
}
