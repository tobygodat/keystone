---
name: cursor-model
description: Model-check how Keystone saves what a remote machine's host sends and when it lets the host trim its journal (Link::saved, State::ack, State::adopt, Logs::append_through, Database::append_logs, the replay after a kill or a replaced journal) with TLA+. Use before merging any change to how a remote session's events are received, drained, saved, acknowledged or replayed, and when a lost or doubled remote row needs every order of kills, drops, saves and journal replacements checked.
---

# Cursor model

A remote machine's host keeps every event in a journal until the app acknowledges it, then trims it. The app acknowledges an event only once the log rows it adds are saved, and saves each session's place (its cursor) in the same transaction as those rows. A place is a journal's identity and a number in it, since a journal can be replaced (the machine's state wiped, `keystone-host` reinstalled, or the SSH name reaching another machine) and the new one numbers from 1 again. After a kill, the relaunch drops replayed events at or below each session's saved cursor in the same journal. `RemoteCursor.tla` models that, and TLC checks every order of host appends, receives, drains, saves, reports, acknowledgements, trims, transport drops, journal replacements, kills and relaunches within the bounds below. A test checks only the orders someone wrote down.

Run all commands from the workspace root.

## The rules

| Rule | In plain words | The harm if it breaks |
|---|---|---|
| `NothingLost` | The host trims an event that adds a log row only once that row is in the app's saved log, in every journal. Events a replaced journal took with it were lost by the host, not trimmed, and the app ends their turn with a note. | A kill of the app, or a replaced journal, at the wrong moment loses transcript rows for good: the host trimmed them and the app never saved them. |
| `SavedOnce` | Each row is saved once, however often the app is killed and its events replayed. | A relaunch shows a reply or a turn end twice. |
| `SavedInOrder` | Each session's rows are saved in the order the host numbered them: by journal, then by number within one. | A restore shows rows out of order after a kill. |

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

It needs `mise` and, on the first run, the network: it fetches TLC 1.7.4 (checksum-pinned) into `~/.cache/keystone/` and Java through `mise` (`java@temurin-21`), as the quit and restore models do. It keeps TLC's state files under `~/.cache/keystone/` too, since they don't fit a small `/tmp`. It runs twenty TLC jobs in parallel in about 3 minutes, prints one line per run, and exits 0 only when every result matches what it expected:

- `code` runs: each rule on the code as it is, under each set of bounds. All must hold.
- `finding` runs: a known way the code breaks a rule, filed in Linear. It must keep breaking until the issue is fixed; then its line becomes a `code` line.
- `can fail` runs: each rule with a deliberate break (`Mutation` in the model). Each must break. If one holds, the rule has stopped checking anything.
  - `ack-on-drain` acknowledges what a connection drained rather than what was saved. That is how the app worked before TOB-319, and `NothingLost` catches it.
  - `cursor-apart` saves a session's cursor in a transaction of its own, before its rows. A kill between the two leaves a cursor ahead of the rows, and `NothingLost` catches it.
  - `no-skip` forgets the saved cursors at launch, so events saved before a kill but not yet acknowledged are saved again. `SavedOnce` and `SavedInOrder` catch it.
  - `cross-journal` compares places by number alone, as the first build of TOB-319 did: it lowers them to the host's `latest` on a welcome, keeps `after` across journals, and only ever raises the saved cursor. `NothingLost` catches it: a replaced journal's new events, numbered from 1, are dropped as saved and trimmed.
  - `lossy-crash` lets a host crash or power loss take back commits the daemon had already sent, as `PRAGMA synchronous = NORMAL` did before TOB-319's third round. The daemon that starts next numbers its restart events over events the app has saved, the app drops them as saved, and `NothingLost` catches it.
  - `no-rotate` lets a journal that went back keep its identity. The link forgets its places for the run, but the saved cursors in the database keep that identity, so once the journal grows past them a later launch drops its new events. `NothingLost` catches it.

The `grows` finding (TOB-455): a journal that went back and numbered new events up to the app's place before the app reconnected has nothing behind that place, so the host can't tell, keeps its identity, and the app drops the new events as saved.

`check.sh --trace RULE MUTATION BOUNDS` prints one run's full TLC output. `RemoteCursor.cfg` is the code as it is, with every rule, for reading a run by hand.

## Read a result

Read every TLC result with the `/statechart` skill when you can. It turns a violation into its shortest path in plain words. If you can't invoke it, the trace in TLC's output is enough for a verdict.

For each step of a failing path, find the code in the coverage table and check that the code really takes that step in that order:

- **A real bug:** every step is something the code does. Report it in the code's words, one step per line, naming the function that takes each step. Do not weaken the rule or the model to make it pass. In committed code, file it in Linear. In a change under review, the change breaks the rule: say so, with the path, and don't call it ready to merge. `remote_tests.rs` (`a_kill_between_receiving_and_saving_loses_nothing`) shows how to replay a path with a real kill.
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

Sync commit: `b5942e54f`. Paths are under `crates/`. Find each function by name.

| Model | Code |
|---|---|
| `jid`, `latest`, `journal`, `acked` | the host's journal: its `identity` row (`Journal::id`, made with the file), `clients.latest`, the `events` table, `clients.acked` (`keystone-agent/src/remote/journal.rs`) |
| `Reset` | the journal file replaced: `Journal::open` on a new file makes a new identity, and the daemon and its transport end |
| `ljournal`, `Connect` | `State.journal` and `State::adopt`, run by `Shared::resend` on every welcome: another journal, or the same one behind a place, forgets every place. The daemon ignores an `after` from another journal (`serve.rs`, on `Frame::Hello`) |
| `Crash`, `sentTo`, `crashes` | a host crash or power loss: the daemon and its provider die, and only commits the journal never sent may be gone. That is the assumption the model rests on. The code makes it true as far as the disk keeps what a sync says it keeps: the journal commits with `synchronous = FULL`, and with `fullfsync` on so that macOS syncs through the drive's cache (`journal.rs` `SYNCHRONOUS`, `Journal::open`), and the daemon delivers an event's frame only after `Journal::append` committed it (`serve.rs` `journal_events`). The restart events are numbered through the same path. The model also assumes the app's own saves survive a power cut, which holds on Linux and not yet on macOS (TOB-463) |
| `Rewind`, `era`, `born`, `fresh` | a journal restored from a backup or damaged: the same identity, numbers back at `b`. `era` tells the events a number named before and after it apart, which the app can't see; `fresh` keeps it from numbering new events before the app connects, except in the `grows` bounds |
| `Seen`, `Rotates` | `State::seen`, sent in `hello`; the daemon's `went_back` and `Journal::rotate`, which give a journal behind a place the app shows a new identity |
| `HostAppends` | the daemon journals an event before it sends it, live once the replay is done (`remote/serve.rs`, `Journal::append`) |
| `wire`, `Receive`, `received`, `inbox` | frames read on the link's thread, `Shared::route` with `State.received` and `RouteState.inbox` (`remote/link.rs`) |
| `savedMem` | `State.saved`, seeded by `Link::with_cursors` and raised by `Link::saved` |
| `Drain`, `unsaved` | `RemoteConnection::drain_placed` (`remote/connection.rs`) and `Shared::drain`, which adds to `State.unsaved`; the pump hands each event and its place over in order and commits the session (`keystone-desktop/src/app_state.rs` `pump`, `receive_placed`, `commit`), which queues the rows and the place with `Logs::append_through` (`keystone-agent/src/log.rs`) |
| `queue`, `Save` | `PendingSession.writes` and `PendingSession.through`; the writer's `Files::retry` and `PendingSession::retry`, which call `Database::append_logs` (`keystone-core/src/database.rs`) with the place, then `Files::through`, which reports `Report::Through` |
| `log`, `cursor` | the `log_entries` and `remote_cursors` tables |
| `Report` | `receive_placed` on `Report::Through`, calling `Link::saved` |
| `SendAck`, `sentAck`, `AckValue` | `State::ack` and `State.sent_ack` |
| `HostAck` | the daemon on `Frame::Ack`, `Journal::ack` |
| `Drop`, `Connect`, `linked` | `Shared::connect_once` ending and starting again with `hello` carrying `after` once `State.linked`; `Shared::resend`; the daemon's `replay` from the larger of `after` and `acked` |
| `Kill`, `Launch` | SIGKILL of the app; `AppState::new` reading `Database::remote_cursors`, each with its journal, and `open_link` giving them to `Link::with_cursors` |
| `trimmed` | a ghost: what `HostAck` deleted. A turn whose events a replaced journal took is ended by `AppState::end_lost_turns`, which the model leaves out |

## Scope

Four sets of bounds, since their product is too large to explore (over 9 minutes and 7 GB at three events, one reset, one kill and one drop, without finishing). Each checks every combination of sessions and rows:

- `kills`: one journal, four events, up to two kills and one drop. About 5.5 million distinct states.
- `reset`: two journals of two events each, one replacement of the journal at any point, one kill and one drop. About 3.7 million.
- `rewind`: two events, the journal going back once at any point, one kill and one drop, with no new event before the app next connects.
- `grows`: as `rewind`, but the journal may number new events before the app connects. That is the finding above.
- `crash`: two events, one host crash or power loss at any point, then the restarted daemon numbering its own events at once.

A pass covers the model, not all of Keystone.

Assumed, so a break there would not show:

- Every row an event adds is taken by the commit right after it is applied (`receive_placed` commits after each event). A row an event caused later, from another update, would be saved with a later place. For a remote session nothing adds one: checkpoints, the only such writer, are off there.
- A loading conversation holds its events until its history is restored, and `commit` gives no place meanwhile, so the model's drain stands for the pump after the load.
- The log writer saves a session's queue whole or not at all; its retries and directory barrier (`docs/design/storage.md`) are one `Save` that may be delayed forever. The store under it honours sync requests.
- One route on the machine, with its connection registered. Events for a route with no connection don't hold the acknowledgement back by design, and are not covered by `NothingLost`.

Left out:

- The journal's cap, which drops item events past 64 MiB of unacknowledged events and records a gap. That loss is designed and shown in the session.
- Ending a turn a replaced journal took with it (`end_lost_turns`); `remote_tests.rs` covers it.
- Calls and their replies, which share `ack` but have their own rule (`docs/design/remote.md`, "Calls run at most once").
