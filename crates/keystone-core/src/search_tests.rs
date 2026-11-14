use super::{Query, SNIPPET_LEAD, snippet};
use unicode_segmentation::UnicodeSegmentation;

fn found<'a>(query: &str, text: &'a str) -> Vec<&'a str> {
    Query::new(query)
        .unwrap()
        .find(text)
        .into_iter()
        .map(|range| &text[range])
        .collect()
}

#[test]
fn an_empty_query_finds_nothing() {
    assert!(Query::new("").is_none());
}

#[test]
fn matches_ignore_case_and_never_overlap() {
    assert_eq!(found("ab", "Ab aB AB"), ["Ab", "aB", "AB"]);
    assert_eq!(found("aa", "aaaa a"), ["aa", "aa"]);
    assert!(found("missing", "nothing here").is_empty());
    let query = Query::new("NEEDLE").unwrap();
    assert!(query.is_in("a needle"));
    assert!(!query.is_in("a needl"));
}

#[test]
fn accented_and_other_scripts_match_their_other_case() {
    assert_eq!(found("café", "CAFÉ and Café"), ["CAFÉ", "Café"]);
    assert_eq!(found("ΣΟΦΊΑ", "σοφία"), ["σοφία"]);
    assert_eq!(found("日本", "東京は日本の首都"), ["日本"]);
    assert_eq!(found("🙂", "ok 🙂 ok"), ["🙂"]);
}

#[test]
fn case_folds_fully() {
    assert_eq!(found("ΟΔΟΣ", "η οδος"), ["οδος"]);
    assert_eq!(found("σ", "ς"), ["ς"]);
    assert_eq!(found("ss", "Straße STRASSE"), ["ß", "SS"]);
    assert_eq!(found("ß", "ẞ ss"), ["ẞ", "ss"]);
    assert_eq!(found("file", "ﬁle"), ["ﬁle"]);
    assert_eq!(found("k", "\u{212A}"), ["\u{212A}"]);
    assert_eq!(found("ſ", "S s"), ["S", "s"]);
}

#[test]
fn a_dotted_capital_i_is_an_i() {
    assert_eq!(found("istanbul", "İstanbul"), ["İstanbul"]);
    assert_eq!(found("İSTANBUL", "istanbul"), ["istanbul"]);
    assert_eq!(found("i\u{307}", "İx"), ["İ"]);
    assert_eq!(found("i", "İ i"), ["İ", "i"]);
    // Dotless `ı` stays its own letter.
    assert!(found("i", "ı").is_empty());
}

#[test]
fn composed_and_decomposed_text_match_each_other() {
    assert_eq!(found("e\u{301}", "café"), ["é"]);
    assert_eq!(found("é", "cafe\u{301}"), ["e\u{301}"]);
    assert_eq!(found("CAFE\u{301}", "café"), ["café"]);
    // A precomposed letter is not its base letter.
    assert!(found("cafe", "café").is_empty());
    assert!(found("cafe", "cafe\u{301}").is_empty());
    assert_eq!(found("caf", "cafe\u{301}"), ["caf"]);
}

#[test]
fn a_match_covers_whole_grapheme_clusters() {
    let family = "👨\u{200D}👩\u{200D}👧";
    let text = format!("the {family} here");
    assert_eq!(found("👩", &text), [family]);
    assert_eq!(found("e 👨", &text), [format!("e {family}")]);
    // One cluster is one match, however often the query is inside it.
    assert_eq!(found("s", "ß"), ["ß"]);
    assert_eq!(found("q", "q\u{301}x"), ["q\u{301}"]);
    // A text whose ASCII sits in a longer cluster takes the slow path.
    assert_eq!(found("1", "\u{600}12"), ["\u{600}1"]);
    assert_eq!(found("a", "\r\na"), ["a"]);
}

#[test]
fn a_snippet_is_the_line_around_the_match() {
    let text = "first line\nthe needle is here\nlast";
    let query = Query::new("needle").unwrap();
    let range = query.find(text)[0].clone();
    let line = snippet(text, range);
    assert_eq!(line.text, "the needle is here");
    assert_eq!(&line.text[line.marked], "needle");
}

#[test]
fn a_very_long_line_is_cut_to_keep_the_match_near_the_start() {
    let text = format!(
        "{} needle {}",
        "word ".repeat(200_000),
        "tail ".repeat(200_000)
    );
    let query = Query::new("NEEDLE").unwrap();
    let ranges = query.find(&text);
    assert_eq!(ranges.len(), 1);
    let line = snippet(&text, ranges[0].clone());
    assert_eq!(&line.text[line.marked.clone()], "needle");
    assert!(line.text.starts_with('…') && line.text.ends_with('…'));
    assert!(line.text[..line.marked.start].chars().count() <= SNIPPET_LEAD + 1);
    assert!(line.text.len() < 400, "{}", line.text.len());
}

#[test]
fn a_snippet_s_lead_counts_wide_characters_twice() {
    let text = format!("{}検索の結果", "設定を変更して".repeat(10));
    let range = Query::new("検索").unwrap().find(&text)[0].clone();
    let line = snippet(&text, range);
    let lead = &line.text[..line.marked.start];
    assert!(lead.starts_with('…'));
    assert_eq!(lead.chars().count(), 1 + SNIPPET_LEAD / 2, "{lead}");
    assert_eq!(&line.text[line.marked], "検索");
}

#[test]
fn an_emoji_lead_leaves_the_match_in_view() {
    // A heart is a text character with an emoji selector; a smiley is an emoji itself.
    // Each draws as one emoji, counted as three columns. A family joined by zero-width
    // joiners counts each of its three people, as a font without its glyph draws them.
    for (lead, kept) in [
        ("❤\u{FE0F}".repeat(30), 8),
        ("🙂".repeat(20), 8),
        ("👩\u{200D}👧\u{200D}👦".repeat(10), 2),
        ("1\u{FE0F}\u{20E3}".repeat(20), 8),
        // Symbols drawn as emoji without a selector.
        ("✅".repeat(30), 8),
        ("✨".repeat(30), 8),
        ("⭐".repeat(30), 8),
        ("☺".repeat(40), 8),
    ] {
        let text = format!("{lead}検索 here");
        let range = Query::new("検索").unwrap().find(&text)[0].clone();
        let line = snippet(&text, range);
        let before = &line.text[..line.marked.start];
        assert!(before.starts_with('…'), "{before}");
        assert_eq!(
            before['…'.len_utf8()..].graphemes(true).count(),
            kept,
            "{before}"
        );
        assert_eq!(&line.text[line.marked], "検索");
    }
}

#[test]
fn a_snippet_shows_runs_of_whitespace_as_one_space() {
    let text = "a\t\tb   needle";
    let range = Query::new("needle").unwrap().find(text)[0].clone();
    let line = snippet(text, range);
    assert_eq!(line.text, "a b needle");
    assert_eq!(&line.text[line.marked], "needle");
}

/// A small seeded generator, so a failure names the case that reproduces it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Characters that stress the matcher: ASCII of both cases, CR and LF, precomposed and
/// decomposed accents, `ß` and `ẞ`, `İ` and a lone dot above, dotless `ı`, the Kelvin
/// sign and long `s` (which fold to ASCII), a ligature, Greek sigmas, CJK, emoji with a
/// zero-width joiner, a prepended mark that joins the next character's cluster,
/// Hangul jamo, regional indicators, an Indic conjunct and a tag.
const ALPHABET: &[char] = &[
    'a',
    'b',
    'k',
    'i',
    's',
    'e',
    'f',
    'q',
    'j',
    'A',
    'K',
    'I',
    'S',
    'J',
    ' ',
    '\n',
    '\r',
    'é',
    'É',
    '\u{301}',
    '\u{323}',
    'ß',
    'ẞ',
    'İ',
    '\u{307}',
    'ı',
    '\u{212A}',
    'ſ',
    'ﬁ',
    'Σ',
    'σ',
    'ς',
    '日',
    '🙂',
    '👩',
    '👧',
    '\u{200D}',
    '\u{600}',
    '\u{1112}',
    '\u{1161}',
    '\u{11AB}',
    '한',
    '🇫',
    '🇷',
    'क',
    '\u{94D}',
    'ष',
    '\u{E0067}',
    'ﬃ',
];

/// Leftmost non-overlapping matches, each widened to whole clusters, found by brute
/// force in each cluster's searched form.
fn reference(query: &str, text: &str) -> Vec<std::ops::Range<usize>> {
    use unicode_segmentation::UnicodeSegmentation;
    let shown = |s: &str| {
        let mut out = String::new();
        for cluster in s.graphemes(true) {
            super::form(cluster, &mut out);
        }
        out
    };
    let wanted = shown(query);
    let clusters: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let forms: Vec<String> = clusters.iter().map(|(_, c)| shown(c)).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < clusters.len() {
        // A match that starts in cluster `i`'s form: the end is the first cluster whose
        // forms, from `i` on, hold it.
        let found = (i..clusters.len()).find_map(|end| {
            let joined: String = forms[i..=end].concat();
            joined
                .char_indices()
                .take_while(|(at, _)| *at < forms[i].len())
                .any(|(at, _)| joined[at..].starts_with(&wanted))
                .then_some(end)
        });
        match found {
            Some(end) => {
                let stop = clusters.get(end + 1).map_or(text.len(), |(at, _)| *at);
                out.push(clusters[i].0..stop);
                i = end + 1;
            }
            None => i += 1,
        }
    }
    out
}

#[test]
fn generated_texts_match_the_brute_force_reference() {
    for seed in 1..=6_000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let text: String = (0..rng.below(40))
            .map(|_| ALPHABET[rng.below(ALPHABET.len())])
            .collect();
        let chars: Vec<char> = text.chars().collect();
        let query: String = if chars.is_empty() || rng.below(4) == 0 {
            (0..1 + rng.below(3))
                .map(|_| ALPHABET[rng.below(ALPHABET.len())])
                .collect()
        } else {
            let start = rng.below(chars.len());
            let len = 1 + rng.below((chars.len() - start).min(5));
            chars[start..start + len]
                .iter()
                .map(|c| {
                    if rng.below(2) == 0 {
                        c.to_uppercase().next().unwrap()
                    } else {
                        *c
                    }
                })
                .collect()
        };
        let Some(parsed) = Query::new(&query) else {
            continue;
        };
        let wanted = reference(&query, &text);
        assert_eq!(
            parsed.find(&text),
            wanted,
            "seed {seed}: query {query:?} in {text:?}"
        );
        assert_eq!(parsed.find_folded(&text), wanted, "seed {seed}: slow path");
        assert_eq!(parsed.is_in(&text), !wanted.is_empty());
    }
}

/// Clusters whose forms take every path of `Folded::new`: ASCII runs with CR and LF,
/// characters that end their clusters, ones that join (marks, joiners, prepended
/// marks, Hangul jamo, regional indicators, tags), and ones that change in form.
const CLUSTERS: &[&str] = &[
    "a",
    "A",
    "i",
    "I",
    "j",
    "s",
    "S",
    "k",
    " ",
    "\n",
    "\r",
    "\r\n",
    "\t",
    "é",
    "É",
    "e\u{301}",
    "\u{301}",
    "\u{302}",
    "\u{323}",
    "\u{307}",
    "\u{345}",
    "ß",
    "ẞ",
    "İ",
    "ı",
    "i\u{307}",
    "i\u{323}\u{307}",
    "j\u{307}",
    "\u{212A}",
    "ſ",
    "ﬁ",
    "ﬃ",
    "ŉ",
    "ǰ",
    "ΐ",
    "ᾈ",
    "Σ",
    "ς",
    "\u{212B}",
    "\u{2126}",
    "한",
    "\u{1112}\u{1161}\u{11AB}",
    "\u{1112}",
    "\u{1161}",
    "\u{11AB}",
    "日",
    "\u{F900}",
    "🙂",
    "👩",
    "\u{200D}",
    "\u{1F3FD}",
    "\u{FE0F}",
    "🇫",
    "🇷",
    "\u{600}",
    "क",
    "\u{94D}",
    "ष",
    "\u{93F}",
    "Ꭰ",
    "ꭰ",
    "Ａ",
    "\u{E0067}",
    "\u{E007F}",
    "\u{10400}",
    "ǅ",
    "Ⅰ",
    "Ⓐ",
];

#[test]
fn the_fast_folded_form_is_each_cluster_s_form() {
    use unicode_segmentation::UnicodeSegmentation;
    for seed in 1..=20_000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let text: String = (0..rng.below(30))
            .map(|_| CLUSTERS[rng.below(CLUSTERS.len())])
            .collect();
        let folded = super::Folded::new(&text);
        let mut wanted = String::new();
        let mut starts = Vec::new();
        for (at, cluster) in text.grapheme_indices(true) {
            starts.push((wanted.len(), at));
            super::form(cluster, &mut wanted);
        }
        assert_eq!(
            String::from_utf8(folded.text.clone()).unwrap(),
            wanted,
            "seed {seed}: {text:?}"
        );
        let got: Vec<(usize, usize)> = folded
            .origins
            .iter()
            .enumerate()
            .filter(|(at, origin)| **origin != super::NONE && *at < folded.text.len())
            .map(|(at, origin)| (at, *origin as usize))
            .collect();
        assert_eq!(got, starts, "seed {seed}: cluster starts in {text:?}");
    }
}
