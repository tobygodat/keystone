//! Found text drawn in place: every match on a quiet wash, the current one stronger.

use crate::ActiveTheme;
use gpui::{App, HighlightStyle, Hsla, SharedString, StyledText, TextLayout, TextRun};
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

/// Where the current match is drawn: its text's layout and the match's first byte, set
/// as the text is drawn, so the host can scroll the match into view once laid out.
pub type MarkSlot = Rc<RefCell<Option<(TextLayout, usize)>>>;

/// A match in one of a row's texts, by the text's place among them, and whether it is
/// the current match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub text: usize,
    pub range: Range<usize>,
    pub current: bool,
}

fn wash(current: bool, cx: &App) -> Hsla {
    let colors = cx.theme().colors;
    if current {
        colors.find_current
    } else {
        colors.find_match
    }
}

/// `text` with the marks of text `index` drawn on it, in the surrounding text style.
pub fn marked_text(
    text: impl Into<SharedString>,
    index: usize,
    marks: &[Mark],
    slot: Option<&MarkSlot>,
    cx: &App,
) -> StyledText {
    let text = text.into();
    let len = text.len();
    let highlights: Vec<(Range<usize>, HighlightStyle)> = marks
        .iter()
        .filter(|mark| mark.text == index && mark.range.end <= text.len())
        .map(|mark| {
            (
                mark.range.clone(),
                HighlightStyle {
                    background_color: Some(wash(mark.current, cx)),
                    ..Default::default()
                },
            )
        })
        .collect();
    let styled = StyledText::new(text).with_highlights(highlights);
    record(styled.layout(), index, marks, len, slot);
    styled
}

/// Records `layout` in `slot` when text `index` holds the current match.
pub(crate) fn record(
    layout: &TextLayout,
    index: usize,
    marks: &[Mark],
    len: usize,
    slot: Option<&MarkSlot>,
) {
    if let Some(slot) = slot
        && let Some(mark) = marks
            .iter()
            .find(|mark| mark.current && mark.text == index && mark.range.end <= len)
    {
        *slot.borrow_mut() = Some((layout.clone(), mark.range.start));
    }
}

/// Splits `runs` so the bytes of each mark of text `index` carry its wash.
pub(crate) fn apply(runs: Vec<TextRun>, index: usize, marks: &[Mark], cx: &App) -> Vec<TextRun> {
    let len: usize = runs.iter().map(|run| run.len).sum();
    // A streaming reply draws less than its text so far, so marks past the end wait.
    let mut marks: Vec<&Mark> = marks
        .iter()
        .filter(|mark| mark.text == index && mark.range.end <= len)
        .collect();
    if marks.is_empty() {
        return runs;
    }
    marks.sort_by_key(|mark| mark.range.start);
    let mut out = Vec::with_capacity(runs.len() + marks.len() * 2);
    let mut at = 0;
    for run in runs {
        let limit = at + run.len;
        let mut cuts = vec![at, limit];
        for mark in &marks {
            for cut in [mark.range.start, mark.range.end] {
                if at < cut && cut < limit {
                    cuts.push(cut);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let mut piece = run.clone();
            piece.len = pair[1] - pair[0];
            if let Some(mark) = marks
                .iter()
                .find(|mark| mark.range.start <= pair[0] && pair[1] <= mark.range.end)
            {
                piece.background_color = Some(wash(mark.current, cx));
            }
            out.push(piece);
        }
        at = limit;
    }
    out
}
