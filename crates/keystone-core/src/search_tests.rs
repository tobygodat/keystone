use super::{Query, SNIPPET_LEAD, snippet};

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
fn a_match_never_ends_inside_a_character_that_lowercases_to_two() {
    // `İ` lowercases to `i` and a combining dot.
    assert!(found("i", "İ").is_empty());
    assert_eq!(found("i\u{307}", "İx"), ["İ"]);
    assert_eq!(found("i", "İ i"), ["i"]);
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

/// Characters that stress folding: ASCII of both cases, accents, `ß`, `İ`, the Kelvin
/// sign (which lowercases to ASCII `k`), Greek final sigma, CJK, emoji and whitespace.
const ALPHABET: &[char] = &[
    'a', 'b', 'k', 'A', 'B', 'K', ' ', '\n', '\t', 'é', 'É', 'ß', 'İ', 'i', '\u{307}', 'K', 'Σ',
    'σ', 'ς', '日', '🙂',
];

/// Leftmost non-overlapping matches by brute force over every pair of character
/// boundaries.
fn reference(query: &str, text: &str) -> Vec<std::ops::Range<usize>> {
    let fold = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
    let folded = fold(query);
    let bounds: Vec<usize> = text
        .char_indices()
        .map(|(at, _)| at)
        .chain([text.len()])
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bounds.len() {
        let start = bounds[i];
        match bounds[i + 1..]
            .iter()
            .position(|&end| fold(&text[start..end]) == folded)
        {
            Some(n) => {
                out.push(start..bounds[i + 1 + n]);
                i += n + 1;
            }
            None => i += 1,
        }
    }
    out
}

#[test]
fn generated_texts_match_the_brute_force_reference() {
    for seed in 1..=3_000u64 {
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
        assert_eq!(
            parsed.find(&text),
            reference(&query, &text),
            "seed {seed}: query {query:?} in {text:?}"
        );
        assert_eq!(parsed.is_in(&text), !reference(&query, &text).is_empty());
    }
}
