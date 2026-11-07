//! Markdown for agent replies. [`Document::parse`] turns CommonMark with GitHub tables
//! and strikethrough into plain data, and [`Markdown`] renders it from theme tokens.
//! A streamed reply goes through an [`IncrementalParser`] instead, which parses only
//! the reply's last blocks again on each delta.
//!
//! Parsing never fails, so a reply can be rendered at every streamed delta. An open
//! code fence runs to the end of the text, which keeps a streaming code block stable.
//!
//! Replies are untrusted, so quotes and lists nest at most [`MAX_NESTING`] deep. Deeper
//! containers are flattened into the innermost kept one, which bounds the recursion in
//! rendering and in dropping a [`Document`]. Inline styles are flat ranges, never a tree.

use crate::button::IconButton;
use crate::color::Color;
use crate::icon::IconName;
use crate::label::Label;
use crate::styled_ext::{StyledExt, h_flex, v_flex};
use crate::theme::ActiveTheme;
use crate::tokens::{Elevation, TextSize};
use crate::tooltip::Tooltip;
use gpui::{
    AnyElement, App, ClipboardItem, ElementId, Entity, FocusHandle, FontFeatures, FontStyle,
    FontWeight, HighlightStyle, InteractiveText, IntoElement, RenderOnce, SharedString,
    StrikethroughStyle, TextStyle, UnderlineStyle, Window, div, prelude::*, px,
};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

mod incremental;
mod mend;
mod selection;

pub use incremental::IncrementalParser;

/// How deeply quotes and lists nest. Real replies rarely pass four levels, and each level
/// costs several GPUI elements and render stack frames.
pub const MAX_NESTING: usize = 8;

/// A parsed reply: its top-level blocks, in order. Each is shared, so a streamed reply's
/// next document reuses the blocks that did not change.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    pub blocks: Vec<Arc<TopBlock>>,
}

/// A top-level block and the byte range of the source it was parsed from.
#[derive(Clone, Debug, PartialEq)]
pub struct TopBlock {
    pub range: Range<usize>,
    pub block: Block,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Paragraph(Inline),
    Heading {
        level: u8,
        text: Inline,
    },
    Code {
        language: Option<SharedString>,
        code: SharedString,
    },
    Diagram {
        source: SharedString,
        closed: bool,
    },
    /// `start` is the first number of an ordered list, or `None` for bullets.
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Table {
        alignments: Vec<Alignment>,
        header: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
    Rule,
    /// A paragraph of nothing but images whose destinations are local paths. The host
    /// draws each one; see [`Markdown::images`].
    Images(Vec<ImageRef>),
}

/// A Markdown image: its destination as written and its alt text.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageRef {
    pub url: SharedString,
    pub alt: SharedString,
}

/// Draws a local image of a reply. Hosts decide what a path means and whether to show it.
pub type ImageRenderer = Rc<dyn Fn(&ImageRef, &mut Window, &mut App) -> AnyElement>;

/// One run of text with styled ranges. Ranges are byte offsets, in order, and never overlap.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inline {
    pub text: SharedString,
    pub spans: Vec<(Range<usize>, Style)>,
    pub links: Vec<(Range<usize>, SharedString)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub strong: bool,
    pub emphasis: bool,
    pub strikethrough: bool,
    pub code: bool,
    pub link: bool,
}

impl Document {
    pub fn parse(source: &str) -> Self {
        Self {
            blocks: parse_at(source, 0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Whether this document has rendered text that supports range selection.
    /// Mermaid diagrams and rules keep their own interaction surfaces.
    pub fn has_selectable_text(&self) -> bool {
        selection::has_selectable_text(self)
    }
}

const OPTIONS: Options = Options::ENABLE_TABLES.union(Options::ENABLE_STRIKETHROUGH);

/// Parses `source`, which starts at byte `offset` of a longer text, into top-level
/// blocks whose ranges are offsets into that text.
fn parse_at(source: &str, offset: usize) -> Vec<Arc<TopBlock>> {
    let mut builder = Builder::default();
    let mut depth = 0usize;
    let mut diagram_start = 0;
    let mut diagram_tail = 0;
    let mut diagram_in_container = false;
    for (event, range) in Parser::new_ext(source, OPTIONS).into_offset_iter() {
        if depth == 0 {
            builder.top = range.start + offset..range.end + offset;
        }
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
        if let Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) = &event
            && info.split_whitespace().next() == Some("mermaid")
        {
            builder.diagram_closed = Some(closed_fence(&source[range.clone()]));
            diagram_start = range.start;
            diagram_tail = source[range.clone()]
                .find('\n')
                .map_or(range.end, |line| range.start + line + 1);
            let line_start = source[..range.start].rfind('\n').map_or(0, |i| i + 1);
            diagram_in_container = line_start != range.start;
        }
        if let Event::Text(text) = &event
            && builder.diagram_closed.is_some()
        {
            if let Some((_, code)) = &mut builder.code {
                append_code_text(code, source, text, range.clone());
            }
            diagram_tail = range.end;
        } else {
            if matches!(event, Event::End(TagEnd::CodeBlock))
                && builder.diagram_closed == Some(true)
                && diagram_tail == range.end
            {
                builder.diagram_closed = Some(false);
            }
            if matches!(event, Event::End(TagEnd::CodeBlock))
                && builder.diagram_closed == Some(false)
            {
                let tail = &source[diagram_tail..range.end];
                if !tail.is_empty() && diagram_in_container && range.end == source.len() {
                    if let Some(verbatim) = unclosed_code_source(source, diagram_start, range.end)
                        && let Some((_, code)) = &mut builder.code
                    {
                        *code = verbatim;
                    }
                } else {
                    builder.text(tail, builder.style);
                }
            }
            builder.event(event);
        }
    }
    builder.finish()
}

fn append_code_text(code: &mut String, source: &str, text: &str, range: Range<usize>) {
    if range.is_empty() {
        code.push_str(text);
        return;
    }
    if source[range.clone()].starts_with('\n')
        && range.start > 0
        && source.as_bytes()[range.start - 1] == b'\r'
    {
        code.push('\r');
    }
    code.push_str(&source[range]);
}

/// One EOF-only probe lets CommonMark resolve partial container prefixes and retain
/// whitespace that it otherwise omits at EOF. This adds at most one parse per document.
fn unclosed_code_source(source: &str, start: usize, end: usize) -> Option<String> {
    let probe = format!("{}x", &source[..end]);
    let tail_start = source[..end].rfind('\n').map_or(0, |i| i + 1);
    let mut active = false;
    let mut code = String::new();
    for (event, range) in Parser::new_ext(&probe, OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) if range.start == start => active = true,
            Event::Text(text) if active => {
                if !range.is_empty() || range.start < tail_start {
                    append_code_text(&mut code, &probe, &text, range);
                }
            }
            Event::End(TagEnd::CodeBlock) if active => break,
            _ => {}
        }
    }
    code.strip_suffix('x').map(str::to_owned)
}

fn closed_fence(block: &str) -> bool {
    let Some((opening, rest)) = block.split_once('\n') else {
        return false;
    };
    let marker = opening.as_bytes()[0];
    let count = opening.bytes().take_while(|&b| b == marker).count();
    let closing = rest.rsplit('\n').next().unwrap_or_default().trim();
    let closing = closing.trim_start_matches([' ', '\t', '>']);
    closing.len() >= count && closing.bytes().all(|b| b == marker)
}

/// An open container while parsing.
enum Frame {
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Item(Vec<Block>),
    Table {
        alignments: Vec<Alignment>,
        header: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
        in_head: bool,
    },
}

/// Where the inline text being collected will go.
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Paragraph,
    Heading(u8),
    Cell,
}

#[derive(Default)]
struct Builder {
    root: Vec<Arc<TopBlock>>,
    /// The source range of the top-level block being parsed.
    top: Range<usize>,
    stack: Vec<Frame>,
    inline: Option<(Target, InlineBuilder)>,
    code: Option<(Option<SharedString>, String)>,
    diagram_closed: Option<bool>,
    style: Style,
    /// Nesting depth of each inline style, so `**a *b* c**` keeps bold after the inner end.
    depth: [u32; 4],
    /// Open links and images: where their text starts, their destination, and whether
    /// each is an image.
    links: Vec<(usize, SharedString, bool)>,
    /// Open quotes, lists, and items past [`MAX_NESTING`]. Their content goes to the
    /// innermost kept container.
    flattened: usize,
}

#[derive(Default)]
struct InlineBuilder {
    text: String,
    spans: Vec<(Range<usize>, Style)>,
    links: Vec<(Range<usize>, SharedString)>,
    /// Images among `links`: their text's range and destination.
    images: Vec<(Range<usize>, SharedString)>,
}

impl InlineBuilder {
    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        if style == Style::default() {
            return;
        }
        match self.spans.last_mut() {
            Some((range, last)) if *last == style && range.end == start => {
                range.end = self.text.len()
            }
            _ => self.spans.push((start..self.text.len(), style)),
        }
    }

    /// The images, when the text is nothing but images with local destinations.
    fn only_local_images(&self) -> Option<Vec<ImageRef>> {
        if self.images.is_empty() || !self.images.iter().all(|(_, url)| is_local(url)) {
            return None;
        }
        let mut at = 0;
        for (range, _) in &self.images {
            if !self.text[at..range.start].trim().is_empty() {
                return None;
            }
            at = range.end;
        }
        if !self.text[at..].trim().is_empty() {
            return None;
        }
        Some(
            self.images
                .iter()
                .map(|(range, url)| ImageRef {
                    url: url.clone(),
                    alt: match &self.text[range.clone()] {
                        alt if alt == url.as_ref() => SharedString::default(),
                        alt => alt.to_owned().into(),
                    },
                })
                .collect(),
        )
    }

    fn finish(self) -> Inline {
        Inline {
            text: self.text.into(),
            spans: self.spans,
            links: self.links,
        }
    }
}

impl Builder {
    /// Adds a finished block to the innermost open quote or item, or to the document.
    fn emit(&mut self, block: Block) {
        for frame in self.stack.iter_mut().rev() {
            match frame {
                Frame::Quote(blocks) | Frame::Item(blocks) => return blocks.push(block),
                // A list holds items, and a table holds cells; neither takes blocks directly.
                Frame::List { .. } | Frame::Table { .. } => {}
            }
        }
        self.root.push(Arc::new(TopBlock {
            range: self.top.clone(),
            block,
        }));
    }

    fn push_block(&mut self, block: Block) {
        self.flush_inline();
        self.emit(block);
    }

    /// Ends implicit paragraph text, such as a tight list item's, before another block.
    fn flush_inline(&mut self) {
        if let Some((target, inline)) = self.inline.take() {
            if target == Target::Paragraph
                && let Some(images) = inline.only_local_images()
            {
                return self.emit(Block::Images(images));
            }
            let inline = inline.finish();
            match target {
                Target::Paragraph if inline.text.trim().is_empty() => {}
                Target::Paragraph => self.emit(Block::Paragraph(inline)),
                Target::Heading(level) => self.emit(Block::Heading {
                    level,
                    text: inline,
                }),
                Target::Cell => {
                    if let Some(Frame::Table {
                        header,
                        rows,
                        in_head,
                        ..
                    }) = self.stack.last_mut()
                    {
                        match rows.last_mut() {
                            Some(row) if !*in_head => row.push(inline),
                            _ => header.push(inline),
                        }
                    }
                }
            }
        }
    }

    fn text(&mut self, text: &str, style: Style) {
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
            return;
        }
        let (_, inline) = self
            .inline
            .get_or_insert_with(|| (Target::Paragraph, InlineBuilder::default()));
        inline.push(text, style);
    }

    fn set_style(&mut self, index: usize, on: bool) {
        if on {
            self.depth[index] += 1;
        } else {
            self.depth[index] = self.depth[index].saturating_sub(1);
        }
        self.style.strong = self.depth[0] > 0;
        self.style.emphasis = self.depth[1] > 0;
        self.style.strikethrough = self.depth[2] > 0;
        self.style.link = self.depth[3] > 0;
    }

    /// Whether a new quote or list would nest too deeply, or sits inside one that did.
    /// Counting it as flattened keeps its end from closing a kept container.
    fn flatten(&mut self) -> bool {
        let depth = self
            .stack
            .iter()
            .filter(|frame| matches!(frame, Frame::Quote(_) | Frame::List { .. }))
            .count();
        if self.flattened > 0 || depth >= MAX_NESTING {
            self.flush_inline();
            self.flattened += 1;
            return true;
        }
        false
    }

    /// Closes a flattened quote, list, or item, if the closing tag belongs to one.
    fn unflatten(&mut self) -> bool {
        if self.flattened == 0 {
            return false;
        }
        self.flush_inline();
        self.flattened -= 1;
        true
    }

    fn start_inline(&mut self, target: Target) {
        self.flush_inline();
        self.inline = Some((target, InlineBuilder::default()));
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text, self.style),
            Event::Code(text) => self.text(
                &text,
                Style {
                    code: true,
                    ..self.style
                },
            ),
            // Raw HTML and math are shown as typed, never interpreted.
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text, self.style),
            Event::InlineMath(text) | Event::DisplayMath(text) => self.text(&text, self.style),
            Event::FootnoteReference(name) => self.text(&format!("[^{name}]"), self.style),
            Event::SoftBreak => self.text(" ", self.style),
            Event::HardBreak => self.text("\n", self.style),
            Event::Rule => self.push_block(Block::Rule),
            Event::TaskListMarker(done) => {
                self.text(if done { "[x] " } else { "[ ] " }, self.style)
            }
        }
    }

    fn open_link(&mut self, url: String, image: bool) {
        let start = self.inline.as_ref().map_or(0, |(_, i)| i.text.len());
        self.links.push((start, url.into(), image));
        self.set_style(3, true);
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.start_inline(Target::Paragraph),
            Tag::Heading { level, .. } => self.start_inline(Target::Heading(level as u8)),
            Tag::BlockQuote(_) | Tag::List(_) if self.flatten() => {}
            // An item of a flattened list is flattened too.
            Tag::Item if self.flattened > 0 => self.flattened += 1,
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.stack.push(Frame::Quote(Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .map(|lang| SharedString::from(lang.to_owned())),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((language, String::new()));
            }
            Tag::List(start) => {
                self.flush_inline();
                self.stack.push(Frame::List {
                    start,
                    items: Vec::new(),
                });
            }
            Tag::Item => {
                self.flush_inline();
                self.stack.push(Frame::Item(Vec::new()));
            }
            Tag::Table(alignments) => {
                self.flush_inline();
                self.stack.push(Frame::Table {
                    alignments,
                    header: Vec::new(),
                    rows: Vec::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(Frame::Table { in_head, .. }) = self.stack.last_mut() {
                    *in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(Frame::Table { rows, .. }) = self.stack.last_mut() {
                    rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.start_inline(Target::Cell),
            Tag::Strong => self.set_style(0, true),
            Tag::Emphasis => self.set_style(1, true),
            Tag::Strikethrough => self.set_style(2, true),
            Tag::Link { dest_url, .. } => self.open_link(dest_url.to_string(), false),
            Tag::Image { dest_url, .. } => self.open_link(dest_url.to_string(), true),
            // Footnotes, metadata, definition lists, and other extensions are off, so these
            // are not produced; their text still lands in the surrounding block.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::TableCell => self.flush_inline(),
            TagEnd::CodeBlock => {
                if let Some((language, mut code)) = self.code.take() {
                    if let Some(closed) = self.diagram_closed.take() {
                        self.push_block(Block::Diagram {
                            source: code.into(),
                            closed,
                        });
                        return;
                    }
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    self.push_block(Block::Code {
                        language,
                        code: code.into(),
                    });
                }
            }
            TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item if self.unflatten() => {}
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                if let Some(Frame::Quote(blocks)) = self.stack.pop() {
                    self.emit(Block::Quote(blocks));
                }
            }
            TagEnd::List(_) => {
                self.flush_inline();
                if let Some(Frame::List { start, items }) = self.stack.pop() {
                    self.emit(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                if let Some(Frame::Item(blocks)) = self.stack.pop()
                    && let Some(Frame::List { items, .. }) = self.stack.last_mut()
                {
                    items.push(blocks);
                }
            }
            TagEnd::TableHead => {
                self.flush_inline();
                if let Some(Frame::Table { in_head, .. }) = self.stack.last_mut() {
                    *in_head = false;
                }
            }
            TagEnd::Table => {
                self.flush_inline();
                if let Some(Frame::Table {
                    alignments,
                    header,
                    rows,
                    ..
                }) = self.stack.pop()
                {
                    self.emit(Block::Table {
                        alignments,
                        header,
                        rows,
                    });
                }
            }
            // An HTML block's text ends with the block, so it gets the block's range.
            TagEnd::HtmlBlock => self.flush_inline(),
            TagEnd::Strong => self.set_style(0, false),
            TagEnd::Emphasis => self.set_style(1, false),
            TagEnd::Strikethrough => self.set_style(2, false),
            TagEnd::Link | TagEnd::Image => {
                let open = self.links.pop();
                // An image with no alt text shows its destination, unless that is still
                // streaming or is the image's data itself.
                if let Some((start, url, true)) = &open
                    && url != mend::PENDING_LINK_URL
                    && !url
                        .get(..5)
                        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
                    && self
                        .inline
                        .as_ref()
                        .is_none_or(|(_, i)| i.text.len() == *start)
                {
                    self.text(&url.clone(), self.style);
                }
                self.set_style(3, false);
                if let Some((start, url, image)) = open
                    && let Some((_, inline)) = &mut self.inline
                    && start < inline.text.len()
                {
                    let end = inline.text.len();
                    // An image or link inside another one is part of that one.
                    if self.links.is_empty() {
                        if image {
                            inline.images.push((start..end, url.clone()));
                        }
                        inline.links.push((start..end, url));
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Arc<TopBlock>> {
        // A stream can stop anywhere, so close whatever is still open.
        if let Some((language, code)) = self.code.take() {
            self.push_block(Block::Code {
                language,
                code: code.into(),
            });
        }
        self.flush_inline();
        while let Some(frame) = self.stack.pop() {
            let block = match frame {
                Frame::Quote(blocks) => Block::Quote(blocks),
                Frame::Item(blocks) => {
                    if let Some(Frame::List { items, .. }) = self.stack.last_mut() {
                        items.push(blocks);
                    }
                    continue;
                }
                Frame::List { start, items } => Block::List { start, items },
                Frame::Table {
                    alignments,
                    header,
                    rows,
                    ..
                } => Block::Table {
                    alignments,
                    header,
                    rows,
                },
            };
            self.emit(block);
        }
        self.root
    }
}

/// Whether an image destination is a path rather than a URL: it has no scheme and is
/// not network-relative. Remote images are never fetched; they stay links.
fn is_local(url: &str) -> bool {
    let scheme = url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    !url.trim().is_empty() && !scheme && !url.starts_with("//") && !url.starts_with('#')
}

/// The text selected in the [`Markdown`] that has focus in `window`, as copying it would
/// give, or `None` when that Markdown has no selection or nothing Markdown has focus.
pub fn focused_selection(window: &Window, cx: &App) -> Option<String> {
    selection::focused_text(window, cx)
}

/// Only web and mail links open. Agent text is not trusted to launch other handlers.
pub fn is_openable(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

/// Renders a [`Document`] with theme colors. Body text inherits the surrounding text size
/// and line height. Code blocks have a copy button, and web links open in the browser.
/// Text ranges stay inside this Markdown instance; copying joins selected text runs with
/// newlines, without Markdown syntax, list markers, or Mermaid diagrams.
#[derive(IntoElement)]
pub struct Markdown {
    id: SharedString,
    document: Arc<Document>,
    copy_focus: Option<(usize, FocusHandle)>,
    diagram_toggle_focus: Option<(usize, FocusHandle)>,
    managed_focus: bool,
    selection_focus: Option<FocusHandle>,
    on_selection_start: Option<selection::StartHandler>,
    images: Option<ImageRenderer>,
}

impl Markdown {
    /// `id` must be unique among sibling markdown elements. It also prefixes the debug
    /// selectors of copy buttons, as `{id}-copy-{n}`.
    pub fn new(id: impl Into<SharedString>, document: Arc<Document>) -> Self {
        Self {
            id: id.into(),
            document,
            copy_focus: None,
            diagram_toggle_focus: None,
            managed_focus: false,
            selection_focus: None,
            on_selection_start: None,
            images: None,
        }
    }

    /// Parses and renders `source`. Hosts that redraw often should cache the parse and
    /// use [`Markdown::new`].
    pub fn from_source(id: impl Into<SharedString>, source: &str) -> Self {
        Self::new(id, Arc::new(Document::parse(source)))
    }

    pub fn copy_focus(mut self, block: usize, focus: &FocusHandle) -> Self {
        self.copy_focus = Some((block, focus.clone()));
        self
    }

    pub fn diagram_toggle_focus(mut self, block: usize, focus: &FocusHandle) -> Self {
        self.diagram_toggle_focus = Some((block, focus.clone()));
        self
    }

    pub fn managed_focus(mut self) -> Self {
        self.managed_focus = true;
        self
    }

    /// Gives a host a keyboard entry point for text selection. Managed Markdown
    /// tracks this handle only while the host's text-selection action is active.
    pub fn selection_focus(mut self, focus: &FocusHandle) -> Self {
        self.selection_focus = Some(focus.clone());
        self
    }

    /// Draws each [`Block::Images`] image. Without it, an image shows as its alt text or path.
    pub fn images(mut self, render: ImageRenderer) -> Self {
        self.images = Some(render);
        self
    }

    /// Activates the host's text-selection action on a pointer press and returns
    /// its reusable focus handle, or rejects a stale press with `None`.
    /// Managed Markdown allocates no private handle.
    pub fn on_selection_start(
        mut self,
        handler: impl Fn(&mut Window, &mut App) -> Option<FocusHandle> + 'static,
    ) -> Self {
        self.on_selection_start = Some(Rc::new(handler));
        self
    }
}

/// Per-render state: the base text style and counters for unique child IDs. Code blocks
/// count separately, so the first block's copy button is always `{id}-copy-1`.
struct Ctx {
    id: SharedString,
    base: TextStyle,
    texts: usize,
    code_blocks: usize,
    copy_focus: Option<(usize, FocusHandle)>,
    diagram_toggle_focus: Option<(usize, FocusHandle)>,
    managed_focus: bool,
    selection: Entity<selection::Selection>,
    images: Option<ImageRenderer>,
}

impl RenderOnce for Markdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let selection = selection::Selection::prepare(
            &self.id,
            &self.document,
            self.selection_focus,
            self.managed_focus,
            window,
            cx,
        );
        let theme = cx.theme();
        let base = TextStyle {
            color: theme.colors.text,
            font_family: theme.fonts.ui.clone(),
            font_features: theme.fonts.features.clone(),
            ..Default::default()
        };
        let mut ctx = Ctx {
            id: self.id.clone(),
            base,
            texts: 0,
            code_blocks: 0,
            copy_focus: self.copy_focus,
            diagram_toggle_focus: self.diagram_toggle_focus,
            managed_focus: self.managed_focus,
            selection: selection.clone(),
            images: self.images,
        };
        let blocks: Vec<_> = self
            .document
            .blocks
            .iter()
            .map(|top| render_block(&top.block, &mut ctx, window, cx))
            .collect();
        let blocks = v_flex()
            .id(ElementId::Name(self.id.clone()))
            .w_full()
            .min_w_0()
            .gap_3()
            .children(blocks);
        selection::surface(self.id, blocks, selection, self.on_selection_start, cx)
    }
}

fn render_blocks(
    blocks: &[Block],
    ctx: &mut Ctx,
    window: &mut Window,
    cx: &mut App,
) -> Vec<AnyElement> {
    blocks
        .iter()
        .map(|block| render_block(block, ctx, window, cx))
        .collect()
}

fn render_block(block: &Block, ctx: &mut Ctx, window: &mut Window, cx: &mut App) -> AnyElement {
    let colors = cx.theme().colors;
    match block {
        Block::Paragraph(inline) => div()
            .min_w_0()
            .child(inline_text(inline, &ctx.base.clone(), ctx, cx))
            .into_any_element(),
        Block::Heading { level, text } => {
            let (size, line) = match level {
                1 => (TextSize::Title.px(), px(28.)),
                2 => (px(17.), px(24.)),
                _ => cx.theme().transcript_text(),
            };
            let style = TextStyle {
                font_weight: FontWeight::SEMIBOLD,
                font_family: if *level == 1 {
                    cx.theme().fonts.display.clone()
                } else {
                    ctx.base.font_family.clone()
                },
                ..ctx.base.clone()
            };
            div()
                .min_w_0()
                .when(*level <= 2, |d| d.pt_1())
                .text_size(size)
                .line_height(line)
                .child(inline_text(text, &style, ctx, cx))
                .into_any_element()
        }
        Block::Code { language, code } => {
            code_block(language.clone(), code.clone(), ctx, window, cx)
        }
        Block::Diagram { source, closed } => {
            ctx.code_blocks += 1;
            crate::diagram::Diagram::new(
                format!("{}-diagram-{}", ctx.id, ctx.code_blocks),
                source.clone(),
                *closed,
            )
            .when(ctx.managed_focus, |diagram| diagram.managed_focus())
            .when_some(
                ctx.copy_focus
                    .as_ref()
                    .filter(|(block, _)| *block == ctx.code_blocks),
                |diagram, (_, focus)| diagram.copy_focus(focus),
            )
            .when_some(
                ctx.diagram_toggle_focus
                    .as_ref()
                    .filter(|(block, _)| *block == ctx.code_blocks),
                |diagram, (_, focus)| diagram.toggle_focus(focus),
            )
            .into_any_element()
        }
        Block::List { start, items } => {
            let mut list = v_flex().min_w_0().gap_1();
            for (index, item) in items.iter().enumerate() {
                let marker = match start {
                    Some(start) => format!("{}.", start + index as u64),
                    None => "•".to_owned(),
                };
                list = list.child(
                    h_flex()
                        .items_start()
                        .gap_2()
                        .child(
                            div()
                                .min_w(px(14.))
                                .flex_shrink_0()
                                .text_color(colors.text_muted)
                                .child(marker),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_2()
                                .children(render_blocks(item, ctx, window, cx)),
                        ),
                );
            }
            list.into_any_element()
        }
        Block::Quote(blocks) => {
            let quoted = TextStyle {
                color: colors.text_muted,
                ..ctx.base.clone()
            };
            let outer = std::mem::replace(&mut ctx.base, quoted);
            let children = render_blocks(blocks, ctx, window, cx);
            ctx.base = outer;
            v_flex()
                .min_w_0()
                .gap_2()
                .pl_3()
                .border_l_2()
                .border_color(colors.border_strong)
                .children(children)
                .into_any_element()
        }
        Block::Table {
            alignments,
            header,
            rows,
        } => table(alignments, header, rows, ctx, cx),
        Block::Rule => div().h(px(1.)).bg(colors.border).into_any_element(),
        Block::Images(images) => h_flex()
            .min_w_0()
            .flex_wrap()
            .gap_2()
            .children(images.iter().map(|image| {
                match &ctx.images {
                    Some(render) => render(image, window, cx),
                    None => Label::new(if image.alt.trim().is_empty() {
                        image.url.clone()
                    } else {
                        image.alt.clone()
                    })
                    .color(Color::Muted)
                    .into_any_element(),
                }
            }))
            .into_any_element(),
    }
}

fn table(
    alignments: &[Alignment],
    header: &[Inline],
    rows: &[Vec<Inline>],
    ctx: &mut Ctx,
    cx: &mut App,
) -> AnyElement {
    let colors = cx.theme().colors;
    let columns = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let body_style = TextStyle {
        font_features: cx.theme().fonts.tabular(),
        ..ctx.base.clone()
    };
    let head_style = TextStyle {
        color: colors.text_muted,
        ..body_style.clone()
    };
    let mut table = v_flex()
        .surface(Elevation::Raised, cx)
        .overflow_hidden()
        .text_size(cx.theme().transcript_code_text().0)
        .line_height(cx.theme().transcript_code_text().1);
    for (index, cells) in std::iter::once(header)
        .chain(rows.iter().map(Vec::as_slice))
        .enumerate()
    {
        let style = if index == 0 { &head_style } else { &body_style };
        let mut row = h_flex().items_start().when(index > 0, |d| d.hairline_t(cx));
        for column in 0..columns {
            let cell = div().flex_1().min_w_0().px_3().py_2();
            let cell = match alignments.get(column) {
                Some(Alignment::Center) => cell.text_center(),
                Some(Alignment::Right) => cell.text_right(),
                _ => cell,
            };
            row = row.child(match cells.get(column) {
                Some(inline) => cell.child(inline_text(inline, style, ctx, cx)),
                None => cell,
            });
        }
        table = table.child(row);
    }
    table.into_any_element()
}

fn code_block(
    language: Option<SharedString>,
    code: SharedString,
    ctx: &mut Ctx,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    ctx.code_blocks += 1;
    let n = ctx.code_blocks;
    let colors = cx.theme().colors;
    let mono = cx.theme().fonts.mono.clone();
    let (code_size, code_line) = cx.theme().transcript_code_text();
    let code_style = TextStyle {
        font_family: mono.clone(),
        font_features: FontFeatures::default(),
        color: colors.code_text,
        ..ctx.base.clone()
    };
    let code_text = inline_text(
        &Inline {
            text: code.clone(),
            ..Default::default()
        },
        &code_style,
        ctx,
        cx,
    );
    // Shows a check for a moment after copying. Kept per block while it stays on screen.
    let copied = window.use_keyed_state(
        ElementId::NamedInteger(format!("{}-copied", ctx.id).into(), n as u64),
        cx,
        |_, _| false,
    );
    let is_copied = *copied.read(cx);
    let copy = IconButton::new(
        ElementId::NamedInteger("copy".into(), n as u64),
        if is_copied {
            IconName::Check
        } else {
            IconName::Copy
        },
    )
    .when_some(
        ctx.copy_focus.as_ref().filter(|(block, _)| *block == n),
        |button, (_, focus)| button.track_focus(focus).reveal_on_focus(),
    )
    .debug_selector({
        let id = ctx.id.clone();
        move || format!("{id}-copy-{n}")
    })
    .tooltip(Tooltip::text(if is_copied {
        "Copied"
    } else {
        "Copy code"
    }))
    .on_click({
        let code = code.clone();
        move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
            copied.update(cx, |copied, cx| {
                *copied = true;
                cx.notify();
                cx.spawn(async move |copied, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(1500))
                        .await;
                    let _ = copied.update(cx, |copied, cx| {
                        *copied = false;
                        cx.notify();
                    });
                })
                .detach();
            });
        }
    });
    v_flex()
        .min_w_0()
        .surface(Elevation::Raised, cx)
        .overflow_hidden()
        .child(
            h_flex()
                .pl_3()
                .pr_1()
                .pt_1()
                .gap_2()
                .child(div().flex_1().children(
                    language.map(|language| {
                        Label::new(language).size(TextSize::Ui).color(Color::Muted)
                    }),
                ))
                .child(copy),
        )
        .child(
            // Long lines scroll sideways instead of wrapping, as in an editor.
            div()
                .id(ElementId::NamedInteger("code".into(), n as u64))
                .overflow_x_scroll()
                .child(
                    div()
                        .px_3()
                        .pb_3()
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .font_family(mono)
                        .font_features(FontFeatures::default())
                        .text_size(code_size)
                        .line_height(code_line)
                        .text_color(colors.code_text)
                        .child(code_text),
                ),
        )
        .into_any_element()
}

/// Builds styled text for one inline run. Links become clickable ranges.
fn inline_text(inline: &Inline, base: &TextStyle, ctx: &mut Ctx, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let colors = theme.colors;
    let mut runs = Vec::new();
    let mut at = 0;
    for (range, style) in &inline.spans {
        if at < range.start {
            runs.push(base.to_run(range.start - at));
        }
        let mut text_style = base.clone();
        if style.code {
            text_style.font_family = theme.fonts.mono.clone();
            text_style.font_features = FontFeatures::default();
        }
        let highlight = HighlightStyle {
            color: if style.link {
                Some(colors.accent)
            } else if style.code {
                Some(colors.code_text)
            } else {
                None
            },
            font_weight: style.strong.then_some(FontWeight::SEMIBOLD),
            font_style: style.emphasis.then_some(FontStyle::Italic),
            background_color: style.code.then_some(colors.element_hover),
            underline: style.link.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(colors.accent),
                wavy: false,
            }),
            strikethrough: style.strikethrough.then_some(StrikethroughStyle {
                thickness: px(1.),
                color: None,
            }),
            ..Default::default()
        };
        runs.push(text_style.highlight(highlight).to_run(range.len()));
        at = range.end;
    }
    if at < inline.text.len() {
        runs.push(base.to_run(inline.text.len() - at));
    }
    let index = ctx.texts;
    ctx.texts += 1;
    let n = ctx.texts;
    let text = selection::text(inline.text.clone(), runs, index, &ctx.selection, cx);
    let layout = text.layout().clone();
    if inline.links.is_empty() {
        return selection::track(
            text.into_any_element(),
            layout,
            index,
            ctx.selection.clone(),
        );
    }
    let tips: Vec<SharedString> = inline.links.iter().map(|(_, url)| url.clone()).collect();
    let tip_ranges: Vec<Range<usize>> = inline.links.iter().map(|(r, _)| r.clone()).collect();
    let text = InteractiveText::new(ElementId::NamedInteger("text".into(), n as u64), text)
        .tooltip(move |at, _, cx| {
            let index = tip_ranges.iter().position(|r| r.contains(&at))?;
            let tip = match tips[index].as_ref() {
                mend::PENDING_LINK_URL => "Link loading".into(),
                _ => tips[index].clone(),
            };
            Some(cx.new(|_| Tooltip::text(tip)).into())
        })
        .into_any_element();
    selection::track(text, layout, index, ctx.selection.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(doc: &Document) -> Vec<Block> {
        doc.blocks.iter().map(|top| top.block.clone()).collect()
    }

    fn paragraph(doc: &Document) -> &Inline {
        match &doc.blocks[0].block {
            Block::Paragraph(inline) => inline,
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn inline_styles_become_ranges() {
        let doc =
            Document::parse("Use **bold *both*** and `code` or ~~old~~ [docs](https://x.dev).");
        let inline = paragraph(&doc);
        assert_eq!(inline.text, "Use bold both and code or old docs.");
        let styled: Vec<_> = inline
            .spans
            .iter()
            .map(|(r, s)| (&inline.text[r.clone()], *s))
            .collect();
        let strong = Style {
            strong: true,
            ..Default::default()
        };
        assert_eq!(styled[0], ("bold ", strong));
        assert_eq!(
            styled[1],
            (
                "both",
                Style {
                    emphasis: true,
                    ..strong
                }
            )
        );
        assert!(styled.contains(&(
            "code",
            Style {
                code: true,
                ..Default::default()
            }
        )));
        assert!(styled.iter().any(|(t, s)| *t == "old" && s.strikethrough));
        assert_eq!(inline.links.len(), 1);
        assert_eq!(&inline.text[inline.links[0].0.clone()], "docs");
        assert_eq!(inline.links[0].1, "https://x.dev");
    }

    #[test]
    fn blocks_nest() {
        let doc = Document::parse(
            "# Title\n\n- one\n- two\n  1. inner\n\n> quoted\n\n---\n\n```rust\nfn main() {}\n```\n",
        );
        assert!(
            matches!(&doc.blocks[0].block, Block::Heading { level: 1, text } if text.text == "Title")
        );
        let Block::List { start: None, items } = &doc.blocks[1].block else {
            panic!("{:?}", doc.blocks[1].block);
        };
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0][0], Block::Paragraph(i) if i.text == "one"));
        assert!(matches!(&items[1][1], Block::List { start: Some(1), items } if items.len() == 1));
        assert!(
            matches!(&doc.blocks[2].block, Block::Quote(b) if matches!(&b[0], Block::Paragraph(i) if i.text == "quoted"))
        );
        assert_eq!(doc.blocks[3].block, Block::Rule);
        assert_eq!(
            doc.blocks[4].block,
            Block::Code {
                language: Some("rust".into()),
                code: "fn main() {}".into()
            }
        );
    }

    #[test]
    fn tables_keep_header_rows_and_alignment() {
        let doc = Document::parse("| A | B |\n|:--|--:|\n| `x` | 2 |\n| y |\n");
        let Block::Table {
            alignments,
            header,
            rows,
        } = &doc.blocks[0].block
        else {
            panic!("{:?}", doc.blocks);
        };
        assert_eq!(alignments, &[Alignment::Left, Alignment::Right]);
        assert_eq!(
            header.iter().map(|c| c.text.as_ref()).collect::<Vec<_>>(),
            ["A", "B"]
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].text, "x");
        assert!(rows[0][0].spans[0].1.code);
        assert_eq!(rows[1][0].text, "y");
    }

    /// Every prefix of a reply parses, as it does while streaming, and an open fence
    /// stays a code block instead of turning back into paragraphs.
    #[test]
    fn every_streamed_prefix_parses() {
        let reply = "## Plan\n\n1. **Read** the `docs`\n2. Run:\n\n```bash\ncargo test\n```\n\n| a | b |\n|---|---|\n| é | 😀 |\n\n> [link](https://e.x) ~~gone~~\n";
        for end in (0..=reply.len()).filter(|&i| reply.is_char_boundary(i)) {
            let doc = Document::parse(&reply[..end]);
            check_ranges(&top(&doc));
        }
        let open = Document::parse("Text\n\n```bash\ncargo te");
        assert_eq!(
            open.blocks[1].block,
            Block::Code {
                language: Some("bash".into()),
                code: "cargo te".into()
            }
        );
    }

    fn check_ranges(blocks: &[Block]) {
        let check = |inline: &Inline| {
            let mut at = 0;
            for (range, _) in &inline.spans {
                assert!(range.start >= at && range.end <= inline.text.len());
                assert!(inline.text.is_char_boundary(range.start));
                assert!(inline.text.is_char_boundary(range.end));
                at = range.end;
            }
            for (range, _) in &inline.links {
                assert!(range.end <= inline.text.len());
            }
        };
        for block in blocks {
            match block {
                Block::Paragraph(i) | Block::Heading { text: i, .. } => check(i),
                Block::List { items, .. } => items.iter().for_each(|b| check_ranges(b)),
                Block::Quote(b) => check_ranges(b),
                Block::Table { header, rows, .. } => {
                    header.iter().chain(rows.iter().flatten()).for_each(check)
                }
                Block::Code { .. } | Block::Diagram { .. } | Block::Rule | Block::Images(_) => {}
            }
        }
    }

    /// How deeply quotes and lists nest, counting a list and its items as one level.
    fn nesting(blocks: &[Block]) -> usize {
        blocks
            .iter()
            .map(|block| match block {
                Block::Quote(b) => 1 + nesting(b),
                Block::List { items, .. } => {
                    1 + items.iter().map(|b| nesting(b)).max().unwrap_or(0)
                }
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }

    fn texts(blocks: &[Block], out: &mut Vec<String>) {
        for block in blocks {
            match block {
                Block::Paragraph(i) | Block::Heading { text: i, .. } => {
                    out.push(i.text.to_string())
                }
                Block::Code { code, .. } => out.push(code.to_string()),
                Block::Diagram { source, .. } => out.push(source.to_string()),
                Block::List { items, .. } => items.iter().for_each(|b| texts(b, out)),
                Block::Quote(b) => texts(b, out),
                Block::Table { .. } | Block::Rule | Block::Images(_) => {}
            }
        }
    }

    /// Replies are untrusted. Deep nesting parses, keeps its text, and drops without
    /// recursing past the limit, all on the test thread's small stack.
    #[test]
    fn nesting_is_bounded_and_keeps_text() {
        let cases = [
            "> ".repeat(100_000) + "hello",
            (0..3_000)
                .map(|i| format!("{}- item {i}\n", "  ".repeat(i)))
                .collect(),
            "> - 1. ".repeat(20_000) + "hello",
            "- > ".repeat(20_000) + "hello\n\n```\ncode\n```",
        ];
        for source in &cases {
            let doc = Document::parse(source);
            assert_eq!(nesting(&top(&doc)), MAX_NESTING, "{}", &source[..40]);
            let mut out = Vec::new();
            texts(&top(&doc), &mut out);
            assert!(
                out.iter()
                    .any(|t| t.contains("hello") || t.contains("item 2999"))
            );
            check_ranges(&top(&doc));
        }
        // Lists past the limit keep their items, in order, as flat blocks.
        let doc = Document::parse(&cases[1]);
        let mut out = Vec::new();
        texts(&top(&doc), &mut out);
        let expected: Vec<String> = (0..3_000).map(|i| format!("item {i}")).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn nesting_up_to_the_limit_is_kept() {
        let doc = Document::parse(&("> ".repeat(MAX_NESTING) + "deep"));
        assert_eq!(nesting(&top(&doc)), MAX_NESTING);
        let mut block = &doc.blocks[0].block;
        for _ in 1..MAX_NESTING {
            let Block::Quote(inner) = block else {
                panic!("{block:?}")
            };
            assert_eq!(inner.len(), 1);
            block = &inner[0];
        }
        assert!(
            matches!(block, Block::Quote(b) if b == &[Block::Paragraph(Inline { text: "deep".into(), ..Default::default() })])
        );
        // One more level is flattened into the innermost kept quote.
        let doc = Document::parse(&("> ".repeat(MAX_NESTING + 1) + "deep\n\nafter"));
        assert_eq!(nesting(&top(&doc)), MAX_NESTING);
        let mut out = Vec::new();
        texts(&top(&doc), &mut out);
        assert_eq!(out, ["deep", "after"]);
    }

    /// Inline styles and links are flat ranges, so their nesting has no depth to bound.
    #[test]
    fn deep_inline_nesting_stays_flat() {
        let source = "*_".repeat(20_000)
            + "hi"
            + &"_*".repeat(20_000)
            + " "
            + &"**~~".repeat(10_000)
            + "x"
            + &"~~**".repeat(10_000)
            + " "
            + &"[".repeat(10_000)
            + "link"
            + &"](https://e.x)".repeat(10_000);
        let doc = Document::parse(&source);
        assert!(paragraph(&doc).text.contains("hi"));
        check_ranges(&top(&doc));
    }

    /// Every prefix of an adversarial reply parses within the limit, as it streams in.
    #[test]
    fn every_streamed_prefix_of_deep_nesting_parses() {
        let reply = "> - 1. **a *b ".repeat(60) + "[x](https://e.x)\n\n```\ncode";
        for end in (0..=reply.len()).filter(|&i| reply.is_char_boundary(i)) {
            let doc = Document::parse(&reply[..end]);
            assert!(nesting(&top(&doc)) <= MAX_NESTING);
            check_ranges(&top(&doc));
        }
    }

    #[test]
    fn raw_html_is_text_and_only_web_links_open() {
        let doc = Document::parse("<script>alert(1)</script>\n\nhi <b>there</b>");
        assert!(matches!(&doc.blocks[0].block, Block::Paragraph(i) if i.text.contains("<script>")));
        assert!(is_openable("https://example.com"));
        assert!(is_openable("mailto:a@b.c"));
        assert!(!is_openable("file:///etc/passwd"));
        assert!(!is_openable("javascript:alert(1)"));
        assert!(!is_openable("/usr/bin/xdg-open"));
    }

    #[test]
    fn a_paragraph_of_local_images_is_an_image_block() {
        let image = |url: &str, alt: &str| ImageRef {
            url: url.to_owned().into(),
            alt: alt.to_owned().into(),
        };
        assert_eq!(
            top(&Document::parse("![](./shot.png)")),
            [Block::Images(vec![image("./shot.png", "")])]
        );
        assert_eq!(
            top(&Document::parse("- ![a](a.png)\n  ![b](/tmp/b.jpg)")),
            [Block::List {
                start: None,
                items: vec![vec![Block::Images(vec![
                    image("a.png", "a"),
                    image("/tmp/b.jpg", "b"),
                ])]],
            }]
        );
    }

    #[test]
    fn remote_and_inline_images_stay_links_showing_their_destination() {
        for (source, text, url) in [
            (
                "![](https://e.x/a.png)",
                "https://e.x/a.png",
                "https://e.x/a.png",
            ),
            ("![](//e.x/a.png)", "//e.x/a.png", "//e.x/a.png"),
            ("![chart](http://e.x/c.png)", "chart", "http://e.x/c.png"),
            ("See ![](./shot.png) here", "./shot.png", "./shot.png"),
        ] {
            let doc = Document::parse(source);
            let inline = paragraph(&doc);
            assert_eq!(inline.links.len(), 1, "{source}");
            assert_eq!(&inline.text[inline.links[0].0.clone()], text, "{source}");
            assert_eq!(inline.links[0].1, url, "{source}");
        }
        assert!(
            !paragraph(&Document::parse("x ![](data:image/png;base64,AAAA)"))
                .text
                .contains("base64")
        );
        let mixed = Document::parse("![](a.png) ![](https://e.x/b.png)");
        assert_eq!(paragraph(&mixed).links.len(), 2);
        for source in [
            "[![chart](shot.png)](https://e.x)",
            "[![](shot.png)](https://e.x)",
        ] {
            let links = paragraph(&Document::parse(source)).links.clone();
            assert_eq!(links.len(), 1, "{source}");
            assert_eq!(links[0].1, "https://e.x", "{source}");
        }
        let mut parser = IncrementalParser::new();
        parser.append("Done.\n\n![](./sh");
        let shown = parser.display();
        let blocks = top(&shown);
        let Some(Block::Paragraph(inline)) = blocks.last() else {
            panic!("{blocks:?}");
        };
        assert!(!inline.text.contains(mend::PENDING_LINK_URL), "{inline:?}");
        assert!(matches!(
            &top(&Document::parse("![![inner](in.png)](out.png)"))[0],
            Block::Images(images) if images.len() == 1 && images[0].url == "out.png"
        ));
        for nested in ["![![inner](in.png)](out.png)", "![![](in.png)](out.png) x"] {
            let mut parser = IncrementalParser::new();
            for ch in nested.chars() {
                parser.append(&ch.to_string());
                parser.display();
            }
        }
    }

    #[test]
    fn mermaid_preserves_source_and_requires_a_real_closing_fence() {
        for (source, expected, closed) in [
            (
                "```mermaid\nflowchart TD\n  A[é] --> B  \n```\n",
                "flowchart TD\n  A[é] --> B  \n",
                true,
            ),
            (
                "```mermaid\r\nflowchart TD\r\n  A --> B  \r\n```\r\n",
                "flowchart TD\r\n  A --> B  \r\n",
                true,
            ),
            (
                "```mermaid\nflowchart TD\nA-->B\n",
                "flowchart TD\nA-->B\n",
                false,
            ),
            (
                "```mermaid\nflowchart TD\nA-->B",
                "flowchart TD\nA-->B",
                false,
            ),
            (
                "~~~~mermaid\nflowchart TD\nA-->B\n~~~~  ",
                "flowchart TD\nA-->B\n",
                true,
            ),
            (
                "````mermaid\nflowchart TD\n```\n",
                "flowchart TD\n```\n",
                false,
            ),
            (
                "```mermaid\nflowchart TD\n``` trailing",
                "flowchart TD\n``` trailing",
                false,
            ),
            ("```mermaid", "", false),
            ("```mermaid\n```", "", true),
            (
                "```mermaid\nflowchart TD\n    ```",
                "flowchart TD\n    ```",
                false,
            ),
            (
                "```mermaid\nflowchart TD\n> ```",
                "flowchart TD\n> ```",
                false,
            ),
        ] {
            assert_eq!(
                Document::parse(source).blocks[0].block,
                Block::Diagram {
                    source: expected.to_owned().into(),
                    closed,
                },
                "{source:?}"
            );
        }
    }

    #[test]
    fn nested_mermaid_preserves_logical_source_line_endings() {
        let doc = Document::parse("> ```mermaid\r\n> flowchart TD\r\n>   A --> B  \r\n> ```\r\n");
        let Block::Quote(blocks) = &doc.blocks[0].block else {
            panic!("{doc:?}");
        };
        assert_eq!(
            blocks[0],
            Block::Diagram {
                source: "flowchart TD\r\n  A --> B  \r\n".into(),
                closed: true,
            }
        );
    }

    #[test]
    fn every_mermaid_stream_prefix_matches_a_whole_parse() {
        let source = "Intro\n\n```mermaid\r\nflowchart TD\r\n  A[é] --> B  \r\n```\r\n\nAfter";
        let mut parser = IncrementalParser::new();
        for end in (0..=source.len()).filter(|&i| source.is_char_boundary(i)) {
            parser.set_text(&source[..end]);
            assert_eq!(*parser.document(), Document::parse(&source[..end]));
            for block in &parser.display().blocks {
                if let Block::Diagram {
                    source: body,
                    closed,
                } = &block.block
                {
                    assert_eq!(*closed, source[..end].contains("\r\n```"));
                    let expected = if *closed {
                        "flowchart TD\r\n  A[é] --> B  \r\n"
                    } else {
                        source
                            .get(source.find("flowchart").unwrap()..end)
                            .unwrap_or("")
                    };
                    assert_eq!(body.as_ref(), expected, "prefix ending at {end}");
                }
            }
        }
        assert_eq!(*parser.finish(source), Document::parse(source));
    }

    #[test]
    fn quoted_mermaid_prefixes_never_copy_container_markers() {
        let source = "> ```mermaid\n> flowchart TD\n>   A[é] --> B  \n> ```\n";
        let body_start = source.find("flowchart").unwrap() - 2;
        let mut parser = IncrementalParser::new();
        for end in (0..=source.len()).filter(|&i| source.is_char_boundary(i)) {
            let prefix = &source[..end];
            parser.set_text(prefix);
            assert_eq!(*parser.document(), Document::parse(prefix));
            let Some(top) = parser.document().blocks.first().cloned() else {
                continue;
            };
            let Block::Quote(blocks) = &top.block else {
                continue;
            };
            let Some(Block::Diagram {
                source: body,
                closed,
            }) = blocks.first()
            else {
                continue;
            };
            let expected = if *closed {
                "flowchart TD\n  A[é] --> B  \n".to_owned()
            } else {
                prefix
                    .get(body_start..)
                    .unwrap_or("")
                    .split_inclusive('\n')
                    .map(|line| line.strip_prefix('>').unwrap_or(line))
                    .map(|line| line.strip_prefix(' ').unwrap_or(line))
                    .collect()
            };
            assert_eq!(body.as_ref(), expected, "quoted prefix ending at {end}");
        }
        for (source, expected) in [
            ("> ```mermaid\n> flowchart TD\n>", "flowchart TD\n"),
            ("> ```mermaid\n> flowchart TD\n>\t ", "flowchart TD\n "),
            ("- ```mermaid\n  flowchart TD\n   ", "flowchart TD\n "),
        ] {
            let doc = Document::parse(source);
            let mut out = Vec::new();
            texts(&top(&doc), &mut out);
            assert_eq!(out, [expected]);
        }
    }

    #[test]
    fn mermaid_container_tabs_keep_synthesized_indentation() {
        for (source, expected) in [
            (
                "- ```mermaid\n  mindmap\n  Root\n\tChild\n  ```\n",
                "mindmap\nRoot\n  Child\n",
            ),
            (
                "- > ```mermaid\n  > mindmap\n  > Root\n\t>\tChild\n  > ```\n",
                "mindmap\nRoot\n  Child\n",
            ),
            (
                ">   ```mermaid\r\n>   mindmap\r\n>   Root\r\n>\t\tChild\r\n>   ```\r\n",
                "mindmap\r\nRoot\r\n\tChild\r\n",
            ),
            ("> ```mermaid\n> a\n>\t\tb\n> ```", "a\n  \tb\n"),
            ("> ```mermaid\n> a\n>\t\tb\n>\t", "a\n  \tb\n"),
        ] {
            let doc = Document::parse(source);
            let mut out = Vec::new();
            texts(&top(&doc), &mut out);
            assert_eq!(out, [expected], "{source:?}");
            let mut parser = IncrementalParser::new();
            for end in (0..=source.len()).filter(|&i| source.is_char_boundary(i)) {
                parser.set_text(&source[..end]);
                assert_eq!(*parser.document(), Document::parse(&source[..end]));
            }
        }
    }
}
