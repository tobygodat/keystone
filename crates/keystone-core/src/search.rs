//! Plain-text search that ignores case: the matcher behind find in a session and the
//! sidebar's search across sessions. No patterns and no options. Case is ignored by
//! each character's lowercase form, so `É` finds `é`, but `ß` does not find `ss`.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use memchr::memmem::Finder;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::OnceLock;

/// What the user typed, folded once for matching. A query is never empty.
///
/// The searching runs in `aho-corasick` and `memchr`, which are built optimized even in
/// a debug build, where this crate's own loops are slow.
#[derive(Clone, Debug)]
pub struct Query {
    typed: String,
    folded: String,
    /// Finds the folded query in lowercased text.
    finder: Finder<'static>,
    /// For a query of ASCII, finds it in any text ignoring ASCII case, which is exact
    /// unless the text holds one of `screens`.
    ascii: Option<AhoCorasick>,
    /// For a query of ASCII: the other characters that lowercase to one of its letters,
    /// such as the Kelvin sign. Otherwise: every character that lowercases to the
    /// query's first non-ASCII character, one of which a matching text must hold.
    screens: Vec<Finder<'static>>,
}

impl PartialEq for Query {
    fn eq(&self, other: &Self) -> bool {
        self.typed == other.typed
    }
}

impl Query {
    /// `None` for an empty query, which finds nothing.
    pub fn new(typed: &str) -> Option<Self> {
        if typed.is_empty() {
            return None;
        }
        let folded = fold(typed);
        let finder = Finder::new(folded.as_bytes()).into_owned();
        let ascii = folded.is_ascii().then(|| {
            AhoCorasickBuilder::new()
                .ascii_case_insensitive(true)
                .match_kind(MatchKind::LeftmostFirst)
                .build([folded.as_str()])
                .expect("one short pattern builds")
        });
        let screened: Vec<char> = match &ascii {
            Some(_) => folded
                .chars()
                .flat_map(|c| others(c).iter().copied())
                .filter(|c| !c.is_ascii())
                .collect(),
            None => {
                let first = folded.chars().find(|c| !c.is_ascii()).expect("non-ASCII");
                std::iter::once(first)
                    .chain(others(first).iter().copied())
                    .collect()
            }
        };
        let mut screens: Vec<Finder<'static>> = Vec::new();
        for c in screened {
            let mut bytes = [0; 4];
            let bytes = c.encode_utf8(&mut bytes).as_bytes();
            if !screens.iter().any(|screen| screen.needle() == bytes) {
                screens.push(Finder::new(bytes).into_owned());
            }
        }
        Some(Self {
            typed: typed.to_owned(),
            folded,
            finder,
            ascii,
            screens,
        })
    }

    /// The query as the user typed it.
    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// Every match in `text`, as byte ranges of `text`, in order and never overlapping.
    /// A match starts and ends on a character of `text`, never inside one whose
    /// lowercase form is longer than one character.
    pub fn find(&self, text: &str) -> Vec<Range<usize>> {
        let mut found = Vec::new();
        self.each(text, |range| {
            found.push(range);
            true
        });
        found
    }

    /// Whether `text` holds a match.
    pub fn is_in(&self, text: &str) -> bool {
        let mut any = false;
        self.each(text, |_| {
            any = true;
            false
        });
        any
    }

    fn screened(&self, text: &str) -> bool {
        self.screens
            .iter()
            .any(|screen| screen.find(text.as_bytes()).is_some())
    }

    /// Calls `found` with each match in order until it returns `false`.
    fn each(&self, text: &str, mut found: impl FnMut(Range<usize>) -> bool) {
        match &self.ascii {
            // An ASCII byte is always a whole character, so these are whole matches.
            Some(ascii) if !self.screened(text) => {
                for hit in ascii.find_iter(text) {
                    if !found(hit.range()) {
                        return;
                    }
                }
                return;
            }
            None if !self.screened(text) => return,
            _ => {}
        }
        let folded = Folded::new(text);
        let mut from = 0;
        while let Some(at) = self.finder.find(&folded.text.as_bytes()[from..]) {
            let start = from + at;
            let end = start + self.folded.len();
            match (folded.origin(start), folded.origin(end)) {
                (Some(origin), Some(limit)) => {
                    from = end;
                    if !found(origin..limit) {
                        return;
                    }
                }
                // Starts or ends inside one character's lowercase form: try one
                // character on.
                _ => {
                    from = start
                        + folded.text[start..]
                            .chars()
                            .next()
                            .map_or(1, char::len_utf8)
                }
            }
        }
    }
}

/// The characters other than `c` whose lowercase form holds `c`, such as `K` and the
/// Kelvin sign for `k`. Built once, on the first query that asks.
fn others(c: char) -> &'static [char] {
    static OTHERS: OnceLock<HashMap<char, Vec<char>>> = OnceLock::new();
    OTHERS
        .get_or_init(|| {
            let mut others: HashMap<char, Vec<char>> = HashMap::new();
            for upper in (0..=char::MAX as u32).filter_map(char::from_u32) {
                let mut lower = upper.to_lowercase();
                if lower.len() == 1 && lower.clone().next() == Some(upper) {
                    continue;
                }
                for c in lower.by_ref() {
                    if c != upper {
                        others.entry(c).or_default().push(upper);
                    }
                }
            }
            others
        })
        .get(&c)
        .map_or(&[], Vec::as_slice)
}

/// Each character's lowercase form in turn. Unlike `str::to_lowercase`, a character
/// folds the same wherever it stands, so a query matches text the same way.
fn fold(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// `text` lowercased, with where each lowercased character came from.
struct Folded {
    text: String,
    /// For each byte of `text` and one past its end: the byte of the original text a
    /// character starting there came from, or `NONE` inside a character or inside a
    /// longer lowercase form.
    origins: Vec<u32>,
}

const NONE: u32 = u32::MAX;

impl Folded {
    fn new(original: &str) -> Self {
        let mut text = String::with_capacity(original.len());
        let mut origins = Vec::with_capacity(original.len() + 1);
        for (at, ch) in original.char_indices() {
            for (n, lower) in ch.to_lowercase().enumerate() {
                origins.push(if n == 0 { at as u32 } else { NONE });
                origins.extend(std::iter::repeat_n(NONE, lower.len_utf8() - 1));
                text.push(lower);
            }
        }
        origins.push(original.len() as u32);
        Self { text, origins }
    }

    fn origin(&self, at: usize) -> Option<usize> {
        self.origins
            .get(at)
            .filter(|origin| **origin != NONE)
            .map(|origin| *origin as usize)
    }
}

/// One line of text around a match, for a result row: `text` is the line, cut to start
/// near the match, and `marked` is the match within it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub marked: Range<usize>,
}

/// Characters a snippet keeps before its match. A row shows what fits after them, so
/// the match is in view at the sidebar's narrowest width.
pub const SNIPPET_LEAD: usize = 24;
/// Characters a snippet keeps after its match. The row cuts the rest to its width.
const SNIPPET_TAIL: usize = 160;

/// The line of `text` that holds `range`, with runs of whitespace shown as one space.
/// More than [`SNIPPET_LEAD`] characters before the match are cut at a word and marked
/// with an ellipsis. `range` must be a match of `text`.
pub fn snippet(text: &str, range: Range<usize>) -> Snippet {
    let line_start = text[..range.start].rfind('\n').map_or(0, |at| at + 1);
    let line_end = text[range.end..]
        .find('\n')
        .map_or(text.len(), |at| range.end + at);
    let before: Vec<char> = text[line_start..range.start].chars().collect();
    let mut lead = before.len().saturating_sub(SNIPPET_LEAD);
    let cut = lead > 0;
    if cut {
        // Start at the next word, unless that leaves nothing before the match.
        if let Some(space) = before[lead..].iter().position(|c| c.is_whitespace()) {
            lead += space + 1;
        }
    }
    let mut out = String::new();
    if cut {
        out.push('…');
    }
    push_spaced(&mut out, before[lead..].iter().copied());
    let start = out.len();
    push_spaced(&mut out, text[range.clone()].chars());
    let end = out.len();
    let after = &text[range.end..line_end];
    let tail: String = after.chars().take(SNIPPET_TAIL).collect();
    push_spaced(&mut out, tail.chars());
    if tail.len() < after.len() {
        out.push('…');
    }
    Snippet {
        text: out,
        marked: start..end,
    }
}

fn push_spaced(out: &mut String, chars: impl Iterator<Item = char>) {
    for ch in chars {
        if ch.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
