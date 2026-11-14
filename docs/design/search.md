# Search

Two searches share one matcher: find in a session (Ctrl/Cmd+F) and the sidebar's search across sessions (Ctrl/Cmd+Shift+F). Both match plain text and ignore case. There are no patterns, options or replace.

## The matcher

`keystone_core::search::Query` is what the user typed, in its searched form. Both the query and the text are compared in that form, built one grapheme cluster at a time: decomposed (NFD), case folded by Unicode's full case folding (`unicase`, from `CaseFolding.txt`), a dot above dropped when it follows an `i` or a `j`, which already have one, then composed (NFC, `icu_normalizer`). So `ΟΔΟΣ` finds `οδος`, `σ` finds `ς`, `ss` finds `ß`, `istanbul` finds `İstanbul`, and a decomposed `é` finds a precomposed one. A precomposed letter is not its base letter: `cafe` does not find `café`. Because a dot above after `i` or `j` drops out, a typed `i` followed by U+0307 matches every `i`, dotted or not, while Turkish dotless `ı` stays its own letter and matches only itself. Both crates were already in the build, through `pulldown-cmark` and `url`.

`Query::find` returns byte ranges of the original text, in order and never overlapping. A match found inside a cluster's form widens to the whole cluster, so `👩` marks a whole family emoji and a mark never splits what draws as one character. A test compares every path below with a brute-force reference on 6,000 generated texts of the characters that stress them.

The loops run inside `aho-corasick` and `memchr`. Workspace crates build unoptimized in debug builds, and the standard library's generic `str::find` and `to_ascii_lowercase` compile into the caller at its level, so a matcher on them took 1.6 s to scan 327 MB. A query whose searched form is ASCII uses an ASCII-insensitive automaton on the text itself. That is exact unless the text holds a screen, a character whose searched form holds one of the query's letters (the Kelvin sign for `k`, `ß` for `s`, a dot above for `i`), or a hit starts or ends inside a cluster, which a grapheme check catches where the neighbouring byte is not ASCII. A text whose screens are each a cluster of their own is searched with their forms spliced into a copy, which `memchr` finds one screen at a time. Every other query is searched in the text's searched form, and only texts holding one of the characters that can yield the query's rarest non-ASCII character get one.

Building a text's searched form cluster by cluster through normalization costs three normalizations and several allocations per cluster, which made non-ASCII search 3 to 18 times slower than lowercasing. `Folded::new` builds the same bytes from a per-character table instead: each character's form, whether it always ends its grapheme cluster when its neighbour does too, and whether its form and its decomposed fold are itself. Runs of ASCII fold in bulk, such characters one at a time, and only clusters holding marks, joiners or changing characters go to the segmenter and the full computation. A test checks the table's form against the cluster-by-cluster one on 20,000 generated texts. The tables are built once, on the first query, on the background task: about 40 ms in a release build and 230 ms in a debug one.

`search::snippet` cuts the line around a match to one row: at most 24 columns before it, a wide character such as a CJK one counting two, cut at a word, runs of whitespace as one space.

## Find in a session

- **Where it reads.** `Conversation::rows` holds the session's whole saved conversation once its history has loaded, so find reads rows, not the virtualized list. Each row's searched texts are the ones it draws, in drawing order (`find::row_texts`): a message's text, a reply's Markdown texts as `Document::texts` lists them, a reasoning summary, a command line and its output, a tool's text, and an edit's label with its paths, then its preview's file names and up to 40 lines. Widgets, attachments, notes, turn errors and recovery reports are not searched: only what the row draws as text can be marked. An open edit marked by find draws its preview in the row rather than from its cached view.
- **Shapes.** A `Match` is a row, which of its texts, a byte range, and whether that text shows only while the row is open. A `FindBar` holds its field, its matches in transcript order, the current index, where focus was when it opened, and the message a sidebar result asked to land on. `find::Index` caches each session's rows' texts with a stamp (item ID, text length, status), so a longer query reads no row again; a test checks that the second keystroke in a 5,100-row session reads 0 rows.
- **Off the UI thread.** The UI stamps rows and hands over the cached texts and the rows that changed; parsing replies and matching run on a background task. A newer search supersedes an older one by generation.
- **Where the current match goes.** A new query goes to the first match at or below the top of the view and shows it. Enter and Shift+Enter step and wrap. While a new query's search runs, the count is hidden and Enter, Shift+Enter and the arrows do nothing, so they never step through the older query's matches; the search shows its first match when it lands. A match in a collapsed row opens it. Showing a match puts its row at the top, then, once drawn, the text records where the match was laid out (`MarkSlot`) and the list scrolls it a third of the way down. That works in replies taller than the window. Rows that change, such as a streaming reply, are searched again 150 ms after they settle, keeping the current match and never scrolling.
- **Marks.** `keystone_ui::Mark` washes every match in `find_match` and the current one in `find_current`, from the theme. A streaming reply draws less than its text so far; marks past what is drawn wait.
- **The bar.** It spans the conversation column with a 16 px inset and is at most 340 px wide, so at the column's 320 px minimum it narrows rather than covering the sidebar. The count sits in a slot as wide as "9999 of 9999" in `TextSize::Sm`, right-aligned, so neither stepping nor typing moves the field.
- **Keys.** Find in session and Close find are rows in the command table, so both are rebindable. Enter and Shift+Enter are read from the keystroke in the bar. In a terminal pane on Linux, Ctrl+F stays the shell's, as every Ctrl+letter does; on macOS Cmd+F opens find.

## Search across sessions

- **What it reads.** The `log_entries` projection, newest first, through `Database::scan_logs` on a read-only connection. That covers every session in the workspace: settled ones, ones whose history is not loaded, and sessions on other machines, whose logs are saved locally. Nothing reads a file. Only user and agent messages are searched; the sidebar finds what was said. A reply still streaming is found once it is saved.
- **The same text as find.** Each message is searched as the texts its row draws, which the desktop gives the scan: a user message as typed, a reply's Markdown as `Document::texts` lists it, and a delivered result as the reply it holds, recognized as find recognizes it, through a snapshot of the delivered results taken on the UI thread. So a link's address, the asterisks around a bold word and a result's "Result from session" wrapper are found by neither search, and the snippet shows the drawn text.
- **Shapes.** `keystone_agent::search::messages` returns, for each session, its newest matching message as a `MessageHit`: the session, the message's item ID and a snippet. Before parsing an entry it checks the entry's kind near its start, then screens its saved JSON by the query's longest run without whitespace, ASCII punctuation or control characters, so only likely matches are parsed. Markdown's syntax is ASCII punctuation and whitespace that the drawn text drops, and JSON escapes only quotes, backslashes and control characters.
- **In the sidebar.** The field still filters titles as the user types. Message results list under the title matches, newest match first, under **In messages**, skipping sessions whose titles matched and keeping the project and provider filters. While a query is typed, the list runs top to bottom with no spacer above Settled. An empty query is the plain sidebar. Each keystroke starts a scan on a background task and stops the one before, which checks every 1,024 rows. Until the new scan lands, the older query's results fade to 40% and open nothing, and Up and Down skip them. Enter pressed meanwhile opens the new query's first result once it lands, but only if nothing came between: another key, a press anywhere, or focus leaving the field drops it, and it opens only while the same session is selected.
- **Opening a result.** Enter (on the highlighted result, or the first) or a click selects the session and opens its find bar on the same words. The bar lands on the hit's message once the session's history has loaded and been searched.

## Why a scan and not FTS5

Measured on a generated workspace of 500 sessions with 127,024 log entries (327 MB, one session of 5,000 entries), debug build, five runs each (`crates/keystone-agent/tests/perf_search.rs`):

| | median | range |
| --- | --- | --- |
| Reading every row, no matching | 76 ms | 75.6–76.7 |
| Scan, rare word | 99.7 ms | 99.4–99.9 |
| Scan, word absent | 86.4 ms | 86.2–86.5 |
| FTS5 trigram, rare word | 4.2 ms | 4.2–4.6 |
| FTS5 trigram, common word | 11.0 ms | 10.9–11.3 |

The scan is bound by SQLite reading the rows. FTS5 answers in a tenth of the time but costs 69 µs per message insert on the log's writer, a 234 MB index for 57,455 messages, a schema migration with a backfill of about 4 s on first open, and no index for queries under three characters. The scan's results arrive about 0.1 s after a keystroke on a workspace far larger than any seen, and the title filter stays instant, so the scan is the simpler choice that meets the budget. Revisit it when real workspaces approach that size.

## How fast it is

`cargo test -p keystone-desktop --features ui-tests --locked bench_ -- --ignored --nocapture --test-threads=1` measures both in the real view, debug build, five runs each (median and range):

| | median | range |
| --- | --- | --- |
| A keystroke in find, on the UI thread, 5,100 rows | 0.64 ms | 0.28–1.25 |
| Find's background search, first query (every row read) | 5.1 ms | 5.1–10.6 |
| Find's background search, a longer query (no row read again) | 2.7 ms | 2.6–3.1 |
| A frame with the bar and 100 marks | 1.6 ms | 1.6–1.9 |
| The sidebar's scan, 500 sessions of 60 messages | 27.6 ms | 27.1–28.1 |
| A sidebar refresh listing 250 message results | 2.3 ms | 2.2–2.3 |

The UI thread only stamps rows and hands their texts over; the first query of a session clones its rows, which is the gap between 0.3 and 0.6 ms. A first search costs twice a later one because it parses every reply's Markdown (5,100 rows read against 0). The first query in a run also builds the matcher's tables, about 230 ms in this debug build, on the background task.

The matcher in four languages, release build, five runs each (`perf_matcher_in_languages` and `perf_sidebar_scan_in_languages` in `perf_search.rs`), every match in a 1,500-row session's texts and in a 525-session workspace's 31,500 drawn message texts, against main before TOB-462:

| Query | Session, main | Session, now | Workspace, main | Workspace, now |
| --- | --- | --- | --- | --- |
| French `café` | 2.8 ms | 1.3 ms | 67 ms | 34 ms |
| French `CRÈME` | 3.0 ms | 1.4 ms | 74 ms | 36 ms |
| German `strasse` | 0.59 ms | 1.36 ms | 18 ms | 34 ms |
| Japanese `検索` | 3.2 ms | 0.70 ms | 79 ms | 18 ms |
| English `s` | 2.0 ms | 1.2 ms | 41 ms | 29 ms |
| English `search` | 0.74 ms | 0.52 ms | 21 ms | 13 ms |

German is the one slower than main: its generated text has a `ß` in every third word, and `strasse` now finds `Straße`, three times the matches, through a pass for the screens and a search of the spliced copy where main made one pass. The sidebar's whole scan of that workspace, each screened reply parsed as Markdown, takes 19 to 25 ms for those queries, and 40 ms for a German or Japanese word no message holds (354 and 348 ms before this pass).

The sidebar's work for each listed session (`Keystone::listed`) runs only when the sidebar rebuilds, not on a composer keystroke. Typing 225 characters into the composer of a 525-session workspace in release builds took a median 575 ms (565–591, 320 ms of the app's CPU) before find and search and 578 ms (564–590, 320 ms) with them, five alternating runs each, launched without the verification driver's geometry so cached views stay cached.

## Limits

- Matches in widgets and attachments, and in an edit's hunk headers and the lines past its preview, are not found. The diff view and Changes show the whole edit.
- The sidebar screens a message by the query's longest word, so a word split by Markdown inside it, such as `foo**bar**`, or written as an entity is found by find but not by the sidebar.
- The sidebar lists one message per session; find shows the rest once it opens.
