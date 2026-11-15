---
name: cursor-model
description: Model-check how Keystone saves what a remote machine's host sends and when it lets the host trim its journal (Link::saved, State::ack, State::adopt, Logs::append_through, Database::append_logs, a session's unfinished items saved with its cursor, the stamps that tell a journal that went back, the replay after a kill or a replaced journal) with TLA+. Use before merging any change to how a remote session's events are received, drained, saved, acknowledged or replayed, and when a lost or doubled remote row or streamed text needs every order of kills, drops, saves and journal replacements checked.
---

# Cursor model

A remote machine's host keeps every event in a journal until the app acknowledges it, then trims it. The app acknowledges an event only once the log rows it adds are saved, and saves each session's place (its cursor) in the same transaction as those rows. An event that adds no row is a part of an unfinished item, such as a streamed delta or a tool that is still running: the app saves the session's unfinished items with its cursor too, in the same transaction, so a part is acknowledged only once they are saved (TOB-453). A place is a journal's identity, a number in it and the stamp of the daemon run that gave the number, since a journal can be replaced (the machine's state wiped, `keystone-host` reinstalled, or the SSH name reaching another machine) and the new one numbers from 1 again, and a journal restored from a backup gives numbers again under the same identity, with a new run's stamp (TOB-455). After a kill, the relaunch puts the saved unfinished items back and drops replayed events at or below each session's saved cursor in the same journal. `RemoteCursor.tla` models that, and TLC checks every order of host appends, receives, drains, saves, reports, acknowledgements, trims, transport drops, journal replacements, kills and relaunches within the bounds below. A test checks only the orders someone wrote down.

Run all commands from the workspace root.

## The rules

| Rule | In plain words | The harm if it breaks |
|---|---|---|
| `NothingLost` | The host trims an event only once the app has saved what it added: a row in the saved log, a part in its session's saved unfinished items or in the saved row that completed its item. In every journal. Events a replaced journal took with it were lost by the host, not trimmed, and the app ends their turn with a note. | A kill of the app, a Quit that ends the turn, or a replaced journal, at the wrong moment loses transcript rows or streamed text for good: the host trimmed them and the app never saved them. |
| `SavedOnce` | Each row is saved once, and each part is applied once, however often the app is killed and its events replayed: an unfinished item holds no part twice, nor one whose item a saved row already completed. | A relaunch shows a reply, a turn end or a streamed word twice. |
| `SavedInOrder` | Each session's rows are saved, and its parts applied, in the order the host numbered them: by journal, then by number within one. | A restore shows rows or streamed text out of order after a kill. |

## When to use

Use it when a change touches any function in the coverage table, or when this shows anything:

```sh
bash crates/keystone-desktop/.agents/skills/cursor-model/check.sh --drift
```

It lists the covered files changed since the sync commit, committed or not, the covered functions with changed lines, and untracked Rust files in the covered folders. The function list is a guide, not a proof. Read the whole diff it prints a command for.

## Run

```sh
bash crates/keystone-desktop/.agents/skills/cursor-model/check.sh
```

It needs `mise` and, on the first run, the network: it fetches TLC 1.7.4 (checksum-pinned) into `~/.cache/keystone/` and Java through `mise` (`java@temurin-21`), as the quit and restore models do. It keeps TLC's state files under `~/.cache/keystone/` too, since they don't fit a small `/tmp`. It runs thirty-one TLC jobs, `CURSOR_MODEL_JOBS` at a time (8 by default, each with two workers), in about 6 minutes at 9, prints one line per run, and exits 0 only when every result matches what it expected:

- `code` runs: each rule on the code as it is, under each set of bounds. All must hold.
- `finding` runs: a known way the code breaks a rule, filed as an issue. It must keep breaking until the issue is fixed; then its line becomes a `code` line. There are none now: `grows`, TOB-455's finding, holds since stamps.
- `can fail` runs: each rule with a deliberate break (`Mutation` in the model). Each must break. If one holds, the rule has stopped checking anything.
  - `ack-on-drain` acknowledges what a connection drained rather than what was saved. That is how the app worked before TOB-319, and `NothingLost` catches it.
  - `cursor-apart` saves a session's cursor in a transaction of its own, before its rows. A kill between the two leaves a cursor ahead of the rows, and `NothingLost` catches it.
  - `no-skip` forgets the saved cursors at launch, so events saved before a kill but not yet acknowledged are saved again. `SavedOnce` and `SavedInOrder` catch it.
  - `cross-journal` compares places by number alone, as the first build of TOB-319 did: it lowers them to the host's `latest` on a welcome, keeps `after` across journals, and only ever raises the saved cursor. `NothingLost` catches it: a replaced journal's new events, numbered from 1, are dropped as saved and trimmed.
  - `lossy-crash` lets a host crash or power loss take back commits the daemon had already sent, as `PRAGMA synchronous = NORMAL` did before TOB-319's third round. The daemon that starts next numbers its restart events over events the app has saved, the app drops them as saved, and `NothingLost` catches it.
  - `no-rotate` lets a journal that went back keep its identity. The link forgets its places for the run, but the saved cursors in the database keep that identity, so once the journal grows past them a later launch drops its new events. `NothingLost` catches it.
  - `no-stamp` compares numbers alone, as the code before TOB-455 did, in the `grows` bounds: a journal that went back and numbered new events up to the app's place before the app reconnected has nothing behind that place, keeps its identity, and the app drops the new events as saved. `NothingLost` catches it. With stamps, the number at the app's place has another run's stamp, and the host takes a new identity.
  - `lossy-crash` lets a host crash or power loss take back commits the daemon had already sent, as `PRAGMA synchronous = NORMAL` did before TOB-319's third round, and compares numbers alone. The daemon that starts next numbers its restart events over events the app has saved, the app drops them as saved, and `NothingLost` catches it. With stamps the host would catch even that, as it catches a restored backup, so the break turns both off.
  - `late-route` leaves one session's route unregistered while its events wait to be drained, as an app that made a route's connection only when it next sent, rather than when it made the link, would at a launch that replays that route's turn. The acknowledgement passes those events, the host trims them, and a kill loses them. `NothingLost` catches it.
  - `ack-unsaved-part` gives a part's place as soon as it is applied, without saving the unfinished items, as the code before TOB-453 did. The host trims a delta whose text the app never saved, and a kill or a Quit that ends the turn loses it. `NothingLost` catches it.
  - `parts-apart` saves the unfinished items in a transaction of their own that leaves the cursor where it was. A kill between that save and the next one that moves the cursor leaves items holding parts past the cursor, and the replay applies those parts again. `SavedOnce` catches it.
  - `no-restore` leaves the saved unfinished items out at launch. The next save of the session's items no longer holds parts the host trimmed. `NothingLost` catches it.
  - `report-unsynced` reports a save's place even when the directory barrier after its commit failed, as an items-only save did in TOB-453's first round. The host trims what the place covers, and a power cut on the app's machine takes the commit back. `NothingLost` catches it.

`check.sh --trace RULE MUTATION BOUNDS` prints one run's full TLC output. `RemoteCursor.cfg` is the code as it is, with every rule, for reading a run by hand.

## Read a result

Read every TLC result with the `/statechart` skill when you can. It turns a violation into its shortest path in plain words. If you can't invoke it, the trace in TLC's output is enough for a verdict.

For each step of a failing path, find the code in the coverage table and check that the code really takes that step in that order:

- **A real bug:** every step is something the code does. Report it in the code's words, one step per line, naming the function that takes each step. Do not weaken the rule or the model to make it pass. In committed code, file it as an issue. In a change under review, the change breaks the rule: say so, with the path, and don't call it ready to merge. `remote_tests.rs` (`a_kill_between_receiving_and_saving_loses_nothing`) shows how to replay a path with a real kill.
- **A modeling error:** a step the code cannot take, or two steps that are one in the code. Fix the model to match the code, cite the code in a comment, and rerun.

TLC errors other than a violation (parse errors, `TypeOK` failing) are always modeling errors. Deadlock checking is off (`-deadlock`): once every event is saved and acknowledged nothing more happens, which is correct.

## Update the model after an intended change

`git checkout -- crates/keystone-desktop/.agents/skills/cursor-model` restores the committed model. A model edit goes in the same commit as the code change it mirrors.

1. **Compare the change to the breaks.** If the code now does what a `Mutation` does, the change very likely breaks that mutation's rule.
2. **Mirror the change.** Find the action for each changed function in the coverage table and make it do what the code now does, in the same order.
3. **Run `check.sh` and judge it** as in "Read a result". If a `code` run breaks and the path is real, stop and report it. Do not touch the expected column in `EXPECT`.
4. **Keep each break meaningful.** If the code now matches a mutation, replace it with a different break of the same rule, in the model's `Mutations` and in `EXPECT`.
5. **New rules** get a sentence and their harm in the table above, a `code` line and a `can fail` line in `EXPECT`.
6. **Move the sync commit** (`SYNC` in `check.sh` and below) only after you have read the covered diff and confirmed the model describes it.

## Coverage

Sync commit: `ef85fd11c`. Paths are under `crates/`. Find each function by name.

| Model | Code |
|---|---|
| `jid`, `latest`, `journal`, `acked` | the host's journal: its `identity` row (`Journal::id`, made with the file), `clients.latest`, the `events` table, `clients.acked` (`keystone-agent/src/remote/journal.rs`) |
| `Reset` | the journal file replaced: `Journal::open` on a new file makes a new identity, and the daemon and its transport end |
| `ljournal`, `Connect` | `State.journal` and `State::adopt`, run by `Shared::resend` on every welcome: another journal, or the same one behind a place, forgets every place. The daemon ignores an `after` from another journal (`serve.rs`, on `Frame::Hello`) |
| `Crash`, `sentTo`, `crashes` | a host crash or power loss: the daemon and its provider die, and only commits the journal never sent may be gone. That is the assumption the model rests on. The code makes it true as far as the disk keeps what a sync says it keeps: the journal commits with `synchronous = FULL`, and with `fullfsync` on so that macOS syncs through the drive's cache (`journal.rs` `SYNCHRONOUS`, `Journal::open`), and the daemon delivers an event's frame only after `Journal::append` committed it (`serve.rs` `journal_events`). The restart events, and the turn ends the host appends when it lets go of a connection or Stop finds a turn no connection runs (`serve.rs` `let_go`, `end_orphan`), are numbered through the same path. The app's own saves are modelled by `Save`'s barrier and `PowerCut` below, which assume a save whose barrier succeeded survives a power cut; that holds on Linux and not yet on macOS (TOB-463) |
| `Rewind`, `era`, `born`, `fresh` | a journal restored from a backup or damaged: the same identity, numbers back at `b`. `born[n]` is the stamp of number `n`: the daemon run that gave it (`Journal::open` draws `Journal.stamp`; `stamp_from` records it in the `stamps` table from the run's first number for the client, and `Journal::stamp` reads it back). A rewind or a crash starts a new run, and a restored backup's events keep the stamps they had, which `era` stands for; `fresh` keeps it from numbering new events before the app connects, except in the `grows` bounds |
| `Seen`, `Rotates`, `recvStamp` | `State::seen`, sent in `hello`: every place the link has, received and saved, each with its stamp (`State.received_stamp`, `Place.stamp`). The daemon's `went_back` and `Journal::rotate`, which give a journal a new identity when a place the app shows is past its latest number, or past its acknowledged cursor with another stamp than `Journal::stamp` gives that number; `no-stamp` compares numbers alone |
| `HostAppends` | the daemon journals an event before it sends it, live once the replay is done (`remote/serve.rs`, `Journal::append`) |
| `wire`, `Receive`, `received`, `inbox` | frames read on the link's thread, `Shared::route` with `State.received` and `RouteState.inbox` (`remote/link.rs`) |
| `savedMem` | `State.saved`, seeded by `Link::with_cursors` and raised by `Link::saved` |
| `Drain(s)`, `unsaved` | `RemoteConnection::drain_placed` (`remote/connection.rs`) and `Shared::drain`, which adds to `State.unsaved`; the pump hands each event and its place over in order and commits the session (`keystone-desktop/src/app_state.rs` `pump`, `receive_placed`, `commit`), which queues the rows and the place with `Logs::append_through` (`keystone-agent/src/log.rs`). Each session is on a route of its own, drained apart, as a Claude and a Codex session or two logins on one machine are. Every route has its connection before the link reads a frame: `open_link` makes the link with `Link::prepared`, registers a connection for each route, then calls `Link::start`, and `remote_connection` makes a new route's connection before its first call; `late-route` breaks that, and `remote_tests.rs` (`a_links_routes_are_registered_before_it_reads_its_first_frame`) fails if the code starts the link first |
| `open`, `dirty`, `handed`, `Flush` | a session's unfinished items as the conversation holds them (`Conversation::take_unfinished`, the provider's items still in progress since this run began) and whether they changed (`Conversation::unfinished_changed`, set by `apply` for `ItemStarted`, `ItemDelta`, `ItemCompleted` and by `settle_unfinished`); `AppState.handed`. `AppState::commit` takes the items with its place when it has rows to write, when `UNFINISHED_EVERY` passed since they were last taken, or at Quit (`flush_logs` sets `unfinished_now`), and otherwise gives no place and marks the session waiting (`unfinished_waiting`); `arm_unfinished`'s timer commits a waiting session once that time has passed, which is `Flush` |
| `queue`, `Save` | `PendingSession.writes`, `PendingSession.through` and `PendingSession.unfinished`; the writer's `Files::retry` and `PendingSession::retry`, which call `Database::append_logs` (`keystone-core/src/database.rs`) with the place and the items, in a transaction of its own when there are no rows, then `Files::through`, which reports `Report::Through` only once both are saved |
| `ok`, `unsynced`, `Barrier`, `durable`, `PowerCut` | whether the directory barrier after a save's commit succeeded (`Database::append_logs` returning `Durability::Synced` or `DirectorySyncPending`). On a failure the commit stands and the place waits: the writer keeps the last row write, or the unfinished items with `PendingSession.unfinished_unsynced`, so `Files::through` reports nothing, and the next retry or `Logs::flush` runs the barrier again (`SessionLog::sync`), which is `Barrier`. `durable` is what a power cut on the app's machine leaves of the database: everything committed up to the last barrier that succeeded. `PowerCut` is that cut, and `Launch` makes everything committed durable, since `Database::open` syncs the folder before anything reads it |
| `log`, `cursor`, `unfinished` | the `log_entries` and `remote_cursors` tables, with `remote_cursors.stamp` and `remote_cursors.unfinished` |
| `done` | a ghost: the parts whose item a saved row completed. `Launch` leaves such an item out, which `Conversation::restore_resuming` does by its ID |
| (outside the model) | the answer the link makes from a reply, `Event::Taken` or `Event::NotSent`, is the app's own event with no place, as a call's error is, so it never holds or moves the acknowledgement. The `Taken` the host journals as it takes a message (`serve.rs` `Daemon::perform`) is an ordinary event with a place, appended through the same `enqueue` → `journal_events` → `append` path as a turn's events, so the model's events already cover it; the app applies the second `Taken` for a message as a no-op. A pending message lives in the session's record (`Session::pending`) and its answer in the host's `sends` table, neither of which the cursors read; Send again under a refused message's ID makes `Journal::accept` drop that refused record and run it, which touches only that table. An `unrecorded` refusal is still the link's own event with no place; it only carries that the message may already have run. Discard drops the link's unanswered call for the message (`link.rs` `Shared::abandon`), which moves only the calls half of `ack`, never the events cursor |
| `Report` | `receive_placed` on `Report::Through`, calling `Link::saved` |
| `SendAck`, `sentAck`, `AckValue` | `State::ack` and `State.sent_ack` |
| `HostAck` | the daemon on `Frame::Ack`, `Journal::ack` |
| `Drop`, `Connect`, `linked` | `Shared::connect_once` ending and starting again with `hello` carrying `after` once `State.linked`; `Shared::resend`; the daemon's `replay` from the larger of `after` and `acked` |
| `Kill`, `Launch` | SIGKILL of the app; `AppState::new` reading `Database::remote_cursors`, each with its journal, stamp and unfinished items, `open_link` giving the places to `Link::with_cursors`, and `receive_placed` giving the items to `Conversation::restore_resuming` when the session's history loads; `no-restore` leaves them out |
| `trimmed` | a ghost: what `HostAck` deleted. A turn whose events a replaced journal took is ended by `AppState::end_lost_turns`, which the model leaves out |

## Scope

Six sets of bounds, since their product is too large to explore (over 9 minutes and 7 GB at three events, one reset, one kill and one drop, without finishing). Each checks every combination of sessions and rows:

- `kills`: one journal, four events, up to two kills and one drop. About 10 million distinct states.
- `reset`: two journals of two events each, one replacement of the journal at any point, one kill and one drop.
- `rewind`: two events, the journal going back once at any point, one kill and one drop, with no new event before the app next connects. About 14 million.
- `grows`: as `rewind`, but the journal may number new events before the app connects, as its own restart events can. The stamps catch it; `no-stamp` shows what they cost to leave out.
- `crash`: two events, one host crash or power loss at any point, then the restarted daemon numbering its own events at once.
- `barrier`: three events, one save whose directory barrier fails after it commits, one kill and one power cut on the app's machine, and one drop. About 1.9 million.

A pass covers the model, not all of Keystone.

Assumed, so a break there would not show:

- Every row an event adds is taken by the commit right after it is applied (`receive_placed` commits after each event). A row an event caused later, from another update, would be saved with a later place. For a remote session nothing adds one: checkpoints, the only such writer, are off there.
- Every event that adds no row is a part, which holds the acknowledgement until the unfinished items are saved. In the code an event that changes no unfinished item, such as a usage report, gives its place at once, as a `Flush` that saves the same items would; the model doesn't take that step apart.
- A row completes every unfinished item of its session. In the code it completes its own item, or a turn's end settles them all, and the other items stay unfinished; but a commit with a row always takes the items as they then are, with the row's place, so what is saved is the same either way.
- An item's ID is the provider's own and unique across turns, which `restore_resuming` relies on to leave out an item a saved row holds.
- A loading conversation holds its events until its history is restored, and `commit` gives no place meanwhile, so the model's drain stands for the pump after the load.
- The log writer saves a session's queue whole or not at all; its retries are one `Save` that may be delayed forever, and its directory barrier (`docs/design/storage.md`) is `Save`'s outcome and `Barrier`. The store under it honours sync requests.
- Two routes on the machine, one per session, each with its connection registered before the link reads a frame (`AppState::open_link` registers every provider and account the machine's sessions run on between `Link::prepared` and `Link::start`, and `remote_connection` makes a new one before its first call). Events for a route with no connection don't hold the acknowledgement back by design, and are not covered by `NothingLost`; `late-route` shows what that would cost. Two sessions on one route are the case where one pump drains both at once.

Left out:

- The journal's cap, which drops item events past 64 MiB of unacknowledged events and records a gap. That loss is designed and shown in the session.
- Ending a turn a replaced journal took with it (`end_lost_turns`); `remote_tests.rs` covers it.
- Calls and their replies, which share `ack` but have their own rule (`docs/design/remote.md`, "Calls run at most once").
