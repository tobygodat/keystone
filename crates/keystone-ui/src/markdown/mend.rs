//! Closes half-streamed inline Markdown for display.
//!
//! Adapted from Zeron's `crates/markdown/src/mend.rs` (https://github.com/zeronsh/zeron),
//! Copyright (c) 2026 Wing, under the MIT License. See `THIRD_PARTY_NOTICES.md`.
//!
//! While a reply streams, an unclosed `**bold`, `*em`, `` `code ``, `~~strike`, or
//! `[link](partial-url` reads as literal text until its closer arrives. Then the markers
//! vanish, the run restyles, and the line rewraps. Adding closers to the displayed text
//! styles the run from its first character. Only the displayed document sees them, so a
//! marker that never closes shows as typed once the reply completes.
//!
//! The scan is approximate on purpose. It prefers a stable result close to the final
//! parse over exact CommonMark delimiter rules, because the next delta or the completed
//! reply corrects any guess.

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

/// The destination of a link whose URL is still streaming. It is not openable.
pub const PENDING_LINK_URL: &str = "keystone:pending-link";

/// `block`, one top-level block, with closers added for the hanging markers of its last
/// run of text, or `None` when nothing hangs. Code, HTML, and tables are verbatim, so a
/// block that ends in one gets none.
pub fn close_block(block: &str) -> Option<String> {
    let start = last_text(block)?;
    // From the line start, so a list bullet still reads as a bullet.
    let line = block[..start].rfind('\n').map_or(0, |i| i + 1);
    let closed = close_hanging(&block[line..])?;
    Some(format!("{}{closed}", &block[..line]))
}

/// Where the last leaf of inline text in `block` starts, unless verbatim text follows it.
/// Text before it is in finished leaves, whose open markers stay literal. A leaf is a
/// paragraph, a heading, or a tight list item's text, and only block events end one.
fn last_text(block: &str) -> Option<usize> {
    let mut start = None;
    let mut verbatim = 0usize;
    let mut fresh = true;
    for (event, range) in Parser::new_ext(block, super::OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_) | Tag::HtmlBlock | Tag::Table(_)) => {
                verbatim += 1;
                start = None;
                fresh = true;
            }
            Event::End(TagEnd::CodeBlock | TagEnd::HtmlBlock | TagEnd::Table) => {
                verbatim -= 1;
                fresh = true;
            }
            Event::Start(ref tag) if !is_inline(tag.to_end()) => fresh = true,
            Event::End(tag) if !is_inline(tag) => fresh = true,
            Event::Rule => fresh = true,
            _ if verbatim == 0 && fresh => {
                start = Some(range.start);
                fresh = false;
            }
            _ => {}
        }
    }
    start
}

fn is_inline(tag: TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image
            | TagEnd::Superscript
            | TagEnd::Subscript
    )
}

/// One unclosed `*`, `_`, or `~~` run.
struct OpenDelim {
    ch: char,
    len: usize,
    /// Char index just past the run, which orders closers and finds the content after it.
    pos: usize,
}

/// `text` with closers added for its hanging inline markers, or `None` when nothing hangs.
pub fn close_hanging(text: &str) -> Option<String> {
    let cs: Vec<(usize, char)> = text.char_indices().collect();
    let n = cs.len();
    let at = |i: usize| cs.get(i).map(|&(_, c)| c);

    let mut delims: Vec<OpenDelim> = Vec::new();
    // Char indices of unmatched `[`.
    let mut brackets: Vec<usize> = Vec::new();
    // An open code span: its backtick run length and its content's char index.
    let mut code: Option<(usize, usize)> = None;
    // The last char that justifies closing an opener before it: not space, not a marker.
    let mut last_content: Option<usize> = None;
    // The `]` of a `](` whose URL runs off the end.
    let mut pending_url: Option<usize> = None;

    let mut i = 0;
    while i < n {
        let c = cs[i].1;
        if code.is_none() && c == '\\' {
            if i + 1 < n {
                last_content = Some(i + 1);
            }
            i += 2;
            continue;
        }
        if c == '`' {
            let run = run_len(&cs, i);
            match code {
                // A span closes only on a run of its opening length.
                Some((open, _)) if run == open => code = None,
                Some(_) => last_content = Some(i + run - 1),
                None => code = Some((run, i + run)),
            }
            i += run;
            continue;
        }
        if code.is_some() {
            last_content = Some(i);
            i += 1;
            continue;
        }
        match c {
            '*' | '_' | '~' => {
                let run = run_len(&cs, i);
                delim(&mut delims, &cs, c, run, i, &mut last_content);
                i += run;
            }
            '[' => {
                brackets.push(i);
                i += 1;
            }
            ']' => {
                if let Some(open) = brackets.pop() {
                    // Emphasis opened inside a closed `[...]` stays literal, as the full
                    // parse decides too.
                    delims.retain(|d| d.pos < open);
                    if at(i + 1) == Some('(') {
                        let mut j = i + 2;
                        let mut depth = 0usize;
                        loop {
                            match at(j) {
                                Some('(') => depth += 1,
                                Some(')') if depth == 0 => break,
                                Some(')') => depth -= 1,
                                Some(_) => {}
                                None => {
                                    pending_url = Some(i);
                                    break;
                                }
                            }
                            j += 1;
                        }
                        if pending_url.is_some() {
                            break;
                        }
                        last_content = Some(j);
                        i = j + 1;
                        continue;
                    }
                }
                last_content = Some(i);
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                last_content = Some(i);
                i += 1;
            }
        }
    }

    // The text ends inside a link URL: drop the partial URL and keep the link text.
    if let Some(close) = pending_url {
        let byte = cs[close].0;
        return Some(format!("{}]({PENDING_LINK_URL})", &text[..byte]));
    }

    // Closers go innermost first, so ones opened inside a link's text close before it.
    let mut pending: Vec<(usize, String)> = Vec::new();
    if let Some((ticks, content)) = code
        && last_content.is_some_and(|last| last >= content)
    {
        pending.push((content, "`".repeat(ticks)));
    }
    for d in &delims {
        if last_content.is_some_and(|last| last >= d.pos) {
            pending.push((d.pos, d.ch.to_string().repeat(d.len)));
        }
    }
    if let Some(&open) = brackets.last()
        && last_content.is_some_and(|last| last > open)
    {
        pending.push((open, format!("]({PENDING_LINK_URL})")));
    }
    pending.sort_by_key(|(pos, _)| std::cmp::Reverse(*pos));
    let closers: String = pending.into_iter().map(|(_, closer)| closer).collect();

    // A last line of one or two `-` or `=` under text would make the paragraph a setext
    // heading, but is usually a list item still streaming. A zero-width space keeps it a
    // paragraph until the next characters decide.
    let setext = setext_partial(text);

    if closers.is_empty() && !setext {
        return None;
    }
    if setext {
        let nl = text.rfind('\n');
        return Some(match (nl, closers.is_empty()) {
            (Some(nl), false) => format!("{}{closers}{}\u{200B}", &text[..nl], &text[nl..]),
            _ => format!("{text}\u{200B}"),
        });
    }
    // A closer after trailing space is not right-flanking and would not close.
    let end = text.trim_end().len();
    Some(format!("{}{closers}{}", &text[..end], &text[end..]))
}

fn run_len(cs: &[(usize, char)], i: usize) -> usize {
    let c = cs[i].1;
    cs[i..].iter().take_while(|&&(_, x)| x == c).count()
}

/// Matches or opens one delimiter run. It closes the innermost opener of the same char,
/// partly for a half-streamed closer such as the second `*` of `**a*`.
fn delim(
    delims: &mut Vec<OpenDelim>,
    cs: &[(usize, char)],
    c: char,
    run: usize,
    i: usize,
    last_content: &mut Option<usize>,
) {
    let end = i + run;
    // Strikethrough is `~~` only. A single `~` may still be half of a closing `~~`.
    if c == '~' && run > 2 {
        *last_content = Some(end - 1);
        return;
    }
    let prev = i.checked_sub(1).map(|p| cs[p].1);
    let next = cs.get(end).map(|&(_, c)| c);
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    // An intraword `_` never delimits, and neither does a single `*`, so `2*3` stays plain.
    if word(prev) && word(next) && (c == '_' || (c == '*' && run == 1)) {
        *last_content = Some(end - 1);
        return;
    }
    let can_close = prev.is_some_and(|c| !c.is_whitespace());
    let can_open = next.is_some_and(|c| !c.is_whitespace());
    let mut rest = run;
    if can_close && let Some(k) = delims.iter().rposition(|d| d.ch == c) {
        let take = rest.min(delims[k].len);
        delims[k].len -= take;
        rest -= take;
        let keep = if delims[k].len == 0 { k } else { k + 1 };
        delims.truncate(keep);
    }
    if rest > 0 {
        if can_open && (c != '~' || rest == 2) {
            delims.push(OpenDelim {
                ch: c,
                len: rest,
                pos: end,
            });
        } else {
            *last_content = Some(end - 1);
        }
    }
}

/// Whether the last line is one or two `-` or `=` under a non-empty line.
fn setext_partial(text: &str) -> bool {
    let Some(nl) = text.rfind('\n') else {
        return false;
    };
    let trimmed = text[nl + 1..].trim_start();
    let underline =
        |c: char| !trimmed.is_empty() && trimmed.len() <= 2 && trimmed.chars().all(|x| x == c);
    (underline('-') || underline('='))
        && text[..nl]
            .lines()
            .last()
            .is_some_and(|line| !line.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn mends(input: &str, expected: &str) {
        assert_eq!(close_hanging(input).as_deref(), Some(expected), "{input:?}");
    }

    #[track_caller]
    fn stays(input: &str) {
        assert_eq!(close_hanging(input), None, "{input:?}");
    }

    #[test]
    fn balanced_text_needs_nothing() {
        stays("plain words, no markers");
        stays("a **b** and *c* and `d` and ~~e~~");
        stays("[docs](https://x.dev) done");
        stays("");
    }

    #[test]
    fn emphasis_closes() {
        mends("**bold", "**bold**");
        mends("some *em", "some *em*");
        mends("a __b", "a __b__");
        mends("a _b", "a _b_");
        mends("***both", "***both***");
        mends("~~gone", "~~gone~~");
    }

    #[test]
    fn half_streamed_closers_complete() {
        mends("**bold*", "**bold**");
        mends("__b_", "__b__");
        mends("~~gone~", "~~gone~~");
    }

    #[test]
    fn nested_closers_come_innermost_first() {
        mends("**a *b", "**a *b***");
        mends("*a **b", "*a **b***");
        mends("_a **b", "_a **b**_");
    }

    #[test]
    fn bare_openers_wait_for_content() {
        stays("**");
        stays("text **");
        stays("text ** ");
        stays("*");
        stays("~~");
        stays("`");
    }

    #[test]
    fn closers_go_before_trailing_space() {
        mends("**bold ", "**bold** ");
        mends("*em\n", "*em*\n");
    }

    #[test]
    fn intraword_markers_escapes_and_list_bullets_are_literal() {
        stays("2*3 equals 6");
        stays("snake_case_name");
        stays("20~25 degrees");
        stays("~single~x");
        stays(r"\*not emphasis");
        stays(r"a \** b");
        stays("* item one");
        stays("- a\n* b");
    }

    #[test]
    fn code_spans_close_and_hide_markers() {
        mends("`code", "`code`");
        mends("call `a ** b", "call `a ** b`");
        mends("``a`", "``a```");
        stays("`done` after");
    }

    #[test]
    fn links_show_their_text_before_the_url_arrives() {
        mends("[docs](https://x.dev/lo", "[docs](keystone:pending-link)");
        mends("[docs](", "[docs](keystone:pending-link)");
        mends("see [do", "see [do](keystone:pending-link)");
        mends("[a](https://x.dev/(y", "[a](keystone:pending-link)");
        mends("[**a", "[**a**](keystone:pending-link)");
        mends("**a [b", "**a [b](keystone:pending-link)**");
        stays("see [");
        stays("[x] task-like");
        stays("[a](https://x.dev/(y)) done");
        stays("[**a] done");
    }

    #[test]
    fn a_streaming_list_item_is_not_a_setext_underline() {
        mends("para\n-", "para\n-\u{200B}");
        mends("para\n==", "para\n==\u{200B}");
        mends("**b\n-", "**b**\n-\u{200B}");
        stays("para\n---");
        stays("-");
        stays("\n-");
    }
}
