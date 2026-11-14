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
            let found = keystone_agent::search::messages(&database, &query, || false)
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
