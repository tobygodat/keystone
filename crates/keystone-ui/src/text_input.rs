use std::ops::Range;

use crate::{ActiveTheme, Color, TextSize};
use gpui::{
    App, AvailableSpace, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, Hsla, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ScrollWheelEvent, SharedString, Style,
    TextRun, TextStyle, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill,
    point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

actions!(
    keystone_text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Copy,
        Cut,
        Newline,
        Up,
        Down,
        SelectUp,
        SelectDown,
        SelectHome,
        SelectEnd,
        DocumentHome,
        DocumentEnd
    ]
);

pub struct Changed;
pub struct RejectedMultiline;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    SingleLine,
    Multiline,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CaretAffinity {
    BeforeWrap,
    #[default]
    AfterWrap,
}

#[derive(Clone)]
struct VisualRow {
    bytes: Range<usize>,
    paragraph: usize,
    x: Pixels,
    top: Pixels,
}

#[derive(Clone)]
struct Paragraph {
    start: usize,
    top: Pixels,
    line: std::rc::Rc<WrappedLine>,
}

#[derive(Clone)]
struct TextLayout {
    paragraphs: Vec<Paragraph>,
    rows: Vec<VisualRow>,
    line_height: Pixels,
}

impl TextLayout {
    fn height(&self) -> Pixels {
        self.line_height * self.rows.len()
    }

    fn row_for_offset(&self, offset: usize, affinity: CaretAffinity) -> usize {
        let mut rows = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.bytes.contains(&offset) || row.bytes.end == offset);
        let first = rows
            .next()
            .map(|(index, _)| index)
            .unwrap_or(self.rows.len() - 1);
        if affinity == CaretAffinity::AfterWrap {
            rows.last().map(|(index, _)| index).unwrap_or(first)
        } else {
            first
        }
    }

    fn x_for_offset(&self, row: &VisualRow, offset: usize) -> Pixels {
        let paragraph = &self.paragraphs[row.paragraph];
        paragraph
            .line
            .unwrapped_layout
            .x_for_index(offset - paragraph.start)
            - row.x
    }

    fn position(&self, offset: usize, affinity: CaretAffinity) -> Point<Pixels> {
        let row = &self.rows[self.row_for_offset(offset, affinity)];
        point(self.x_for_offset(row, offset), row.top)
    }

    fn hit_test(&self, position: Point<Pixels>) -> (usize, CaretAffinity) {
        let row_index =
            ((position.y.max(px(0.)) / self.line_height) as usize).min(self.rows.len() - 1);
        let row = &self.rows[row_index];
        let paragraph = &self.paragraphs[row.paragraph];
        let index = paragraph
            .line
            .unwrapped_layout
            .closest_index_for_x(position.x + row.x)
            + paragraph.start;
        let index = index.clamp(row.bytes.start, row.bytes.end);
        let affinity = if index == row.bytes.end {
            CaretAffinity::BeforeWrap
        } else {
            CaretAffinity::AfterWrap
        };
        (index, affinity)
    }

    fn range_bounds(&self, range: Range<usize>, width: Pixels) -> Vec<Bounds<Pixels>> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let start = range.start.max(row.bytes.start);
                let end = range.end.min(row.bytes.end);
                let newline = range.start <= row.bytes.end
                    && range.end > row.bytes.end
                    && self
                        .rows
                        .get(index + 1)
                        .is_some_and(|next| next.bytes.start > row.bytes.end);
                if start >= end && !newline {
                    return None;
                }
                let left = self.x_for_offset(row, start.min(row.bytes.end));
                let right = if newline {
                    (self.x_for_offset(row, row.bytes.end) + px(5.)).min(width)
                } else {
                    self.x_for_offset(row, end)
                };
                Some(Bounds::new(
                    point(left, row.top),
                    size((right - left).max(px(1.)), self.line_height),
                ))
            })
            .collect()
    }
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    /// Ranges of the content that read and delete as one unit, in order.
    tokens: Vec<Range<usize>>,
    mode: InputMode,
    affinity: CaretAffinity,
    desired_x: Option<Pixels>,
    reveal_cursor: bool,
    last_layout: Option<TextLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    scroll_offset: Pixels,
    is_selecting: bool,
    text_size: TextSize,
    text_color: Color,
    fill: bool,
}

impl EventEmitter<Changed> for TextInput {}
impl EventEmitter<RejectedMultiline> for TextInput {}

impl TextInput {
    pub fn new(placeholder: &str, mode: InputMode, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.to_owned().into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            tokens: Vec::new(),
            mode,
            affinity: CaretAffinity::AfterWrap,
            desired_x: None,
            reveal_cursor: true,
            last_layout: None,
            last_bounds: None,
            scroll_offset: px(0.),
            is_selecting: false,
            text_size: TextSize::Base,
            text_color: Color::Default,
            fill: false,
        }
    }

    /// A multiline field that fills its container's height and scrolls inside it,
    /// instead of growing with its text up to eight rows.
    pub fn fill_height(mut self) -> Self {
        self.fill = true;
        self
    }

    pub fn text_style(mut self, size: TextSize, color: Color) -> Self {
        self.text_size = size;
        self.text_color = color;
        self
    }

    pub fn value(&self) -> &str {
        &self.content
    }

    /// The selection in bytes of `value`; empty at the cursor when nothing is selected.
    pub fn selected_range(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    pub fn set_value(&mut self, value: String, cx: &mut Context<Self>) {
        let end = value.len();
        self.content = value.into();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.tokens.clear();
        self.last_layout = None;
        self.scroll_offset = px(0.);
        self.reveal_cursor = true;
        self.desired_x = None;
        self.affinity = CaretAffinity::AfterWrap;
        cx.notify();
    }

    /// The caret's byte offset in the content.
    pub fn cursor(&self) -> usize {
        self.cursor_offset()
    }

    /// The tokens in the content, in order. Backspace at a token's end and Delete at
    /// its start remove it whole, and an edit inside it or against its edges makes it
    /// plain text again.
    pub fn tokens(&self) -> &[Range<usize>] {
        &self.tokens
    }

    /// Makes `tokens` the content's tokens. A range that is empty, overlaps an earlier
    /// one, or does not fall on character boundaries with whitespace or an end on each
    /// side is left out.
    pub fn set_tokens(
        &mut self,
        tokens: impl IntoIterator<Item = Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        self.tokens.clear();
        for token in tokens {
            if self
                .tokens
                .last()
                .is_none_or(|last| last.end <= token.start)
                && self.stands_alone(&token)
            {
                self.tokens.push(token);
            }
        }
        cx.notify();
    }

    /// Replaces `range` with `token` as one token, with a space before it when other
    /// text would touch it and a space after it unless one follows already, and puts
    /// the caret after that space. An empty `range` inside a token moves to its end,
    /// so the token stays whole. Like [`Self::set_value`], it emits no [`Changed`].
    pub fn insert_token(&mut self, range: Range<usize>, token: &str, cx: &mut Context<Self>) {
        let range = match self
            .tokens
            .iter()
            .find(|t| range.is_empty() && t.start < range.start && range.start < t.end)
        {
            Some(inside) => inside.end..inside.end,
            None => range,
        };
        let before = self.content[..range.start]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace());
        let after = self.content[range.end..].starts_with(' ');
        let start = range.start + usize::from(before);
        let inserted = format!(
            "{}{token}{}",
            if before { " " } else { "" },
            if after { "" } else { " " }
        );
        let caret = range.start + inserted.len() + usize::from(after);
        self.content = format!(
            "{}{inserted}{}",
            &self.content[..range.start],
            &self.content[range.end..]
        )
        .into();
        self.shift_tokens(range, inserted.len());
        if self.stands_alone(&(start..start + token.len())) {
            let at = self.tokens.partition_point(|t| t.end <= start);
            self.tokens.insert(at, start..start + token.len());
        }
        self.marked_range = None;
        self.last_layout = None;
        self.move_to(caret, cx);
    }

    /// Whether `range` is non-empty, on character boundaries, and has whitespace or an
    /// end of the content on each side.
    fn stands_alone(&self, range: &Range<usize>) -> bool {
        let text = self.content.as_ref();
        range.start < range.end
            && range.end <= text.len()
            && text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end)
            && text[..range.start]
                .chars()
                .next_back()
                .is_none_or(char::is_whitespace)
            && text[range.end..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
    }

    /// Updates the tokens for `edited`, a range of the old content now replaced by
    /// `inserted` bytes: later tokens move, and any it touched or glued to other text
    /// stop being tokens.
    fn shift_tokens(&mut self, edited: Range<usize>, inserted: usize) {
        let mut tokens = std::mem::take(&mut self.tokens);
        tokens.retain_mut(|token| {
            if edited.end <= token.start {
                token.start = token.start - edited.end + edited.start + inserted;
                token.end = token.end - edited.end + edited.start + inserted;
                true
            } else {
                edited.start >= token.end
            }
        });
        tokens.retain(|token| self.stands_alone(token));
        self.tokens = tokens;
    }

    pub fn bind_keys(cx: &mut App) {
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, Some("TextInput")),
            KeyBinding::new("delete", Delete, Some("TextInput")),
            KeyBinding::new("left", Left, Some("TextInput")),
            KeyBinding::new("right", Right, Some("TextInput")),
            KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
            KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
            KeyBinding::new("cmd-a", SelectAll, Some("TextInput")),
            KeyBinding::new("secondary-a", SelectAll, Some("TextInput")),
            KeyBinding::new("cmd-v", Paste, Some("TextInput")),
            KeyBinding::new("secondary-v", Paste, Some("TextInput")),
            KeyBinding::new("cmd-c", Copy, Some("TextInput")),
            KeyBinding::new("secondary-c", Copy, Some("TextInput")),
            KeyBinding::new("cmd-x", Cut, Some("TextInput")),
            KeyBinding::new("secondary-x", Cut, Some("TextInput")),
            KeyBinding::new("home", Home, Some("TextInput")),
            KeyBinding::new("end", End, Some("TextInput")),
            KeyBinding::new("shift-home", SelectHome, Some("TextInput")),
            KeyBinding::new("shift-end", SelectEnd, Some("TextInput")),
            KeyBinding::new("secondary-home", DocumentHome, Some("TextInput")),
            KeyBinding::new("secondary-end", DocumentEnd, Some("TextInput")),
            KeyBinding::new("enter", Newline, Some("TextInput")),
            KeyBinding::new("shift-enter", Newline, Some("TextInput")),
            KeyBinding::new("up", Up, Some("TextInput")),
            KeyBinding::new("down", Down, Some("TextInput")),
            KeyBinding::new("shift-up", SelectUp, Some("TextInput")),
            KeyBinding::new("shift-down", SelectDown, Some("TextInput")),
        ]);
        // Mac keyboards have no Home or End key; Command and an arrow stand in for them.
        #[cfg(target_os = "macos")]
        cx.bind_keys([
            KeyBinding::new("cmd-left", Home, Some("TextInput")),
            KeyBinding::new("cmd-right", End, Some("TextInput")),
            KeyBinding::new("cmd-shift-left", SelectHome, Some("TextInput")),
            KeyBinding::new("cmd-shift-right", SelectEnd, Some("TextInput")),
            KeyBinding::new("cmd-up", DocumentHome, Some("TextInput")),
            KeyBinding::new("cmd-down", DocumentEnd, Some("TextInput")),
        ]);
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.reveal_cursor = true;
        self.desired_x = None;
        self.affinity = CaretAffinity::AfterWrap;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let anchor = if self.selection_reversed {
            self.selected_range.end
        } else {
            self.selected_range.start
        };
        self.selected_range = anchor.min(offset)..anchor.max(offset);
        self.selection_reversed = offset < anchor;
        self.reveal_cursor = true;
        self.desired_x = None;
        self.affinity = CaretAffinity::AfterWrap;
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.content.chars() {
            if utf16 >= offset {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.content[..offset].encode_utf16().count()
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> (usize, CaretAffinity) {
        let (Some(bounds), Some(layout)) = (self.last_bounds, self.last_layout.as_ref()) else {
            return (0, CaretAffinity::AfterWrap);
        };
        let mut local = position - bounds.origin;
        if self.mode == InputMode::Multiline {
            local.y += self.scroll_offset;
        } else {
            local.x += self.scroll_offset;
        }
        let (offset, affinity) = layout.hit_test(local);
        let offset = self.nearest_boundary(offset);
        (offset, affinity)
    }

    fn nearest_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain([self.content.len()])
            .min_by_key(|index| index.abs_diff(offset))
            .unwrap_or(0)
    }

    fn vertical(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        if self.mode == InputMode::SingleLine {
            return;
        }
        let Some(layout) = self.last_layout.as_ref() else {
            return;
        };
        let row = layout.row_for_offset(self.cursor_offset(), self.affinity);
        let x = self
            .desired_x
            .unwrap_or_else(|| layout.position(self.cursor_offset(), self.affinity).x);
        let target = row.saturating_add_signed(delta).min(layout.rows.len() - 1);
        let (offset, affinity) = layout.hit_test(point(x, layout.rows[target].top));
        let offset = self.nearest_boundary(offset);
        if extend {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
        self.affinity = affinity;
        self.desired_x = Some(x);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, false, cx);
    }
    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, false, cx);
    }
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, true, cx);
    }
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, true, cx);
    }

    fn row_edge(&mut self, end: bool, extend: bool, cx: &mut Context<Self>) {
        let offset = if self.mode == InputMode::Multiline {
            if let Some(layout) = self.last_layout.as_ref() {
                let row = &layout.rows[layout.row_for_offset(self.cursor_offset(), self.affinity)];
                if end { row.bytes.end } else { row.bytes.start }
            } else {
                let cursor = self.cursor_offset();
                if end {
                    cursor
                        + self.content[cursor..]
                            .find('\n')
                            .unwrap_or(self.content.len() - cursor)
                } else {
                    self.content[..cursor]
                        .rfind('\n')
                        .map_or(0, |offset| offset + 1)
                }
            }
        } else if end {
            self.content.len()
        } else {
            0
        };
        if extend {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
        self.affinity = if end {
            CaretAffinity::BeforeWrap
        } else {
            CaretAffinity::AfterWrap
        };
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.row_edge(false, true, cx);
    }
    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.row_edge(true, true, cx);
    }
    fn document_home(&mut self, _: &DocumentHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }
    fn document_end(&mut self, _: &DocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    /// A single line has no use for Enter, so it goes on to the input's container,
    /// such as a dialog that submits on it.
    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == InputMode::Multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        } else {
            cx.propagate();
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode == InputMode::Multiline {
            if let (Some(layout), Some(bounds)) = (&self.last_layout, self.last_bounds) {
                self.scroll_offset = (self.scroll_offset
                    - event.delta.pixel_delta(layout.line_height).y)
                    .clamp(px(0.), (layout.height() - bounds.size.height).max(px(0.)));
                self.reveal_cursor = false;
                cx.notify();
            }
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.selected_range.is_empty() {
            self.previous_boundary(self.cursor_offset())
        } else {
            self.selected_range.start
        };
        self.move_to(offset, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.selected_range.is_empty() {
            self.next_boundary(self.cursor_offset())
        } else {
            self.selected_range.end
        };
        self.move_to(offset, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    pub fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        self.reveal_cursor = true;
        self.desired_x = None;
        self.affinity = CaretAffinity::AfterWrap;
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.row_edge(false, false, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.row_edge(true, false, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        if let Some(token) = self.tokens.iter().find(|t| t.end == cursor)
            && self.selected_range.is_empty()
        {
            self.selected_range = token.clone();
        } else if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(cursor), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        if let Some(token) = self.tokens.iter().find(|t| t.start == cursor)
            && self.selected_range.is_empty()
        {
            self.selected_range = token.clone();
        } else if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(cursor), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_owned(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        let (offset, affinity) = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
        self.affinity = affinity;
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            if self.mode == InputMode::Multiline {
                if let (Some(bounds), Some(layout)) = (self.last_bounds, self.last_layout.as_ref())
                {
                    if event.position.y < bounds.top() {
                        self.scroll_offset -= layout.line_height;
                    }
                    if event.position.y > bounds.bottom() {
                        self.scroll_offset += layout.line_height;
                    }
                    self.scroll_offset = self
                        .scroll_offset
                        .clamp(px(0.), (layout.height() - bounds.size.height).max(px(0.)));
                }
            }
            let (offset, affinity) = self.index_for_mouse_position(event.position);
            self.select_to(offset, cx);
            self.affinity = affinity;
        }
    }

    fn replace_content(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        let next = format!(
            "{}{}{}",
            &self.content[..range.start],
            new_text,
            &self.content[range.end..]
        );
        let changed = next != self.content;
        self.content = next.into();
        self.shift_tokens(range.clone(), new_text.len());
        self.last_layout = None;
        self.move_to(range.start + new_text.len(), cx);
        if changed {
            cx.emit(Changed);
        }
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode == InputMode::SingleLine && new_text.contains(['\r', '\n']) {
            cx.emit(RejectedMultiline);
            return;
        }
        let normalized = new_text.replace("\r\n", "\n").replace('\r', "\n");
        let new_text = normalized.as_str();
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.marked_range = None;
        self.replace_content(range, new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode == InputMode::SingleLine && new_text.contains(['\r', '\n']) {
            cx.emit(RejectedMultiline);
            return;
        }
        let new_selected_range_utf16 = new_selected_range_utf16.map(|selection| {
            let normalized_offset = |offset| {
                let mut consumed = 0;
                let prefix: String = new_text
                    .chars()
                    .take_while(|ch| {
                        let keep = consumed < offset;
                        consumed += ch.len_utf16();
                        keep
                    })
                    .collect();
                prefix
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .encode_utf16()
                    .count()
            };
            normalized_offset(selection.start)..normalized_offset(selection.end)
        });
        let normalized = new_text.replace("\r\n", "\n").replace('\r', "\n");
        let new_text = normalized.as_str();
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let start = range.start;
        self.replace_content(range, new_text, cx);
        self.marked_range = (!new_text.is_empty()).then_some(start..start + new_text.len());
        if let Some(selection) = new_selected_range_utf16 {
            let relative = |offset: usize| -> usize {
                let mut utf16 = 0;
                let mut utf8 = 0;
                for ch in new_text.chars() {
                    if utf16 >= offset {
                        break;
                    }
                    utf16 += ch.len_utf16();
                    utf8 += ch.len_utf8();
                }
                utf8
            };
            self.selected_range =
                start + relative(selection.start)..start + relative(selection.end);
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let bounds = self.last_bounds.unwrap_or(bounds);
        let range = self.range_from_utf16(&range_utf16);
        let scroll = if self.mode == InputMode::Multiline {
            point(px(0.), self.scroll_offset)
        } else {
            point(self.scroll_offset, px(0.))
        };
        let fragments = if range.is_empty() {
            vec![Bounds::new(
                layout.position(range.start, self.affinity),
                size(px(1.), layout.line_height),
            )]
        } else {
            layout.range_bounds(range, bounds.size.width)
        };
        fragments
            .into_iter()
            .map(|fragment| Bounds::new(bounds.origin + fragment.origin - scroll, fragment.size))
            .find(|fragment| fragment.intersects(&bounds))
            .map(|fragment| fragment.intersect(&bounds))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.last_layout.as_ref()?;
        Some(self.offset_to_utf16(self.index_for_mouse_position(point).0))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    layout: TextLayout,
    placeholder_layout: Option<TextLayout>,
    scroll_offset: Pixels,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

/// How many rows `text` wraps to at `width`, counting no further than `limit`, which
/// is all a field capped at `limit` rows needs to know for its height. It reads only
/// the wrap boundaries GPUI caches between frames: building every row's position, as
/// [`shape_layout`] does to paint, costs a scan of the line per row, which for a long
/// draft in a 1px layout pass took most of a second each frame.
fn wrapped_rows(
    text: &str,
    width: Pixels,
    style: &TextStyle,
    limit: usize,
    window: &mut Window,
) -> usize {
    let font_size = style.font_size.to_pixels(window.rem_size());
    let mut rows = 0;
    for paragraph in text.split('\n') {
        if rows >= limit {
            break;
        }
        let runs = [TextRun {
            len: paragraph.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }];
        rows += window
            .text_system()
            .shape_text(
                paragraph.to_owned().into(),
                font_size,
                &runs,
                Some(width),
                None,
            )
            .unwrap()
            .first()
            .map_or(1, |line| line.wrap_boundaries.len() + 1);
    }
    rows.min(limit)
}

/// Shapes `text`, drawing the byte ranges in `tokens` in `token_color`.
#[allow(clippy::too_many_arguments)]
fn shape_layout(
    input: &TextInput,
    text: &str,
    tokens: &[Range<usize>],
    token_color: Hsla,
    width: Pixels,
    style: &TextStyle,
    line_height: Pixels,
    placeholder: Hsla,
    window: &mut Window,
) -> TextLayout {
    let color = if input.content.is_empty() {
        placeholder
    } else {
        style.color
    };
    let font_size = style.font_size.to_pixels(window.rem_size());
    let mut paragraphs = Vec::new();
    let mut rows = Vec::new();
    let mut start = 0;
    for text in text.split('\n') {
        let end = start + text.len();
        let marked = input.marked_range.as_ref();
        let mut cuts = vec![start, end];
        for range in marked.into_iter().chain(tokens) {
            cuts.extend(
                [range.start, range.end]
                    .into_iter()
                    .filter(|&b| b > start && b < end),
            );
        }
        cuts.sort_unstable();
        cuts.dedup();
        let run = TextRun {
            len: 0,
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let mut runs: Vec<TextRun> = cuts
            .windows(2)
            .map(|pair| {
                let inside = |range: &Range<usize>| range.start <= pair[0] && pair[1] <= range.end;
                TextRun {
                    len: pair[1] - pair[0],
                    color: if tokens.iter().any(inside) {
                        token_color
                    } else {
                        color
                    },
                    underline: marked
                        .filter(|range| inside(range))
                        .map(|_| UnderlineStyle {
                            color: Some(color),
                            thickness: px(1.),
                            wavy: false,
                        }),
                    ..run.clone()
                }
            })
            .collect();
        if runs.is_empty() {
            runs.push(run);
        }
        let wrap = (input.mode == InputMode::Multiline).then_some(width.max(px(1.)));
        let line = window
            .text_system()
            .shape_text(text.to_owned().into(), font_size, &runs, wrap, None)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let boundaries = std::iter::once(0)
            .chain(line.wrap_boundaries.iter().map(|boundary| {
                line.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].index
            }))
            .chain([text.len()])
            .collect::<Vec<_>>();
        let paragraph = paragraphs.len();
        let top = line_height * rows.len();
        for pair in boundaries.windows(2) {
            rows.push(VisualRow {
                bytes: start + pair[0]..start + pair[1],
                paragraph,
                x: line.unwrapped_layout.x_for_index(pair[0]),
                top: line_height * rows.len(),
            });
        }
        paragraphs.push(Paragraph {
            start,
            top,
            line: line.into(),
        });
        start = end + 1;
    }
    TextLayout {
        paragraphs,
        rows,
        line_height,
    }
}

impl Element for TextElement {
    type RequestLayoutState = (TextStyle, Pixels);
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        if self.input.read(cx).fill {
            style.size.height = relative(1.).into();
        }
        let text_style = window.text_style();
        let line_height = window.line_height();
        let input = self.input.clone();
        let measured_style = text_style.clone();
        let layout = window.request_measured_layout(style, move |known, available, window, cx| {
            let width = known
                .width
                .unwrap_or_else(|| match available.width {
                    AvailableSpace::Definite(width) => width,
                    _ => px(400.),
                })
                .max(px(1.));
            let input = input.read(cx);
            let height = if let (true, Some(height)) = (input.fill, known.height) {
                height
            } else if input.mode == InputMode::Multiline {
                let rows = wrapped_rows(&input.content, width, &measured_style, 8, window);
                (line_height * rows).max(px(28.))
            } else {
                line_height
            };
            size(width, height)
        });
        (layout, (text_style, line_height))
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let colors = &cx.theme().colors;
        let (placeholder, selection_color, caret, accent) = (
            colors.text_placeholder,
            colors.selection,
            colors.caret,
            colors.accent,
        );
        let input = self.input.read(cx);
        let layout = shape_layout(
            input,
            &input.content,
            &input.tokens,
            accent,
            bounds.size.width,
            &state.0,
            state.1,
            placeholder,
            window,
        );
        let placeholder_layout = input.content.is_empty().then(|| {
            shape_layout(
                input,
                &input.placeholder,
                &[],
                accent,
                bounds.size.width,
                &state.0,
                state.1,
                placeholder,
                window,
            )
        });
        let cursor_position = layout.position(input.cursor_offset(), input.affinity);
        let multiline = input.mode == InputMode::Multiline;
        let max_scroll = if multiline {
            (layout.height() - bounds.size.height).max(px(0.))
        } else {
            (layout.paragraphs[0].line.unwrapped_layout.width - bounds.size.width + px(1.))
                .max(px(0.))
        };
        let mut scroll_offset = input.scroll_offset.clamp(px(0.), max_scroll);
        if input.reveal_cursor || input.last_bounds.is_none_or(|old| old.size != bounds.size) {
            let (position, extent, viewport) = if multiline {
                (cursor_position.y, layout.line_height, bounds.size.height)
            } else {
                (cursor_position.x, px(1.), bounds.size.width)
            };
            if position < scroll_offset {
                scroll_offset = position;
            }
            if position + extent > scroll_offset + viewport {
                scroll_offset = position + extent - viewport;
            }
        }
        let scroll = if multiline {
            point(px(0.), scroll_offset)
        } else {
            point(scroll_offset, px(0.))
        };
        let selection = layout
            .range_bounds(input.selected_range.clone(), bounds.size.width)
            .into_iter()
            .map(|rect| {
                fill(
                    Bounds::new(bounds.origin + rect.origin - scroll, rect.size),
                    selection_color,
                )
            })
            .collect();
        let cursor = input.selected_range.is_empty().then(|| {
            fill(
                Bounds::new(
                    bounds.origin + cursor_position - scroll,
                    size(px(1.), layout.line_height),
                ),
                caret,
            )
        });
        PrepaintState {
            layout,
            placeholder_layout,
            scroll_offset,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus_handle.clone();
        let multiline = self.input.read(cx).mode == InputMode::Multiline;
        self.input.update(cx, |input, _| {
            input.last_layout = Some(prepaint.layout.clone());
            input.last_bounds = Some(bounds);
            input.scroll_offset = prepaint.scroll_offset;
            input.reveal_cursor = false;
        });
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let scroll = if multiline {
            point(px(0.), prepaint.scroll_offset)
        } else {
            point(prepaint.scroll_offset, px(0.))
        };
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for selection in prepaint.selection.drain(..) {
                window.paint_quad(selection);
            }
            let paint_layout = prepaint
                .placeholder_layout
                .as_ref()
                .unwrap_or(&prepaint.layout);
            for paragraph in &paint_layout.paragraphs {
                paragraph
                    .line
                    .paint(
                        bounds.origin + point(px(0.), paragraph.top) - scroll,
                        prepaint.layout.line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .unwrap();
            }
            if focus.is_focused(window) {
                if let Some(cursor) = prepaint.cursor.take() {
                    window.paint_quad(cursor);
                }
            }
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::document_home))
            .on_action(cx.listener(Self::document_end))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .flex()
            .items_center()
            .w_full()
            // A single line is centred in a 28 px field that still takes clicks.
            .when(self.mode == InputMode::SingleLine, |d| d.h(px(28.)))
            .when(self.fill, |d| d.h_full())
            .overflow_hidden()
            .text_size(self.text_size.px())
            .line_height(px(20.))
            .text_color(self.text_color.to_hsla(cx))
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod tests {
    use super::*;

    #[gpui::test]
    fn multiline_editing_uses_visual_rows_and_unicode(cx: &mut gpui::TestAppContext) {
        cx.update(TextInput::bind_keys);
        cx.update(crate::init);
        let (input, cx) = cx.add_window_view(|window, cx| {
            let input = TextInput::new("Draft", InputMode::Multiline, cx);
            window.focus(&input.focus_handle, cx);
            input
        });
        for navigation in ["end", "shift-end"] {
            input.update(cx, |input, cx| input.set_value(String::new(), cx));
            cx.run_until_parked();
            cx.simulate_keystrokes(navigation);
            cx.simulate_input("x");
            input.read_with(cx, |input, _| assert_eq!(input.value(), "x"));
        }
        input.update(cx, |input, cx| input.set_value(String::new(), cx));
        cx.simulate_input("first 😀");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("second");
        cx.simulate_keystrokes("home shift-end");
        cx.simulate_input("第三");
        cx.update(|_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("\r\nlast\rtail".into()))
        });
        cx.simulate_keystrokes("secondary-v");
        cx.run_until_parked();
        input.read_with(cx, |input, _| {
            assert_eq!(input.value(), "first 😀\n第三\nlast\ntail")
        });
        cx.simulate_keystrokes("up home shift-end");
        cx.simulate_input("changed");
        cx.run_until_parked();
        input.read_with(cx, |input, _| {
            assert_eq!(input.value(), "first 😀\n第三\nchanged\ntail");
            let layout = input.last_layout.as_ref().unwrap();
            assert_eq!(layout.rows.len(), 4);
            assert_eq!(
                layout.position(11, CaretAffinity::AfterWrap).y,
                layout.line_height
            );
            let bounds = input.last_bounds.unwrap();
            assert_eq!(
                input
                    .index_for_mouse_position(bounds.origin + point(px(0.), layout.line_height * 3))
                    .0,
                26
            );
        });
        input.update(cx, |input, cx| input.set_value("abc ".repeat(500), cx));
        cx.run_until_parked();
        input.read_with(cx, |input, _| {
            let layout = input.last_layout.as_ref().unwrap();
            assert!(layout.rows.len() > 8);
            assert!(input.last_bounds.unwrap().size.height <= layout.line_height * 8);
            assert!(input.scroll_offset > px(0.));
            let boundary = layout.rows[1].bytes.start;
            assert_eq!(
                layout.position(boundary, CaretAffinity::AfterWrap).y,
                layout.line_height
            );
            assert_eq!(
                layout.position(boundary, CaretAffinity::BeforeWrap).y,
                px(0.)
            );
            assert_eq!(
                layout.hit_test(point(px(0.), layout.line_height)).0,
                boundary
            );
        });
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.select_all(&SelectAll, window, cx);
                input.replace_text_in_range(None, "😀\ntail", window, cx);
                input.home(&Home, window, cx);
                input.replace_text_in_range(None, "界", window, cx);
                input.end(&End, window, cx);
                input.replace_text_in_range(None, "!", window, cx);
                assert_eq!(input.value(), "😀\n界tail!");
                input.select_all(&SelectAll, window, cx);
                input.replace_text_in_range(None, "é", window, cx);
                input.end(&End, window, cx);
                input.replace_text_in_range(None, "x", window, cx);
                assert_eq!(input.value(), "éx");
            })
        });
        input.update(cx, |input, cx| input.set_value(String::new(), cx));
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "a\r\n😀z", Some(3..5), window, cx);
                assert_eq!(input.value(), "a\n😀z");
                assert_eq!(input.range_to_utf16(&input.selected_range), 2..4);
                assert_eq!(input.marked_range, Some(0..7));
                input.replace_text_in_range(Some(2..4), "界", window, cx);
                assert_eq!(input.value(), "a\n界z");
            })
        });
    }

    #[gpui::test]
    fn measured_rows_match_the_shaped_layout_up_to_the_cap(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Draft", InputMode::Multiline, cx));
        let long = format!("{}\n\n{}", "word ".repeat(3_000), "x".repeat(400));
        let texts = [
            "",
            "WWW",
            "one line",
            "a\nb\n\nc",
            "abc ".repeat(40).as_str(),
            &long,
        ]
        .map(str::to_owned);
        cx.update(|window, cx| {
            let style = window.text_style();
            let line_height = window.line_height();
            for text in &texts {
                for width in [px(1.), px(30.), px(400.), px(602.)] {
                    let shaped = shape_layout(
                        input.read(cx),
                        text,
                        &[],
                        style.color,
                        width,
                        &style,
                        line_height,
                        style.color,
                        window,
                    );
                    assert_eq!(
                        wrapped_rows(text, width, &style, 8, window),
                        shaped.rows.len().min(8),
                        "{} bytes at {width:?}",
                        text.len()
                    );
                }
            }
            assert_eq!(wrapped_rows(&long, px(602.), &style, 8, window), 8);
            assert_eq!(wrapped_rows("a\nb\n\nc", px(602.), &style, 8, window), 4);
            assert_eq!(wrapped_rows("one line", px(602.), &style, 8, window), 1);
        });
    }

    #[gpui::test]
    fn search_rejects_multiline_without_losing_selection(cx: &mut gpui::TestAppContext) {
        cx.update(TextInput::bind_keys);
        cx.update(crate::init);
        let (input, cx) = cx.add_window_view(|window, cx| {
            let input = TextInput::new("Search", InputMode::SingleLine, cx);
            window.focus(&input.focus_handle, cx);
            input
        });
        cx.simulate_input("query");
        cx.simulate_keystrokes("secondary-a");
        cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("one\r\ntwo".into())));
        cx.simulate_keystrokes("secondary-v enter");
        input.read_with(cx, |input, _| {
            assert_eq!(input.value(), "query");
            assert_eq!(input.selected_range, 0..5);
        });
        cx.simulate_input("next");
        input.read_with(cx, |input, _| assert_eq!(input.value(), "next"));
    }

    #[gpui::test]
    fn tokens_delete_whole_and_become_text_when_edited(cx: &mut gpui::TestAppContext) {
        cx.update(TextInput::bind_keys);
        cx.update(crate::init);
        let (input, cx) = cx.add_window_view(|window, cx| {
            let input = TextInput::new("Draft", InputMode::Multiline, cx);
            window.focus(&input.focus_handle, cx);
            input
        });
        cx.simulate_input("read @mod");
        input.update(cx, |input, cx| input.insert_token(5..9, "@src/a b.rs", cx));
        input.read_with(cx, |input, _| {
            assert_eq!(input.value(), "read @src/a b.rs ");
            assert_eq!(input.tokens(), [5..16]);
            assert_eq!(input.cursor(), 17);
        });
        cx.simulate_input("now");
        cx.simulate_keystrokes("home");
        cx.simulate_input("> ");
        input.read_with(cx, |input, _| {
            assert_eq!(input.value(), "> read @src/a b.rs now");
            assert_eq!(input.tokens(), [7..18], "an edit before a token moves it");
        });
        cx.simulate_keystrokes("end backspace backspace backspace backspace");
        input.read_with(cx, |input, _| {
            assert_eq!(input.value(), "> read @src/a b.rs")
        });
        cx.simulate_keystrokes("backspace");
        input.read_with(cx, |input, _| {
            assert_eq!(
                input.value(),
                "> read ",
                "Backspace at its end removes it whole"
            );
            assert!(input.tokens().is_empty());
        });

        // Inserted against other text, a token gets a space before it and takes the
        // space that follows instead of adding one.
        input.update(cx, |input, cx| {
            input.set_value("see it".into(), cx);
            input.insert_token(3..3, "@a.rs", cx);
        });
        input.read_with(cx, |input, _| {
            assert_eq!(input.value(), "see @a.rs it");
            assert_eq!(input.tokens(), [4..9]);
            assert_eq!(input.cursor(), 10);
        });
        cx.simulate_keystrokes("home right right right right delete");
        input.read_with(cx, |input, _| {
            assert_eq!(
                input.value(),
                "see  it",
                "Delete at its start removes it whole"
            );
        });

        // Typing inside a token, or against its end, makes it plain text.
        input.update(cx, |input, cx| {
            input.set_value("@a.rs @b.rs".into(), cx);
            input.set_tokens([0..5, 6..11, 3..4], cx);
        });
        input.read_with(cx, |input, _| assert_eq!(input.tokens(), [0..5, 6..11]));
        cx.simulate_keystrokes("end");
        cx.simulate_input("x");
        cx.simulate_keystrokes("home right");
        cx.simulate_input("y");
        input.read_with(cx, |input, _| {
            assert_eq!(input.value(), "@ya.rs @b.rsx");
            assert!(input.tokens().is_empty());
        });
        cx.simulate_keystrokes("end backspace");
        input.read_with(cx, |input, _| assert_eq!(input.value(), "@ya.rs @b.rs"));
        // A token inserted with the caret inside another goes after it.
        input.update(cx, |input, cx| {
            input.set_value("see @a.rs now".into(), cx);
            input.set_tokens([4..9], cx);
            input.insert_token(6..6, "@b.rs", cx);
        });
        input.read_with(cx, |input, _| {
            assert_eq!(input.value(), "see @a.rs @b.rs now");
            assert_eq!(input.tokens(), [4..9, 10..15]);
        });
        input.update(cx, |input, cx| input.set_tokens([0..2], cx));
        input.read_with(cx, |input, _| {
            assert!(input.tokens().is_empty(), "a token must stand alone");
        });
    }
}
