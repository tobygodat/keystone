use crate::{ActiveTheme, Button, ButtonSize, ButtonStyle, h_flex, radius};
use gpui::{App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, px};

/// Mutually exclusive choices on one track, such as a theme or a density. The caller
/// builds each segment as a [`Button`], marks the chosen one `selected` and owns the
/// value.
#[derive(IntoElement)]
pub struct SegmentedControl {
    id: SharedString,
    segments: Vec<Button>,
}

impl SegmentedControl {
    pub fn new(id: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            segments: Vec::new(),
        }
    }

    pub fn segment(mut self, segment: Button) -> Self {
        self.segments.push(segment);
        self
    }

    pub fn segments(mut self, segments: impl IntoIterator<Item = Button>) -> Self {
        self.segments.extend(segments);
        self
    }
}

impl RenderOnce for SegmentedControl {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let track = h_flex()
            .flex_shrink_0()
            .p(px(2.))
            .gap(px(2.))
            .rounded(radius::ROW)
            .bg(cx.theme().colors.element_hover)
            .children(self.segments.into_iter().map(|segment| {
                segment
                    .style(ButtonStyle::Ghost)
                    .size(ButtonSize::Default)
                    .keyboard_focus_ring()
            }));
        crate::automation::tag(track, self.id.to_string())
    }
}
