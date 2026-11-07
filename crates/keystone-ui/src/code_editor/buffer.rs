//! A file's text as lines, with grouped undo and the offsets an input method asks for.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use unicode_segmentation::UnicodeSegmentation;

/// A position in the text. `column` is a byte offset into the line, on a char boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Point {
    pub line: usize,
    pub column: usize,
}

impl Point {
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
}

/// Revisions are unique across buffers, so a host's saved revision never matches a
/// buffer it did not come from.
fn fresh_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// One replacement, in the coordinates of the text just before it.
struct Edit {
    start: Point,
    old: String,
    new: String,
}

/// What one undo takes back.
struct Step {
    edits: Vec<Edit>,
    revision_before: u64,
    revision_after: u64,
    /// The range the step first replaced, in the text before the step.
    selection_before: Range<Point>,
    /// The cursor after the step's last edit.
    cursor_after: Point,
}

/// The last step, while further typing at `at` can still join it.
#[derive(Clone, Copy)]
enum Open {
    Insert { at: Point, ends_in_space: bool },
    Delete { at: Point },
}

#[derive(Default)]
struct History {
    undo: Vec<Step>,
    redo: Vec<Step>,
    open: Option<Open>,
}

pub struct Buffer {
    /// Never empty.
    lines: Vec<String>,
    line_ending: LineEnding,
    utf16_lens: Vec<usize>,
    /// Where each line starts in UTF-16 units, counting one per line break. Entries from
    /// `utf16_valid` on are stale after an edit and rebuilt on the next lookup.
    utf16_starts: RefCell<Vec<usize>>,
    utf16_valid: Cell<usize>,
    history: History,
    revision: u64,
}

impl Buffer {
    /// Splits `text` into lines. A text whose every `'\n'` follows a `'\r'` reads as CRLF
    /// and writes back as CRLF; otherwise any `'\r'` stays in its line.
    pub fn new(text: &str) -> Self {
        let crlf = text.contains('\n')
            && text
                .match_indices('\n')
                .all(|(at, _)| text[..at].ends_with('\r'));
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        if crlf {
            let last = lines.len() - 1;
            for line in &mut lines[..last] {
                line.pop();
            }
        }
        let utf16_lens = lines.iter().map(|line| utf16_len(line)).collect();
        Self {
            lines,
            line_ending: if crlf {
                LineEnding::CrLf
            } else {
                LineEnding::Lf
            },
            utf16_lens,
            utf16_starts: RefCell::new(vec![0]),
            utf16_valid: Cell::new(1),
            history: History::default(),
            revision: fresh_revision(),
        }
    }

    /// The text as the file would hold it, with the line ending it was read with.
    pub fn text(&self) -> String {
        self.lines.join(match self.line_ending {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        })
    }

    /// The text with `'\n'` line breaks.
    pub fn text_lf(&self) -> String {
        self.lines.join("\n")
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, line: usize) -> &str {
        &self.lines[line]
    }

    pub fn end(&self) -> Point {
        let line = self.lines.len() - 1;
        Point::new(line, self.lines[line].len())
    }

    /// Identifies the text's state through its history: undo returns to the previous
    /// state's revision and redo to the next one's.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The nearest valid position at or before `point`.
    pub fn clamp(&self, point: Point) -> Point {
        let line = point.line.min(self.lines.len() - 1);
        let text = &self.lines[line];
        let mut column = point.column.min(text.len());
        while !text.is_char_boundary(column) {
            column -= 1;
        }
        Point::new(line, column)
    }

    fn ordered(&self, range: Range<Point>) -> (Point, Point) {
        let (a, b) = (self.clamp(range.start), self.clamp(range.end));
        (a.min(b), a.max(b))
    }

    /// The text between two positions, with `'\n'` line breaks.
    pub fn text_in(&self, range: Range<Point>) -> String {
        let (start, end) = self.ordered(range);
        if start.line == end.line {
            return self.lines[start.line][start.column..end.column].to_owned();
        }
        let mut text = self.lines[start.line][start.column..].to_owned();
        for line in &self.lines[start.line + 1..end.line] {
            text.push('\n');
            text.push_str(line);
        }
        text.push('\n');
        text.push_str(&self.lines[end.line][..end.column]);
        text
    }

    /// Replaces `range` with `text`, whose `"\r\n"` and `'\n'` break lines, and returns
    /// where the new text now lies. Typing that continues the last edit joins its undo
    /// step: inserted characters at its end, unless a space or line break follows other
    /// characters, and deletions next to the last one.
    pub fn replace(&mut self, range: Range<Point>, text: &str) -> Range<Point> {
        let (start, end) = self.ordered(range);
        let new: Cow<str> = if text.contains("\r\n") {
            text.replace("\r\n", "\n").into()
        } else {
            text.into()
        };
        let old = self.text_in(start..end);
        if old == new {
            return start..end;
        }
        let new = new.into_owned();
        let new_end = advance(start, &new);
        self.splice(start, end, &new);
        self.record(start, end, old, new, new_end);
        start..new_end
    }

    fn record(&mut self, start: Point, end: Point, old: String, new: String, new_end: Point) {
        self.history.redo.clear();
        let revision = fresh_revision();
        let joins = match (self.history.open, old.is_empty(), new.is_empty()) {
            (Some(Open::Insert { at, ends_in_space }), true, false) => {
                at == start && (ends_in_space || !starts_with_space(&new))
            }
            (Some(Open::Delete { at }), false, true) => at == start || at == end,
            _ => false,
        };
        let step = match self.history.undo.last_mut() {
            Some(step) if joins => {
                // A deletion before the group widens what undo selects to its left;
                // one after it widens it to the right.
                if new.is_empty() && end == step.cursor_after {
                    step.selection_before.start = start;
                } else if new.is_empty() {
                    step.selection_before.end = advance(step.selection_before.end, &old);
                }
                step
            }
            _ => {
                self.history.undo.push(Step {
                    edits: Vec::new(),
                    revision_before: self.revision,
                    revision_after: revision,
                    selection_before: start..end,
                    cursor_after: new_end,
                });
                self.history.undo.last_mut().unwrap()
            }
        };
        self.history.open = Some(if new.is_empty() {
            Open::Delete { at: start }
        } else {
            Open::Insert {
                at: new_end,
                ends_in_space: new.ends_with(char::is_whitespace),
            }
        });
        step.revision_after = revision;
        step.cursor_after = new_end;
        step.edits.push(Edit { start, old, new });
        self.revision = revision;
    }

    /// Makes the next edit a step of its own, as after the cursor moves.
    pub fn break_group(&mut self) {
        self.history.open = None;
    }

    /// Takes back the last step and returns the selection before it.
    pub fn undo(&mut self) -> Option<Range<Point>> {
        let step = self.history.undo.pop()?;
        for edit in step.edits.iter().rev() {
            let end = advance(edit.start, &edit.new);
            self.splice(edit.start, end, &edit.old);
        }
        self.revision = step.revision_before;
        self.history.open = None;
        let selection = step.selection_before.clone();
        self.history.redo.push(step);
        Some(selection)
    }

    /// Applies the last undone step again and returns the cursor after it.
    pub fn redo(&mut self) -> Option<Range<Point>> {
        let step = self.history.redo.pop()?;
        for edit in &step.edits {
            let end = advance(edit.start, &edit.old);
            self.splice(edit.start, end, &edit.new);
        }
        self.revision = step.revision_after;
        self.history.open = None;
        let cursor = step.cursor_after;
        self.history.undo.push(step);
        Some(cursor..cursor)
    }

    fn splice(&mut self, start: Point, end: Point, new: &str) {
        let suffix = self.lines[end.line][end.column..].to_owned();
        let mut pieces = new.split('\n');
        let mut first = self.lines[start.line][..start.column].to_owned();
        first.push_str(pieces.next().unwrap_or_default());
        let mut replacement = vec![first];
        replacement.extend(pieces.map(str::to_owned));
        replacement.last_mut().unwrap().push_str(&suffix);
        let lens: Vec<usize> = replacement.iter().map(|line| utf16_len(line)).collect();
        self.lines.splice(start.line..=end.line, replacement);
        self.utf16_lens.splice(start.line..=end.line, lens);
        let valid = self.utf16_valid.get().min(start.line + 1);
        self.utf16_valid.set(valid);
        self.utf16_starts.get_mut().truncate(valid);
    }

    fn utf16_starts(&self) -> std::cell::Ref<'_, Vec<usize>> {
        let valid = self.utf16_valid.get();
        if valid < self.lines.len() {
            let mut starts = self.utf16_starts.borrow_mut();
            starts.truncate(valid);
            for line in valid..self.lines.len() {
                let start = starts[line - 1] + self.utf16_lens[line - 1] + 1;
                starts.push(start);
            }
            self.utf16_valid.set(self.lines.len());
        }
        self.utf16_starts.borrow()
    }

    /// The offset of `point` in UTF-16 units from the start of the text.
    pub fn utf16_offset(&self, point: Point) -> usize {
        let point = self.clamp(point);
        self.utf16_starts()[point.line] + utf16_len(&self.lines[point.line][..point.column])
    }

    /// The position at a UTF-16 offset from the start of the text. An offset inside a
    /// character or a line break lands after it.
    pub fn point_at_utf16(&self, offset: usize) -> Point {
        let line = self
            .utf16_starts()
            .partition_point(|&start| start <= offset)
            - 1;
        let mut remaining = offset - self.utf16_starts()[line];
        let text = &self.lines[line];
        for (column, ch) in text.char_indices() {
            if remaining == 0 {
                return Point::new(line, column);
            }
            remaining = remaining.saturating_sub(ch.len_utf16());
        }
        Point::new(line, text.len())
    }

    /// The grapheme boundary before `point`, or the end of the line above.
    pub fn previous_boundary(&self, point: Point) -> Point {
        if point.column == 0 {
            return match point.line {
                0 => point,
                line => Point::new(line - 1, self.lines[line - 1].len()),
            };
        }
        let text = &self.lines[point.line];
        let column = text
            .grapheme_indices(true)
            .map(|(at, _)| at)
            .take_while(|&at| at < point.column)
            .last()
            .unwrap_or(0);
        Point::new(point.line, column)
    }

    /// The grapheme boundary after `point`, or the start of the line below.
    pub fn next_boundary(&self, point: Point) -> Point {
        let text = &self.lines[point.line];
        if point.column >= text.len() {
            return if point.line + 1 < self.lines.len() {
                Point::new(point.line + 1, 0)
            } else {
                point
            };
        }
        let column = text
            .grapheme_indices(true)
            .map(|(at, _)| at)
            .find(|&at| at > point.column)
            .unwrap_or(text.len());
        Point::new(point.line, column)
    }

    /// The start of the word before `point`, or the end of the line above at a line start.
    pub fn previous_word_start(&self, point: Point) -> Point {
        if point.column == 0 {
            return self.previous_boundary(point);
        }
        let text = &self.lines[point.line];
        let column = segments(text)
            .filter(|range| range.start < point.column && is_word(&text[range.clone()]))
            .last()
            .map_or(0, |range| range.start);
        Point::new(point.line, column)
    }

    /// The end of the word after `point`, or the start of the line below at a line end.
    pub fn next_word_end(&self, point: Point) -> Point {
        let text = &self.lines[point.line];
        if point.column >= text.len() {
            return self.next_boundary(point);
        }
        let column = segments(text)
            .find(|range| range.end > point.column && is_word(&text[range.clone()]))
            .map_or(text.len(), |range| range.end);
        Point::new(point.line, column)
    }

    /// The word, run of spaces, or punctuation mark at `point`.
    pub fn word_at(&self, point: Point) -> Range<Point> {
        let text = &self.lines[point.line];
        let bounds = segments(text)
            .find(|range| range.contains(&point.column))
            .or_else(|| segments(text).last())
            .unwrap_or(0..0);
        Point::new(point.line, bounds.start)..Point::new(point.line, bounds.end)
    }

    /// The column after the line's leading spaces and tabs.
    pub fn indent_end(&self, line: usize) -> usize {
        let text = &self.lines[line];
        text.len() - text.trim_start_matches([' ', '\t']).len()
    }

    /// A tab if the file's first indented line starts with one, otherwise four spaces.
    pub fn indent_unit(&self) -> &'static str {
        let first = self
            .lines
            .iter()
            .find_map(|line| line.chars().next().filter(|c| *c == ' ' || *c == '\t'));
        if first == Some('\t') { "\t" } else { "    " }
    }

    /// Where `point` is drawn, in columns: a tab takes four and any other grapheme one.
    pub fn display_column(&self, point: Point) -> usize {
        self.lines[point.line][..point.column]
            .graphemes(true)
            .map(display_width)
            .sum()
    }

    /// The position on `line` drawn nearest `column`.
    pub fn point_at_display_column(&self, line: usize, column: usize) -> Point {
        let text = &self.lines[line];
        let mut drawn = 0;
        for (at, grapheme) in text.grapheme_indices(true) {
            let width = display_width(grapheme);
            if drawn + width > column {
                let past = column - drawn;
                let at = if past * 2 <= width {
                    at
                } else {
                    at + grapheme.len()
                };
                return Point::new(line, at);
            }
            drawn += width;
        }
        Point::new(line, text.len())
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn display_width(grapheme: &str) -> usize {
    if grapheme == "\t" { 4 } else { 1 }
}

fn starts_with_space(text: &str) -> bool {
    text.starts_with(char::is_whitespace)
}

fn is_word(segment: &str) -> bool {
    segment.chars().any(|c| c.is_alphanumeric() || c == '_')
}

/// The line's words, runs of spaces, and punctuation marks, by UAX 29 except that a word
/// splits at punctuation inside it: code reads `x.len` and `a::b` as three parts each.
fn segments(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    text.split_word_bound_indices().flat_map(|(at, segment)| {
        let mut cuts = vec![at];
        let mut previous = None;
        for (offset, grapheme) in segment.grapheme_indices(true) {
            let word = is_word(grapheme);
            if previous.is_some_and(|previous| previous != word) {
                cuts.push(at + offset);
            }
            previous = Some(word);
        }
        cuts.push(at + segment.len());
        cuts.windows(2)
            .map(|pair| pair[0]..pair[1])
            .collect::<Vec<_>>()
    })
}

/// Where `text` ends when inserted at `start`.
fn advance(start: Point, text: &str) -> Point {
    match text.rfind('\n') {
        Some(last) => Point::new(
            start.line + text.matches('\n').count(),
            text.len() - last - 1,
        ),
        None => Point::new(start.line, start.column + text.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: usize, column: usize) -> Point {
        Point::new(line, column)
    }

    #[test]
    fn unedited_text_round_trips_byte_for_byte() {
        for text in [
            "",
            "one",
            "one\ntwo",
            "one\ntwo\n",
            "one\r\ntwo",
            "one\r\ntwo\r\n",
            "one\r\ntwo\nthree\r\n",
            "a\rb\nc",
            "\r\n",
            "\n\n",
            "tail\r",
            "a\r\nb\r",
        ] {
            assert_eq!(Buffer::new(text).text(), text, "{text:?}");
        }
    }

    #[test]
    fn crlf_files_keep_crlf_and_mixed_files_keep_their_carriage_returns() {
        let mut crlf = Buffer::new("one\r\ntwo\r\n");
        assert_eq!(crlf.line_count(), 3);
        assert_eq!(crlf.line(0), "one");
        crlf.replace(p(1, 3)..p(1, 3), "\nthree");
        assert_eq!(crlf.text(), "one\r\ntwo\r\nthree\r\n");

        let mut mixed = Buffer::new("one\r\ntwo\n");
        assert_eq!(mixed.line(0), "one\r");
        mixed.replace(p(2, 0)..p(2, 0), "x");
        assert_eq!(mixed.text(), "one\r\ntwo\nx");
    }

    #[test]
    fn replace_spans_lines_and_returns_where_the_text_landed() {
        let mut buffer = Buffer::new("alpha\nbeta\ngamma");
        let landed = buffer.replace(p(0, 2)..p(2, 3), "X\r\nY\nZ");
        assert_eq!(buffer.text(), "alX\nY\nZma");
        assert_eq!(landed, p(0, 2)..p(2, 1));
        let landed = buffer.replace(p(1, 0)..p(0, 3), "");
        assert_eq!(buffer.text(), "alXY\nZma");
        assert_eq!(landed, p(0, 3)..p(0, 3));
        assert_eq!(buffer.text_in(p(0, 1)..p(1, 2)), "lXY\nZm");
    }

    #[test]
    fn positions_out_of_range_or_inside_a_character_are_clamped() {
        let mut buffer = Buffer::new("é\nab");
        assert_eq!(buffer.clamp(p(0, 1)), p(0, 0));
        assert_eq!(buffer.clamp(p(9, 9)), p(1, 2));
        buffer.replace(p(0, 1)..p(7, 0), "x");
        assert_eq!(buffer.text(), "xab");
    }

    #[test]
    fn typing_groups_by_word_and_a_delete_starts_a_new_step() {
        let mut buffer = Buffer::new("");
        for (column, ch) in "let x".char_indices() {
            buffer.replace(p(0, column)..p(0, column), &ch.to_string());
        }
        buffer.replace(p(0, 5)..p(0, 4), "");
        assert_eq!(buffer.text(), "let ");
        assert_eq!(buffer.undo(), Some(p(0, 4)..p(0, 5)));
        assert_eq!(buffer.text(), "let x");
        assert_eq!(buffer.undo(), Some(p(0, 3)..p(0, 3)));
        assert_eq!(buffer.text(), "let");
        assert_eq!(buffer.undo(), Some(p(0, 0)..p(0, 0)));
        assert_eq!(buffer.text(), "");
        assert_eq!(buffer.undo(), None);
        assert_eq!(buffer.redo(), Some(p(0, 3)..p(0, 3)));
        assert_eq!(buffer.redo(), Some(p(0, 5)..p(0, 5)));
        assert_eq!(buffer.text(), "let x");
    }

    #[test]
    fn a_cursor_jump_or_a_new_line_ends_the_step() {
        let mut buffer = Buffer::new("");
        buffer.replace(p(0, 0)..p(0, 0), "a");
        buffer.break_group();
        buffer.replace(p(0, 1)..p(0, 1), "b");
        buffer.replace(p(0, 2)..p(0, 2), "\n");
        buffer.replace(p(1, 0)..p(1, 0), "c");
        buffer.undo();
        assert_eq!(buffer.text(), "ab");
        buffer.undo();
        assert_eq!(buffer.text(), "a");
    }

    #[test]
    fn undoing_deletions_selects_what_comes_back() {
        let mut buffer = Buffer::new("abcdef");
        for column in [4, 3, 2] {
            buffer.replace(p(0, column - 1)..p(0, column), "");
        }
        buffer.replace(p(0, 1)..p(0, 2), "");
        assert_eq!(buffer.text(), "af");
        assert_eq!(buffer.undo(), Some(p(0, 1)..p(0, 5)));
        assert_eq!(buffer.text(), "abcdef");

        buffer.replace(p(0, 1)..p(0, 4), "X");
        buffer.replace(p(0, 2)..p(0, 2), "Y");
        assert_eq!(buffer.text(), "aXYef");
        assert_eq!(
            buffer.undo(),
            Some(p(0, 1)..p(0, 4)),
            "typing over a selection"
        );
        assert_eq!(buffer.text(), "abcdef");
    }

    #[test]
    fn undoing_back_to_the_saved_revision_reads_clean() {
        let mut buffer = Buffer::new("saved");
        let saved = buffer.revision();
        buffer.replace(p(0, 5)..p(0, 5), "!");
        assert_ne!(buffer.revision(), saved);
        buffer.undo();
        assert_eq!(buffer.revision(), saved);
        buffer.redo();
        let edited = buffer.revision();
        assert_ne!(edited, saved);
        buffer.replace(p(0, 6)..p(0, 6), "?");
        assert_ne!(buffer.revision(), edited, "every edit is a new state");
        buffer.undo();
        assert_eq!(buffer.revision(), edited);
        buffer.undo();
        assert_eq!(buffer.revision(), saved);
        assert_ne!(Buffer::new("saved").revision(), saved);
        buffer.replace(p(0, 0)..p(0, 0), "");
        assert_eq!(buffer.revision(), saved, "an edit that changes nothing");
    }

    #[test]
    fn utf16_offsets_count_emoji_as_two_and_line_breaks_as_one() {
        let mut buffer = Buffer::new("a😀b\n界\nz");
        assert_eq!(buffer.utf16_offset(p(0, 5)), 3);
        assert_eq!(buffer.utf16_offset(p(1, 0)), 5);
        assert_eq!(buffer.utf16_offset(p(1, 3)), 6);
        assert_eq!(buffer.utf16_offset(p(2, 1)), 8);
        assert_eq!(buffer.point_at_utf16(3), p(0, 5));
        assert_eq!(
            buffer.point_at_utf16(2),
            p(0, 5),
            "inside the surrogate pair"
        );
        assert_eq!(buffer.point_at_utf16(6), p(1, 3));
        assert_eq!(buffer.point_at_utf16(99), p(2, 1));
        buffer.replace(p(0, 0)..p(0, 0), "😀\n");
        assert_eq!(buffer.utf16_offset(p(3, 1)), 11);
        assert_eq!(buffer.point_at_utf16(10), p(3, 0));
        buffer.replace(p(0, 0)..p(2, 0), "");
        assert_eq!(buffer.point_at_utf16(1), p(0, 3));
        assert_eq!(buffer.point_at_utf16(2), p(1, 0));
        assert_eq!(buffer.utf16_offset(p(1, 1)), 3);
    }

    #[test]
    fn utf16_offsets_stay_in_range_after_lines_join() {
        let mut buffer = Buffer::new("a\nb");
        assert_eq!(buffer.point_at_utf16(2), p(1, 0));
        buffer.replace(p(0, 1)..p(1, 0), "");
        assert_eq!(buffer.text(), "ab");
        assert_eq!(buffer.point_at_utf16(2), p(0, 2));
        assert_eq!(buffer.utf16_offset(p(0, 2)), 2);
    }

    #[test]
    fn word_moves_skip_punctuation_and_cross_lines() {
        let buffer = Buffer::new("let foo_bar = x.len();\nnext");
        assert_eq!(buffer.next_word_end(p(0, 0)), p(0, 3));
        assert_eq!(buffer.next_word_end(p(0, 3)), p(0, 11));
        assert_eq!(buffer.next_word_end(p(0, 11)), p(0, 15));
        assert_eq!(buffer.next_word_end(p(0, 19)), p(0, 22));
        assert_eq!(buffer.next_word_end(p(0, 22)), p(1, 0));
        assert_eq!(buffer.previous_word_start(p(0, 14)), p(0, 4));
        assert_eq!(buffer.previous_word_start(p(1, 0)), p(0, 22));
        assert_eq!(buffer.word_at(p(0, 6)), p(0, 4)..p(0, 11));
        assert_eq!(buffer.word_at(p(0, 22)), p(0, 21)..p(0, 22));
    }

    #[test]
    fn display_columns_count_a_tab_as_four() {
        let buffer = Buffer::new("\tab\né😀x");
        assert_eq!(buffer.display_column(p(0, 2)), 5);
        assert_eq!(buffer.point_at_display_column(0, 5), p(0, 2));
        assert_eq!(buffer.point_at_display_column(0, 1), p(0, 0));
        assert_eq!(buffer.point_at_display_column(0, 3), p(0, 1));
        assert_eq!(buffer.point_at_display_column(1, 2), p(1, 6));
        assert_eq!(buffer.point_at_display_column(1, 9), p(1, 7));
        assert_eq!(buffer.indent_unit(), "\t");
        assert_eq!(Buffer::new("a\n  b\n\tc").indent_unit(), "    ");
    }
}
