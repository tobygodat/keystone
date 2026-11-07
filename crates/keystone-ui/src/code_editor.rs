//! An editor for one file. The text is kept as lines and only the lines on screen are
//! shaped each frame, so a file of several megabytes edits as quickly as a short one.

mod buffer;

pub use buffer::Point;

use crate::automation::AutomationExt;
use crate::highlight::{self, Language, Span};
use crate::{ActiveTheme, SyntaxColors, TextSize};
use buffer::Buffer;
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font,
    GlobalElementId, Hsla, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, ScrollWheelEvent, ShapedLine, SharedString, Style, Task,
    TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px,
    relative, size,
};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

actions!(
    code_editor,
    [
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        LineStart,
        LineEnd,
        SelectToLineStart,
        SelectToLineEnd,
        DocumentStart,
        DocumentEnd,
        SelectToDocumentStart,
        SelectToDocumentEnd,
        PageUp,
        PageDown,
        SelectPageUp,
        SelectPageDown,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        Newline,
        Indent,
        Outdent,
        Undo,
        Redo,
        SelectAll,
        Copy,
        Cut,
        Paste
    ]
);

/// Emitted after every edit, undo and redo.
pub struct Changed;

const CONTEXT: &str = "CodeEditor";
const HIGHLIGHT_DELAY: Duration = Duration::from_millis(50);
const GUTTER_PAD: Pixels = px(8.);
const GUTTER_GAP: Pixels = px(12.);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(bindings(cfg!(target_os = "macos")));
}

/// The editor's key bindings on macOS or elsewhere. Every key is one of
/// [`crate::text_input::editing_keys`], so a host keeps its own shortcuts off them and
/// no rebinding can take one while the editor has focus.
pub fn bindings(macos: bool) -> Vec<KeyBinding> {
    let context = Some(CONTEXT);
    let mut bindings = vec![
        KeyBinding::new("left", MoveLeft, context),
        KeyBinding::new("right", MoveRight, context),
        KeyBinding::new("up", MoveUp, context),
        KeyBinding::new("down", MoveDown, context),
        KeyBinding::new("shift-left", SelectLeft, context),
        KeyBinding::new("shift-right", SelectRight, context),
        KeyBinding::new("shift-up", SelectUp, context),
        KeyBinding::new("shift-down", SelectDown, context),
        KeyBinding::new("home", LineStart, context),
        KeyBinding::new("end", LineEnd, context),
        KeyBinding::new("shift-home", SelectToLineStart, context),
        KeyBinding::new("shift-end", SelectToLineEnd, context),
        KeyBinding::new("secondary-home", DocumentStart, context),
        KeyBinding::new("secondary-end", DocumentEnd, context),
        KeyBinding::new("secondary-shift-home", SelectToDocumentStart, context),
        KeyBinding::new("secondary-shift-end", SelectToDocumentEnd, context),
        KeyBinding::new("pageup", PageUp, context),
        KeyBinding::new("pagedown", PageDown, context),
        KeyBinding::new("shift-pageup", SelectPageUp, context),
        KeyBinding::new("shift-pagedown", SelectPageDown, context),
        KeyBinding::new("backspace", Backspace, context),
        KeyBinding::new("delete", Delete, context),
        KeyBinding::new("enter", Newline, context),
        KeyBinding::new("tab", Indent, context),
        KeyBinding::new("shift-tab", Outdent, context),
        KeyBinding::new("secondary-z", Undo, context),
        KeyBinding::new("secondary-shift-z", Redo, context),
        KeyBinding::new("secondary-a", SelectAll, context),
        KeyBinding::new("secondary-c", Copy, context),
        KeyBinding::new("secondary-x", Cut, context),
        KeyBinding::new("secondary-v", Paste, context),
    ];
    if macos {
        // Mac keyboards move by word with Option, and Command and an arrow stand in for
        // the Home and End keys they lack.
        bindings.extend([
            KeyBinding::new("alt-left", WordLeft, context),
            KeyBinding::new("alt-right", WordRight, context),
            KeyBinding::new("alt-shift-left", SelectWordLeft, context),
            KeyBinding::new("alt-shift-right", SelectWordRight, context),
            KeyBinding::new("alt-backspace", DeleteWordLeft, context),
            KeyBinding::new("alt-delete", DeleteWordRight, context),
            KeyBinding::new("cmd-left", LineStart, context),
            KeyBinding::new("cmd-right", LineEnd, context),
            KeyBinding::new("cmd-shift-left", SelectToLineStart, context),
            KeyBinding::new("cmd-shift-right", SelectToLineEnd, context),
            KeyBinding::new("cmd-up", DocumentStart, context),
            KeyBinding::new("cmd-down", DocumentEnd, context),
            KeyBinding::new("cmd-shift-up", SelectToDocumentStart, context),
            KeyBinding::new("cmd-shift-down", SelectToDocumentEnd, context),
        ]);
    } else {
        bindings.extend([
            KeyBinding::new("secondary-left", WordLeft, context),
            KeyBinding::new("secondary-right", WordRight, context),
            KeyBinding::new("secondary-shift-left", SelectWordLeft, context),
            KeyBinding::new("secondary-shift-right", SelectWordRight, context),
            KeyBinding::new("secondary-backspace", DeleteWordLeft, context),
            KeyBinding::new("secondary-delete", DeleteWordRight, context),
            KeyBinding::new("secondary-y", Redo, context),
        ]);
    }
    bindings
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Selection {
    anchor: Point,
    head: Point,
}

impl Selection {
    fn cursor(point: Point) -> Self {
        Self {
            anchor: point,
            head: point,
        }
    }

    fn range(&self) -> Range<Point> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
}

#[derive(Clone, Copy)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
    PageUp,
    PageDown,
    WordLeft,
    WordRight,
}

/// Spans from a highlight pass, kept until a newer pass replaces them.
struct Highlights {
    language: Language,
    revision: u64,
    lines: Vec<Vec<Span>>,
}

/// One shaped line on screen. `tabs` holds the columns of its tabs, each drawn as four
/// spaces, to map buffer columns to the shaped text and back.
struct Row {
    shaped: ShapedLine,
    tabs: Vec<usize>,
}

impl Row {
    fn x(&self, column: usize) -> Pixels {
        self.shaped.x_for_index(display_offset(&self.tabs, column))
    }

    fn column(&self, x: Pixels) -> usize {
        buffer_column(&self.tabs, self.shaped.closest_index_for_x(x))
    }
}

/// The last painted frame, for hit testing.
struct Frame {
    bounds: Bounds<Pixels>,
    text_left: Pixels,
    line_height: Pixels,
    advance: Pixels,
    first_line: usize,
    rows: Vec<Row>,
    max_scroll_x: Pixels,
}

/// A highlight pass in flight. Dropping it, as a newer pass or a language change does,
/// stops the background parse instead of letting it run on to the end of the file.
struct Pass {
    _task: Task<()>,
    cancelled: Arc<AtomicUsize>,
}

impl Drop for Pass {
    fn drop(&mut self) {
        self.cancelled.store(1, Ordering::Relaxed);
    }
}

pub struct CodeEditor {
    id: SharedString,
    buffer: Buffer,
    selection: Selection,
    /// The display column Up and Down aim for, kept across short lines.
    desired_column: Option<usize>,
    marked: Option<Range<Point>>,
    scroll: gpui::Point<Pixels>,
    /// Whether the next frame scrolls the cursor into view.
    reveal: bool,
    focus_handle: FocusHandle,
    read_only: bool,
    language: Option<Language>,
    highlights: Option<Highlights>,
    highlighting: Option<Pass>,
    frame: Option<Frame>,
    is_selecting: bool,
}

impl EventEmitter<Changed> for CodeEditor {}

impl Focusable for CodeEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl CodeEditor {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            id: "code-editor".into(),
            buffer: Buffer::new(""),
            selection: Selection::default(),
            desired_column: None,
            marked: None,
            scroll: gpui::Point::default(),
            reveal: false,
            focus_handle: cx.focus_handle(),
            read_only: false,
            language: None,
            highlights: None,
            highlighting: None,
            frame: None,
            is_selecting: false,
        }
    }

    /// Names the editor for debug selectors and native verification. Defaults to
    /// `code-editor`.
    pub fn with_id(mut self, id: impl Into<SharedString>) -> Self {
        self.id = id.into();
        self
    }

    /// Shows `text` as a new file: history cleared, cursor at the start, scrolled to the top.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer = Buffer::new(text);
        self.selection = Selection::default();
        self.scroll = gpui::Point::default();
        self.highlights = None;
        self.reset_view(cx);
    }

    /// Replaces the text with the file's new contents, keeping the cursor's line and
    /// column where they still exist and the scroll position. History starts over.
    pub fn reload(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer = Buffer::new(text);
        let (anchor, head) = (self.selection.anchor, self.selection.head);
        self.selection = Selection {
            anchor: self.buffer.clamp(anchor),
            head: self.buffer.clamp(head),
        };
        self.reset_view(cx);
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.desired_column = None;
        self.marked = None;
        self.reveal = false;
        self.highlight(Duration::ZERO, cx);
        cx.notify();
    }

    /// The text as the file holds it, with the line ending it was read with.
    pub fn text(&self) -> String {
        self.buffer.text()
    }

    /// The text's state through its history. Undoing back to a saved state returns its
    /// revision, so a host compares this with the revision it saved to know if it's clean.
    pub fn revision(&self) -> u64 {
        self.buffer.revision()
    }

    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    pub fn set_language(&mut self, language: Option<Language>, cx: &mut Context<Self>) {
        self.language = language;
        self.highlight(Duration::ZERO, cx);
    }

    pub fn selection(&self) -> Range<Point> {
        self.selection.range()
    }

    pub fn selected_text(&self) -> String {
        self.buffer.text_in(self.selection.range())
    }

    /// A read-only editor still selects and copies, but ignores every edit.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Highlights the text after `delay`, unless the spans shown are already for it. The
    /// spans shown stay until the pass finishes, even if the text has changed since.
    fn highlight(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let Some(language) = self.language else {
            self.highlights = None;
            self.highlighting = None;
            return;
        };
        let revision = self.buffer.revision();
        if self
            .highlights
            .as_ref()
            .is_some_and(|shown| shown.language == language && shown.revision == revision)
        {
            self.highlighting = None;
            return;
        }
        let cancelled = Arc::new(AtomicUsize::new(0));
        let flag = cancelled.clone();
        let task = cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let Ok((revision, text)) = this.read_with(cx, |this, _| {
                (this.buffer.revision(), this.buffer.text_lf())
            }) else {
                return;
            };
            let Some(lines) = cx
                .background_spawn(async move { highlight::highlight_until(language, &text, &flag) })
                .await
            else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.highlights = Some(Highlights {
                    language,
                    revision,
                    lines,
                });
                this.highlighting = None;
                cx.notify();
            });
        });
        self.highlighting = Some(Pass {
            _task: task,
            cancelled,
        });
    }

    fn move_cursor(&mut self, motion: Motion, extend: bool, cx: &mut Context<Self>) {
        let head = self.selection.head;
        let range = self.selection.range();
        let collapse = !extend && !range.is_empty();
        let page = self.frame.as_ref().map_or(20, |frame| {
            ((frame.bounds.size.height / frame.line_height) as isize - 1).max(1)
        });
        let target = match motion {
            Motion::Left if collapse => range.start,
            Motion::Right if collapse => range.end,
            Motion::Left => self.buffer.previous_boundary(head),
            Motion::Right => self.buffer.next_boundary(head),
            Motion::Up => self.vertical(-1),
            Motion::Down => self.vertical(1),
            Motion::PageUp => self.vertical(-page),
            Motion::PageDown => self.vertical(page),
            Motion::LineStart => {
                let indent = self.buffer.indent_end(head.line);
                Point::new(head.line, if head.column == indent { 0 } else { indent })
            }
            Motion::LineEnd => Point::new(head.line, self.buffer.line(head.line).len()),
            Motion::DocumentStart => Point::default(),
            Motion::DocumentEnd => self.buffer.end(),
            Motion::WordLeft => self.buffer.previous_word_start(head),
            Motion::WordRight => self.buffer.next_word_end(head),
        };
        if !matches!(
            motion,
            Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown
        ) {
            self.desired_column = None;
        }
        self.buffer.break_group();
        self.selection = Selection {
            anchor: if extend {
                self.selection.anchor
            } else {
                target
            },
            head: target,
        };
        self.reveal = true;
        cx.notify();
    }

    fn vertical(&mut self, lines: isize) -> Point {
        let head = self.selection.head;
        let column = *self
            .desired_column
            .get_or_insert_with(|| self.buffer.display_column(head));
        match head.line.checked_add_signed(lines) {
            None => Point::default(),
            Some(line) if line >= self.buffer.line_count() => self.buffer.end(),
            Some(line) => self.buffer.point_at_display_column(line, column),
        }
    }

    /// Replaces `range` with `text` and puts the cursor after it, unless read-only.
    fn edit(
        &mut self,
        range: Range<Point>,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Option<Range<Point>> {
        if self.read_only {
            return None;
        }
        let revision = self.buffer.revision();
        let landed = self.buffer.replace(range, text);
        self.selection = Selection::cursor(landed.end);
        self.edited(revision, cx);
        Some(landed)
    }

    fn edited(&mut self, revision_before: u64, cx: &mut Context<Self>) {
        self.marked = None;
        self.desired_column = None;
        self.reveal = true;
        if self.buffer.revision() != revision_before {
            self.highlight(HIGHLIGHT_DELAY, cx);
            cx.emit(Changed);
        }
        cx.notify();
    }

    /// The selection, or the range from the cursor to `to` when nothing is selected.
    fn selection_or(&self, to: impl FnOnce(&Buffer, Point) -> Point) -> Range<Point> {
        let range = self.selection.range();
        if !range.is_empty() {
            return range;
        }
        let other = to(&self.buffer, self.selection.head);
        other.min(range.start)..other.max(range.start)
    }

    fn delete_to(&mut self, to: impl FnOnce(&Buffer, Point) -> Point, cx: &mut Context<Self>) {
        let range = self.selection_or(to);
        self.edit(range, "", cx);
    }

    /// A new line keeps the leading spaces and tabs of the line it splits.
    fn newline(&mut self, cx: &mut Context<Self>) {
        let range = self.selection.range();
        let line = self.buffer.line(range.start.line);
        let indent = self
            .buffer
            .indent_end(range.start.line)
            .min(range.start.column);
        let text = format!("\n{}", &line[..indent]);
        self.edit(range, &text, cx);
    }

    /// Tab inserts one indent; with a selection across lines it indents each line.
    fn indent(&mut self, cx: &mut Context<Self>) {
        let range = self.selection.range();
        if range.start.line == range.end.line {
            let unit = self.buffer.indent_unit();
            self.edit(range, unit, cx);
        } else {
            self.shift_lines(true, cx);
        }
    }

    /// Indents or outdents every line the selection touches, as one undo step. A
    /// selection ending at the start of a line leaves that line alone.
    fn shift_lines(&mut self, indent: bool, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let range = self.selection.range();
        let first = range.start.line;
        let last = if range.end.line > first && range.end.column == 0 {
            range.end.line - 1
        } else {
            range.end.line
        };
        let unit = self.buffer.indent_unit();
        let mut text = String::new();
        let mut shifts = Vec::new();
        for line in first..=last {
            let content = self.buffer.line(line);
            let (removed, added) = if indent {
                (0, unit)
            } else if content.starts_with('\t') {
                (1, "")
            } else {
                (
                    (content.len() - content.trim_start_matches(' ').len()).min(4),
                    "",
                )
            };
            if line > first {
                text.push('\n');
            }
            text.push_str(added);
            text.push_str(&content[removed..]);
            shifts.push(added.len() as isize - removed as isize);
        }
        let end = Point::new(last, self.buffer.line(last).len());
        let revision = self.buffer.revision();
        self.buffer.break_group();
        self.buffer.replace(Point::new(first, 0)..end, &text);
        self.buffer.break_group();
        let shift = |point: Point| {
            if (first..=last).contains(&point.line) && point.column > 0 {
                let column = point.column as isize + shifts[point.line - first];
                Point::new(point.line, column.max(0) as usize)
            } else {
                point
            }
        };
        self.selection = Selection {
            anchor: shift(self.selection.anchor),
            head: shift(self.selection.head),
        };
        self.edited(revision, cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.restore(Buffer::undo, cx);
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        self.restore(Buffer::redo, cx);
    }

    fn restore(&mut self, step: fn(&mut Buffer) -> Option<Range<Point>>, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let revision = self.buffer.revision();
        if let Some(selection) = step(&mut self.buffer) {
            self.selection = Selection {
                anchor: selection.start,
                head: selection.end,
            };
            self.edited(revision, cx);
        }
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.buffer.break_group();
        self.selection = Selection {
            anchor: Point::default(),
            head: self.buffer.end(),
        };
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if !self.selection.range().is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text()));
        }
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        self.copy(cx);
        self.edit(self.selection.range(), "", cx);
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.buffer.break_group();
            self.edit(self.selection.range(), &text, cx);
            self.buffer.break_group();
        }
    }

    fn max_scroll_y(&self, frame: &Frame) -> Pixels {
        (frame.line_height * self.buffer.line_count() - frame.bounds.size.height).max(px(0.))
    }

    fn point_for_position(&self, position: gpui::Point<Pixels>) -> Point {
        let Some(frame) = &self.frame else {
            return self.selection.head;
        };
        let y = position.y - frame.bounds.top() + self.scroll.y;
        let line =
            ((y / frame.line_height).floor().max(0.) as usize).min(self.buffer.line_count() - 1);
        let x = (position.x - frame.text_left + self.scroll.x).max(px(0.));
        let point = match line
            .checked_sub(frame.first_line)
            .and_then(|row| frame.rows.get(row))
        {
            Some(row) => Point::new(line, row.column(x)),
            None => self
                .buffer
                .point_at_display_column(line, (x / frame.advance).round() as usize),
        };
        self.buffer.clamp(point)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let point = self.point_for_position(event.position);
        self.buffer.break_group();
        self.desired_column = None;
        self.is_selecting = true;
        self.selection = match event.click_count {
            0 | 1 if event.modifiers.shift => Selection {
                head: point,
                ..self.selection
            },
            0 | 1 => Selection::cursor(point),
            2 => {
                let word = self.buffer.word_at(point);
                Selection {
                    anchor: word.start,
                    head: word.end,
                }
            }
            _ => Selection {
                anchor: Point::new(point.line, 0),
                head: if point.line + 1 < self.buffer.line_count() {
                    Point::new(point.line + 1, 0)
                } else {
                    self.buffer.end()
                },
            },
        };
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    /// Dragging past the top or bottom edge scrolls a line per move.
    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }
        if let Some(frame) = &self.frame {
            if event.position.y < frame.bounds.top() {
                self.scroll.y -= frame.line_height;
            } else if event.position.y > frame.bounds.bottom() {
                self.scroll.y += frame.line_height;
            }
            self.scroll.y = self.scroll.y.clamp(px(0.), self.max_scroll_y(frame));
        }
        self.selection.head = self.point_for_position(event.position);
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(frame) = &self.frame else {
            return;
        };
        let delta = event.delta.pixel_delta(frame.line_height);
        self.scroll = point(
            (self.scroll.x - delta.x).clamp(px(0.), frame.max_scroll_x),
            (self.scroll.y - delta.y).clamp(px(0.), self.max_scroll_y(frame)),
        );
        self.reveal = false;
        cx.notify();
    }

    /// The range an input method names in UTF-16, or else the marked text or selection.
    fn input_range(&self, range_utf16: Option<Range<usize>>) -> Range<Point> {
        range_utf16
            .map(|range| {
                self.buffer.point_at_utf16(range.start)..self.buffer.point_at_utf16(range.end)
            })
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.range())
    }

    fn range_to_utf16(&self, range: &Range<Point>) -> Range<usize> {
        self.buffer.utf16_offset(range.start)..self.buffer.utf16_offset(range.end)
    }
}

impl EntityInputHandler for CodeEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.input_range(Some(range_utf16));
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.buffer.text_in(range))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selection.range()),
            reversed: self.selection.head < self.selection.anchor,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.input_range(range_utf16);
        self.edit(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.input_range(range_utf16);
        let Some(landed) = self.edit(range, text, cx) else {
            return;
        };
        if let Some(selected) = selected_utf16 {
            let start = self.buffer.utf16_offset(landed.start);
            self.selection = Selection {
                anchor: self.buffer.point_at_utf16(start + selected.start),
                head: self.buffer.point_at_utf16(start + selected.end),
            };
        }
        self.marked = (!text.is_empty()).then_some(landed);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let frame = self.frame.as_ref()?;
        let range = self.input_range(Some(range_utf16));
        let row = frame
            .rows
            .get(range.start.line.checked_sub(frame.first_line)?)?;
        let left = row.x(range.start.column);
        let right = if range.end.line == range.start.line {
            row.x(range.end.column)
        } else {
            row.shaped.width()
        };
        Some(Bounds::new(
            point(
                frame.text_left + left - self.scroll.x,
                frame.bounds.top() + frame.line_height * range.start.line - self.scroll.y,
            ),
            size((right - left).max(px(1.)), frame.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.frame.as_ref()?;
        Some(self.buffer.utf16_offset(self.point_for_position(point)))
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.buffer.utf16_offset(self.buffer.end()))
    }
}

/// The byte offset in the drawn text of a buffer column, with each tab drawn as four spaces.
fn display_offset(tabs: &[usize], column: usize) -> usize {
    column + 3 * tabs.partition_point(|&tab| tab < column)
}

/// The buffer column of a byte offset in the drawn text; inside a tab, its nearer edge.
fn buffer_column(tabs: &[usize], offset: usize) -> usize {
    let mut shift = 0;
    for &tab in tabs {
        let start = tab + shift;
        if offset <= start {
            break;
        }
        if offset < start + 4 {
            return if offset - start < 2 { tab } else { tab + 1 };
        }
        shift += 3;
    }
    offset - shift
}

/// A line as drawn: tabs as four spaces and other control characters as one space each,
/// so only tabs move later columns. Returns the text and the columns of its tabs.
fn display_text(line: &str) -> (SharedString, Vec<usize>) {
    if !line.bytes().any(|byte| byte.is_ascii_control()) {
        return (line.to_owned().into(), Vec::new());
    }
    let mut text = String::with_capacity(line.len());
    let mut tabs = Vec::new();
    for (column, ch) in line.char_indices() {
        match ch {
            '\t' => {
                tabs.push(column);
                text.push_str("    ");
            }
            ch if ch.is_ascii_control() => text.push(' '),
            ch => text.push(ch),
        }
    }
    (text.into(), tabs)
}

/// The runs that color `line`, split at highlight span and marked text boundaries. Spans
/// may be stale, so each is clamped to the line and its characters.
fn line_runs(
    line: &str,
    tabs: &[usize],
    spans: &[Span],
    marked: Option<Range<usize>>,
    font: &Font,
    text: Hsla,
    syntax: &SyntaxColors,
) -> Vec<TextRun> {
    let boundary = |offset: usize| {
        let mut offset = offset.min(line.len());
        while !line.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    };
    let spans: Vec<(Range<usize>, Hsla)> = spans
        .iter()
        .map(|span| {
            let range = boundary(span.range.start)..boundary(span.range.end);
            (range, syntax.color(span.syntax))
        })
        .collect();
    let marked = marked.map(|range| boundary(range.start)..boundary(range.end));
    let mut cuts = vec![0, line.len()];
    for range in spans.iter().map(|(range, _)| range).chain(marked.as_ref()) {
        cuts.extend([range.start, range.end]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let run = TextRun {
        len: 0,
        font: font.clone(),
        color: text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    // Spans come in order without overlap, so one cursor walks them with the cuts and
    // a long line with many spans stays linear.
    let mut span = 0;
    let mut runs: Vec<TextRun> =
        cuts.windows(2)
            .map(|pair| {
                let inside = |range: &Range<usize>| range.start <= pair[0] && pair[1] <= range.end;
                while spans
                    .get(span)
                    .is_some_and(|(range, _)| range.end <= pair[0])
                {
                    span += 1;
                }
                TextRun {
                    len: display_offset(tabs, pair[1]) - display_offset(tabs, pair[0]),
                    color: spans
                        .get(span)
                        .filter(|(range, _)| inside(range))
                        .map_or(text, |(_, color)| *color),
                    underline: marked.as_ref().filter(|range| inside(range)).map(|_| {
                        UnderlineStyle {
                            color: Some(text),
                            thickness: px(1.),
                            wavy: false,
                        }
                    }),
                    ..run.clone()
                }
            })
            .collect();
    if runs.is_empty() {
        runs.push(run);
    }
    runs
}

struct EditorElement {
    editor: Entity<CodeEditor>,
}

struct Prepaint {
    frame: Option<Frame>,
    scroll: gpui::Point<Pixels>,
    numbers: Vec<(ShapedLine, gpui::Point<Pixels>)>,
    selection: Vec<PaintQuad>,
    caret: Option<PaintQuad>,
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

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
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let theme = cx.theme();
        let colors = &theme.colors;
        let font = gpui::font(theme.fonts.mono.clone());
        let font_size = TextSize::Sm.px();
        let line_height = TextSize::Sm.line_height();
        let text_system = window.text_system().clone();
        let advance = text_system
            .advance(text_system.resolve_font(&font), font_size, '0')
            .map_or(font_size, |size| size.width);
        let editor = self.editor.read(cx);
        let buffer = &editor.buffer;
        let line_count = buffer.line_count();
        let number_width = (advance * line_count.to_string().len() as f32).ceil();
        let text_left = bounds.left() + GUTTER_PAD + number_width + GUTTER_GAP;
        let text_width = (bounds.right() - text_left).max(px(0.));
        let head = editor.selection.head;

        let mut scroll = editor.scroll;
        if editor.reveal {
            let top = line_height * head.line;
            if top < scroll.y {
                scroll.y = top;
            }
            if top + line_height > scroll.y + bounds.size.height {
                scroll.y = top + line_height - bounds.size.height;
            }
        }
        let max_scroll_y = (line_height * line_count - bounds.size.height).max(px(0.));
        scroll.y = scroll.y.clamp(px(0.), max_scroll_y);
        let first_line = ((scroll.y / line_height).floor() as usize).min(line_count);
        let last_line =
            (((scroll.y + bounds.size.height) / line_height).ceil() as usize).min(line_count);

        let marked = editor.marked.clone();
        let rows: Vec<Row> = (first_line..last_line)
            .map(|line| {
                let content = buffer.line(line);
                let (display, tabs) = display_text(content);
                let spans = editor
                    .highlights
                    .as_ref()
                    .and_then(|highlights| highlights.lines.get(line))
                    .map_or(&[][..], Vec::as_slice);
                let marked = marked
                    .as_ref()
                    .filter(|range| (range.start.line..=range.end.line).contains(&line))
                    .map(|range| {
                        let start = if range.start.line == line {
                            range.start.column
                        } else {
                            0
                        };
                        let end = if range.end.line == line {
                            range.end.column
                        } else {
                            content.len()
                        };
                        start..end
                    });
                let runs = line_runs(
                    content,
                    &tabs,
                    spans,
                    marked,
                    &font,
                    colors.code_text,
                    &colors.syntax,
                );
                Row {
                    shaped: text_system.shape_line(display, font_size, &runs, None),
                    tabs,
                }
            })
            .collect();
        let row = |line: usize| line.checked_sub(first_line).and_then(|i| rows.get(i));

        let widest = rows
            .iter()
            .map(|row| row.shaped.width())
            .fold(px(0.), Pixels::max);
        let max_scroll_x = (widest + advance - text_width).max(px(0.));
        let caret_x = row(head.line).map(|row| row.x(head.column));
        if let (true, Some(x)) = (editor.reveal, caret_x) {
            if x < scroll.x {
                scroll.x = x;
            }
            if x + advance > scroll.x + text_width {
                scroll.x = x + advance - text_width;
            }
        }
        scroll.x = scroll.x.clamp(px(0.), max_scroll_x);
        let origin = |x: Pixels, line: usize| {
            point(
                text_left + x - scroll.x,
                bounds.top() + line_height * line - scroll.y,
            )
        };

        let range = editor.selection.range();
        let selection = (range.start.line.max(first_line)..=range.end.line.min(last_line))
            .filter_map(|line| {
                let row = row(line)?;
                let start = if line == range.start.line {
                    range.start.column
                } else {
                    0
                };
                let end = if line == range.end.line {
                    range.end.column
                } else {
                    buffer.line(line).len()
                };
                let newline = line < range.end.line;
                if start == end && !newline {
                    return None;
                }
                let left = row.x(start);
                let right = row.x(end) + if newline { advance / 2. } else { px(0.) };
                Some(fill(
                    Bounds::new(origin(left, line), size(right - left, line_height)),
                    colors.selection,
                ))
            })
            .collect();
        let caret = caret_x
            .filter(|_| editor.focus_handle.is_focused(window))
            .map(|x| {
                fill(
                    Bounds::new(origin(x, head.line), size(px(1.), line_height)),
                    colors.caret,
                )
            });
        let numbers = (first_line..last_line)
            .map(|line| {
                let color = if line == head.line {
                    colors.text_muted
                } else {
                    colors.text_faint
                };
                let number: SharedString = (line + 1).to_string().into();
                let run = TextRun {
                    len: number.len(),
                    font: font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = text_system.shape_line(number, font_size, &[run], None);
                let x = bounds.left() + GUTTER_PAD + number_width - shaped.width();
                let y = bounds.top() + line_height * line - scroll.y;
                (shaped, point(x, y))
            })
            .collect();
        Prepaint {
            frame: Some(Frame {
                bounds,
                text_left,
                line_height,
                advance,
                first_line,
                rows,
                max_scroll_x,
            }),
            scroll,
            numbers,
            selection,
            caret,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        let Some(frame) = prepaint.frame.take() else {
            return;
        };
        let text_bounds =
            Bounds::from_corners(point(frame.text_left, bounds.top()), bounds.bottom_right());
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (number, origin) in &prepaint.numbers {
                let _ = number.paint(
                    *origin,
                    frame.line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            window.with_content_mask(
                Some(ContentMask {
                    bounds: text_bounds,
                }),
                |window| {
                    for quad in prepaint.selection.drain(..) {
                        window.paint_quad(quad);
                    }
                    for (index, row) in frame.rows.iter().enumerate() {
                        let origin = point(
                            frame.text_left - prepaint.scroll.x,
                            bounds.top() + frame.line_height * (frame.first_line + index)
                                - prepaint.scroll.y,
                        );
                        let _ = row.shaped.paint(
                            origin,
                            frame.line_height,
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                    if let Some(caret) = prepaint.caret.take() {
                        window.paint_quad(caret);
                    }
                },
            );
        });
        let scroll = prepaint.scroll;
        self.editor.update(cx, |editor, _| {
            editor.scroll = scroll;
            editor.reveal = false;
            editor.frame = Some(frame);
        });
    }
}

impl Render for CodeEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .size_full()
            .overflow_hidden()
            .automation_id(self.id.to_string())
            .on_action(
                cx.listener(|this, _: &MoveLeft, _, cx| this.move_cursor(Motion::Left, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveRight, _, cx| {
                    this.move_cursor(Motion::Right, false, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &MoveUp, _, cx| this.move_cursor(Motion::Up, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveDown, _, cx| this.move_cursor(Motion::Down, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectLeft, _, cx| this.move_cursor(Motion::Left, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectRight, _, cx| {
                    this.move_cursor(Motion::Right, true, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &SelectUp, _, cx| this.move_cursor(Motion::Up, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectDown, _, cx| this.move_cursor(Motion::Down, true, cx)),
            )
            .on_action(cx.listener(|this, _: &LineStart, _, cx| {
                this.move_cursor(Motion::LineStart, false, cx)
            }))
            .on_action(
                cx.listener(|this, _: &LineEnd, _, cx| {
                    this.move_cursor(Motion::LineEnd, false, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &SelectToLineStart, _, cx| {
                this.move_cursor(Motion::LineStart, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectToLineEnd, _, cx| {
                this.move_cursor(Motion::LineEnd, true, cx)
            }))
            .on_action(cx.listener(|this, _: &DocumentStart, _, cx| {
                this.move_cursor(Motion::DocumentStart, false, cx)
            }))
            .on_action(cx.listener(|this, _: &DocumentEnd, _, cx| {
                this.move_cursor(Motion::DocumentEnd, false, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectToDocumentStart, _, cx| {
                this.move_cursor(Motion::DocumentStart, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectToDocumentEnd, _, cx| {
                this.move_cursor(Motion::DocumentEnd, true, cx)
            }))
            .on_action(
                cx.listener(|this, _: &PageUp, _, cx| this.move_cursor(Motion::PageUp, false, cx)),
            )
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                this.move_cursor(Motion::PageDown, false, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| {
                this.move_cursor(Motion::PageUp, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| {
                this.move_cursor(Motion::PageDown, true, cx)
            }))
            .on_action(cx.listener(|this, _: &WordLeft, _, cx| {
                this.move_cursor(Motion::WordLeft, false, cx)
            }))
            .on_action(cx.listener(|this, _: &WordRight, _, cx| {
                this.move_cursor(Motion::WordRight, false, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWordLeft, _, cx| {
                this.move_cursor(Motion::WordLeft, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWordRight, _, cx| {
                this.move_cursor(Motion::WordRight, true, cx)
            }))
            .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                this.delete_to(Buffer::previous_boundary, cx)
            }))
            .on_action(
                cx.listener(|this, _: &Delete, _, cx| this.delete_to(Buffer::next_boundary, cx)),
            )
            .on_action(cx.listener(|this, _: &DeleteWordLeft, _, cx| {
                this.delete_to(Buffer::previous_word_start, cx)
            }))
            .on_action(cx.listener(|this, _: &DeleteWordRight, _, cx| {
                this.delete_to(Buffer::next_word_end, cx)
            }))
            .on_action(cx.listener(|this, _: &Newline, _, cx| this.newline(cx)))
            .on_action(cx.listener(|this, _: &Indent, _, cx| this.indent(cx)))
            .on_action(cx.listener(|this, _: &Outdent, _, cx| this.shift_lines(false, cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.redo(cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &Cut, _, cx| this.cut(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(EditorElement {
                editor: cx.entity(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Syntax;

    /// A host refuses a shortcut on any editing key while a field has focus, so the
    /// editor's typing and editing keep working whatever the user binds.
    #[test]
    fn every_key_the_editor_binds_is_an_editing_key() {
        for macos in [false, true] {
            for binding in bindings(macos) {
                let [keystroke] = binding.keystrokes() else {
                    panic!("the editor binds single keystrokes");
                };
                assert!(
                    crate::text_input::editing_key(keystroke.inner()).is_some(),
                    "{} (macOS: {macos}) is in editing_keys",
                    keystroke.inner().unparse()
                );
            }
        }
    }

    #[test]
    fn tabs_map_between_buffer_columns_and_drawn_offsets() {
        let (text, tabs) = display_text("\ta\tb\r");
        assert_eq!(text.as_ref(), "    a    b ");
        assert_eq!(tabs, [0, 2]);
        assert_eq!(
            [0, 1, 2, 3, 4, 5].map(|column| display_offset(&tabs, column)),
            [0, 4, 5, 9, 10, 11]
        );
        assert_eq!(
            [0, 1, 2, 3, 4, 5, 9, 10].map(|offset| buffer_column(&tabs, offset)),
            [0, 0, 1, 1, 1, 2, 3, 4]
        );
    }

    #[test]
    fn spans_color_their_text_and_stale_spans_are_clamped() {
        let colors = crate::Colors::dark();
        let (text, syntax) = (colors.code_text, colors.syntax);
        let font = gpui::font("mono");
        let span = |range: Range<usize>, syntax| Span { range, syntax };
        let line = "\tlet é = 1;";
        let (shown, tabs) = display_text(line);
        let runs = line_runs(
            line,
            &tabs,
            &[
                span(1..4, Syntax::Keyword),
                span(6..9, Syntax::String),
                span(10..40, Syntax::Number),
            ],
            Some(9..10),
            &font,
            text,
            &syntax,
        );
        let lens: Vec<usize> = runs.iter().map(|run| run.len).collect();
        assert_eq!(lens, [4, 3, 1, 4, 1, 2]);
        assert_eq!(lens.iter().sum::<usize>(), shown.len());
        assert_eq!(runs[1].color, syntax.keyword);
        assert_eq!(
            runs[3].color, syntax.string,
            "a span starting inside é takes all of it"
        );
        assert_eq!(runs[4].color, text);
        assert!(runs[4].underline.is_some(), "marked text is underlined");
        assert_eq!(
            runs[5].color, syntax.number,
            "a span past the line ends with it"
        );
        let empty = line_runs(
            "",
            &[],
            &[span(3..5, Syntax::Comment)],
            None,
            &font,
            text,
            &syntax,
        );
        assert_eq!(empty.iter().map(|run| run.len).collect::<Vec<_>>(), [0]);
    }
}
