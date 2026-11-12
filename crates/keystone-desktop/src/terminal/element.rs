//! Paints a pane's visible screen. Work follows the rows on screen, never the scrollback.

use super::SessionTerminal;
use super::pane::GridSize;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors as Overrides;
use alacritty_terminal::vte::ansi::{Color as CellColor, CursorShape, NamedColor};
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, Font, FontStyle, FontWeight, GlobalElementId,
    Hsla, IntoElement, LayoutId, Pixels, ShapedLine, SharedString, StrikethroughStyle, Style,
    TextAlign, TextRun, UnderlineStyle, WeakEntity, Window, fill, outline, point, px, relative,
    size,
};
use keystone_ui::{ActiveTheme, TerminalColors, TextSize};

/// The monospace font, its size, and the cell it gives.
pub(crate) struct Metrics {
    pub font: Font,
    pub font_size: Pixels,
    pub cell: gpui::Size<Pixels>,
}

impl Metrics {
    pub(crate) fn new(window: &Window, cx: &App) -> Self {
        let font = gpui::font(cx.theme().fonts.mono.clone());
        let font_size = TextSize::Base.px();
        let text = window.text_system();
        let width = text
            .advance(text.resolve_font(&font), font_size, 'm')
            .map_or(font_size * 0.6, |advance| advance.width);
        Self {
            font,
            font_size,
            cell: size(width, (font_size * 1.35).round()),
        }
    }

    pub(crate) fn grid(&self, bounds: Bounds<Pixels>) -> GridSize {
        let fit = |extent: Pixels, cell: Pixels| (extent / cell).floor().max(1.) as usize;
        GridSize {
            columns: fit(bounds.size.width, self.cell.width).max(2),
            lines: fit(bounds.size.height, self.cell.height),
            cell: self.cell,
        }
    }
}

pub(crate) struct Grid {
    pub terminal: WeakEntity<SessionTerminal>,
    pub pane: u64,
    pub focused: bool,
}

impl IntoElement for Grid {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

#[derive(Default)]
pub(crate) struct Frame {
    background: Option<Hsla>,
    rects: Vec<(Bounds<Pixels>, Hsla)>,
    text: Vec<(gpui::Point<Pixels>, ShapedLine)>,
    cursor: Option<gpui::PaintQuad>,
    line_height: Pixels,
}

#[derive(Clone, PartialEq)]
struct CellStyle {
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

/// Cells shaped as one line, a run per style. Wide characters and ones a fallback font
/// may draw start their own segment, painted at its own column, so they cannot push
/// the cells after them off the grid. Every painted line costs GPUI a layer, so a row
/// is usually one segment.
struct Segment {
    column: usize,
    columns: usize,
    text: String,
    runs: Vec<(usize, CellStyle)>,
    closed: bool,
}

impl Segment {
    fn decorated(&self) -> bool {
        self.runs
            .iter()
            .any(|(_, style)| style.underline || style.strike)
    }
}

impl Element for Grid {
    type RequestLayoutState = ();
    type PrepaintState = Frame;

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
    ) -> Frame {
        let Some(terminal) = self.terminal.upgrade() else {
            return Frame::default();
        };
        let terminal = terminal.read(cx);
        let Some(pane) = terminal.pane(self.pane) else {
            return Frame::default();
        };
        let metrics = Metrics::new(window, cx);
        if super::presented(window) {
            let showing = super::showing(terminal.session, cx);
            pane.paint_in(
                window.window_handle().window_id(),
                bounds,
                metrics.grid(bounds),
                &showing,
            );
        }
        let term = pane.term.clone();
        let palette = cx.theme().colors.terminal;
        let term = term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let overrides = content.colors;
        let selection = content.selection;
        let cursor = content.cursor;
        let (cw, lh) = (metrics.cell.width, metrics.cell.height);
        let at =
            |column: usize, row: i32| bounds.origin + point(cw * column as f32, lh * row as f32);
        let mut frame = Frame {
            background: Some(palette.background),
            line_height: lh,
            ..Frame::default()
        };
        let mut row_text: Vec<Segment> = Vec::new();
        let mut row_backgrounds: Vec<(usize, usize, Hsla)> = Vec::new();
        let mut row_selection: Vec<(usize, usize)> = Vec::new();
        let mut current_row = None;
        let mut under_cursor: Option<(char, CellStyle)> = None;
        let flush_row = |row: i32,
                         segments: &mut Vec<Segment>,
                         backgrounds: &mut Vec<(usize, usize, Hsla)>,
                         selected: &mut Vec<(usize, usize)>,
                         frame: &mut Frame,
                         window: &mut Window| {
            for (start, end, color) in backgrounds.drain(..) {
                frame.rects.push((
                    Bounds::new(at(start, row), size(cw * (end - start) as f32, lh)),
                    color,
                ));
            }
            for (start, end) in selected.drain(..) {
                frame.rects.push((
                    Bounds::new(at(start, row), size(cw * (end - start) as f32, lh)),
                    palette.selection,
                ));
            }
            for mut segment in segments.drain(..) {
                if !segment.decorated() {
                    let mut blank = segment.text.len() - segment.text.trim_end().len();
                    segment.text.truncate(segment.text.len() - blank);
                    while blank > 0 {
                        let Some((len, _)) = segment.runs.last_mut() else {
                            break;
                        };
                        let cut = blank.min(*len);
                        *len -= cut;
                        blank -= cut;
                        if *len == 0 {
                            segment.runs.pop();
                        }
                    }
                }
                if segment.text.is_empty() {
                    continue;
                }
                let runs: Vec<TextRun> = segment
                    .runs
                    .iter()
                    .map(|(len, style)| text_run(&metrics.font, *len, style))
                    .collect();
                let line = window.text_system().shape_line(
                    SharedString::from(segment.text),
                    metrics.font_size,
                    &runs,
                    None,
                );
                frame.text.push((at(segment.column, row), line));
            }
        };
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if current_row != Some(row) {
                if let Some(previous) = current_row {
                    flush_row(
                        previous,
                        &mut row_text,
                        &mut row_backgrounds,
                        &mut row_selection,
                        &mut frame,
                        window,
                    );
                }
                current_row = Some(row);
            }
            let cell = indexed.cell;
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let column = indexed.point.column.0;
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            let mut fg = resolve(cell.fg, &palette, overrides);
            let mut bg = resolve(cell.bg, &palette, overrides);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.flags.contains(Flags::DIM) {
                fg = TerminalColors::dim(fg);
            }
            if cell.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            if bg != palette.background {
                match row_backgrounds.last_mut() {
                    Some((_, end, color)) if *end == column && *color == bg => *end += width,
                    _ => row_backgrounds.push((column, column + width, bg)),
                }
            }
            if selection.is_some_and(|range| range.contains(indexed.point)) {
                match row_selection.last_mut() {
                    Some((_, end)) if *end == column => *end += width,
                    _ => row_selection.push((column, column + width)),
                }
            }
            let style = CellStyle {
                color: fg,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                strike: cell.flags.contains(Flags::STRIKEOUT),
            };
            if indexed.point == cursor.point {
                under_cursor = Some((cell.c, style.clone()));
            }
            let lone = width == 2 || !single_cell(cell.c);
            let start = starts_segment(&row_text, column, lone);
            if start {
                row_text.push(Segment {
                    column,
                    columns: 0,
                    text: String::new(),
                    runs: Vec::new(),
                    closed: lone,
                });
            }
            let segment = row_text.last_mut().unwrap();
            let before = segment.text.len();
            segment.text.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                segment.text.extend(extra);
                segment.closed = true;
            }
            segment.columns += width;
            let added = segment.text.len() - before;
            match segment.runs.last_mut() {
                Some((len, last)) if *last == style => *len += added,
                _ => segment.runs.push((added, style)),
            }
        }
        if let Some(row) = current_row {
            flush_row(
                row,
                &mut row_text,
                &mut row_backgrounds,
                &mut row_selection,
                &mut frame,
                window,
            );
        }

        let row = cursor.point.line.0 + offset;
        if cursor.shape != CursorShape::Hidden && (0..term.screen_lines() as i32).contains(&row) {
            let origin = at(cursor.point.column.0, row);
            let block = Bounds::new(origin, metrics.cell);
            let color = palette.cursor;
            frame.cursor = Some(match (self.focused, cursor.shape) {
                (_, CursorShape::Beam) => fill(Bounds::new(origin, size(px(2.), lh)), color),
                (_, CursorShape::Underline) => fill(
                    Bounds::new(origin + point(px(0.), lh - px(2.)), size(cw, px(2.))),
                    color,
                ),
                (true, _) => fill(block, color),
                (false, _) => outline(block, color, gpui::BorderStyle::Solid),
            });
            if self.focused
                && matches!(cursor.shape, CursorShape::Block)
                && let Some((c, mut style)) = under_cursor.filter(|(c, _)| !c.is_whitespace())
            {
                style.color = palette.background;
                let text = c.to_string();
                let run = text_run(&metrics.font, text.len(), &style);
                let line =
                    window
                        .text_system()
                        .shape_line(text.into(), metrics.font_size, &[run], None);
                frame.text.push((origin, line));
            }
        }
        frame
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut App,
    ) {
        // One layer gives every cell the same draw order, which spares the scene a
        // bounds-tree insert per glyph; GPUI still draws quads before text within it.
        window.paint_layer(bounds, |window| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                if let Some(background) = frame.background {
                    window.paint_quad(fill(bounds, background));
                }
                for (rect, color) in frame.rects.drain(..) {
                    window.paint_quad(fill(rect, color));
                }
                if let Some(cursor) = frame.cursor.take() {
                    window.paint_quad(cursor);
                }
                for (origin, line) in &frame.text {
                    let _ = line.paint(
                        *origin,
                        frame.line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            })
        });
    }
}

/// Whether the cell at `column` starts a new segment rather than extending the last.
fn starts_segment(segments: &[Segment], column: usize, lone: bool) -> bool {
    match segments.last() {
        Some(segment) => lone || segment.closed || segment.column + segment.columns != column,
        None => true,
    }
}

fn text_run(font: &Font, len: usize, style: &CellStyle) -> TextRun {
    let mut font = font.clone();
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font,
        color: style.color,
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(style.color),
            wavy: false,
        }),
        strikethrough: style.strike.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(style.color),
        }),
    }
}

/// Characters a monospace font draws in one cell. Others, such as emoji or symbols a
/// fallback font supplies, get a segment of their own.
fn single_cell(c: char) -> bool {
    matches!(c, ' '..='~' | '\u{a0}'..='\u{24f}' | '\u{2500}'..='\u{259f}')
}

/// A cell's color: a program's own choice (OSC 4, 10, 11) first, then the theme.
fn resolve(color: CellColor, palette: &TerminalColors, overrides: &Overrides) -> Hsla {
    let chosen =
        |index: usize| overrides[index].map(|rgb| TerminalColors::true_color(rgb.r, rgb.g, rgb.b));
    match color {
        CellColor::Spec(rgb) => TerminalColors::true_color(rgb.r, rgb.g, rgb.b),
        CellColor::Indexed(index) => {
            chosen(index as usize).unwrap_or_else(|| palette.indexed(index))
        }
        CellColor::Named(name) => chosen(name as usize).unwrap_or_else(|| named(name, palette)),
    }
}

fn named(name: NamedColor, palette: &TerminalColors) -> Hsla {
    let index = name as usize;
    match name {
        NamedColor::Foreground | NamedColor::BrightForeground => palette.foreground,
        NamedColor::DimForeground => TerminalColors::dim(palette.foreground),
        NamedColor::Background => palette.background,
        NamedColor::Cursor => palette.cursor,
        _ if index < 16 => palette.ansi[index],
        _ => {
            let normal = index - NamedColor::DimBlack as usize;
            TerminalColors::dim(palette.ansi[normal.min(7)])
        }
    }
}
