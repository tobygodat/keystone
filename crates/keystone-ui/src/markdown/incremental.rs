//! Incremental parsing for streamed replies.
//!
//! Adapted from Zeron's `IncrementalParser` in `crates/markdown/src/parser.rs`
//! (https://github.com/zeronsh/zeron), Copyright (c) 2026 Wing, under the MIT License.
//! See `THIRD_PARTY_NOTICES.md`.

use super::{Document, mend, parse_at};
use std::sync::Arc;

/// A streamed reply's source and its parsed [`Document`]. Each append parses again
/// from the start of the second-to-last top-level block and keeps the blocks before it,
/// so the work per delta follows the reply's tail, not its length. The result always
/// equals [`Document::parse`] of the whole source.
#[derive(Clone, Debug, Default)]
pub struct IncrementalParser {
    source: String,
    document: Arc<Document>,
    /// The document with the last block's hanging inline markers closed. It is the
    /// same as `document` when nothing hangs.
    display: Arc<Document>,
    /// A link reference definition can turn text anywhere into a link, so a source
    /// that may have one is always parsed whole. Every definition has a `]:`.
    whole: bool,
    /// Whether `document` is a whole parse of the finished reply.
    finished: bool,
}

impl IncrementalParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// The parse of the source as it stands.
    pub fn document(&self) -> Arc<Document> {
        self.document.clone()
    }

    /// The document to show once the reply is complete: a whole parse of `text`, made
    /// once, so the finished reply never depends on how it streamed.
    pub fn finish(&mut self, text: &str) -> Arc<Document> {
        if !self.finished || text != self.source {
            self.source = text.to_owned();
            self.whole = text.contains("]:");
            self.document = Arc::new(Document::parse(text));
            self.display = self.document.clone();
            self.finished = true;
        }
        self.document()
    }

    /// The document to show while the reply streams. Unclosed bold, italic, code, and
    /// links in the last block are closed, so their styling does not jump when the
    /// closer arrives.
    pub fn display(&self) -> Arc<Document> {
        self.display.clone()
    }

    /// Replaces the source. Text that extends the current source is appended; any
    /// other text is parsed whole. Returns whether the source changed.
    pub fn set_text(&mut self, text: &str) -> bool {
        if text == self.source {
            return false;
        }
        match text.strip_prefix(self.source.as_str()) {
            Some(delta) => self.append(delta),
            None => {
                self.source = text.to_owned();
                self.whole = text.contains("]:");
                self.finished = false;
                self.document = Arc::new(Document::parse(text));
                self.mend();
            }
        }
        true
    }

    pub fn append(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.whole = self.whole
            || delta.contains("]:")
            || (self.source.ends_with(']') && delta.starts_with(':'));
        self.finished = false;
        self.source.push_str(delta);
        if self.whole {
            self.document = Arc::new(Document::parse(&self.source));
        } else {
            // The last block can grow into the one before it, as a trailing `3` becomes
            // `3.` and joins a list above. Whether a block joins the one before is
            // decided by its own first bytes, so two blocks are enough.
            let blocks = &self.document.blocks;
            let boundary = match blocks.len() {
                0 | 1 => 0,
                n => blocks[n - 2].range.start,
            };
            // From the line start, so indentation still decides code blocks and lists.
            let boundary = self.source[..boundary].rfind('\n').map_or(0, |i| i + 1);
            let tail = parse_at(&self.source[boundary..], boundary);
            let blocks = &mut Arc::make_mut(&mut self.document).blocks;
            blocks.retain(|top| top.range.start < boundary);
            blocks.extend(tail);
        }
        self.mend();
    }

    /// Only the last block can have hanging markers: a blank line ends a block, and
    /// CommonMark leaves a marker that is still open at a block's end as literal text.
    fn mend(&mut self) {
        self.display = self.document.clone();
        let Some(last) = self.document.blocks.last() else {
            return;
        };
        let start = last.range.start;
        let Some(mended) = mend::close_block(&self.source[start..]) else {
            return;
        };
        self.display = Arc::new(if self.whole {
            Document::parse(&format!("{}{mended}", &self.source[..start]))
        } else {
            let kept = self.document.blocks.len() - 1;
            let mut blocks = self.document.blocks[..kept].to_vec();
            blocks.extend(parse_at(&mended, start));
            Document { blocks }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{Block, Inline, Style};

    const FIXTURE: &str = include_str!("../../../keystone-desktop/src/fixture.md");

    /// Ordinary replies, then cases where a chunk boundary can land inside a construct
    /// that changes the meaning of text before it.
    const CORPORA: &[&str] = &[
        FIXTURE,
        "# Title\n\nHello **bold** and *italic* and `code` and ~~gone~~.\n",
        "Paragraph one\nlazy continuation\n\nParagraph two with a [link](https://x.dev).\n",
        "- item one\n- item two\n  - nested a\n  - nested b\n- item three\n\ntail\n",
        "1. first\n2. second\n\n   loose paragraph in item\n\n3. third\n",
        "para\n\n3\n\n- a\n- b\n\n3. joins\n",
        "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nafter code\n",
        "intro\n\n```\nunclosed fence streaming",
        "~~~\nfence with `~`\n~~~\n\n````md\n```\nnested\n```\n````\n",
        "> quoted line\n> more quote\n>\n> - a list in a quote\n\nplain\n",
        "| a | b |\n|:--|--:|\n| 1 | **2** |\n| 3 | `4` |\n\ndone\n",
        "text above\n| a | b |\n|---|---|\n| 1 | 2 |\n",
        "setext candidate\n===\n\nnext para\n---\n\n- list\n-\n",
        "***\n\ntext between rules\n\n---\n",
        "    indented code line one\n    line two\n\npara\n",
        "para with <span>inline html</span> inside\n\n<div>\nblock html\n</div>\n\nafter\n",
        "**bold across\nlines** and *em\nacross* and `code\nspan` and [link\ntext](https://e.x)\n",
        "- **a\n- b**\n\n> *c\n\nd*\n",
        "Unicode é 😀 **ß** `→`\r\n\r\n- ü\r\n- 🦀\r\n",
        "###### deep heading\n\n#### h4\n",
        "- item\n\n  ```sh\n  echo **hello\n  ```\n\n> ~~~\n> x `y\n> ~~~\n",
        REFERENCE_CORPORA[0],
        REFERENCE_CORPORA[1],
        REFERENCE_CORPORA[2],
        REFERENCE_CORPORA[3],
    ];

    /// Link reference definitions at any depth or across lines, which change text above them.
    const REFERENCE_CORPORA: &[&str] = &[
        "See [docs].\n\nSecond.\n\nThird.\n\n> [docs]: https://example.com\n",
        "See [docs].\n\nSecond.\n\nThird.\n\n- [docs]: https://example.com\n",
        "See [foo bar].\n\nSecond.\n\nThird.\n\n[foo\nbar]: https://example.com\n",
        "[docs]: https://example.com\n\nIntro.\n\nSee [docs] and **bold** and *em*.\n",
    ];

    fn stream(text: &str, chunk: usize) -> IncrementalParser {
        let mut parser = IncrementalParser::new();
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + chunk).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
            parser.append(&text[start..end]);
            start = end;
        }
        parser
    }

    #[test]
    fn streaming_matches_a_whole_parse_at_every_chunk_size() {
        for corpus in CORPORA {
            let whole = Document::parse(corpus);
            for chunk in 1..=64 {
                assert_eq!(
                    *stream(corpus, chunk).document(),
                    whole,
                    "chunk {chunk}:\n{corpus}"
                );
            }
        }
    }

    #[test]
    fn every_streamed_prefix_matches_a_whole_parse() {
        for corpus in CORPORA {
            let mut parser = IncrementalParser::new();
            for (i, ch) in corpus.char_indices() {
                parser.append(ch.encode_utf8(&mut [0; 4]));
                let prefix = &corpus[..i + ch.len_utf8()];
                assert_eq!(*parser.document(), Document::parse(prefix), "{prefix:?}");
            }
        }
    }

    #[test]
    fn link_reference_definitions_resolve_while_streaming() {
        let corpus =
            "See [docs] for more.\n\nMore.\n\n- And\n\nmore.\n\n[docs]: https://example.com\n";
        let whole = Document::parse(corpus);
        for chunk in [1, 3, 9] {
            assert_eq!(*stream(corpus, chunk).document(), whole, "chunk {chunk}");
        }
        let Block::Paragraph(inline) = &whole.blocks[0].block else {
            panic!("{whole:?}");
        };
        assert_eq!(inline.links[0].1, "https://example.com");
    }

    #[test]
    fn definitions_inside_containers_or_across_lines_resolve() {
        for corpus in &REFERENCE_CORPORA[..3] {
            let whole = Document::parse(corpus);
            let Block::Paragraph(first) = &whole.blocks[0].block else {
                panic!("{whole:?}");
            };
            assert_eq!(first.links[0].1, "https://example.com", "{corpus:?}");
            for chunk in 1..=16 {
                let mut parser = stream(corpus, chunk);
                assert_eq!(*parser.document(), whole, "chunk {chunk}:\n{corpus}");
                assert_eq!(*parser.finish(corpus), whole, "chunk {chunk}:\n{corpus}");
            }
        }
    }

    #[test]
    fn appends_reuse_the_blocks_they_did_not_reach() {
        let mut parser = IncrementalParser::new();
        parser.set_text("first **bold** paragraph\n\nsecond paragraph\n\nthird");
        let before = parser.document();
        parser.append(" grows\n\nfourth");
        let after = parser.document();
        assert!(Arc::ptr_eq(&before.blocks[0], &after.blocks[0]));
        assert_eq!(after.blocks.len(), 4);
        assert_eq!(before.blocks.len(), 3, "an earlier document never changes");
    }

    #[test]
    fn set_text_appends_or_starts_over() {
        let mut parser = IncrementalParser::new();
        assert!(parser.set_text("hello"));
        assert!(parser.set_text("hello world"));
        assert!(!parser.set_text("hello world"));
        assert_eq!(*parser.document(), Document::parse("hello world"));
        assert!(parser.set_text("different\n\n[a]: https://e.x"));
        assert_eq!(parser.source(), "different\n\n[a]: https://e.x");
        assert_eq!(
            *parser.document(),
            Document::parse("different\n\n[a]: https://e.x")
        );
        parser.set_text("plain");
        parser.append(" [a]\n\n[a]: https://e.x");
        assert_eq!(
            *parser.document(),
            Document::parse("plain [a]\n\n[a]: https://e.x")
        );
    }

    fn last_inline(document: &Document) -> &Inline {
        fn leaf(block: &Block) -> &Inline {
            match block {
                Block::Paragraph(inline) | Block::Heading { text: inline, .. } => inline,
                Block::List { items, .. } => leaf(items.last().unwrap().last().unwrap()),
                Block::Quote(blocks) => leaf(blocks.last().unwrap()),
                other => panic!("{other:?}"),
            }
        }
        leaf(&document.blocks.last().unwrap().block)
    }

    /// The text shown and the style of its every byte.
    fn styled(inline: &Inline) -> Vec<(String, Style)> {
        let mut out = Vec::new();
        let mut at = 0;
        for (range, style) in &inline.spans {
            if at < range.start {
                out.push((inline.text[at..range.start].to_owned(), Style::default()));
            }
            out.push((inline.text[range.clone()].to_owned(), *style));
            at = range.end;
        }
        if at < inline.text.len() {
            out.push((inline.text[at..].to_owned(), Style::default()));
        }
        out
    }

    #[test]
    fn display_styles_unclosed_markers_and_the_document_does_not() {
        let mut parser = IncrementalParser::new();
        parser.set_text("Done.\n\nUse **cargo te");
        let strong = Style {
            strong: true,
            ..Default::default()
        };
        assert_eq!(
            styled(last_inline(&parser.display())),
            [
                ("Use ".to_owned(), Style::default()),
                ("cargo te".to_owned(), strong)
            ]
        );
        assert_eq!(last_inline(&parser.document()).text, "Use **cargo te");
        assert_eq!(parser.display().blocks[0], parser.document().blocks[0]);

        parser.append("st** now");
        assert_eq!(parser.display(), parser.document());
        assert_eq!(last_inline(&parser.display()).text, "Use cargo test now");
    }

    #[test]
    fn display_keeps_styled_text_stable_while_it_streams() {
        let reply =
            "Run `cargo test` with **all features** and see [the docs](https://e.x) or ~~not~~.";
        let finished = Document::parse(reply);
        let words: Vec<_> = styled(last_inline(&finished))
            .into_iter()
            .filter(|(_, style)| *style != Style::default())
            .collect();
        let mut parser = IncrementalParser::new();
        for (i, ch) in reply.char_indices() {
            parser.append(ch.encode_utf8(&mut [0; 4]));
            let shown = styled(last_inline(&parser.display()));
            // Every styled word shown so far has its final style, never literal markers.
            for (text, style) in shown.iter().filter(|(_, s)| *s != Style::default()) {
                let full = words
                    .iter()
                    .find(|(word, _)| word.starts_with(text.as_str()));
                assert!(
                    full.is_some_and(|(_, s)| s == style),
                    "after {:?}: {text:?} {style:?}",
                    &reply[..=i]
                );
            }
            let plain: String = shown
                .iter()
                .filter(|(_, s)| *s == Style::default())
                .map(|(t, _)| t.as_str())
                .collect();
            // A bare opener waits for content, so markers show only at the very end.
            let settled = plain.trim_end_matches(['*', '`', '~', '[', ' ']);
            assert!(
                !settled.contains("**"),
                "after {:?}: {plain:?}",
                &reply[..=i]
            );
        }
        assert_eq!(parser.display(), parser.document());
    }

    #[test]
    fn a_closed_inner_marker_keeps_the_outer_one_open_across_lines() {
        let failing: Vec<_> = [
            "**outer\n*inner* tail",
            "- **outer\n  *inner* tail",
            "> **outer\n> *inner* tail",
            "Intro.\n\n- a\n- **outer `x`\n  *inner* tail",
            "# **outer *inner* tail",
            "**outer\n*inner* tail\n=",
        ]
        .into_iter()
        .filter(|source| {
            let mut parser = IncrementalParser::new();
            parser.set_text(source);
            let shown = styled(last_inline(&parser.display()));
            !shown
                .iter()
                .any(|(text, style)| text.starts_with("outer") && style.strong)
        })
        .collect();
        assert!(failing.is_empty(), "outer bold lost: {failing:?}");
    }

    #[test]
    fn a_link_shows_styled_before_its_url_arrives_and_never_opens() {
        let mut parser = IncrementalParser::new();
        parser.set_text("See [the docs](https://exa");
        let inline = last_inline(&parser.display()).clone();
        assert_eq!(inline.text, "See the docs");
        assert_eq!(&inline.text[inline.links[0].0.clone()], "the docs");
        assert!(!crate::markdown::is_openable(&inline.links[0].1));
    }

    /// Every link, as its text and destination, with links still streaming left out.
    fn links(document: &Document) -> Vec<(String, String)> {
        fn walk(block: &Block, out: &mut Vec<(String, String)>) {
            let mut inline = |inline: &Inline| {
                for (range, url) in &inline.links {
                    if url != mend::PENDING_LINK_URL {
                        out.push((inline.text[range.clone()].to_owned(), url.to_string()));
                    }
                }
            };
            match block {
                Block::Paragraph(text) | Block::Heading { text, .. } => inline(text),
                Block::Table { header, rows, .. } => {
                    header.iter().chain(rows.iter().flatten()).for_each(inline)
                }
                Block::List { items, .. } => items.iter().flatten().for_each(|b| walk(b, out)),
                Block::Quote(blocks) => blocks.iter().for_each(|b| walk(b, out)),
                Block::Code { .. } | Block::Diagram { .. } | Block::Rule => {}
            }
        }
        let mut out = Vec::new();
        for top in &document.blocks {
            walk(&top.block, &mut out);
        }
        out
    }

    #[test]
    fn display_links_resolve_as_the_document_does() {
        let mut parser = IncrementalParser::new();
        parser.set_text("[docs]: https://example.com\n\nIntro.\n\nSee [docs] and **bold");
        let shown = last_inline(&parser.display()).clone();
        assert_eq!(&shown.text[shown.links[0].0.clone()], "docs");
        assert_eq!(shown.links[0].1, "https://example.com");
        assert!(styled(&shown).contains(&(
            "bold".to_owned(),
            Style {
                strong: true,
                ..Default::default()
            }
        )));
        for corpus in CORPORA {
            let mut parser = IncrementalParser::new();
            for (i, ch) in corpus.char_indices() {
                parser.append(ch.encode_utf8(&mut [0; 4]));
                let prefix = &corpus[..i + ch.len_utf8()];
                assert_eq!(
                    links(&parser.display()),
                    links(&parser.document()),
                    "{prefix:?}"
                );
            }
        }
    }

    #[test]
    fn a_finished_reply_displays_as_its_document() {
        for corpus in CORPORA {
            let parser = stream(corpus, 5);
            assert_eq!(parser.display(), parser.document(), "{corpus}");
        }
    }

    #[test]
    fn display_adds_no_closers_inside_nested_code() {
        let mut parser = IncrementalParser::new();
        for source in [
            "- item\n\n  ```sh\n  echo hello",
            "- item\n\n  ```sh\n  echo `hello",
            "> ```\n> let **x",
            "> - a\n>\n>   ~~~\n>   x *y",
            "- item\n\n      indented `code",
            "- **a\n\n  b",
        ] {
            parser.set_text(source);
            assert_eq!(parser.display(), parser.document(), "{source:?}");
        }
        parser.set_text("- a\n\n  ```\n  x\n  ```\n\n  then **bold");
        assert_ne!(parser.display(), parser.document());
    }

    #[test]
    fn display_leaves_code_blocks_and_finished_text_alone() {
        let mut parser = IncrementalParser::new();
        parser.set_text("Text\n\n```bash\necho **not bold");
        assert_eq!(parser.display(), parser.document());
        parser.set_text("para\n-");
        assert!(matches!(
            parser.display().blocks[0].block,
            Block::Paragraph(_)
        ));
    }
}
