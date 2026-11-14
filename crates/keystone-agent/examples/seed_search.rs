//! Adds labelled test sessions to a closed workspace for the search journey
//! (`scripts/search-journey.py`): `count` short sessions, some settled, some of whose
//! messages mention the needle, and one long session to find in.
//!
//! cargo run -p keystone-agent --example seed_search --locked -- STATE FOLDER COUNT TURNS

use keystone_agent::log::Entry;
use keystone_agent::{Item, ItemKind, ItemStatus, TurnStatus};
use keystone_core::{Provider, Store, WorkingDirectory};

fn item(id: String, kind: ItemKind, text: String) -> String {
    serde_json::to_string(&Entry::Item {
        item: Item {
            id,
            kind,
            text,
            status: ItemStatus::Completed,
        },
        seq: None,
        run: None,
    })
    .unwrap()
}

fn turn_end() -> String {
    serde_json::to_string(&Entry::TurnEnded {
        status: TurnStatus::Completed,
        error: None,
        limit: None,
        seq: None,
        run: None,
    })
    .unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [state, folder, count, turns] = args.as_slice() else {
        panic!("usage: seed_search STATE FOLDER COUNT TURNS");
    };
    let (count, turns): (u64, usize) = (count.parse().unwrap(), turns.parse().unwrap());
    let directory = WorkingDirectory::new(std::path::Path::new(folder)).unwrap();
    let mut store = Store::open(state).unwrap();
    let database = store.database();
    let started = serde_json::to_string(&Entry::TurnStarted).unwrap();
    let mut key = 0u64;
    let mut records = |entries: Vec<String>| -> Vec<(String, String)> {
        entries
            .into_iter()
            .map(|payload| {
                key += 1;
                (format!("seed-search-{key}"), payload)
            })
            .collect()
    };

    let long = store.create(Provider::Claude, &directory, 1).unwrap().value;
    store.rename(long, "Long session: release notes").unwrap();
    let mut entries = Vec::new();
    for turn in 0..turns {
        entries.push(started.clone());
        entries.push(item(
            format!("lu-{turn}"),
            ItemKind::UserMessage,
            format!("Step {turn}: check the build and summarise it."),
        ));
        let output = if turn % 97 == 13 {
            format!("compiling 214 crates\nwarning: the needle in step {turn} is unused")
        } else {
            "compiling 214 crates\nfinished dev profile in 6.2s".to_owned()
        };
        entries.push(item(
            format!("lc-{turn}"),
            ItemKind::Command {
                command: "cargo build --locked".into(),
                exit_code: Some(0),
            },
            output,
        ));
        let reply = if turn % 40 == 7 {
            format!(
                "Step {turn} is done. The build passed, and the **needle** we were \
                 looking for is in `crates/core/src/search.rs`.\n\n- Tests: 214 passed\n\
                 - Warnings: none"
            )
        } else {
            format!(
                "Step {turn} is done. The build passed in 6.2 seconds with no warnings, \
                 and every test still passes. Nothing else changed since the last step."
            )
        };
        entries.push(item(format!("la-{turn}"), ItemKind::AgentMessage, reply));
        entries.push(turn_end());
    }
    database.append_logs(long, &records(entries), None).unwrap();

    const TOPICS: [&str; 6] = [
        "sidebar polish",
        "flaky timing test",
        "terminal resize",
        "storage migration",
        "palette ranking",
        "diff colors",
    ];
    for n in 0..count {
        let provider = if n % 2 == 0 {
            Provider::Claude
        } else {
            Provider::Codex
        };
        let id = store.create(provider, &directory, 1).unwrap().value;
        store
            .rename(id, &format!("{} {n}", TOPICS[n as usize % TOPICS.len()]))
            .unwrap();
        if n % 4 == 3 {
            store.set_settled(id, true).unwrap();
        }
        let mut entries = Vec::new();
        for turn in 0..8 {
            entries.push(started.clone());
            entries.push(item(
                format!("u{n}-{turn}"),
                ItemKind::UserMessage,
                format!(
                    "Question {turn} about {}.",
                    TOPICS[n as usize % TOPICS.len()]
                ),
            ));
            let reply = if turn == 5 && n % 3 == 0 {
                format!(
                    "In session {n} the needle turned up in the {} code, line {}.",
                    TOPICS[n as usize % TOPICS.len()],
                    40 + n
                )
            } else {
                format!("Answer {turn}: nothing unusual here, the change is small.")
            };
            entries.push(item(format!("a{n}-{turn}"), ItemKind::AgentMessage, reply));
            entries.push(turn_end());
        }
        database.append_logs(id, &records(entries), None).unwrap();
    }
    store.flush().unwrap();
    println!("seeded {} sessions; long session {long}", count + 1);
}
