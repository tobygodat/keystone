//! Case-insensitive subsequence matching for pickers and palettes. A query matches a
//! candidate when its characters appear in order. Among the ways they can line up,
//! [`score`] picks the one that best rewards word starts and runs of adjacent
//! characters and least penalises the gaps between them.

use std::cell::RefCell;

const MATCH: i32 = 16;
const WORD_START: i32 = 16;
const CONSECUTIVE: i32 = 8;
const GAP_OPEN: i32 = 5;
const GAP_EXTEND: i32 = 1;
const LEADING: i32 = 2;
const LEADING_CAP: i32 = 10;
const NONE: i32 = i32::MIN / 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    /// Byte offsets in the candidate of each matched character, ascending.
    pub positions: Vec<usize>,
}

#[derive(Default)]
struct Scratch {
    query: Vec<char>,
    chars: Vec<char>,
    offsets: Vec<usize>,
    word_start: Vec<bool>,
    lo: Vec<usize>,
    hi: Vec<usize>,
    row_start: Vec<usize>,
    scores: Vec<[i32; 2]>,
    from: Vec<[Step; 2]>,
}

/// A cell's predecessor in the best alignment: its column and layer.
#[derive(Clone, Copy, Default)]
struct Step {
    column: u32,
    layer: u8,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::default();
}

fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Scores `candidate` against `query`, or `None` when the query is not a subsequence.
/// An empty query matches everything with score 0.
pub fn score(query: &str, candidate: &str) -> Option<Match> {
    if query.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    SCRATCH.with_borrow_mut(|s| s.score(query, candidate))
}

/// The byte ranges of `candidate` that a match's `positions` cover, with adjacent
/// characters joined into one range, for highlighting.
pub fn ranges(candidate: &str, positions: &[usize]) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for &start in positions {
        let end = start + candidate[start..].chars().next().map_or(0, char::len_utf8);
        match ranges.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
}

impl Scratch {
    fn score(&mut self, query: &str, candidate: &str) -> Option<Match> {
        self.query.clear();
        self.query.extend(query.chars().map(fold));
        if !self.is_subsequence(candidate) {
            return None;
        }
        self.decode(candidate);
        self.bound_columns();
        self.fill();
        Some(self.trace())
    }

    fn is_subsequence(&self, candidate: &str) -> bool {
        let mut query = self.query.iter().peekable();
        for c in candidate.chars() {
            match query.peek() {
                Some(&&q) if q == fold(c) => {
                    query.next();
                }
                Some(_) => {}
                None => break,
            }
        }
        query.peek().is_none()
    }

    fn decode(&mut self, candidate: &str) {
        self.chars.clear();
        self.offsets.clear();
        self.word_start.clear();
        let mut prev: Option<char> = None;
        for (offset, c) in candidate.char_indices() {
            let start = match prev {
                None => true,
                Some(p) => !p.is_alphanumeric() || (p.is_lowercase() && c.is_uppercase()),
            };
            self.chars.push(fold(c));
            self.offsets.push(offset);
            self.word_start.push(start);
            prev = Some(c);
        }
    }

    /// The earliest and latest column each query character can take in any full
    /// match, from a greedy pass in each direction. The DP visits only these.
    fn bound_columns(&mut self) {
        let (m, n) = (self.query.len(), self.chars.len());
        self.lo.clear();
        self.hi.clear();
        self.hi.resize(m, 0);
        let mut j = 0;
        for &q in &self.query {
            while self.chars[j] != q {
                j += 1;
            }
            self.lo.push(j);
            j += 1;
        }
        let mut j = n;
        for i in (0..m).rev() {
            j -= 1;
            while self.chars[j] != self.query[i] {
                j -= 1;
            }
            self.hi[i] = j;
        }
        self.row_start.clear();
        let mut cells = 0;
        for i in 0..m {
            self.row_start.push(cells);
            cells += self.hi[i] - self.lo[i] + 1;
        }
        self.row_start.push(cells);
        self.scores.clear();
        self.scores.resize(cells, [NONE; 2]);
        self.from.clear();
        self.from.resize(cells, [Step::default(); 2]);
    }

    fn cell(&self, i: usize, j: usize) -> Option<usize> {
        (self.lo[i]..=self.hi[i])
            .contains(&j)
            .then(|| self.row_start[i] + j - self.lo[i])
    }

    fn best(&self, i: usize, j: usize) -> (i32, u8) {
        self.cell(i, j).map_or((NONE, 0), |cell| {
            let [plain, word] = self.scores[cell];
            if word > plain { (word, 1) } else { (plain, 0) }
        })
    }

    fn bonus(&self, j: usize) -> i32 {
        if self.word_start[j] { WORD_START } else { 0 }
    }

    /// `scores[cell][layer]` is the best total for query characters `0..=i` with
    /// character `i` matched at column `j`. Layer 1 holds alignments whose current run
    /// of adjacent matches began at a word start, layer 0 the rest. A run character
    /// earns the larger of its own bonus, its run's starting bonus, and `CONSECUTIVE`,
    /// so a matched word prefix scores like word starts throughout. Keeping the two
    /// layers apart keeps the result optimal, since a lower total that started at a
    /// word start can overtake a higher one further along the run. A gap of `len`
    /// skipped characters costs `GAP_OPEN + GAP_EXTEND * (len - 1)`; `gap` carries the
    /// best such predecessor as `j` advances, so each cell is O(1).
    fn fill(&mut self) {
        let q0 = self.query[0];
        for j in self.lo[0]..=self.hi[0] {
            if self.chars[j] == q0 {
                let leading = (LEADING * j as i32).min(LEADING_CAP);
                let layer = self.word_start[j] as usize;
                self.scores[j - self.lo[0]][layer] = MATCH + self.bonus(j) - leading;
            }
        }
        for i in 1..self.query.len() {
            let q = self.query[i];
            let (mut gap, mut gap_from) = (NONE, Step::default());
            for j in self.lo[i - 1] + 1..=self.hi[i] {
                if j >= 2 {
                    let (open, layer) = self.best(i - 1, j - 2);
                    gap -= GAP_EXTEND;
                    if open - GAP_OPEN > gap {
                        gap = open - GAP_OPEN;
                        gap_from = Step {
                            column: (j - 2) as u32,
                            layer,
                        };
                    }
                }
                if j < self.lo[i] || self.chars[j] != q {
                    continue;
                }
                let bonus = self.bonus(j);
                let word = self.word_start[j] as usize;
                let cell = self.row_start[i] + j - self.lo[i];
                let mut offers = [
                    (word, gap + bonus + MATCH, gap_from),
                    (0, NONE, gap_from),
                    (0, NONE, gap_from),
                ];
                if let Some(prev) = self.cell(i - 1, j - 1) {
                    for started in 0..2 {
                        let run = bonus.max(started as i32 * WORD_START).max(CONSECUTIVE);
                        let step = Step {
                            column: (j - 1) as u32,
                            layer: started as u8,
                        };
                        offers[1 + started] = (
                            started | word,
                            self.scores[prev][started] + run + MATCH,
                            step,
                        );
                    }
                }
                for (layer, total, step) in offers {
                    if total > NONE / 2 && total > self.scores[cell][layer] {
                        self.scores[cell][layer] = total;
                        self.from[cell][layer] = step;
                    }
                }
            }
        }
    }

    fn trace(&self) -> Match {
        let last = self.query.len() - 1;
        let (mut j, (score, mut layer)) = (self.lo[last]..=self.hi[last])
            .map(|j| (j, self.best(last, j)))
            .rev()
            .max_by_key(|&(_, (score, _))| score)
            .expect("a subsequence has a full match");
        let mut positions = vec![0; self.query.len()];
        for i in (0..=last).rev() {
            positions[i] = self.offsets[j];
            if i > 0 {
                let step = self.from[self.row_start[i] + j - self.lo[i]][layer as usize];
                (j, layer) = (step.column as usize, step.layer);
            }
        }
        Match { score, positions }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank<'a>(query: &str, candidates: &[&'a str]) -> Vec<&'a str> {
        let mut scored: Vec<_> = candidates
            .iter()
            .filter_map(|c| score(query, c).map(|m| (m.score, *c)))
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, c)| c).collect()
    }

    fn positions(query: &str, candidate: &str) -> Vec<usize> {
        score(query, candidate).expect("match").positions
    }

    #[test]
    fn fuzzy_orders_best_match_first() {
        for (query, candidates, best) in [
            (
                "nwss",
                &[
                    "New Claude session",
                    "Show sessions",
                    "Rename session",
                    "New window",
                    "New session",
                    "Open new sessions view",
                ][..],
                "New session",
            ),
            ("set", &["Reset layout", "Settings"], "Settings"),
            ("tog", &["Stop logging", "Toggle sidebar"], "Toggle sidebar"),
            ("ses", &["Sale ends soon", "Session"], "Session"),
            ("ts", &["Tabs", "Toggle sidebar"], "Toggle sidebar"),
        ] {
            assert_eq!(rank(query, candidates)[0], best, "{query}");
        }
    }

    #[test]
    fn fuzzy_word_start_beats_mid_word() {
        let start = score("side", "Toggle sidebar").unwrap().score;
        let middle = score("side", "Toggle insider").unwrap().score;
        assert!(start > middle, "{start} > {middle}");
    }

    #[test]
    fn fuzzy_consecutive_beats_scattered() {
        let run = score("open", "Open tab").unwrap().score;
        let scattered = score("open", "Old pane note").unwrap().score;
        assert!(run > scattered, "{run} > {scattered}");
    }

    #[test]
    fn fuzzy_ignores_case() {
        for (query, candidate) in [
            ("NEW", "new session"),
            ("new", "NEW SESSION"),
            ("ÉT", "état"),
        ] {
            assert!(score(query, candidate).is_some(), "{query} in {candidate}");
        }
        assert_eq!(score("ABC", "abc"), score("abc", "ABC"));
    }

    #[test]
    fn fuzzy_finds_word_starts() {
        for (query, candidate, expected) in [
            ("ns", "newSession", vec![0, 3]),
            ("fb", "foo-bar", vec![0, 4]),
            ("fb", "foo_bar", vec![0, 4]),
            ("sm", "src/main.rs", vec![0, 4]),
            ("ts", "Toggle sidebar", vec![0, 7]),
            ("gs", "getString", vec![0, 3]),
        ] {
            assert_eq!(
                positions(query, candidate),
                expected,
                "{query} in {candidate}"
            );
        }
    }

    #[test]
    fn fuzzy_positions_are_byte_offsets() {
        assert_eq!(positions("nwss", "New session"), vec![0, 2, 4, 6]);
        let candidate = "Café über alles";
        let found = positions("éüa", candidate);
        assert_eq!(found, vec![3, 6, 12]);
        for (pos, q) in found.iter().zip("éüa".chars()) {
            assert_eq!(candidate[*pos..].chars().next(), Some(q));
        }
    }

    #[test]
    fn fuzzy_rejects_non_subsequences() {
        for (query, candidate) in [
            ("zzzz", "New session"),
            ("sn", "n s"),
            ("abc", "ab"),
            ("a", ""),
        ] {
            assert_eq!(score(query, candidate), None, "{query} in {candidate}");
        }
    }

    #[test]
    fn fuzzy_empty_query_matches_everything() {
        assert_eq!(
            score("", "anything"),
            Some(Match {
                score: 0,
                positions: vec![]
            })
        );
    }

    /// Scores one alignment of char indices by the rules `fill` optimises.
    fn alignment_score(candidate: &[char], picks: &[usize]) -> i32 {
        let word_start = |j: usize| {
            j == 0
                || !candidate[j - 1].is_alphanumeric()
                || (candidate[j - 1].is_lowercase() && candidate[j].is_uppercase())
        };
        let bonus = |j| if word_start(j) { WORD_START } else { 0 };
        let mut total = MATCH + bonus(picks[0]) - (LEADING * picks[0] as i32).min(LEADING_CAP);
        let mut started = bonus(picks[0]);
        for pair in picks.windows(2) {
            let (prev, j) = (pair[0], pair[1]);
            if j == prev + 1 {
                total += MATCH + bonus(j).max(started).max(CONSECUTIVE);
                started = started.max(bonus(j));
            } else {
                total += MATCH + bonus(j) - GAP_OPEN - GAP_EXTEND * (j - prev - 2) as i32;
                started = bonus(j);
            }
        }
        total
    }

    fn brute_force(query: &[char], candidate: &[char]) -> Option<i32> {
        fn walk(
            q: &[char],
            c: &[char],
            from: usize,
            picks: &mut Vec<usize>,
            best: &mut Option<i32>,
            all: &[char],
        ) {
            if picks.len() == q.len() {
                let total = alignment_score(all, picks);
                *best = Some(best.map_or(total, |b: i32| b.max(total)));
                return;
            }
            for j in from..c.len() {
                if fold(c[j]) == fold(q[picks.len()]) {
                    picks.push(j);
                    walk(q, c, j + 1, picks, best, all);
                    picks.pop();
                }
            }
        }
        let mut best = None;
        walk(query, candidate, 0, &mut Vec::new(), &mut best, candidate);
        best
    }

    #[test]
    fn fuzzy_matches_brute_force_optimum() {
        let alphabet: Vec<char> = "aAbBs -_/é".chars().collect();
        let mut seed = 0x2545_f491_u64;
        let mut next = |bound: usize| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) as usize % bound
        };
        for _ in 0..4_000 {
            let candidate: Vec<char> = (0..1 + next(11))
                .map(|_| alphabet[next(alphabet.len())])
                .collect();
            let query: Vec<char> = (0..1 + next(4)).map(|_| alphabet[next(5)]).collect();
            let (candidate_text, query_text): (String, String) =
                (candidate.iter().collect(), query.iter().collect());
            let found = score(&query_text, &candidate_text);
            assert_eq!(
                found.as_ref().map(|m| m.score),
                brute_force(&query, &candidate),
                "{query_text:?} in {candidate_text:?}"
            );
            if let Some(found) = found {
                let picks: Vec<usize> = found
                    .positions
                    .iter()
                    .map(|&byte| candidate_text[..byte].chars().count())
                    .collect();
                assert_eq!(
                    alignment_score(&candidate, &picks),
                    found.score,
                    "{query_text:?} in {candidate_text:?}"
                );
            }
        }
    }
}
