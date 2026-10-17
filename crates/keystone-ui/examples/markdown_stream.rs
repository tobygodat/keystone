//! Times parsing a long reply as it streams: a whole parse on every delta, as the
//! transcript did before, against appending to an `IncrementalParser`.
//!
//! cargo run --release -p keystone-ui --example markdown_stream -- [lines] [chunk] [runs]
//!
//! With `--tail`, times each of the last deltas of a code-heavy reply one by one, where
//! a `]:` near the top sends the parser to a whole parse on every delta.
//!
//! cargo run --release -p keystone-ui --example markdown_stream -- --tail [lines] [chunk] [runs] [deltas]

use keystone_ui::markdown::{Document, IncrementalParser};
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn reply(lines: usize) -> String {
    let section = [
        "## Step {n}",
        "",
        "Run **cargo test** with `--locked`, then read [the docs](https://e.x/{n}) and *check* ~~old~~ output.",
        "The parser keeps a long paragraph going across a soft break",
        "so that wrapped lines still belong to one block.",
        "",
        "- first item with `code`",
        "- second item with **bold**",
        "  - nested item",
        "",
        "```rust",
        "fn step_{n}() -> usize {",
        "    {n}",
        "}",
        "```",
        "",
        "| name | value |",
        "|:-----|------:|",
        "| a    | {n}   |",
        "",
        "> A quoted note about step {n}.",
        "",
    ];
    let mut out = String::new();
    let lines = section.iter().cycle().take(lines);
    for (i, line) in lines.enumerate() {
        out.push_str(&line.replace("{n}", &(i / section.len()).to_string()));
        out.push('\n');
    }
    out
}

/// A TypeScript-heavy reply. With `index`, its first interface has an index signature,
/// whose `]:` reads as a possible link reference definition.
fn code_reply(lines: usize, index: bool) -> String {
    let section = [
        "### Module {n}",
        "",
        "The `Store{n}` type keeps **every entry** keyed by id. Read it with `get{n}`, and see",
        "[the guide](https://e.x/{n}) before you change *its* shape.",
        "",
        "```ts",
        "export interface Store{n}<T> {",
        "  entries: Map<string, T>;",
        "  version: number;",
        "}",
        "",
        "export function get{n}<T>(store: Store{n}<T>, id: string): T | undefined {",
        "  const entry = store.entries.get(id);",
        "  if (entry === undefined) {",
        "    return undefined;",
        "  }",
        "  return entry;",
        "}",
        "",
        "export function put{n}<T>(store: Store{n}<T>, id: string, value: T): void {",
        "  store.entries.set(id, value);",
        "  store.version += 1;",
        "}",
        "```",
        "",
        "- `put{n}` bumps **version** on every write",
        "- `get{n}` never throws",
        "",
    ];
    let mut out = String::from("Here is the store module.\n\n```ts\nexport interface Index<T> {\n");
    out.push_str(if index {
        "  [key: string]: T;\n"
    } else {
        "  key: string;\n"
    });
    out.push_str("}\n```\n\n");
    let lines = section.iter().cycle().take(lines.saturating_sub(7));
    for (i, line) in lines.enumerate() {
        out.push_str(&line.replace("{n}", &(i / section.len()).to_string()));
        out.push('\n');
    }
    out
}

fn chunks(text: &str, size: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + size).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        out.push(&text[start..end]);
        start = end;
    }
    out
}

fn whole(chunks: &[&str]) -> (Duration, Document) {
    let started = Instant::now();
    let mut source = String::new();
    let mut last = Document::default();
    for chunk in chunks {
        source.push_str(chunk);
        last = black_box(Document::parse(&source));
    }
    (started.elapsed(), last)
}

fn incremental(chunks: &[&str]) -> (Duration, Document) {
    let started = Instant::now();
    let mut parser = IncrementalParser::new();
    for chunk in chunks {
        parser.append(chunk);
        black_box(parser.display());
    }
    (started.elapsed(), (*parser.document()).clone())
}

fn summary(times: &mut [Duration], deltas: usize) -> String {
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let median = times[times.len() / 2];
    format!(
        "median {:.1} ms (range {:.1} to {:.1}), {:.1} µs per delta",
        ms(median),
        ms(times[0]),
        ms(times[times.len() - 1]),
        median.as_secs_f64() * 1e6 / deltas as f64,
    )
}

/// Per-delta times of the last `deltas` chunks, from a parser primed with the text before
/// them, and how many of those deltas showed a mended display, which parses twice.
fn tail(chunks: &[&str], deltas: usize, parse: bool) -> (Vec<Duration>, usize, Document) {
    let split = chunks.len() - deltas;
    let mut source: String = chunks[..split].concat();
    let mut parser = IncrementalParser::new();
    parser.set_text(&source);
    let mut times = Vec::with_capacity(deltas);
    let mut mended = 0;
    let mut last = Document::default();
    for chunk in &chunks[split..] {
        if parse {
            source.push_str(chunk);
            let started = Instant::now();
            last = black_box(Document::parse(&source));
            times.push(started.elapsed());
        } else {
            let started = Instant::now();
            parser.append(chunk);
            black_box(parser.display());
            times.push(started.elapsed());
            mended += usize::from(!Arc::ptr_eq(&parser.display(), &parser.document()));
        }
    }
    if !parse {
        last = (*parser.document()).clone();
    }
    (times, mended, last)
}

fn tail_main(mut args: impl Iterator<Item = usize>) {
    let lines = args.next().unwrap_or(2_000);
    let size = args.next().unwrap_or(20);
    let runs = args.next().unwrap_or(7);
    let deltas = args.next().unwrap_or(200);
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let with = code_reply(lines, true);
    let without = code_reply(lines, false);
    assert!(with.contains("]:") && !without.contains("]:"));
    let cases = [
        ("whole parse per delta", &with, true),
        ("incremental, `]:` fallback", &with, false),
        ("incremental, no `]:`", &without, false),
    ];
    let first = chunks(&with, size);
    println!(
        "code-heavy reply: {lines} lines, {} bytes, {} top-level blocks, last {deltas} of {} deltas of {size} bytes, {runs} runs each, interleaved",
        with.len(),
        Document::parse(&with).blocks.len(),
        first.len(),
    );
    let mut results: Vec<Vec<(f64, f64, f64, usize)>> = vec![Vec::new(); cases.len()];
    for run in 0..runs {
        for k in 0..cases.len() {
            let k = (k + run) % cases.len();
            let (_, text, parse) = cases[k];
            let chunks = chunks(text, size);
            let (mut times, mended, document) = tail(&chunks, deltas, parse);
            assert_eq!(document, Document::parse(text), "{}", cases[k].0);
            times.sort();
            results[k].push((
                us(times[times.len() / 2]),
                us(times[times.len() * 99 / 100]),
                us(times[times.len() - 1]),
                mended,
            ));
        }
    }
    let range = |values: Vec<f64>| {
        let mut v = values;
        v.sort_by(f64::total_cmp);
        format!(
            "{:.0} ({:.0} to {:.0})",
            v[v.len() / 2],
            v[0],
            v[v.len() - 1]
        )
    };
    println!(
        "µs per delta at the end, median of runs (range): median delta, p99 delta, max delta, mended deltas"
    );
    for (k, (name, _, _)) in cases.iter().enumerate() {
        let r = &results[k];
        println!(
            "{name:28} {:>18} {:>18} {:>18}  {}",
            range(r.iter().map(|x| x.0).collect()),
            range(r.iter().map(|x| x.1).collect()),
            range(r.iter().map(|x| x.2).collect()),
            r[0].3,
        );
    }
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().is_some_and(|a| a == "--tail") {
        args.next();
        return tail_main(args.map(|a| a.parse::<usize>().unwrap()));
    }
    let mut args = args.map(|a| a.parse::<usize>().unwrap());
    let lines = args.next().unwrap_or(2_000);
    let size = args.next().unwrap_or(20);
    let runs = args.next().unwrap_or(7);
    let text = reply(lines);
    let chunks = chunks(&text, size);
    let expected = Document::parse(&text);
    println!(
        "{lines} lines, {} bytes, {} top-level blocks, {} deltas of {size} bytes, {runs} runs each, interleaved",
        text.len(),
        expected.blocks.len(),
        chunks.len(),
    );
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for _ in 0..runs {
        let (time, document) = whole(&chunks);
        assert_eq!(document, expected, "whole parse result");
        a.push(time);
        let (time, document) = incremental(&chunks);
        assert_eq!(document, expected, "incremental parse result");
        b.push(time);
    }
    println!("whole parse per delta: {}", summary(&mut a, chunks.len()));
    println!("incremental:           {}", summary(&mut b, chunks.len()));
}
