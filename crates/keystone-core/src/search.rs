//! Plain-text search that ignores case: the matcher behind find in a session and the
//! sidebar's search across sessions. No patterns and no options.
//!
//! Text is compared in a searched form: Unicode's full case folding, so `ΟΔΟΣ` finds
//! `οδος` and `ß` finds `ss`, with both sides in NFC, so a decomposed `é` finds a
//! precomposed one. A dot above an `i` or `j`, which they already have, is dropped, so
//! `istanbul` finds `İstanbul`. Matches are ranges of the original text, widened to
//! whole grapheme clusters, so a mark never splits what draws as one character.
//!
//! A cluster's searched form is defined by [`form`]. Building a text's form cluster by
//! cluster that way costs three normalizations and several allocations each, so
//! [`Folded`] takes it from a per-character table wherever the text allows: runs of
//! ASCII in bulk, and characters that always end their cluster one at a time.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use icu_normalizer::properties::CanonicalCombiningClassMapBorrowed;
use icu_normalizer::{ComposingNormalizerBorrowed, DecomposingNormalizerBorrowed};
use memchr::memmem::Finder;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::OnceLock;
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

const NFC: ComposingNormalizerBorrowed<'static> = ComposingNormalizerBorrowed::new_nfc();
const NFD: DecomposingNormalizerBorrowed<'static> = DecomposingNormalizerBorrowed::new_nfd();
const CLASSES: CanonicalCombiningClassMapBorrowed<'static> =
    CanonicalCombiningClassMapBorrowed::new();

/// What the user typed, in its searched form. A query is never empty.
///
/// The searching runs in `aho-corasick` and `memchr`, which are built optimized even in
/// a debug build, where this crate's own loops are slow.
#[derive(Clone, Debug)]
pub struct Query {
    typed: String,
    /// The searched form of `typed`.
    folded: String,
    /// Finds `folded` in a text's searched form.
    finder: Finder<'static>,
    plan: Plan,
}

/// A character whose searched form holds one of a query of ASCII's characters.
#[derive(Clone, Debug)]
struct Screen {
    /// Finds it. In text full of other non-ASCII letters, `memchr`'s search for each
    /// screen is faster than one automaton for all of them.
    finder: Finder<'static>,
    /// Its searched form, and whether it can share a grapheme cluster with a neighbour.
    shown: Box<[u8]>,
    joins: bool,
}

/// How a text is searched before, or instead of, building its searched form.
#[derive(Clone, Debug)]
enum Plan {
    /// The query's searched form is ASCII: an automaton that ignores ASCII case finds it
    /// in the text itself. That is exact when no hit starts or ends inside a grapheme
    /// cluster and the text holds none of `screens`, the characters whose searched form
    /// holds one of the query's characters, such as `ß` for `s`. A text that holds some
    /// is searched with their forms spliced in ([`spliced`]).
    Ascii {
        automaton: AhoCorasick,
        screens: Vec<Screen>,
    },
    /// A text that holds none of these characters holds no match: every character whose
    /// decomposed searched form holds the query's rarest non-ASCII character.
    Screened(AhoCorasick),
    /// Every text is searched in its searched form.
    Slow,
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
        let folded = String::from_utf8(Folded::new(typed).text).expect("forms are UTF-8");
        let finder = Finder::new(folded.as_bytes()).into_owned();
        let tables = tables();
        let plan = if folded.is_ascii() && !folded.contains(['\r', '\n']) {
            let mut screens: Vec<char> = Vec::new();
            for byte in folded.bytes() {
                for c in &tables.in_form[byte as usize] {
                    if !screens.contains(c) {
                        screens.push(*c);
                    }
                }
            }
            Plan::Ascii {
                automaton: AhoCorasickBuilder::new()
                    .ascii_case_insensitive(true)
                    .match_kind(MatchKind::LeftmostFirst)
                    .build([folded.as_str()])
                    .expect("one short pattern builds"),
                screens: screens
                    .iter()
                    .map(|&c| {
                        let mut shown = String::new();
                        form(&c.to_string(), &mut shown);
                        Screen {
                            finder: Finder::new(c.to_string().as_bytes()).into_owned(),
                            shown: shown.into_bytes().into_boxed_slice(),
                            joins: !tables.ends(c),
                        }
                    })
                    .collect(),
            }
        } else {
            // The rarest of the query's non-ASCII characters screens texts best.
            let decomposed = NFD.normalize(&folded);
            match decomposed
                .chars()
                .filter(|c| !c.is_ascii())
                .min_by_key(|c| tables.in_canon.get(c).map_or(0, Vec::len))
            {
                Some(rare) => {
                    let mut screens = vec![rare.to_string()];
                    screens.extend(
                        tables
                            .in_canon
                            .get(&rare)
                            .into_iter()
                            .flatten()
                            .map(char::to_string),
                    );
                    Plan::Screened(AhoCorasick::new(screens).expect("characters build"))
                }
                None => Plan::Slow,
            }
        };
        Some(Self {
            typed: typed.to_owned(),
            folded,
            finder,
            plan,
        })
    }

    /// The query as the user typed it.
    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// Every match in `text`, as byte ranges of `text`, in order and never overlapping.
    /// Each range starts and ends on a grapheme cluster boundary of `text`.
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

    /// Calls `found` with each match in order until it returns `false`.
    fn each(&self, text: &str, mut found: impl FnMut(Range<usize>) -> bool) {
        match &self.plan {
            Plan::Ascii { automaton, screens } => {
                // A text of ASCII has no cluster longer than CR and LF, which the query
                // never holds, and no screen.
                if text.is_ascii() {
                    for hit in automaton.find_iter(text) {
                        if !found(hit.range()) {
                            return;
                        }
                    }
                    return;
                }
                let screened = screened(text.as_bytes(), screens);
                let hits: Option<Vec<Range<usize>>> = match screened.is_empty() {
                    true => automaton
                        .find_iter(text)
                        .map(|hit| whole(text, hit.range()).then_some(hit.range()))
                        .collect(),
                    false => spliced(text, &screened, automaton),
                };
                let Some(hits) = hits else {
                    return self.each_folded(text, found);
                };
                for hit in hits {
                    if !found(hit) {
                        return;
                    }
                }
            }
            Plan::Screened(screens) if !screens.is_match(text) => {}
            _ => self.each_folded(text, found),
        }
    }

    /// Searches `text`'s searched form: the way every match is defined, which the plans
    /// only shortcut.
    fn each_folded(&self, text: &str, mut found: impl FnMut(Range<usize>) -> bool) {
        let folded = Folded::new(text);
        let mut from = 0;
        while let Some(at) = self.finder.find(&folded.text[from..]) {
            let start = folded.cluster_start(from + at);
            let end = folded.cluster_end(from + at + self.folded.len());
            from = end;
            if !found(folded.origins[start] as usize..folded.origins[end] as usize) {
                return;
            }
        }
    }

    #[cfg(test)]
    fn find_folded(&self, text: &str) -> Vec<Range<usize>> {
        let mut found = Vec::new();
        self.each_folded(text, |range| {
            found.push(range);
            true
        });
        found
    }
}

/// Where in `bytes` each of `screens` starts, in order.
fn screened<'a>(bytes: &[u8], screens: &'a [Screen]) -> Vec<(usize, &'a Screen)> {
    let mut found: Vec<(usize, &Screen)> = screens
        .iter()
        .flat_map(|screen| screen.finder.find_iter(bytes).map(move |at| (at, screen)))
        .collect();
    found.sort_unstable_by_key(|(at, _)| *at);
    found
}

/// The matches of a query of ASCII in `text`, whose `screens`, in order of the byte
/// each starts at, are the characters whose forms hold the query's characters. The text
/// is searched with each screen's form spliced in: every other character's form holds
/// none of the query's characters, so neither it nor its own bytes, kept here, can take
/// part in a match. `None`, for the folded search to decide, when a screen shares its
/// grapheme cluster, or a match starts or ends inside a cluster or ends inside a
/// screen's form.
fn spliced(
    text: &str,
    screens: &[(usize, &Screen)],
    automaton: &AhoCorasick,
) -> Option<Vec<Range<usize>>> {
    let bytes = text.as_bytes();
    // Each splice: where its form starts and ends in `joined`, and its character in
    // `text`.
    let mut splices: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(screens.len());
    let mut joined: Vec<u8> = Vec::with_capacity(text.len() + screens.len() * 2);
    let mut copied = 0;
    for &(at, screen) in screens {
        let end = at + screen.finder.needle().len();
        // A screen that ends its cluster, beside ASCII, is a cluster of its own.
        let alone = |at: usize, before: u8| {
            at == 0
                || at == bytes.len()
                || (!screen.joins && before.is_ascii())
                || boundary(text, at)
        };
        if !alone(at, bytes.get(at.wrapping_sub(1)).copied().unwrap_or(0))
            || !alone(end, bytes.get(end).copied().unwrap_or(0))
        {
            return None;
        }
        joined.extend_from_slice(&bytes[copied..at]);
        let start = joined.len();
        joined.extend_from_slice(&screen.shown);
        splices.push((start, joined.len(), at, end));
        copied = end;
    }
    joined.extend_from_slice(&bytes[copied..]);
    // Where byte `at` of `joined` falls in `text`, and whether it is inside a form:
    // the splices before it are counted, from the one at `next`, as `at` moves on.
    let mut next = 0;
    let mut shift = 0isize;
    let mut to_text = |at: usize, up: bool| -> (usize, bool) {
        while let Some(&(start, end, from, to)) = splices.get(next) {
            if at < end {
                return match (at <= start, up) {
                    (true, _) => ((at as isize - shift) as usize, false),
                    (false, true) => (to, true),
                    (false, false) => (from, true),
                };
            }
            shift = end as isize - to as isize;
            next += 1;
        }
        ((at as isize - shift) as usize, false)
    };
    let mut hits = Vec::new();
    for hit in automaton.find_iter(&joined) {
        let (start, _) = to_text(hit.start(), false);
        let (end, inside) = to_text(hit.end(), true);
        // A match ending inside a form widens to its end, past where the next search
        // starts; the folded search handles that.
        if inside || !whole(text, start..end) {
            return None;
        }
        hits.push(start..end);
    }
    Some(hits)
}

/// Whether a hit of ASCII bytes starts and ends on grapheme cluster boundaries. A
/// boundary between two ASCII bytes always is one, unless they are CR and LF, and a
/// query of ASCII never holds either.
fn whole(text: &str, range: Range<usize>) -> bool {
    let bytes = text.as_bytes();
    [range.start, range.end].into_iter().all(|at| {
        at == 0
            || at == bytes.len()
            || (bytes[at - 1].is_ascii() && bytes[at].is_ascii())
            || boundary(text, at)
    })
}

/// Whether byte `at`, between two characters of `text`, is a grapheme cluster boundary.
fn boundary(text: &str, at: usize) -> bool {
    let (Some(before), Some(after)) = (text[..at].chars().next_back(), text[at..].chars().next())
    else {
        return true;
    };
    let tables = tables();
    (tables.ends(before) && tables.ends(after))
        || GraphemeCursor::new(at, text.len(), true).is_boundary(text, 0) == Ok(true)
}

/// Appends the searched form of `text`, one grapheme cluster or a whole query, to `out`:
/// decomposed, case folded, without a dot above that only repeats an `i`'s or a `j`'s
/// own, and composed. This defines the searched form; [`Folded`] reaches the same
/// bytes faster where it can.
fn form(text: &str, out: &mut String) {
    if text.is_ascii() {
        out.extend(text.bytes().map(|b| b.to_ascii_lowercase() as char));
        return;
    }
    let decomposed = NFD.normalize(text);
    let folded = unicase::UniCase::unicode(decomposed.as_ref()).to_folded_case();
    let mut kept = String::with_capacity(folded.len());
    // Whether the last base character is `i` or `j` with no mark above it since.
    let mut dotted = false;
    for c in NFD.normalize(&folded).chars() {
        match CLASSES.get_u8(c) {
            0 => dotted = c == 'i' || c == 'j',
            _ if c == '\u{307}' && dotted => continue,
            // Above.
            230 => dotted = false,
            _ => {}
        }
        kept.push(c);
    }
    out.push_str(&NFC.normalize(&kept));
}

/// A character joins a neighbour's grapheme cluster in some text: it is not one whose
/// clusters always end on both sides when its neighbour is one too.
const JOINS: u32 = 1 << 31;
/// A character whose searched form is not itself, or which changes when decomposed and
/// folded, or a dot above: a cluster holding one is built by [`form`].
const CHANGES: u32 = 1 << 30;
/// The rest of a character's entry: 0 when its form is itself, else 1 + its index in
/// [`Tables::forms`].
const SHOWN: u32 = (1 << 30) - 1;

/// For the plans and [`Folded`]: which characters' searched forms hold which
/// characters, and each character's entry.
struct Tables {
    /// For each ASCII byte, the non-ASCII characters whose searched form holds it.
    in_form: Vec<Vec<char>>,
    /// For each non-ASCII character, the other characters whose searched form holds it
    /// once decomposed.
    in_canon: HashMap<char, Vec<char>>,
    /// Each character's entry, by its top bits, then its low byte within the block
    /// that names. Most blocks are all zero and share block 0.
    blocks: Vec<u16>,
    entries: Vec<u32>,
    forms: Vec<Box<str>>,
}

impl Tables {
    #[inline]
    fn entry(&self, c: char) -> u32 {
        let c = c as u32;
        self.entries[self.blocks[(c >> 8) as usize] as usize * 256 + (c & 0xFF) as usize]
    }

    /// Whether a cluster boundary falls on both sides of `c` whenever its neighbour is
    /// such a character too.
    #[inline]
    fn ends(&self, c: char) -> bool {
        self.entry(c) & JOINS == 0
    }
}

/// Built once, on the first query.
fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut in_form = vec![Vec::new(); 128];
        let mut in_canon: HashMap<char, Vec<char>> = HashMap::new();
        let mut entries: HashMap<u32, u32> = HashMap::new();
        let mut forms: Vec<Box<str>> = Vec::new();
        // Han ideographs, private use and the unassigned planes have no case and no
        // decomposition, and always end their clusters. Hangul syllables, which
        // decompose, compatibility ideographs, and the tags and variation selectors,
        // which join, stay in.
        let skipped = |c: u32| {
            matches!(
                c,
                0x3400..=0x4DBF
                    | 0x4E00..=0x9FFF
                    | 0xE000..=0xF8FF
                    | 0x20000..=0x2F7FF
                    | 0x30000..=0xDFFFF
                    | 0xF0000..=0x10FFFF
            )
        };
        let mut shown = String::new();
        let mut buf = [0; 4];
        for c in (0..=char::MAX as u32)
            .filter(|c| !skipped(*c))
            .filter_map(char::from_u32)
        {
            let own: &str = c.encode_utf8(&mut buf);
            shown.clear();
            form(own, &mut shown);
            let mut entry = 0;
            // Whether a boundary falls between `a` and `c`, `c` and `a`, and two of `c`.
            let mut probe = [0u8; 10];
            probe[0] = b'a';
            probe[1..=own.len()].copy_from_slice(own.as_bytes());
            probe[own.len() + 1..=2 * own.len()].copy_from_slice(own.as_bytes());
            probe[2 * own.len() + 1] = b'a';
            let probe = std::str::from_utf8(&probe[..2 * own.len() + 2]).expect("UTF-8");
            let ends = [1, own.len() + 1, 2 * own.len() + 1].into_iter().all(|at| {
                GraphemeCursor::new(at, probe.len(), true).is_boundary(probe, 0) == Ok(true)
            });
            if !ends || c == '\r' || c == '\n' {
                entry |= JOINS;
            }
            let decomposed = NFD.normalize(own);
            if shown != own
                || unicase::UniCase::unicode(decomposed.as_ref()).to_folded_case() != decomposed
                || c == '\u{307}'
            {
                entry |= CHANGES;
            }
            if shown != own {
                forms.push(shown.as_str().into());
                entry |= forms.len() as u32;
            }
            if entry != 0 {
                entries.insert(c as u32, entry);
            }
            if c.is_ascii() {
                continue;
            }
            for byte in shown.bytes().filter(u8::is_ascii) {
                let found = &mut in_form[byte as usize];
                if !found.contains(&c) {
                    found.push(c);
                }
            }
            for inner in NFD.normalize(&shown).chars() {
                if !inner.is_ascii() && inner != c {
                    let found = in_canon.entry(inner).or_default();
                    if !found.contains(&c) {
                        found.push(c);
                    }
                }
            }
        }
        // A dot above after an `i` or a `j` drops out of their cluster's form, which is
        // then ASCII.
        for &letter in b"ij" {
            in_form[letter as usize].push('\u{307}');
        }
        let mut blocks = vec![0u16; 0x110000 >> 8];
        let mut packed = vec![0u32; 256];
        for (block, slot) in blocks.iter_mut().enumerate() {
            let base = (block as u32) << 8;
            if (0..256).all(|low| !entries.contains_key(&(base + low))) {
                continue;
            }
            *slot = (packed.len() / 256) as u16;
            packed.extend((0..256).map(|low| entries.get(&(base + low)).copied().unwrap_or(0)));
        }
        Tables {
            in_form,
            in_canon,
            blocks,
            entries: packed,
            forms,
        }
    })
}

/// A text's searched form, with where each grapheme cluster's form starts.
struct Folded {
    text: Vec<u8>,
    /// For each byte of `text` and one past its end: the byte of the original text the
    /// grapheme cluster whose form starts there starts at, or `NONE` inside a form.
    origins: Vec<u32>,
}

const NONE: u32 = u32::MAX;

impl Folded {
    /// The searched form of `original`, cluster by cluster as [`form`] gives it.
    fn new(original: &str) -> Self {
        let tables = tables();
        let bytes = original.as_bytes();
        let mut folded = Self {
            text: Vec::with_capacity(bytes.len() + bytes.len() / 8),
            origins: Vec::with_capacity(bytes.len() + bytes.len() / 8 + 1),
        };
        // `at` is a cluster boundary; everything before it is folded.
        let mut at = 0;
        while at < bytes.len() {
            let run = bytes[at..]
                .iter()
                .position(|b| !b.is_ascii())
                .map_or(bytes.len(), |n| at + n);
            if run == bytes.len() {
                folded.push_ascii(bytes, at, run);
                break;
            }
            // A non-ASCII character that ends its cluster, between characters that do
            // too, is a cluster of its own, and so is each ASCII character before it.
            let c = original[run..]
                .chars()
                .next()
                .expect("a character starts here");
            let next = run + c.len_utf8();
            let before = run == at || !matches!(bytes[run - 1], b'\r' | b'\n');
            let after = match bytes.get(next) {
                None => true,
                Some(&b) if b.is_ascii() => b != b'\r' && b != b'\n',
                Some(_) => tables.ends(original[next..].chars().next().expect("a character")),
            };
            let entry = tables.entry(c);
            if before && after && entry & JOINS == 0 {
                folded.push_ascii(bytes, at, run);
                folded.push_entry(run, c, entry, tables);
                at = next;
                continue;
            }
            // Otherwise the segmenter decides, from the last boundary that is certain:
            // before the ASCII character that may join `c`, or before CR and LF.
            let mut start = run;
            if run > at {
                start -= 1;
                if start > at && bytes[start - 1] == b'\r' && bytes[start] == b'\n' {
                    start -= 1;
                }
            }
            folded.push_ascii(bytes, at, start);
            // To the first boundary between two characters that end their clusters.
            let mut previous = original[start..].chars().next().expect("a character");
            let from = start + previous.len_utf8();
            let mut end = bytes.len();
            for (offset, c) in original[from..].char_indices() {
                if tables.ends(previous) && tables.ends(c) {
                    end = from + offset;
                    break;
                }
                previous = c;
            }
            for (offset, cluster) in original[start..end].grapheme_indices(true) {
                folded.push_cluster(start + offset, cluster, tables);
            }
            at = end;
        }
        folded.origins.push(bytes.len() as u32);
        folded
    }

    /// Appends the ASCII bytes `start..end` of the original, each its own cluster but a
    /// CR and LF together.
    fn push_ascii(&mut self, bytes: &[u8], start: usize, end: usize) {
        if start == end {
            return;
        }
        let first = self.origins.len();
        let run = &bytes[start..end];
        if run.len() < 16 {
            for (n, &b) in run.iter().enumerate() {
                self.text.push(b.to_ascii_lowercase());
                self.origins.push((start + n) as u32);
            }
        } else {
            self.text.extend(run.iter().map(u8::to_ascii_lowercase));
            self.origins.extend(start as u32..end as u32);
        }
        if run.contains(&b'\r') {
            for (n, pair) in run.windows(2).enumerate() {
                if pair == b"\r\n" {
                    self.origins[first + n + 1] = NONE;
                }
            }
        }
    }

    /// Appends one character's form, as a cluster of its own at byte `at`.
    #[inline]
    fn push_char(&mut self, at: usize, c: char, tables: &Tables) {
        if c.is_ascii() {
            self.text.push((c as u8).to_ascii_lowercase());
            self.origins.push(at as u32);
            return;
        }
        self.push_entry(at, c, tables.entry(c), tables);
    }

    /// Appends the form of `c`, whose table entry is `entry`, as a cluster of its own at
    /// byte `at`.
    #[inline]
    fn push_entry(&mut self, at: usize, c: char, entry: u32, tables: &Tables) {
        let index = entry & SHOWN;
        let mut buf = [0; 4];
        let shown: &[u8] = match index {
            0 => c.encode_utf8(&mut buf).as_bytes(),
            index => tables.forms[index as usize - 1].as_bytes(),
        };
        self.text.extend_from_slice(shown);
        self.origins.push(at as u32);
        for _ in 1..shown.len() {
            self.origins.push(NONE);
        }
    }

    /// Appends the form of `cluster`, which starts at byte `at`.
    fn push_cluster(&mut self, at: usize, cluster: &str, tables: &Tables) {
        let mut chars = cluster.chars();
        let first = chars.next().expect("clusters are not empty");
        if chars.next().is_none() {
            return self.push_char(at, first, tables);
        }
        let before = self.text.len();
        // Characters that keep their form, decomposed and folded, already in NFC: the
        // form is the cluster itself.
        if cluster.chars().all(|c| tables.entry(c) & CHANGES == 0) && NFC.is_normalized(cluster) {
            self.text.extend_from_slice(cluster.as_bytes());
        } else {
            let mut shown = String::new();
            form(cluster, &mut shown);
            self.text.extend_from_slice(shown.as_bytes());
        }
        self.start(at, before);
    }

    /// Records that the form from byte `before` of `text` on is a cluster's from byte
    /// `at` of the original.
    fn start(&mut self, at: usize, before: usize) {
        if self.text.len() > before {
            self.origins.push(at as u32);
            self.origins
                .extend(std::iter::repeat_n(NONE, self.text.len() - before - 1));
        }
    }

    /// The start of the cluster form that byte `at` of `text` is in.
    fn cluster_start(&self, at: usize) -> usize {
        (0..=at)
            .rev()
            .find(|&n| self.origins[n] != NONE)
            .expect("the first byte starts a cluster")
    }

    /// The end of the cluster form that byte `at` of `text` ends, or is inside.
    fn cluster_end(&self, at: usize) -> usize {
        (at..self.origins.len())
            .find(|&n| self.origins[n] != NONE)
            .expect("the end is a boundary")
    }
}

/// One line of text around a match, for a result row: `text` is the line, cut to start
/// near the match, and `marked` is the match within it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub marked: Range<usize>,
}

/// Columns a snippet keeps before its match, a wide character such as a CJK one taking
/// two. A row shows what fits after them, so the match is in view at the sidebar's
/// narrowest width.
pub const SNIPPET_LEAD: usize = 24;
/// Characters a snippet keeps after its match. The row cuts the rest to its width.
const SNIPPET_TAIL: usize = 160;

/// The line of `text` that holds `range`, with runs of whitespace shown as one space.
/// More than [`SNIPPET_LEAD`] columns before the match are cut at a word and marked with
/// an ellipsis. `range` must be a match of `text`.
pub fn snippet(text: &str, range: Range<usize>) -> Snippet {
    let line_start = text[..range.start].rfind('\n').map_or(0, |at| at + 1);
    let line_end = text[range.end..]
        .find('\n')
        .map_or(text.len(), |at| range.end + at);
    let before: Vec<char> = text[line_start..range.start].chars().collect();
    // The first character kept: the earliest whose columns to the match fit the lead.
    let mut columns = 0;
    let mut lead = before.len();
    while lead > 0 {
        columns += unicode_width::UnicodeWidthChar::width(before[lead - 1]).unwrap_or(0);
        if columns > SNIPPET_LEAD {
            break;
        }
        lead -= 1;
    }
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
