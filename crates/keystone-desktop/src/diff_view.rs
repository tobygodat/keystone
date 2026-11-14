//! A read-only diff in the conversation pane: what one turn changed, or the working
//! tree against HEAD. Lines are selected as in a Files view, comments are left on them,
//! and the comments go to the session as one message. The patch is read and laid out
//! off the UI thread, and only the rows in view are drawn.

use crate::Keystone;
use crate::view_state::ViewKey;
use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, KeyBinding,
    ListAlignment, ListOffset, ListState, MouseButton, Pixels, Render, WeakEntity, Window, div,
    list, prelude::*, px,
};
use keystone_agent::checkpoint;
use keystone_agent::diff::{Change, FileDiff, Kind, Line, Patch};
use keystone_ui::prelude::*;
use keystone_ui::text_input::{InputMode, Newline};
use std::collections::HashMap;
use std::path::PathBuf;

gpui::actions!(
    diff_view,
    [
        Up,
        Down,
        SelectUp,
        SelectDown,
        NextFile,
        PreviousFile,
        Comment,
        ViewChanges,
        ViewUncommitted
    ]
);

const CONTEXT: &str = "DiffView";
/// The comment box, outside [`CONTEXT`] so typing in it never moves the diff.
const COMMENT_CONTEXT: &str = "DiffComment";

/// Characters of a line drawn; the rest of a longer line is still quoted in a comment.
const MAX_SHOWN: usize = 2000;

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("shift-up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("shift-down", SelectDown, Some(CONTEXT)),
        KeyBinding::new("n", NextFile, Some(CONTEXT)),
        KeyBinding::new("p", PreviousFile, Some(CONTEXT)),
        KeyBinding::new("c", Comment, Some(CONTEXT)),
        KeyBinding::new("enter", Comment, Some(CONTEXT)),
    ]);
}

/// What a diff shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Turn(u64),
    WorkingTree,
    /// One agent edit, from the transcript row at this place. Read only: an edit takes no
    /// line comments, since the turn's diff already does.
    Edit(usize),
}

impl Source {
    fn title(self) -> String {
        match self {
            Self::Turn(turn) => format!("Changes in turn {turn}"),
            Self::WorkingTree => "Uncommitted changes".into(),
            Self::Edit(_) => "Changes in this edit".into(),
        }
    }
}

/// Reads the patch on a background thread. `None` is a turn whose start was not saved.
pub(crate) type Load = Box<dyn FnOnce() -> Result<Option<Patch>, String> + Send>;

enum Content {
    Loading,
    Ready(Patch),
    NoStart,
    Failed(String),
}

/// One line of a patch: its file, its hunk in that file and its place in that hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct LineAt {
    file: usize,
    hunk: usize,
    line: usize,
}

/// One row of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    File(usize),
    Hunk(usize, usize),
    Line(LineAt),
    Comment(usize),
    Binary,
    /// The patch was cut at its limit here.
    Truncated,
    /// Nothing changed.
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineComment {
    first: LineAt,
    last: LineAt,
    text: String,
}

/// Selected rows: where the selection started and where it ends now.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Selected {
    anchor: usize,
    head: usize,
}

impl Selected {
    fn range(self) -> std::ops::RangeInclusive<usize> {
        self.anchor.min(self.head)..=self.anchor.max(self.head)
    }
}

/// The rows of `patch` with `comments` after the line each ends on, and the row each
/// file starts at.
fn layout(patch: &Patch, comments: &[LineComment]) -> (Vec<Row>, Vec<usize>) {
    let mut after: HashMap<LineAt, Vec<usize>> = HashMap::new();
    for (index, comment) in comments.iter().enumerate() {
        after.entry(comment.last).or_default().push(index);
    }
    let mut rows = Vec::new();
    let mut starts = Vec::with_capacity(patch.files.len());
    for (file, diff) in patch.files.iter().enumerate() {
        starts.push(rows.len());
        rows.push(Row::File(file));
        if diff.binary {
            rows.push(Row::Binary);
        }
        for (hunk, lines) in diff.hunks.iter().enumerate() {
            rows.push(Row::Hunk(file, hunk));
            for line in 0..lines.lines.len() {
                let at = LineAt { file, hunk, line };
                rows.push(Row::Line(at));
                if let Some(comments) = after.get(&at) {
                    rows.extend(comments.iter().map(|&index| Row::Comment(index)));
                }
            }
        }
    }
    if patch.truncated {
        rows.push(Row::Truncated);
    } else if patch.files.is_empty() {
        rows.push(Row::Empty);
    }
    (rows, starts)
}

fn line(patch: &Patch, at: LineAt) -> &Line {
    &patch.files[at.file].hunks[at.hunk].lines[at.line]
}

/// "line 12", "lines 12-14", or for lines only on the old side, "removed lines 3-4".
fn lines_label(lines: &[&Line]) -> String {
    let new: Vec<u32> = lines.iter().filter_map(|line| line.new).collect();
    let (numbers, removed) = if new.is_empty() {
        (lines.iter().filter_map(|line| line.old).collect(), true)
    } else {
        (new, false)
    };
    let (Some(first), Some(last)) = (numbers.first(), numbers.last()) else {
        return String::new();
    };
    let prefix = if removed { "removed " } else { "" };
    if first == last {
        format!("{prefix}line {first}")
    } else {
        format!("{prefix}lines {first}-{last}")
    }
}

/// The message the comments send: each with its file, its lines as the diff shows them,
/// and what was said.
fn review_prompt(source: Source, patch: &Patch, comments: &[LineComment]) -> String {
    let mut prompt = String::from(match source {
        Source::Turn(_) | Source::Edit(_) => "Comments on your changes:",
        Source::WorkingTree => "Comments on the uncommitted changes:",
    });
    for comment in comments {
        let file = &patch.files[comment.first.file];
        let lines: Vec<&Line> = file
            .hunks
            .iter()
            .enumerate()
            .flat_map(|(hunk, lines)| {
                (0..lines.lines.len()).map(move |line| LineAt {
                    file: comment.first.file,
                    hunk,
                    line,
                })
            })
            .filter(|at| (comment.first..=comment.last).contains(at))
            .map(|at| line(patch, at))
            .collect();
        let quoted: Vec<String> = lines
            .iter()
            .map(|line| {
                let sign = match line.kind {
                    Kind::Added => '+',
                    Kind::Removed => '-',
                    Kind::Context => ' ',
                };
                format!("{sign}{}", line.text)
            })
            .collect();
        let quoted = keystone_agent::cap_output(&quoted.join("\n"));
        let ticks = quoted.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(ticks.max(2) + 1);
        prompt.push_str(&format!(
            "\n\n{} {}:\n{fence}diff\n{quoted}\n{fence}\n{}",
            file.path,
            lines_label(&lines),
            comment.text.trim()
        ));
    }
    prompt
}

/// A line as drawn: tabs as four spaces, and at most [`MAX_SHOWN`] characters.
pub(crate) fn shown(text: &str) -> String {
    let mut shown: String = text.chars().take(MAX_SHOWN).collect();
    if shown.len() < text.len() {
        shown.push('…');
    }
    shown.replace('\t', "    ")
}

/// The width of a line number column for numbers up to `biggest`, in the mono font at
/// [`TextSize::Sm`].
pub(crate) fn gutter(biggest: u32, cx: &App) -> Pixels {
    let text = cx.text_system();
    let font = text.resolve_font(&gpui::font(cx.theme().fonts.mono.clone()));
    let digit = text
        .advance(font, TextSize::Sm.px(), '0')
        .map_or(TextSize::Sm.px(), |size| size.width);
    (digit * biggest.to_string().len() as f32).ceil() + px(16.)
}

/// A hunk's `@@` line. Like [`line_row`], it goes under the mono font at
/// [`TextSize::Sm`].
pub(crate) fn hunk_row(header: &str, cx: &App) -> gpui::Div {
    div()
        .px(px(8.))
        .py(px(2.))
        .text_color(cx.theme().colors.text_faint)
        .child(shown(header))
}

/// One line of a diff: its old and new numbers in a `gutter` wide column each, its mark,
/// and its text, on the added or removed wash. A line the file ends on without a newline
/// is followed by a muted `\ No newline at end of file`.
pub(crate) fn line_row(line: &Line, gutter: Pixels, selected: bool, cx: &App) -> gpui::Div {
    line_row_with(line, shown(&line.text), gutter, selected, cx)
}

/// [`line_row`] with the line's text drawn as `text`, such as with find's marks.
pub(crate) fn line_row_with(
    line: &Line,
    text: impl IntoElement,
    gutter: Pixels,
    selected: bool,
    cx: &App,
) -> gpui::Div {
    let colors = cx.theme().colors;
    let (sign, mark, wash) = match line.kind {
        Kind::Added => ("+", colors.success, Some(colors.diff_added)),
        Kind::Removed => ("-", colors.danger, Some(colors.diff_removed)),
        Kind::Context => (" ", colors.text_muted, None),
    };
    let number = |n: Option<u32>| {
        div()
            .flex_none()
            .w(gutter)
            .pr(px(8.))
            .text_right()
            .text_color(colors.text_faint)
            .child(n.map(|n| n.to_string()).unwrap_or_default())
    };
    let line_row = h_flex()
        .items_start()
        .w_full()
        .pl(px(4.))
        .min_h(TextSize::Sm.line_height())
        .when_some(wash, |d, wash| d.bg(wash))
        .when(selected, |d| d.bg(colors.selection))
        .child(number(line.old))
        .child(number(line.new))
        .child(div().flex_none().w(px(16.)).text_color(mark).child(sign))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pr(px(8.))
                .text_color(colors.code_text)
                .child(text),
        );
    v_flex()
        .w_full()
        .child(line_row)
        .when(line.no_newline, |d| {
            d.child(
                div()
                    .pl(gutter * 2. + px(20.))
                    .min_h(TextSize::Sm.line_height())
                    .text_color(colors.text_faint)
                    .child(keystone_agent::diff::NO_NEWLINE),
            )
        })
}

pub(crate) struct DiffView {
    session: u64,
    source: Source,
    content: Content,
    rows: Vec<Row>,
    /// The row each file starts at.
    starts: Vec<usize>,
    comments: Vec<LineComment>,
    selection: Option<Selected>,
    /// The comment being written, for the lines it was opened on.
    draft: Option<(LineAt, LineAt, Entity<TextInput>)>,
    /// A file to show once the patch arrives.
    reveal: Option<String>,
    /// The file Next or Previous file last showed.
    jumped: Option<usize>,
    /// The longest line number, for the gutter's width.
    biggest: u32,
    dragging: bool,
    _loading: Option<gpui::Task<()>>,
    list: ListState,
    focus: FocusHandle,
    gutter: Pixels,
    notice: Option<(String, Color)>,
    keystone: WeakEntity<Keystone>,
}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DiffView {
    fn new(
        session: u64,
        source: Source,
        load: Load,
        reveal: Option<String>,
        keystone: WeakEntity<Keystone>,
        cx: &mut Context<Self>,
    ) -> Self {
        let read = cx.background_spawn(async move {
            let patch = load()?;
            Ok(patch.map(|patch| {
                let (rows, starts) = layout(&patch, &[]);
                let biggest = patch
                    .files
                    .iter()
                    .flat_map(|file| &file.hunks)
                    .flat_map(|hunk| &hunk.lines)
                    .flat_map(|line| line.old.max(line.new))
                    .max()
                    .unwrap_or(1);
                (patch, rows, starts, biggest)
            }))
        });
        let loading = cx.spawn(async move |this, cx| {
            let read: Result<_, String> = read.await;
            let _ = this.update(cx, |this, cx| {
                match read {
                    Ok(Some((patch, rows, starts, biggest))) => {
                        this.biggest = biggest;
                        this.list.reset(rows.len());
                        (this.rows, this.starts) = (rows, starts);
                        this.content = Content::Ready(patch);
                        if let Some(path) = this.reveal.take() {
                            this.reveal_file(&path);
                        }
                    }
                    Ok(None) => this.content = Content::NoStart,
                    Err(error) => this.content = Content::Failed(error),
                }
                this._loading = None;
                cx.notify();
            });
        });
        Self {
            session,
            source,
            content: Content::Loading,
            rows: Vec::new(),
            starts: Vec::new(),
            comments: Vec::new(),
            selection: None,
            draft: None,
            reveal,
            jumped: None,
            biggest: 1,
            dragging: false,
            _loading: Some(loading),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            focus: cx.focus_handle(),
            gutter: px(0.),
            notice: None,
            keystone,
        }
    }

    fn patch(&self) -> Option<&Patch> {
        match &self.content {
            Content::Ready(patch) => Some(patch),
            _ => None,
        }
    }

    /// The file row `row` is in.
    fn file_at(&self, row: usize) -> Option<usize> {
        self.starts
            .partition_point(|start| *start <= row)
            .checked_sub(1)
    }

    /// The file the selection is in, else the one last jumped to, else the one at the
    /// top of the view. A file with no lines leaves no selection, and the list can't
    /// always scroll the last files to its top, so the jump itself is remembered.
    fn current_file(&self) -> Option<usize> {
        match self.selection {
            Some(selection) => self.file_at(selection.head),
            None => self
                .jumped
                .or_else(|| self.file_at(self.list.logical_scroll_top().item_ix)),
        }
    }

    fn selected_lines(&self) -> Option<(LineAt, LineAt)> {
        let selection = self.selection?;
        let mut lines = self.rows[selection.range()]
            .iter()
            .filter_map(|row| match row {
                Row::Line(at) => Some(*at),
                _ => None,
            });
        let first = lines.next()?;
        Some((first, lines.next_back().unwrap_or(first)))
    }

    /// The next line row from `from` in `step`'s direction, within `file` when given.
    fn line_from(&self, from: usize, step: isize, file: Option<usize>) -> Option<usize> {
        let mut row = from;
        loop {
            row = row.checked_add_signed(step)?;
            match self.rows.get(row)? {
                Row::Line(at) if file.is_none_or(|file| at.file == file) => return Some(row),
                Row::Line(_) => return None,
                _ => {}
            }
        }
    }

    fn select(&mut self, row: usize, extend: bool, cx: &mut Context<Self>) {
        self.selection = Some(match self.selection {
            Some(selection) if extend => Selected {
                head: row,
                ..selection
            },
            _ => Selected {
                anchor: row,
                head: row,
            },
        });
        cx.notify();
    }

    fn step(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        let step = if down { 1 } else { -1 };
        let row = match self.selection {
            // From the file Next or Previous file showed, which may have no lines and sit
            // below the top of the view, else from the top of the view.
            None => {
                let from = self
                    .jumped
                    .and_then(|file| self.starts.get(file).copied())
                    .unwrap_or_else(|| self.list.logical_scroll_top().item_ix);
                self.line_from(from.saturating_sub(1), 1, None)
                    .or_else(|| self.line_from(from, -1, None))
            }
            Some(Selected { anchor, head }) => {
                // A selection stays in one file, so a comment names one path.
                let file = extend.then(|| self.file_at(anchor)).flatten();
                self.line_from(head, step, file)
            }
        };
        if let Some(row) = row {
            self.select(row, extend, cx);
            self.list.scroll_to_reveal_item(row);
        }
    }

    /// Selects the first line of the file `offset` files from the current one, and
    /// shows that file from its top.
    fn jump_file(&mut self, offset: isize, cx: &mut Context<Self>) {
        let count = self.starts.len();
        if count == 0 {
            return;
        }
        let file = match self.current_file() {
            Some(file) => file.saturating_add_signed(offset).min(count - 1),
            None => 0,
        };
        self.show_file(file);
        cx.notify();
    }

    fn show_file(&mut self, file: usize) {
        self.jumped = Some(file);
        let start = self.starts[file];
        self.selection = self.line_from(start, 1, Some(file)).map(|row| Selected {
            anchor: row,
            head: row,
        });
        self.list.scroll_to(ListOffset {
            item_ix: start,
            offset_in_item: px(0.),
        });
    }

    fn reveal_file(&mut self, path: &str) {
        let file = self
            .patch()
            .and_then(|patch| patch.files.iter().position(|file| file.path == path));
        if let Some(file) = file {
            self.show_file(file);
        }
    }

    fn press(&mut self, row: usize, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if !matches!(self.rows.get(row), Some(Row::Line(_))) {
            return;
        }
        let other_file = self
            .selection
            .is_some_and(|s| self.file_at(s.anchor) != self.file_at(row));
        self.dragging = true;
        self.select(row, extend && !other_file, cx);
    }

    fn drag_to(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            return;
        };
        if selection.head != row
            && matches!(self.rows.get(row), Some(Row::Line(_)))
            && self.file_at(selection.anchor) == self.file_at(row)
        {
            self.select(row, true, cx);
        }
    }

    /// Opens the comment box for the selected lines.
    /// One edit's diff takes no comments.
    fn read_only(&self) -> bool {
        matches!(self.source, Source::Edit(_))
    }

    fn comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only() {
            return;
        }
        if let Some((_, _, input)) = &self.draft {
            return window.focus(&input.focus_handle(cx), cx);
        }
        let Some((first, last)) = self.selected_lines() else {
            self.notice = Some(("Select a line to comment on.".into(), Color::Muted));
            return cx.notify();
        };
        let input =
            cx.new(|cx| TextInput::new("Comment on these lines", InputMode::SingleLine, cx));
        window.focus(&input.focus_handle(cx), cx);
        self.draft = Some((first, last, input));
        self.notice = None;
        cx.notify();
    }

    /// Keeps the comment being written, if it says anything. Says whether one was kept.
    fn save_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((first, last, input)) = self.draft.take() else {
            return false;
        };
        let text = input.read(cx).value().trim().to_owned();
        window.focus(&self.focus, cx);
        cx.notify();
        if text.is_empty() {
            return false;
        }
        self.comments.push(LineComment { first, last, text });
        self.relayout();
        true
    }

    fn cancel_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn delete_comment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.comments.len() {
            self.comments.remove(index);
            self.relayout();
            cx.notify();
        }
    }

    /// Lays the rows out again after the comments changed, keeping the scroll position
    /// and the selected lines.
    fn relayout(&mut self) {
        let Content::Ready(patch) = &self.content else {
            return;
        };
        let at = |rows: &[Row], row: usize| match rows.get(row) {
            Some(Row::Line(at)) => Some(*at),
            _ => None,
        };
        let selected = self
            .selection
            .and_then(|s| Some((at(&self.rows, s.anchor)?, at(&self.rows, s.head)?)));
        let top = self.list.logical_scroll_top();
        let (rows, starts) = layout(patch, &self.comments);
        let find = |line: LineAt| rows.iter().position(|row| *row == Row::Line(line));
        self.selection = selected.and_then(|(anchor, head)| {
            Some(Selected {
                anchor: find(anchor)?,
                head: find(head)?,
            })
        });
        let top_row = at(&self.rows, top.item_ix)
            .and_then(find)
            .unwrap_or(top.item_ix.min(rows.len().saturating_sub(1)));
        (self.rows, self.starts) = (rows, starts);
        self.list.reset(self.rows.len());
        self.list.scroll_to(ListOffset {
            item_ix: top_row,
            offset_in_item: top.offset_in_item,
        });
    }

    /// Sends every comment to the session as one message, with the one being written.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft.is_some() {
            self.save_comment(window, cx);
        }
        let Some(patch) = self.patch().filter(|_| !self.comments.is_empty()) else {
            return;
        };
        let prompt = review_prompt(self.source, patch, &self.comments);
        let session = self.session;
        let sent = self
            .keystone
            .update(cx, |keystone, cx| keystone.send_review(session, prompt, cx))
            .unwrap_or(Err(None));
        let count = self.comments.len();
        self.notice = Some(match sent {
            Ok(()) => {
                self.comments.clear();
                self.relayout();
                let noun = if count == 1 { "comment" } else { "comments" };
                (format!("Sent {count} {noun}."), Color::Muted)
            }
            Err(reason) => (
                reason.unwrap_or_else(|| "Not sent. The session can't take a message now.".into()),
                Color::Danger,
            ),
        });
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let keep = self.unsent(cx);
        let _ = self
            .keystone
            .update(cx, |keystone, cx| keystone.close_diff(keep, window, cx));
    }

    fn file_row(&self, file: &FileDiff, cx: &App) -> gpui::AnyElement {
        let colors = cx.theme().colors;
        let (added, removed) = file.counts();
        let change = match file.change {
            Change::Added => Some("Added"),
            Change::Deleted => Some("Deleted"),
            Change::Renamed => Some("Renamed"),
            Change::Modified => None,
        };
        let name = match &file.old_path {
            Some(old) => format!("{old} → {}", file.path),
            None => file.path.clone(),
        };
        // A raised block: a fill and no hairline (ADR 0011).
        h_flex()
            .mx(px(4.))
            .mt_2()
            .px(px(8.))
            .py(px(6.))
            .gap_2()
            .rounded(radius::ROW)
            .bg(colors.surface_raised)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(name).size(TextSize::Ui).truncate()),
            )
            .children(
                change.map(|change| Label::new(change).size(TextSize::Sm).color(Color::Muted)),
            )
            .when(added > 0, |d| {
                d.child(
                    Label::new(format!("+{added}"))
                        .size(TextSize::Sm)
                        .color(Color::Success),
                )
            })
            .when(removed > 0, |d| {
                d.child(
                    Label::new(format!("−{removed}"))
                        .size(TextSize::Sm)
                        .color(Color::Danger),
                )
            })
            .into_any_element()
    }

    fn row(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (Some(patch), Some(row)) = (self.patch(), self.rows.get(index).copied()) else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors;
        let note = |text: &'static str| {
            div()
                .px(px(14.))
                .py_1()
                .child(Label::new(text).size(TextSize::Sm).color(Color::Muted))
                .into_any_element()
        };
        let body = match row {
            Row::File(file) => self.file_row(&patch.files[file], cx),
            Row::Binary => note("Binary file. Its contents aren't shown."),
            Row::Truncated => div()
                .automation_id("diff-truncated")
                .child(note(match self.source {
                    Source::Edit(_) => "This edit is too long to keep whole. The rest isn't shown.",
                    _ => "This diff is too long. The rest isn't shown.",
                }))
                .into_any_element(),
            Row::Empty => note("No files changed."),
            Row::Hunk(file, hunk) => {
                hunk_row(&patch.files[file].hunks[hunk].header, cx).into_any_element()
            }
            Row::Comment(comment) => {
                let text = self.comments[comment].text.clone();
                h_flex()
                    .mx(px(8.))
                    .my_1()
                    .px(px(10.))
                    .py(px(6.))
                    .gap_2()
                    .items_start()
                    .rounded(radius::PANEL)
                    .bg(colors.surface_raised)
                    .font_family(cx.theme().fonts.ui.clone())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .automation_id(format!("diff-comment-{comment}"))
                            .child(Label::new(text).size(TextSize::Ui)),
                    )
                    .child(
                        IconButton::new(("diff-comment-delete", comment), IconName::Close)
                            .size(ButtonSize::Compact)
                            .aria_label("Delete comment")
                            .tooltip(Tooltip::text("Delete comment"))
                            .debug_selector(move || format!("diff-comment-delete-{comment}"))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.delete_comment(comment, cx)),
                            ),
                    )
                    .into_any_element()
            }
            Row::Line(at) => {
                let selected = self.selection.is_some_and(|s| s.range().contains(&index));
                line_row(line(patch, at), self.gutter, selected, cx).into_any_element()
            }
        };
        div()
            .id(("diff-row", index))
            .automation_id(format!("diff-row-{index}"))
            .w_full()
            .when(matches!(row, Row::Line(_)), |d| {
                d.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                        this.press(index, event.modifiers.shift, window, cx)
                    }),
                )
                .on_mouse_move(cx.listener(
                    move |this, event: &gpui::MouseMoveEvent, _, cx| {
                        this.dragging &= event.pressed_button == Some(MouseButton::Left);
                        if this.dragging {
                            this.drag_to(index, cx);
                        }
                    },
                ))
            })
            .child(body)
            .into_any_element()
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let files = self.starts.len();
        let position = self
            .patch()
            .zip(self.current_file())
            .map(|(patch, file)| format!("{} of {files} · {}", file + 1, patch.files[file].path));
        h_flex()
            .flex_none()
            .gap_1()
            .px(px(8.))
            .py(px(6.))
            .border_b_1()
            .border_color(colors.border)
            .child(
                IconButton::new("diff-back", IconName::Left)
                    .size(ButtonSize::Compact)
                    .debug_selector(|| "diff-back".into())
                    .aria_label("Back to the conversation")
                    .tooltip(Tooltip::with_key("Back to the conversation", "Esc"))
                    .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
            )
            .child(
                div().flex_none().automation_id("diff-title").child(
                    Label::new(self.source.title())
                        .size(TextSize::Ui)
                        .weight(FontWeight::MEDIUM),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl_2()
                    .automation_id("diff-file")
                    .children(position.map(|position| {
                        Label::new(position)
                            .size(TextSize::Sm)
                            .color(Color::Muted)
                            .truncate()
                    })),
            )
            .child(
                IconButton::new("diff-previous-file", IconName::Up)
                    .size(ButtonSize::Compact)
                    .disabled(files < 2)
                    .debug_selector(|| "diff-previous-file".into())
                    .aria_label("Previous file")
                    .tooltip(Tooltip::with_key("Previous file", "P"))
                    .on_click(cx.listener(|this, _, _, cx| this.jump_file(-1, cx))),
            )
            .child(
                IconButton::new("diff-next-file", IconName::Down)
                    .size(ButtonSize::Compact)
                    .disabled(files < 2)
                    .debug_selector(|| "diff-next-file".into())
                    .aria_label("Next file")
                    .tooltip(Tooltip::with_key("Next file", "N"))
                    .on_click(cx.listener(|this, _, _, cx| this.jump_file(1, cx))),
            )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let send_label = self.keystone.upgrade().and_then(|keystone| {
            let keystone = keystone.read(cx);
            let provider = keystone.provider(self.session, cx)?;
            Some(format!("Send to {}", crate::provider_name(provider)))
        });
        let count = self.comments.len();
        let summary = match count {
            0 if self.notice.is_some() => String::new(),
            0 => "No comments yet".to_owned(),
            1 => "1 comment".to_owned(),
            count => format!("{count} comments"),
        };
        v_flex()
            .flex_none()
            .border_t_1()
            .border_color(colors.border)
            .children(self.draft.as_ref().map(|(_, _, input)| {
                h_flex()
                    .key_context(COMMENT_CONTEXT)
                    .px(px(10.))
                    .pt(px(8.))
                    .gap_1()
                    .on_action(cx.listener(|this, _: &Newline, window, cx| {
                        this.save_comment(window, cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.cancel_comment(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .automation_id("diff-comment-input")
                            .child(input.clone()),
                    )
                    .child(
                        Button::new("diff-comment-save", "Add comment")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .debug_selector(|| "diff-comment-save".into())
                            .tooltip(Tooltip::with_key("Add comment", "Enter"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_comment(window, cx);
                            })),
                    )
            }))
            .children(self.notice.as_ref().map(|(notice, color)| {
                div()
                    .px(px(14.))
                    .pt(px(6.))
                    .automation_id("diff-notice")
                    .child(Label::new(notice.clone()).size(TextSize::Sm).color(*color))
            }))
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .px(px(10.))
                    .py(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pl_1()
                            .automation_id("diff-comment-count")
                            .child(Label::new(summary).size(TextSize::Sm).color(Color::Muted)),
                    )
                    .child(
                        Button::new("diff-comment", "Comment")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .disabled(self.selection.is_none() || self.draft.is_some())
                            .debug_selector(|| "diff-comment".into())
                            .tooltip(Tooltip::with_key("Comment on the selected lines", "C"))
                            .on_click(cx.listener(|this, _, window, cx| this.comment(window, cx))),
                    )
                    .children(send_label.map(|label| {
                        Button::new("diff-send", label)
                            .style(ButtonStyle::Primary)
                            .size(ButtonSize::Compact)
                            .disabled(count == 0)
                            .debug_selector(|| "diff-send".into())
                            .tooltip(Tooltip::with_key(
                                "Send the comments to the session",
                                keystone_ui::shortcut_text("secondary-enter"),
                            ))
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                    })),
            )
    }
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().fonts.mono.clone();
        self.gutter = gutter(self.biggest, cx);
        let body = match &self.content {
            Content::Loading => div()
                .flex_1()
                .px(px(14.))
                .py_2()
                .automation_id("diff-loading")
                .child(
                    Label::new("Reading the changes…")
                        .size(TextSize::Ui)
                        .color(Color::Muted),
                )
                .into_any_element(),
            Content::NoStart => div()
                .flex_1()
                .px(px(14.))
                .py_2()
                .automation_id("diff-unavailable")
                .child(
                    Label::new(
                        "This turn's starting point wasn't saved, so its changes can't be shown.",
                    )
                    .size(TextSize::Ui)
                    .color(Color::Muted),
                )
                .into_any_element(),
            Content::Failed(error) => div()
                .flex_1()
                .px(px(14.))
                .py_2()
                .automation_id("diff-failed")
                .child(
                    Label::new(format!("Couldn't read the changes. {error}"))
                        .size(TextSize::Ui)
                        .color(Color::Danger),
                )
                .into_any_element(),
            Content::Ready(_) => {
                let view = cx.weak_entity();
                div()
                    .flex_1()
                    .min_h_0()
                    .pb_1()
                    .font_family(mono)
                    .text_size(TextSize::Sm.px())
                    .line_height(TextSize::Sm.line_height())
                    .automation_id("diff-lines")
                    .child(
                        list(self.list.clone(), move |index, _, cx| {
                            view.update(cx, |view, cx| view.row(index, cx))
                                .unwrap_or_else(|_| div().into_any_element())
                        })
                        .size_full(),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .automation_id("diff-view")
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = false),
            )
            .child(self.header(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .key_context(CONTEXT)
                    .track_focus(&self.focus)
                    // Bubbling after the window's capture, so Escape closes an open menu first.
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &Up, _, cx| this.step(false, false, cx)))
                    .on_action(cx.listener(|this, _: &Down, _, cx| this.step(true, false, cx)))
                    .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step(false, true, cx)))
                    .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step(true, true, cx)))
                    .on_action(cx.listener(|this, _: &NextFile, _, cx| this.jump_file(1, cx)))
                    .on_action(cx.listener(|this, _: &PreviousFile, _, cx| this.jump_file(-1, cx)))
                    .on_action(
                        cx.listener(|this, _: &Comment, window, cx| this.comment(window, cx)),
                    )
                    .child(body),
            )
            // The window binds Cmd+Enter to Send, so here Send sends the comments.
            .on_action(
                cx.listener(|this, _: &crate::SendMessage, window, cx| this.send(window, cx)),
            )
            .when(!self.read_only(), |d| d.child(self.footer(cx)))
    }
}

/// The diff open in a window, and the conversation view it replaces.
struct OpenDiff {
    key: ViewKey,
    view: Entity<DiffView>,
    /// The transcript row and control that opened it, which take focus back on close.
    opener: Option<(usize, crate::view_state::CopyTarget)>,
}

/// A window's diffs: the one open, and closed ones whose comments weren't sent, which
/// come back as they were when the same diff opens again.
#[derive(Default)]
pub(crate) struct Diffs {
    open: Option<OpenDiff>,
    kept: Vec<Entity<DiffView>>,
}

impl DiffView {
    /// Whether closing it would lose something the user wrote.
    fn unsent(&self, cx: &App) -> bool {
        !self.comments.is_empty()
            || self
                .draft
                .as_ref()
                .is_some_and(|(_, _, input)| !input.read(cx).value().trim().is_empty())
    }
}

impl Keystone {
    /// The diff to draw in place of conversation view `key`, if one is open there.
    pub(crate) fn diff_in(&self, key: ViewKey) -> Option<Entity<DiffView>> {
        self.diff
            .open
            .as_ref()
            .filter(|open| open.key == key)
            .map(|open| open.view.clone())
    }

    fn open_diff(
        &mut self,
        key: ViewKey,
        source: Source,
        load: Load,
        reveal: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keep = self
            .diff
            .open
            .as_ref()
            .is_some_and(|open| open.view.read(cx).unsent(cx));
        self.take_open_diff(keep);
        let kept = self.diff.kept.iter().position(|view| {
            let view = view.read(cx);
            (view.session, view.source) == (key.session, source)
        });
        let view = match kept {
            Some(index) => {
                let view = self.diff.kept.remove(index);
                if let Some(path) = reveal {
                    view.update(cx, |view, cx| {
                        view.reveal_file(&path);
                        cx.notify();
                    });
                }
                view
            }
            None => {
                let keystone = cx.weak_entity();
                cx.new(|cx| DiffView::new(key.session, source, load, reveal, keystone, cx))
            }
        };
        // From Changes in a narrow window, the conversation is a tab behind it.
        self.reveal_view(key.view, window, cx);
        window.focus(&view.focus_handle(cx), cx);
        self.diff.open = Some(OpenDiff {
            key,
            view,
            opener: None,
        });
        self.refresh(cx);
    }

    /// Closes the open diff, and with `keep`, keeps it to come back to.
    fn take_open_diff(&mut self, keep: bool) -> Option<OpenDiff> {
        let open = self.diff.open.take()?;
        if keep {
            self.diff.kept.push(open.view.clone());
        }
        Some(open)
    }

    /// Closes the open diff, keeping it when it has comments that weren't sent.
    fn close_diff(&mut self, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.take_open_diff(keep) else {
            return;
        };
        let views = &mut self.presentation.current.views;
        match open.opener {
            Some((row, control)) => {
                let keyboard = views.keyboard(open.key, cx);
                keyboard.row = Some(row);
                keyboard.action = Some(control);
                keyboard.selection_from_pointer = false;
                keyboard.action_focus.clone().focus(window, cx);
                views.activate(open.key);
            }
            None => views.focus(open.key, cx).focus(window, cx),
        }
        self.refresh(cx);
    }

    /// Shows what the turn whose last reply is at `index` changed.
    pub(crate) fn open_turn_diff(
        &mut self,
        key: ViewKey,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let turn = self
            .state
            .read(cx)
            .conversations()
            .get(&key.session)
            .and_then(|conversation| conversation.checkpoint_at(index));
        let Some(turn) = turn else {
            self.error = Some("Choose a turn's last reply.".into());
            return self.refresh(cx);
        };
        match self
            .state
            .update(cx, |state, _| state.turn_diff(key.session, turn))
        {
            Ok(load) => self.open_diff(key, Source::Turn(turn), load, None, window, cx),
            Err(error) => {
                self.error = Some(error);
                self.refresh(cx);
            }
        }
    }

    /// Shows the whole of the edit at transcript row `index`, read only, with its paths
    /// as [`crate::transcript::EditFolder`] shows them. `opener`, the row's control that
    /// asked for it, takes focus back when it closes.
    pub(crate) fn open_edit_diff(
        &mut self,
        key: ViewKey,
        index: usize,
        opener: Option<crate::view_state::CopyTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(patch) = self.edit_patch(key, index, cx) else {
            return;
        };
        let patch = self.edit_folder(key.session, cx).patch(&patch);
        let load: Load = Box::new(move || Ok(Some(patch)));
        self.open_diff(key, Source::Edit(index), load, None, window, cx);
        if let Some(open) = &mut self.diff.open {
            open.opener = opener.map(|control| (index, control));
        }
    }

    /// The patch of the edit at transcript row `index`, when it has one.
    fn edit_patch(&self, key: ViewKey, index: usize, cx: &App) -> Option<Patch> {
        let conversation = self.state.read(cx).conversations().get(&key.session)?;
        match conversation.rows.get(index)? {
            crate::conversation::Row::Item(keystone_agent::Item {
                kind: keystone_agent::ItemKind::FileChange { patch, .. },
                ..
            }) => patch.clone(),
            _ => None,
        }
    }

    /// View changes from the transcript row the keyboard is on: the whole edit on an
    /// edit's row, else the turn's changes.
    pub(crate) fn view_changes_focused_row(
        &mut self,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = self
            .presentation
            .current
            .views
            .keyboard_state(key)
            .and_then(|keyboard| keyboard.row);
        match row {
            Some(index) if self.edit_patch(key, index, cx).is_some() => {
                self.open_edit_diff(key, index, None, window, cx)
            }
            Some(index) => self.open_turn_diff(key, index, window, cx),
            None => {
                self.error = Some("Choose a turn's last reply.".into());
                self.refresh(cx);
            }
        }
    }

    /// Shows what the working tree of the repository holding `folder` changes from its
    /// HEAD, in place of the selected session's conversation, starting at `file`.
    pub(crate) fn open_working_diff(
        &mut self,
        folder: PathBuf,
        file: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected(cx) else {
            return;
        };
        // A narrow window can show Changes in front of the conversation, which is then
        // not on screen: the view the session shows in is brought forward instead.
        let showing = self.showing(session, cx);
        let Some(key) = self
            .presentation
            .current
            .views
            .send_target(&showing, session)
            .or_else(|| self.active_view(cx).map(|view| ViewKey { view, session }))
        else {
            return;
        };
        let load: Load = Box::new(move || {
            let root = checkpoint::root(&folder)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "This folder isn't in a git repository.".to_owned())?;
            checkpoint::working_tree_diff(&root)
                .map(Some)
                .map_err(|error| error.to_string())
        });
        self.open_diff(key, Source::WorkingTree, load, file, window, cx);
    }

    /// Sends review comments to `session` as a message of their own. The composer's
    /// draft is left as it is.
    pub(crate) fn send_review(
        &mut self,
        session: u64,
        text: String,
        cx: &mut Context<Self>,
    ) -> Result<(), Option<String>> {
        if self.importing(session) {
            return Err(None);
        }
        let sent = self
            .state
            .update(cx, |state, cx| state.send_review(session, text, cx));
        self.refresh(cx);
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_agent::diff::{Hunk, parse};

    fn patch() -> Patch {
        parse(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n\
             @@ -1,3 +1,3 @@\n a\n-b\n+c\n d\n\
             diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n\
             diff --git a/notes.md b/notes.md\n--- a/notes.md\n+++ b/notes.md\n@@ -10,2 +10,0 @@\n-gone\n-`also` ```gone```\n"
                .as_bytes(),
            keystone_agent::diff::MAX_LINES,
        )
    }

    fn at(file: usize, hunk: usize, line: usize) -> LineAt {
        LineAt { file, hunk, line }
    }

    #[test]
    fn rows_put_each_comment_after_the_line_it_ends_on() {
        let patch = patch();
        let comment = LineComment {
            first: at(0, 0, 1),
            last: at(0, 0, 2),
            text: "why".into(),
        };
        let (rows, starts) = layout(&patch, &[comment]);
        assert_eq!(
            rows,
            [
                Row::File(0),
                Row::Hunk(0, 0),
                Row::Line(at(0, 0, 0)),
                Row::Line(at(0, 0, 1)),
                Row::Line(at(0, 0, 2)),
                Row::Comment(0),
                Row::Line(at(0, 0, 3)),
                Row::File(1),
                Row::Binary,
                Row::File(2),
                Row::Hunk(2, 0),
                Row::Line(at(2, 0, 0)),
                Row::Line(at(2, 0, 1)),
            ]
        );
        assert_eq!(starts, [0, 7, 9]);
    }

    #[test]
    fn an_empty_or_cut_patch_says_so() {
        assert_eq!(layout(&Patch::default(), &[]).0, [Row::Empty]);
        let cut = Patch {
            files: vec![FileDiff {
                path: "a".into(),
                hunks: vec![Hunk::default()],
                ..FileDiff::default()
            }],
            truncated: true,
        };
        assert_eq!(
            layout(&cut, &[]).0,
            [Row::File(0), Row::Hunk(0, 0), Row::Truncated]
        );
    }

    #[test]
    fn the_review_names_each_file_and_its_lines_and_quotes_them() {
        let patch = patch();
        let comments = [
            LineComment {
                first: at(0, 0, 1),
                last: at(0, 0, 2),
                text: " Why change this? ".into(),
            },
            LineComment {
                first: at(2, 0, 0),
                last: at(2, 0, 1),
                text: "Keep these.".into(),
            },
        ];
        assert_eq!(
            review_prompt(Source::Turn(3), &patch, &comments),
            "Comments on your changes:\n\n\
             src/lib.rs line 2:\n```diff\n-b\n+c\n```\nWhy change this?\n\n\
             notes.md removed lines 10-11:\n````diff\n-gone\n-`also` ```gone```\n````\nKeep these."
        );
        assert!(
            review_prompt(Source::WorkingTree, &patch, &comments[..1])
                .starts_with("Comments on the uncommitted changes:")
        );
    }
}
