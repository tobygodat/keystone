# Search

Two searches share one matcher: find in a session (Ctrl/Cmd+F) and the sidebar's search across sessions (Ctrl/Cmd+Shift+F). Both match plain text and ignore case. There are no patterns, options or replace.

## The matcher

`keystone_core::search::Query` is what the user typed, folded once. Case is ignored by each character's lowercase form (`É` finds `é`; `ß` does not find `ss`). A query folds the same wherever a character stands, unlike `str::to_lowercase`, which treats a final `Σ` differently. `Query::find` returns byte ranges of the text, in order, never overlapping, and never ending inside a character whose lowercase form is longer than one character (`İ`). A test compares it with a brute-force reference on 3,000 generated texts.

The loops run inside `aho-corasick` and `memchr`. Workspace crates build unoptimized in debug builds, and the standard library's generic `str::find` and `to_ascii_lowercase` compile into the caller at its level, so a matcher on them took 1.6 s to scan 327 MB. A query of ASCII uses an ASCII-insensitive automaton, exact unless the text holds one of the few other characters that lowercase to its letters, such as the Kelvin sign. Those, and every other query, go through a folded copy of the text, and only texts holding the query's first non-ASCII character in some case get one.

`search::snippet` cuts the line around a match to one row: at most 24 characters before it, cut at a word, runs of whitespace as one space.

## Find in a session

- **Where it reads.** `Conversation::rows` holds the session's whole saved conversation once its history has loaded, so find reads rows, not the virtualized list. Each row's searched texts are the ones it draws, in drawing order (`find::row_texts`): a message's text, a reply's Markdown texts as `Document::texts` lists them, a reasoning summary, a command line and its output, a tool's text. Edit rows, widgets, attachments, notes, turn errors and recovery reports are not searched: only what the row draws as text can be marked.
- **Shapes.** A `Match` is a row, which of its texts, a byte range, and whether that text shows only while the row is open. A `FindBar` holds its field, its matches in transcript order, the current index, where focus was when it opened, and the message a sidebar result asked to land on. `find::Index` caches each session's rows' texts with a stamp (item ID, text length, status), so a longer query reads no row again; a test checks that the second keystroke in a 5,100-row session reads 0 rows.
- **Off the UI thread.** The UI stamps rows and hands over the cached texts and the rows that changed; parsing replies and matching run on a background task. A newer search supersedes an older one by generation.
- **Where the current match goes.** A new query goes to the first match at or below the top of the view and shows it. Enter and Shift+Enter step and wrap. A match in a collapsed row opens it. Showing a match puts its row at the top, then, once drawn, the text records where the match was laid out (`MarkSlot`) and the list scrolls it a third of the way down. That works in replies taller than the window. Rows that change, such as a streaming reply, are searched again 150 ms after they settle, keeping the current match and never scrolling.
- **Marks.** `keystone_ui::Mark` washes every match in `find_match` and the current one in `find_current`, from the theme. A streaming reply draws less than its text so far; marks past what is drawn wait.
- **Keys.** Find in session and Close find are rows in the command table, so both are rebindable. Enter and Shift+Enter are read from the keystroke in the bar. In a terminal pane on Linux, Ctrl+F stays the shell's, as every Ctrl+letter does; on macOS Cmd+F opens find.

## Search across sessions

- **What it reads.** The `log_entries` projection, newest first, through `Database::scan_logs` on a read-only connection. That covers every session in the workspace: settled ones, ones whose history is not loaded, and sessions on other machines, whose logs are saved locally. Nothing reads a file. Only user and agent messages are searched; the sidebar finds what was said. A reply still streaming is found once it is saved.
- **Shapes.** `keystone_agent::search::messages` returns, for each session, its newest matching message as a `MessageHit`: the session, the message's item ID and a snippet. Before parsing an entry it checks the entry's kind near its start and the query in the saved JSON (skipped for a query with a quote, backslash or control character, which JSON escapes), so only matching messages are parsed.
- **In the sidebar.** The field still filters titles as the user types. Message results list under the title matches, newest match first, under **In messages**, skipping sessions whose titles matched and keeping the project and provider filters. While a query is typed, the list runs top to bottom with no spacer above Settled. An empty query is the plain sidebar. Each keystroke starts a scan on a background task and stops the one before, which checks every 1,024 rows.
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

The UI thread only stamps rows and hands their texts over; the first query of a session clones its rows, which is the gap between 0.3 and 0.6 ms. A first search costs twice a later one because it parses every reply's Markdown (5,100 rows read against 0). The first query in a run also builds the matcher's case table, about 40 ms, on the background task.

## Limits

- Matches in edit rows, widgets and attachments are not found.
- A delivered result's row is searched as the reply it draws.
- The sidebar lists one message per session; find shows the rest once it opens.
