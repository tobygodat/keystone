//! A pane's text input: the characters a keyboard layout types, dead keys, and input
//! methods that compose text, such as for Chinese, Japanese or Korean. The platform
//! hands a focused pane the text a key types once the pane's key handler passes it on;
//! see [`super::input::bytes`].

use super::SessionTerminal;
use gpui::{App, Bounds, InputHandler, Pixels, UTF16Selection, WeakEntity, Window, point};
use std::ops::Range;

/// The input handler a focused pane registers as it paints. The composition, which
/// the platform calls marked text, is all the text it has: what the shell has is
/// beyond its reach, so the selection is the composition's end.
pub(crate) struct PaneInput {
    pub terminal: WeakEntity<SessionTerminal>,
    pub pane: u64,
}

impl PaneInput {
    fn marked(&self, cx: &App) -> Option<String> {
        self.terminal
            .upgrade()?
            .read(cx)
            .pane(self.pane)?
            .marked
            .clone()
    }

    fn marked_len(&self, cx: &App) -> Option<usize> {
        self.marked(cx).map(|text| text.encode_utf16().count())
    }

    fn update(
        &self,
        cx: &mut App,
        f: impl FnOnce(&mut SessionTerminal, u64, &mut gpui::Context<SessionTerminal>),
    ) {
        if let Some(terminal) = self.terminal.upgrade() {
            terminal.update(cx, |terminal, cx| f(terminal, self.pane, cx));
        }
    }
}

impl InputHandler for PaneInput {
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        let end = self.marked_len(cx).unwrap_or(0);
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.marked_len(cx).map(|len| 0..len)
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        let marked: Vec<u16> = self.marked(cx)?.encode_utf16().collect();
        let range = range.start.min(marked.len())..range.end.min(marked.len());
        *adjusted = Some(range.clone());
        String::from_utf16(&marked[range]).ok()
    }

    /// Text the layout or an input method typed. It replaces the composition, if any,
    /// and goes to the shell.
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut App,
    ) {
        self.update(cx, |terminal, pane, cx| {
            terminal.commit_text(pane, text, cx)
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut App,
    ) {
        self.update(cx, |terminal, pane, cx| terminal.mark_text(pane, text, cx));
    }

    /// The composition is accepted as it stands, as AppKit means it.
    fn unmark_text(&mut self, _: &mut Window, cx: &mut App) {
        if let Some(text) = self.marked(cx) {
            self.update(cx, |terminal, pane, cx| {
                terminal.commit_text(pane, &text, cx)
            });
        }
    }

    /// Where the composition's text sits, from the cursor, so the input method's
    /// candidate window opens beside it.
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let terminal = self.terminal.upgrade()?;
        let terminal = terminal.read(cx);
        let pane = terminal.pane(self.pane)?;
        let view = pane.view(window.window_handle().window_id())?;
        let cell = pane.size().cell;
        let (cursor, offset) = {
            let term = pane.term.lock();
            (term.grid().cursor.point, term.grid().display_offset())
        };
        let column = cursor.column.0 + range.start;
        let row = cursor.line.0 + offset as i32;
        let origin =
            view.bounds.origin + point(cell.width * column as f32, cell.height * row as f32);
        Some(Bounds::new(origin, cell))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<usize> {
        None
    }

    /// A held key repeats, as in other terminals, rather than offering accents.
    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}
