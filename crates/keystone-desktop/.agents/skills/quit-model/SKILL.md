---
name: quit-model
description: Model-check how Keystone quits on its own after its last window closes (quit_when_idle, quit_if_idle, request_quit, result delivery, scheduled runs) with TLA+. Use before merging any change to the quit, window-close, pump, result-delivery or scheduler code listed in its coverage table, and when a quit-related bug needs every event order checked.
---

# Quit model

Closing the last window while a turn runs keeps Keystone up with no window until the last turn ends, then quits. `QuitWhenIdle.tla` models that flow, and TLC checks every order of events in it. A test checks only the orders someone wrote down; this check covers them all within the model's scope below.

Run all commands from the workspace root.

## The rules

| Rule | In plain words | The harm if it breaks |
|---|---|---|
| `OnlyQuitStopsTurns` | Keystone stops a running turn, or drops a message it accepted for one, only when the user chooses Quit. | An agent's work is cut off mid-turn, or a follow-up the user sent never runs, after the user closed the window so that it would finish. |
| `NoScheduledRunWhileWaiting` | No scheduled run starts while Keystone has no window open. With no window, Keystone is waiting to quit (`WindowlessMeansWaiting`). | A run starts unseen and holds Keystone open past the turn the user was waiting on, though `docs/design/scheduled-tasks.md` promises it waits for the next launch. |
| `WindowlessMeansWaiting` | Whenever Keystone has no window and is not quitting, it is waiting to quit. | Keystone runs on with no window and nothing that will ever quit it. |
| `WindowlessEventuallyEnds` | With no window, Keystone eventually exits or shows a window again. | Same as above, as a liveness check, assuming turns end, providers answer, and a pump runs whenever something wakes it. A machine's link may drop and never come back. |
| `OnlyUserQuitEndsRemoteTurns` | A turn on another machine is ended by Keystone only when the user chose Quit and the machine is set to end its sessions. Closing the last window never ends one. | Work left running on a remote machine so it would go on while the laptop is closed is cut off, though `docs/design/remote.md` promises only Quit with **End sessions on this machine when Keystone quits** does that. |

## When to use

Use it when a change touches any function in the coverage table, or when this shows anything:

```sh
bash crates/keystone-desktop/.agents/skills/quit-model/check.sh --drift
```

It lists the covered files changed since the sync commit, committed or not, the covered functions with changed lines, untracked Rust files in the covered crates, and whether the tGPUI rev the model relies on has moved. A new file that takes over covered code shows only in that untracked list, and not at all once it is committed: when the diff moves covered code, check where it went. The function list is a guide, not a proof. Read the whole diff it prints a command for.

## Run

```sh
bash crates/keystone-desktop/.agents/skills/quit-model/check.sh
```

It needs `mise` and, on the first run, the network: it fetches TLC 1.7.4 (checksum-pinned) into `~/.cache/keystone/` and Java through `mise` (`java@temurin-21`). It runs eighteen TLC jobs in parallel in a few seconds, prints one line per run, and exits 0 only when every result matches what it expected:

- `code` runs: each rule on the code as it is. All must hold.
- `can fail` runs: each rule with a deliberate break of the code it guards (`Mutation` in the model). Each must break. If one holds, the rule has stopped checking anything. `quit-skips-recheck`, `turns-ignore-connections`, `work-ignores-queued`, `work-ignores-steer` and `steer-settles-silently` each undo part of the TOB-350 fix. `work-ignores-result` omits a result still waiting for its durable save from work accounting. `result-save-no-wake` loses the wake after that save. `quit-forgets-who` has `reserve_quit` never record that the user didn't choose the quit, which is how the first build of TOB-441 let closing the last window end a remote turn. `work-counts-remote` counts a remote session's turn in `has_work`, so a windowless Keystone waits on a link that may never come back.

A line marked `UNEXPECTED` prints the path of TLC's output, which holds the full trace. Each run of `check.sh` first deletes the outputs a previous run left there.

`QuitWhenIdle.cfg` is the code as it is, with every rule, for reading a run by hand.

## Read a result

Read every TLC result with the `/statechart` skill. It turns each violation into its shortest path in plain words, and when everything holds, it draws how Keystone moves between states. The user may have to start it. If you can't invoke it, read the trace in TLC's output: it is enough for a verdict, and `/statechart` is for showing the path to people, so offer it to the user in your report. Give it `QuitWhenIdle.tla` and a copy of `QuitWhenIdle.cfg` that keeps `TypeOK` and only the rule in question, with `Mutation` set as in the run you are reading. Have its view label states as the user sees them: windows open or not, waiting to quit, saving to quit, exited, which turns run.

For each step of a failing path, find the code in the coverage table and check that the code can really take that step in that order:

- **A real bug:** every step is something the code does. Report it in the code's words, one step per line, naming the function that takes each step. Do not change the rule or weaken the model to make it pass, and do not change product code unless asked.
  - In committed code, file it in Linear.
  - In a change under review, the change breaks the rule: say so, with the path, and don't call it ready to merge. If the rule itself is meant to change, that is the user's decision, and the rule, its sentence here, and the docs that promise it change together.
  - Whoever fixes it can replay the path as a GPUI test (`#[gpui::test]`, see `docs/CONVENTIONS.md`). Desktop GPUI tests such as `windows_tests.rs` compile only with `--features ui-tests`; without it, `cargo test -p keystone-desktop` runs none of them and still passes.
- **A modeling error:** a step the code cannot take, such as an action firing while the code would refuse it, or two steps that are one update in the code. Fix the model to match the code, cite the code in a comment, and rerun.

An invariant's trace is a shortest path. A `WindowlessEventuallyEnds` trace is a loop that never exits or shows a window, and it need not be the shortest such loop: look for the state where nothing can quit any more.

Other results:

- **A `can fail` run now holds.** The change made Keystone unable to reach the break's state another way, for example by never quitting at all, or the break no longer touches code the rule depends on. Read the whole table, then replace the break as in step 4 below.
- **TLC errors other than a violation** (parse errors, a deadlock, `TypeOK` failing) are always modeling errors. `deferred` longer than 2 breaks `TypeOK`: if the code now queues more, raise the bound and say why.

## Update the model after an intended change

Before you edit, make sure you can undo it: `git checkout -- crates/keystone-desktop/.agents/skills/quit-model` restores the committed model. A model edit goes in the same commit as the code change it mirrors, and it is reverted with it.

1. **Compare the change to the breaks.** If the code now does what a `Mutation` in the model does, the change very likely breaks that mutation's rule. Expect it in step 3, and say so in your report.
2. **Mirror the change.** Find the operator for each changed function in the coverage table, and change it to do what the code now does, in the same order. One GPUI update is one action. A `cx.defer` callback is one entry in `deferred`. If the change adds a way to start a turn, stop providers, close or open windows, or set `quit_when_idle`, add it as an action and add that to `Next`. A new outcome of the quit save is a new disjunct in `SaveAndTeardownDeferred`.
3. **Run `check.sh` and judge it** as in "Read a result". If a `code` run now breaks and the path is real, the change breaks a rule: stop here and report it. Keep the model edit with the change, so whoever decides sees what the code does. Do not touch the expected column in `EXPECT`, and do not go on to step 4.
4. **Keep each break meaningful.** If step 1 found a mutation that the code now matches, its `can fail` run proves nothing. Replace it with a different break of the same rule, in both the model's `Mutations` and the mutation column of `EXPECT`, and check that it breaks.
5. **New rules.** A new rule gets a plain sentence and its harm in the table above, a `code` line in `EXPECT`, and a `Mutation` that makes it fail.
6. **Run `check.sh`** until every line is `ok`.
7. **Move the sync commit.** When the model matches the code at the new commit, set `SYNC` in `check.sh` and the sync commit below to that commit, and update the coverage table and the state count in Scope. Do this only after you have read the covered diff and confirmed the model describes it, never just to quiet `--drift`. If the tGPUI rev changed, update `GPUI_REV` in `check.sh` too, once you have confirmed how `cx.defer` runs.

## Coverage

Sync commit: `11b9013`. The model was confirmed against the code at this commit. Paths are under `crates/keystone-desktop/src/`. The line numbers are where each function is at the sync commit; find it by name after that.

| Model | Code |
|---|---|
| `phase`, `quitWhenIdle` | `AppState.phase`, `AppState.quit_when_idle` (`app_state.rs`) |
| `running`, `asking` | `Conversation.running`, set by `push_sent` and cleared on a turn end in `apply` (`conversation.rs`); `awaiting_approval` |
| `inbox` | provider events not yet drained: `Event::ApprovalRequested` and `Event::TurnCompleted` |
| `followUp` | a message sent into Parent's turn (`keystone-agent` `connection.rs`, `codex.rs`, `claude.rs`). `"queued"`: counted in the connection's `queued` until its worker hands it to the driver (`send_inner`, `unqueue`). `"steer"`: Codex's `Pending::Steer` with `strict: false`. `"held"`: kept for the next turn, as Codex's `waiting` or by `hold_follow_ups` until `release_follow_ups`. `"start"`: a turn the connection runs (`Connection::is_running`) before a pump applies its `Event::TurnStarted` |
| `woken` | `CodexDriver::take_recheck`, read by the Codex connection's `on_line`: it wakes Keystone with no event, which `deliver` otherwise does only when its inbox stops being empty |
| `windows` | `open_windows` (`main.rs`) |
| `delivery` | Child's `DeliveryState` and pending send in `AppState.unsent`. `Saving` and `Saved` both mean Delivered in memory, before and after its watched save is durable; `Delivered` means sent. |
| `WriterConfirmsDelivery` | The Store writer confirms the watched revision and wakes the app through `wake_when_durable`. |
| `deferred` | callbacks queued with `cx.defer`, run FIFO after the outermost update ends (tGPUI `App::finish_update` and `flush_effects`) |
| `idleQuit` | `AppState.idle_quit`, set by `reserve_quit` (`app_state.rs`) for a quit the user didn't choose and cleared by `quit_failed` |
| `remoteRunning`, `remoteShown`, `linkUp`, `endOnQuit` | a Claude turn on another machine, run by its `keystone-host`; that session's `Conversation.running`; its `Link` status being `Connected` (`keystone-agent` `remote/link.rs`); `Machine::end_on_quit` (`keystone-core` `machine.rs`) |
| `UserSendsRemote`, `RemoteEndsTurn`, `LinkChanges`, `RemotePump` | `send_message_from` to a remote session (`Place::Remote`); the host's provider ending the turn; `Link::supervise` dropping and reconnecting; `pump` draining the remote route's turn end, then `publish` |
| `Runs`, `HasWork`, `TurnRunning`, `Asking` | `runs_turn` (`app_state.rs:1471`), `Connection::has_work` (`keystone-agent` `connection.rs:62`), `has_work` (`app_state.rs:1465`) through `started` (`:2488`) and `route` (`:2261`), `asking` (`app_state.rs:1478`). Both leave out remote sessions (`is_remote`) |
| `QuitIfIdle` | `quit_if_idle` (`app_state.rs:1489`), run by every `publish` (`:1643`) and `refresh` |
| `RequestQuit` | `request_quit` (`main.rs:373`) and `reserve_quit` (`app_state.rs:352`), which records with `idle_quit` whether the user chose the quit |
| `CloseLastWindow` | `close_window` with no other window (`main.rs:280`) |
| `UserQuits` | the `Quit` action (`main.rs:226`) |
| `UserReopens` | `reopen` (`windows.rs:299`) |
| `UserSends`, `UserAnswers` | `send_message_from` (`app_state.rs:3232`) from the composer; answering an approval |
| `UserSteers` | `send_message_from` into Parent's running turn, which the connection queues for its worker |
| `WorkerTakesFollowUp` | the worker job `send_inner` queued: Codex's `send` steers a running turn or starts the next one, Claude's `enqueue` adds it to the running turn or starts one |
| `CodexAnswersSteer` | Codex's answer to the steer (`on_response`, `Pending::Steer`): a warning while the turn runs, the next turn after its end, or `wake_without_event` when accepted after its end |
| `ProviderReleasesFollowUp` | the worker running `release_follow_ups`, queued by `turn_ended`; Codex's `flush` starts the held turn |
| `ProviderEndsTurn`, `ProviderAsks` | provider events queued for the next pump (`Event::TurnCompleted`, `Event::ApprovalRequested`) |
| `ParentLaunchesChild` | the orchestrator's `session_launch` (`orchestrator.rs`), which saves a Working delivery and starts the child |
| `Pump` | `pump` (`app_state.rs:1868`): `receive` (`:1926`) with `turn_ended` (`checkpoints.rs:247`), `save_results` (`delivery.rs:247`) and its closing `publish`, then `deliver_results` (`delivery.rs:297`) with `result_hold` (`delivery.rs:186`), `send_message_from` and its closing `refresh` |
| `ScheduledRun` | `check_schedules` (`scheduled.rs:201`) with `quitting` (`scheduled.rs:196`), `run_task` and `start_run` |
| `RequestQuitDeferred` | the callback `quit_if_idle` defers: it checks `has_work` again and sets `quit_when_idle` back while a turn runs, else calls `request_quit` |
| `ShowApprovalDeferred` | the callback `quit_if_idle` defers for an approval: `notifications::show` (`notifications.rs:219`), then re-arming `quit_when_idle` |
| `SaveAndTeardownDeferred` | `save_and_teardown` (`main.rs:404`): `save` (`app_state.rs:371`), `quit_failed` (`:360`), `teardown` (`:429`) with `leave_machines`, which sends each connected machine `quit` with `keep_running` unless the user quit and the machine ends its sessions, and `reopen` after a failed save |

## Scope

The original synced model explored 8,311 distinct states. An integration extended it with a pending Delivered save, its background confirmation and wake, and pending-result work accounting. TOB-441 adds one session on another machine, its link and its machine's Quit setting, and the model now explores 126,070 distinct states. The sync marker stays where it was, so drift from before these integrations stays visible. TLC explores the bounded state space for three sessions (Parent, Child, a scheduled task's Task), one window and one launched result. A pass covers the model, not all of Keystone.

Assumed, so a break there would not show:

- Store writes for results and turn records eventually succeed. Delivered may remain pending across UI updates, and its successful completion wakes the pump. The quit save can fail, and the model covers both its outcomes. The model does not prove failed-write retention, FIFO commits, retries, permission repair or the user Send durability fence; actual storage and native tests cover those paths.
- Opening windows succeeds, and the quick composer is closed.
- Parent's history is loaded, it has no recovery and is not settled.
- Only Parent takes a follow-up, one at a time. Its provider does not end the turn a follow-up starts before a pump applies its start. A turn the provider is starting (Codex's `Turn::Starting`) counts as `"start"`: `is_running` counts it, and its `Event::TurnStarted` follows.
- A pump runs only when woken. Queued connection events ensure a pump: `deliver` wakes when its inbox becomes non-empty, while `push` and process exits wake directly. The model has no other source of pumps, such as the inner pump of `send_message_from`.
- The user sends only from a window, and `UserSends` skips its inner pump, which cannot start a quit while a window is open.
- `ParentLaunchesChild` treats the asynchronous launch and its answer as one update. It also permits Parent's turn end to be queued before the launch; TLC's shortest counterexample uses that order. Launching Child before Parent ends is also reachable in the code.

Left out:

- More than one remote session or machine, a remote approval, and the next launch's catch-up. `remote_tests.rs` covers those.

- The scheduled continue after a usage limit (`continue_when_due`), `session_send` to an existing session, widget answers, crash recovery, and `Event::Failed`.
- Steering a running task, more than one window, more than one launched session.
- What teardown does to provider processes, log flushing, and when `arm_scheduler` next wakes the scheduler. Scheduled claims and their save failures are abstracted into the eventual guarded `ScheduledRun` action; pending scheduled saves are not modeled.

The drift check watches only the files in `COVERED` in `check.sh`. It does not watch `quick.rs`, whose closed-composer assumption is listed above, so recheck that assumption when changing it. `checkpoints.rs` is covered for its release of held follow-ups.

Over-approximated: `UserQuits` may fire with no window, which only macOS's menu and Dock allow.
