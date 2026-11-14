//! Measures search across sessions on a generated workspace of 500 sessions, to choose
//! between scanning the log and an FTS5 index. Run with
//! `cargo test -p keystone-agent --locked --test perf_search -- --ignored --nocapture`.
//! Prints `PERF name=value` lines.

use keystone_agent::log::Entry;
use keystone_agent::{Item, ItemKind, ItemStatus};
use keystone_core::Database;
use keystone_core::search::Query;
use std::time::Instant;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn vocabulary(rng: &mut Rng) -> Vec<String> {
    const SYLLABLES: [&str; 24] = [
        "ka", "lo", "mi", "ter", "an", "so", "re", "vi", "na", "pul", "ex", "or", "ing", "da",
        "be", "fu", "sh", "to", "ra", "en", "co", "de", "it", "um",
    ];
    (0..3000)
        .map(|_| {
            (0..2 + rng.below(3))
                .map(|_| SYLLABLES[rng.below(SYLLABLES.len() as u64) as usize])
                .collect()
        })
        .collect()
}

fn prose(rng: &mut Rng, words: &[String], len: u64) -> String {
    let mut text = String::new();
    while (text.len() as u64) < len {
        // Zipf-like: common words far more often than rare ones.
        let pick =
            (rng.below(words.len() as u64) * rng.below(words.len() as u64)) / words.len() as u64;
        text.push_str(&words[pick as usize]);
        text.push(if rng.below(12) == 0 { '\n' } else { ' ' });
    }
    text
}

fn entry(rng: &mut Rng, words: &[String], n: u64) -> Entry {
    let roll = rng.below(100);
    let (kind, len) = match roll {
        0..20 => (ItemKind::UserMessage, 60 + rng.below(400)),
        20..45 => (ItemKind::AgentMessage, 300 + rng.below(2700)),
        45..55 => (ItemKind::Reasoning, 200 + rng.below(800)),
        55..85 => (
            ItemKind::Command {
                command: "cargo test".into(),
                exit_code: Some(0),
            },
            if rng.below(40) == 0 {
                64 * 1024
            } else {
                100 + rng.below(8000)
            },
        ),
        _ => (
            ItemKind::Tool {
                name: "read".into(),
            },
            200 + rng.below(1800),
        ),
    };
    Entry::Item {
        item: Item {
            id: format!("item-{n}"),
            kind,
            text: prose(rng, words, len),
            status: ItemStatus::Completed,
        },
        seq: Some(n),
        run: Some(1),
    }
}

fn messages_text(entry: &Entry) -> Option<&str> {
    match entry {
        Entry::Item { item, .. }
            if matches!(item.kind, ItemKind::UserMessage | ItemKind::AgentMessage) =>
        {
            Some(&item.text)
        }
        _ => None,
    }
}

fn median(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    (
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1],
    )
}

#[test]
#[ignore]
fn perf_search_across_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.sqlite3");
    let database = Database::open(&path).unwrap();
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let words = vocabulary(&mut rng);
    let mut texts: Vec<String> = Vec::new();
    let (mut entries, mut bytes) = (0u64, 0u64);
    let mut n = 0;
    for session in 0..500u64 {
        let rows = match session {
            0 => 5000,
            _ if rng.below(20) == 0 => 1500,
            _ => 40 + rng.below(300),
        };
        let mut records = Vec::new();
        for _ in 0..rows {
            n += 1;
            let entry = entry(&mut rng, &words, n);
            if let Some(text) = messages_text(&entry) {
                texts.push(text.to_owned());
            }
            let payload = serde_json::to_string(&entry).unwrap();
            bytes += payload.len() as u64;
            records.push((format!("perf-{n}"), payload));
        }
        entries += rows;
        database.append_logs(session, &records, None).unwrap();
    }
    drop(database);
    let database = Database::open(&path).unwrap();
    println!(
        "PERF search_corpus_entries={entries} search_corpus_mb={:.1} search_messages={}",
        bytes as f64 / 1e6,
        texts.len()
    );

    // Reading every row without matching: the floor for any scan.
    let mut samples = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let mut seen = 0usize;
        database
            .scan_logs(|_, payload| {
                seen += payload.len();
                true
            })
            .unwrap();
        assert!(seen > 0);
        samples.push(start.elapsed().as_secs_f64() * 1e3);
    }
    let (med, low, high) = median(samples);
    println!("PERF search_read_only_ms={med:.1} range {low:.1}..{high:.1}");

    let rare = words[2999].clone();
    let common = words[0].clone();
    for (name, query) in [
        ("rare", rare.as_str()),
        ("common", common.as_str()),
        ("two_letters", "ka"),
        ("absent", "zzqx"),
        ("accented", "é"),
    ] {
        let query = Query::new(query).unwrap();
        let mut samples = Vec::new();
        let mut last = None;
        for _ in 0..5 {
            let start = Instant::now();
            let found = keystone_agent::search::messages(
                &database,
                &query,
                |_, item: &keystone_agent::Item| vec![item.text.clone()],
                || false,
            )
            .unwrap()
            .unwrap();
            samples.push(start.elapsed().as_secs_f64() * 1e3);
            last = Some(found);
        }
        let found = last.unwrap();
        let (med, low, high) = median(samples);
        println!(
            "PERF search_scan_{name}_ms={med:.1} range {low:.1}..{high:.1} hits={} scanned={} parsed={}",
            found.hits.len(),
            found.scanned,
            found.parsed
        );
    }

    // The FTS5 alternative: a trigram index over message text, in its own file so the
    // workspace stays as it is.
    let fts = rusqlite::Connection::open(dir.path().join("fts.sqlite3")).unwrap();
    fts.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
         CREATE VIRTUAL TABLE messages USING fts5(text, tokenize='trigram');",
    )
    .unwrap();
    let start = Instant::now();
    let tx = fts.unchecked_transaction().unwrap();
    {
        let mut insert = tx
            .prepare("INSERT INTO messages(text) VALUES (?1)")
            .unwrap();
        for text in &texts {
            insert.execute([text]).unwrap();
        }
    }
    tx.commit().unwrap();
    let build = start.elapsed().as_secs_f64() * 1e3;
    let size: i64 = fts
        .query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |row| row.get(0),
        )
        .unwrap();
    println!(
        "PERF fts_build_ms={build:.0} fts_insert_us={:.1} fts_index_mb={:.1}",
        build * 1e3 / texts.len() as f64,
        size as f64 / 1e6
    );
    for (name, query) in [("rare", rare.as_str()), ("common", common.as_str())] {
        let mut samples = Vec::new();
        let mut hits = 0;
        for _ in 0..5 {
            let start = Instant::now();
            let mut statement = fts
                .prepare("SELECT rowid FROM messages WHERE text MATCH ?1")
                .unwrap();
            hits = statement
                .query_map([format!("\"{query}\"")], |row| row.get::<_, i64>(0))
                .unwrap()
                .count();
            samples.push(start.elapsed().as_secs_f64() * 1e3);
        }
        let (med, low, high) = median(samples);
        println!("PERF fts_{name}_ms={med:.1} range {low:.1}..{high:.1} rows={hits}");
    }
}

/// A language's words for generated messages. Some carry case, accents or folding the
/// matcher has to handle, and each holds the words the benchmark searches for.
struct Language {
    name: &'static str,
    words: &'static [&'static str],
    /// Between words, and at a sentence's end.
    space: &'static str,
    stop: &'static str,
}

const FRENCH: Language = Language {
    name: "french",
    words: &[
        "le",
        "la",
        "les",
        "un",
        "une",
        "des",
        "et",
        "est",
        "été",
        "très",
        "déjà",
        "où",
        "café",
        "Café",
        "crème",
        "CRÈME",
        "élève",
        "garçon",
        "naïve",
        "fenêtre",
        "première",
        "réponse",
        "problème",
        "système",
        "fichier",
        "modifié",
        "créé",
        "vérifié",
        "à",
        "après",
        "même",
        "ça",
        "voilà",
        "terminé",
        "étape",
        "dernière",
        "sécurité",
    ],
    space: " ",
    stop: ".",
};

const GERMAN: Language = Language {
    name: "german",
    words: &[
        "der",
        "die",
        "das",
        "und",
        "ist",
        "nicht",
        "über",
        "für",
        "müssen",
        "Größe",
        "Straße",
        "straße",
        "Strasse",
        "Fuß",
        "heißt",
        "weiß",
        "groß",
        "schließen",
        "Datei",
        "geändert",
        "Prüfung",
        "Änderung",
        "möglich",
        "zurück",
        "Schlüssel",
        "außerdem",
        "Lösung",
        "Fehler",
        "bestätigt",
        "erstellt",
        "später",
        "nächste",
    ],
    space: " ",
    stop: ".",
};

const JAPANESE: Language = Language {
    name: "japanese",
    words: &[
        "検索",
        "日本語",
        "テスト",
        "ファイル",
        "変更",
        "確認",
        "します",
        "です",
        "ました",
        "これ",
        "それ",
        "の",
        "を",
        "に",
        "は",
        "が",
        "で",
        "設定",
        "結果",
        "実行",
        "完了",
        "問題",
        "修正",
        "追加",
        "削除",
        "画面",
        "入力",
        "出力",
        "表示",
        "保存",
        "読み込み",
    ],
    space: "",
    stop: "。",
};

const ENGLISH: Language = Language {
    name: "english",
    words: &[
        "the", "build", "passed", "and", "every", "test", "still", "passes", "nothing", "else",
        "changed", "since", "last", "step", "search", "sessions", "files", "this", "is", "a",
        "reply", "with", "some", "words", "in", "it", "from", "agent", "fixed", "bug", "release",
        "notes",
    ],
    space: " ",
    stop: ".",
};

/// A reply's Markdown source and the texts its row draws, one per paragraph: the
/// generator writes both, so the matcher's benchmark needs no Markdown parser.
fn reply(rng: &mut Rng, language: &Language, len: usize) -> (String, Vec<String>) {
    let (mut source, mut drawn) = (String::new(), vec![String::new()]);
    let mut words = 0;
    while source.len() < len {
        let word = language.words[rng.below(language.words.len() as u64) as usize];
        if words > 0 {
            source.push_str(language.space);
            drawn.last_mut().unwrap().push_str(language.space);
        }
        match rng.below(20) {
            0 => source.push_str(&format!("**{word}**")),
            1 => source.push_str(&format!("`{word}`")),
            2 => source.push_str(&format!("[{word}](https://example.com/{})", rng.below(999))),
            _ => source.push_str(word),
        }
        drawn.last_mut().unwrap().push_str(word);
        words += 1;
        if rng.below(12) == 0 {
            source.push_str(language.stop);
            drawn.last_mut().unwrap().push_str(language.stop);
            if rng.below(4) == 0 {
                source.push_str("\n\n");
                drawn.push(String::new());
                words = 0;
            }
        }
    }
    (source, drawn)
}

/// A user message: plain text, drawn as typed.
fn question(rng: &mut Rng, language: &Language) -> String {
    let len = 40 + rng.below(200) as usize;
    reply(rng, language, len).1.join(" ")
}

/// The texts find reads in a 1,500-row session: a question, a command's output and a
/// reply each turn.
fn session_texts(language: &Language) -> Vec<String> {
    let mut rng = Rng(0x1500_0000_0000_0001);
    let mut texts = Vec::new();
    for turn in 0..500 {
        texts.push(question(&mut rng, language));
        texts.push(format!(
            "$ cargo test --locked\nrunning 214 tests\ntest result: ok. step {turn}"
        ));
        let len = 300 + rng.below(1200) as usize;
        texts.extend(reply(&mut rng, language, len).1);
    }
    texts
}

/// A workspace of 525 sessions of 60 messages, alternating question and reply: each
/// message's saved source and its drawn texts.
fn workspace_messages(language: &Language) -> Vec<(bool, String, Vec<String>)> {
    let mut rng = Rng(0x0525_0060_0000_0001);
    let mut messages = Vec::new();
    for _ in 0..525 {
        for n in 0..60 {
            if n % 2 == 0 {
                let text = question(&mut rng, language);
                messages.push((false, text.clone(), vec![text]));
            } else {
                let len = 200 + rng.below(1000) as usize;
                let (source, drawn) = reply(&mut rng, language, len);
                messages.push((true, source, drawn));
            }
        }
    }
    messages
}

/// The workloads the search follow-ups were checked on (TOB-462): a query, and the
/// language its texts are in.
const WORKLOADS: [(&str, &Language); 6] = [
    ("café", &FRENCH),
    ("CRÈME", &FRENCH),
    ("strasse", &GERMAN),
    ("検索", &JAPANESE),
    ("s", &ENGLISH),
    ("search", &ENGLISH),
];

/// The matcher alone, as find and the sidebar use it, in each language: every match in
/// each of a 1,500-row session's texts, as find reads them, and in each of a 525-session
/// workspace's drawn message texts. Release build:
/// `cargo test --release -p keystone-agent --test perf_search --locked -- --ignored --nocapture perf_matcher`
#[test]
#[ignore]
fn perf_matcher_in_languages() {
    for (typed, language) in WORKLOADS {
        let session = session_texts(language);
        let workspace: Vec<String> = workspace_messages(language)
            .into_iter()
            .flat_map(|(_, _, drawn)| drawn)
            .collect();
        let query = Query::new(typed).unwrap();
        for (corpus, texts) in [("session_1500", &session), ("workspace_525x60", &workspace)] {
            let mut samples = Vec::new();
            let mut matches = 0;
            for _ in 0..5 {
                let start = Instant::now();
                matches = texts
                    .iter()
                    .map(|text| query.find(text).len())
                    .sum::<usize>();
                samples.push(start.elapsed().as_secs_f64() * 1e3);
            }
            let (med, low, high) = median(samples);
            let mb = texts.iter().map(String::len).sum::<usize>() as f64 / 1e6;
            println!(
                "PERF matcher_{}_{typed}_{corpus}_ms={med:.2} range {low:.2}..{high:.2} matches={matches} mb={mb:.1}",
                language.name
            );
        }
    }
}

/// The texts a reply's row draws, through the same parser and options as the desktop's
/// `Document::texts`: each paragraph's, heading's and code block's text.
fn drawn_markdown(source: &str) -> Vec<String> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut texts = Vec::new();
    let mut current: Option<String> = None;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(Tag::Paragraph | Tag::Heading { .. } | Tag::CodeBlock(_)) => {
                current = Some(String::new())
            }
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::CodeBlock) => {
                texts.extend(current.take())
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(current) = current.as_mut() {
                    current.push_str(&text)
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(current) = current.as_mut() {
                    current.push(' ')
                }
            }
            _ => {}
        }
    }
    texts
}

/// The sidebar's whole scan in each language over a 525-session workspace, each
/// screened reply parsed as Markdown for the texts its row draws: the workloads, which
/// each session holds early, and a word no message holds, which reads every message.
#[test]
#[ignore]
fn perf_sidebar_scan_in_languages() {
    let absent: [(&str, &Language); 4] = [
        ("œuvre", &FRENCH),
        ("Straßenbahn", &GERMAN),
        ("存在しない", &JAPANESE),
        ("zzqx", &ENGLISH),
    ];
    for (typed, language) in WORKLOADS.into_iter().chain(absent) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.sqlite3");
        let database = Database::open(&path).unwrap();
        let mut n = 0u64;
        let messages = workspace_messages(language);
        for (session, chunk) in messages.chunks(60).enumerate() {
            let records: Vec<(String, String)> = chunk
                .iter()
                .map(|(reply, source, _)| {
                    n += 1;
                    let entry = Entry::Item {
                        item: Item {
                            id: format!("m{n}"),
                            kind: if *reply {
                                ItemKind::AgentMessage
                            } else {
                                ItemKind::UserMessage
                            },
                            text: source.clone(),
                            status: ItemStatus::Completed,
                        },
                        seq: Some(n),
                        run: Some(1),
                    };
                    (format!("perf-{n}"), serde_json::to_string(&entry).unwrap())
                })
                .collect();
            database
                .append_logs(session as u64, &records, None)
                .unwrap();
        }
        let query = Query::new(typed).unwrap();
        let mut samples = Vec::new();
        let mut last = None;
        for _ in 0..5 {
            let start = Instant::now();
            let found = keystone_agent::search::messages(
                &database,
                &query,
                |_, item: &Item| match item.kind {
                    ItemKind::AgentMessage => drawn_markdown(&item.text),
                    _ => vec![item.text.clone()],
                },
                || false,
            )
            .unwrap()
            .unwrap();
            samples.push(start.elapsed().as_secs_f64() * 1e3);
            last = Some(found);
        }
        let found = last.unwrap();
        let (med, low, high) = median(samples);
        println!(
            "PERF sidebar_scan_{}_{typed}_ms={med:.1} range {low:.1}..{high:.1} hits={} parsed={}",
            language.name,
            found.hits.len(),
            found.parsed
        );
    }
}
