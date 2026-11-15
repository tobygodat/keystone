---
name: restore-model
description: Model-check how a checkpoint restore moves the user's files and how a cancel or a put back after a crash undoes it (checkpoint::restore, Journal::apply, Journal::undo, put_back, Work::unfinished) with TLA+. Use before merging any change to the restore, journal, undo or put-back code listed in its coverage table, and when a restore bug needs every order of crashes and file writes checked.
---

# Restore model

Restoring a checkpoint moves the user's files aside and the checkpoint's files into their working tree, one rename at a time. A journal in the git folder records each step, so a restore that is cancelled, fails, or is cut off by a crash can be undone, then or at the next launch (`put_back`). `RestoreJournal.tla` models that flow. TLC checks every order of its steps, crashes, failures, and someone else's file writes. A test checks only the orders someone wrote down. This check covers all of them within the scope below.

Run all commands from the workspace root.

## The rules

| Rule | In plain words | The harm if it breaks |
|---|---|---|
| `NothingLost` | Until the user keeps a restore, every file it replaced or deleted is in the working tree or aside. A file someone else wrote is always in one of the two. | A user's file is deleted for good. |
| `OthersWritesStay` | A file someone else wrote is where they wrote it whenever Keystone is not moving files: while it asks whether to keep, while it is stopped, and once it is done. | A user's save vanishes from their tree, or is replaced by the checkpoint's version. |
| `PutBackRestoresFiles` | A cancel or put back that reports success leaves every file as it was before the restore, except those someone else wrote since. | Keystone says the files are back, and they are not. |
| `PutBackRestoresFolders` | ... and every folder: those the restore removed are back, and those it made are gone unless someone wrote a file into them. | Folders the restore made are left behind. |
| `AsideNeverForgotten` | While Keystone is stopped or done, any file left aside is either offered for a put back or the user was told where it is. | A user's original sits in `.git/keystone-restore-*/aside/` and nothing tells them. |

"Someone else" is the user or their tools (an editor's save, a formatter, a shell). Keystone refuses to restore or put back while a turn runs in the repository (`idle_in` in `crates/keystone-desktop/src/checkpoints.rs`), so it is never an agent.

## Known findings

Four runs break on the code at the sync commit. Each is a real path, filed as issues (TOB-352, TOB-353). The model's paths `x`, `y` and `r` stand for `d/x` (deleted, the only file in folder `d`), `n/y` (added, in a folder `n` that does not exist yet) and `r` (replaced).

**`PutBackRestoresFolders`: a folder made but not yet recorded stays (TOB-352).**

1. `restore` places `n/y`. `place` runs `fs::create_dir` for the missing folder `n`.
2. Keystone stops before `self.record(Step::Made(n))`.
3. At the next launch the user takes the put back. `put_back` undoes every recorded step and reports success.
4. Folder `n` stays, empty. `Removing` is recorded before its folder is removed for this reason; `Made` is recorded after.

**`AsideNeverForgotten`: a stop after the journal goes hides files aside (TOB-352).**

1. `restore` deletes `d/x`: `set_aside` moves it aside. Keystone stops before any journal record.
2. While Keystone is down, the user saves `d/x` again.
3. At the next launch the put back is offered, because aside holds a file. `undo` has no steps. `put_aside_back` finds `d/x` taken, so the original stays aside and `failed` is set.
4. `undo` removes the journal, then `remove_dir_all(stage)`, then returns `Err(aside)`, which reaches the conversation as `Error::Kept`.
5. Keystone stops between removing the journal and that error reaching the user (removing a large stage takes a while). At the next launch `Work::unfinished` finds no journal, so nothing is offered, and nothing says where the original `d/x` is. `expire` keeps the folder because it holds files, so the file is not deleted. The user is just never told.

**`NothingLost` with `Races = TRUE`: a rename replaces a file written after its check (TOB-353).**

1. `apply` checks `n/y`: nothing stands there (`taken` is false).
2. `place` makes folder `n`, takes the staged file's `Stamp`, its `digest` (reads the whole file), and records `Placing`.
3. Meanwhile the user's editor saves `n/y`.
4. `move_file(staged, n/y)` is `fs::rename`, which replaces what stands there. The user's `n/y` is gone.

`put_aside_back` has the same shape: it checks that nothing stands at the path, then renames the original over whatever does by then. So does undoing `Placed` and `Placing`: a stat, then `fs::remove_file`.

**`OthersWritesStay` with `Races = TRUE`: a file written after its check is moved aside (TOB-353).**

1. `apply` checks `d/x` with `changed_since_read`: unchanged.
2. The user saves `d/x`.
3. `set_aside` moves the new `d/x` aside. If the restore is then kept, it is deleted with the restore's folder.

The race windows run from a check to the call it guards. They are short, but `place`'s window holds a full read of the staged file. A model of the fix for them is not part of the check.

The `Fix = TRUE` runs check fixes for the first two findings only. They are not in the code yet:

- `Made` is recorded before `fs::create_dir`, as `Removing` is. Undoing a `Made` whose folder was never made is already fine (`NotFound`).
- Before `undo` removes the journal while a file stays aside, it saves a notice of where the file is, which every launch shows until the user deals with it (`UNotice`). Toby chose this over keeping the journal, which would block new restores in that repository. The `notice-after-journal` break checks the order: a notice saved after the journal goes leaves the same gap.

Because the first two rules already break on the code, a new way to break them does not change the `code` runs. The `candidate fix` and `can fail` runs catch it instead: they expect every rule to hold with the fixes, and each deliberate break to fail its rule.

## When to use

Use it when a change touches any function in the coverage table, or when this shows anything:

```sh
bash crates/keystone-agent/.agents/skills/restore-model/check.sh --drift
```

It lists the covered files changed since the sync commit, committed or not, the covered functions with changed lines, and untracked Rust files in `keystone-agent/src`. A new file that takes over covered code shows only in that untracked list, and not at all once it is committed: when the diff moves covered code, check where it went. Some function names (`record`, `drop`, `write`) also match code the model does not cover. The list is a guide, not a proof. Read the whole diff it prints a command for.

## Run

```sh
bash crates/keystone-agent/.agents/skills/restore-model/check.sh
```

It needs `mise` and, on the first run, the network: it fetches TLC 1.7.4 (checksum-pinned) into `~/.cache/keystone/` and Java through `mise` (`java@temurin-21`). It runs eighteen TLC jobs in parallel in about five seconds, prints one line per run, and exits 0 only when every result matches what it expected:

- `code` and `finding` runs: each rule on the code as it is. With `RACES TRUE`, someone else may also write between any two of Keystone's calls. With `FALSE`, only while Keystone waits on `keep`, is stopped, or is done.
- `candidate fix` runs: each rule with `Fix = TRUE`. All must hold.
- `can fail` runs: each rule with a deliberate break of the code it guards (`Mutation` in the model). Each must break. If one holds, the rule has stopped checking anything.

A line marked `UNEXPECTED` prints the path of TLC's output, which holds the trace. Each run of `check.sh` first deletes the outputs a previous run in the same checkout left there. To see any one run's trace:

```sh
bash crates/keystone-agent/.agents/skills/restore-model/check.sh --trace PutBackRestoresFiles FALSE FALSE none
```

The arguments are the rule, `Races`, `Fix` and `Mutation`, as in a result line. It prints only the variables that change at each state. `RestoreJournal.cfg` is the code as it is, with every rule, for reading a run by hand.

## Read a result

Read every TLC result with the `/statechart` skill. It turns each violation into its shortest path in plain words, and when everything holds, it draws how a restore moves between states. It may be one only the user can start; if you can't invoke it, ask them to. Until then, read the trace from `--trace`: it is enough for a verdict, and `/statechart` is for showing the path to people, so offer it to the user in your report. Give it `RestoreJournal.tla` and a copy of `RestoreJournal.cfg` that keeps `TypeOK` and only the rule in question, with `Races`, `Fix` and `Mutation` set as in the run you are reading. Have its view label states as the user sees them: which files are in the tree and which aside, whether a put back is offered, and whether Keystone is restoring, asking, stopped or done.

In a trace, `pc` is where Keystone is (the labels match the coverage table), `tree` and `aside` say what stands at each path (`orig`, `ckpt` or `user`, numbered), and `journal` is what the journal file holds. For each step of a failing path, find the code in the coverage table and check that the code can really take that step in that order:

- **A real bug:** every step is something the code does. Report it in the code's words, as in the known findings above. Do not change the rule or weaken the model to make it pass, and do not change product code unless asked.
  - In committed code, file it as an issue.
  - In a change under review, the change breaks the rule: say so, with the path, and don't call it ready to merge. If the rule itself is meant to change, that is the user's decision, and the rule, its sentence here and the doc comments that promise it change together.
  - Whoever fixes it can replay the path as a test in `checkpoint.rs`'s `tests` module. The put-back tests there stop a restore with `crash_mid_restore` (a panic in `keep`, once every file is placed), then cut the journal short or move files by hand to stand for a crash at an earlier step, as `a_replaced_file_placed_as_the_crash_came_gets_its_original_back` does.
- **A modeling error:** a step the code cannot take, such as a `Fail` where the code has no call that can fail, a user write while Keystone moves files in a `Races = FALSE` run, or two steps that are one call in the code. Fix the model to match the code, cite the code in a comment, and rerun.

When several lines break, trace each. If they share a path, report it once with every rule it breaks.

Other results:

- **A `code` run expected to break now holds.** The finding is gone. Either the change fixed it (confirm the path can no longer happen in the code, then follow step 7 below), or the change stopped Keystone reaching that state some other way, for example by no longer putting files aside. Then another rule usually breaks: read the whole table before calling it a fix.
- **A `candidate fix` run breaks.** The change breaks the rule whether or not the findings are fixed, so treat it as a `code` break.
- **TLC errors other than a violation** (parse errors, a deadlock, `TypeOK` failing) are always modeling errors. A journal longer than 9 records breaks `TypeOK`: if the code now records more, raise the bound and say why.

## Update the model after an intended change

Before you edit, make sure you can undo it: `git checkout -- crates/keystone-agent/.agents/skills/restore-model` restores the committed model. A model edit goes in the same commit as the code change it mirrors, and it is reverted with it.

1. **Compare the change to the breaks.** If the code now has the same effect as a `Mutation` in the model, even by another route, the change very likely breaks that mutation's rule. Expect it in step 3, and say so in your report.
2. **Mirror the change.** Find the action for each changed function in the coverage table and change it to do what the code now does, in the same order. One file system call, or one journal record, is one action. A read (`Stamp::of`, `digest`, a stat) is folded into the action that uses it, as the coverage table says. When a change moves a read away from its use, keep the value in a variable, or use a constant when it can only be one file (the staged file's stamp is always `CkptY` or `CkptR`). A call that can fail with `?` in `apply` goes in `FailLabels`, except journal writes, which the model assumes succeed (see Scope). A new kind of journal record is a new `kind` in `Steps`, a branch in `UStep`, and a line in this skill's coverage table. If the change adds a path kind (a type change, a link, a case-only rename), add a path to `Paths` only if its steps differ from the three there.
3. **Run `check.sh` and judge it** as in "Read a result". If a `code` or `candidate fix` run now breaks and the path is real, the change breaks a rule: stop here and report it. Keep the model edit with the change, so whoever decides sees what the code does. Do not touch the expected column in `EXPECT`, and do not go on to step 4.
4. **Keep each break meaningful.** If step 1 found a mutation the code now matches, its `can fail` run proves nothing. Replace it with a different break of the same rule, in both the model's `Mutations` and the mutation column of `EXPECT`, and check that it breaks.
5. **Fixes and new rules.** If the change fixes a finding, mirror the fix in place of its `Fix` branch (for `AsideNeverForgotten`, `UNotice` becomes the code's step, and `notice-after-journal` stays as its break), change that `EXPECT` line's expected result to `holds`, remove it from the known findings here, and keep a `Mutation` that makes the rule fail. A fix for the race findings changes the `Races = TRUE` lines the same way. A new rule gets a plain sentence and its harm in the table above, `code` and `candidate fix` lines in `EXPECT`, and a `Mutation` that makes it fail.
6. **Run `check.sh`** until every line is `ok`.
7. **Move the sync commit.** When the model matches the code at the new commit, set `SYNC` in `check.sh` and the sync commit below to that commit, and update the coverage table and the state count in Scope. Do this only after you have read the covered diff and confirmed the model describes it, never just to quiet `--drift`.

## Coverage

Sync commit: `243fec7`. The model was confirmed against the code at this commit. Paths are under `crates/keystone-agent/src/checkpoint.rs` unless named. The line numbers are where each function is at the sync commit; find it by name after that.

| Model | Code |
|---|---|
| `Init` | `restore` (`:630`) up to `journal.apply`: the tree read, before-restore saved, the diff, `convert` of every file that can be written into `stage`, `unchanged_since_read` (`:1492`), `Work::new` (`:894`) |
| `tree`, `dirs` | the working tree; `aside`, `stage`, `folder`, `journalFile` are `Work`'s folders and its `journal` file |
| `journal`, `Steps` | `Step` (`:1052`), `Step::fields`, `KEPT`, `Work::write` (`:968`), `parse` (`:1104`); `Journal::record` (`:1217`) |
| `unchanged`, `ChangedSinceRead` | `Journal.unchanged`, `changed_since_read` (`:1212`) |
| stamps (file records) | `Stamp` (`:1447`), `Stamp::of` (`:1455`), `digest` (`:1408`) |
| `XCheck` .. `XRemoved` | `apply` (`:1167`), the deletes: `delete` (`:1276`) with `set_aside` (`:1264`) and its folder loop |
| `YCheck` .. `YPlaced`, `RCheck` .. `RPlaced` | `apply`, the writes: `taken`, then `place` (`:1297`): `set_aside`, `fs::create_dir` and `Made`, `Placing` (with the `Stamp::of` and `digest` of the staged file before it), `move_file` (`:1534`), `Placed` (with the `Stamp::of` the placed file before it) |
| `Fail`, `FailLabels` | a `?` in `apply`, or a cancel or "no" from `keep`; `restore`'s `match outcome` |
| `Keep`, `KeptRecord`, `KeptRemove` | `keep()` saying yes, `journal.work.write(&[KEPT])`, `Work`'s drop (`:979`) |
| `UStep`, `UUnlink` | `Journal::undo` (`:1326`), one step at a time, newest first |
| `UAside`, `UMove` | `put_aside_back` (`:1423`) |
| `UHolds`, `URmJournal`, `UEnd` | the end of `undo`: `holds_files` (`:561`), removing the journal, `Err(aside)` or the drop of `Work` |
| `Crash` | Keystone stopping at any point: in-memory steps go, the folder and journal stay |
| `PutBack`, `Offered` | `put_back` (`:1024`) with `Work::unfinished` (`:927`); `list` (`:431`) reports it; `put_back` and `restore_checkpoint` in `crates/keystone-desktop/src/checkpoints.rs` (`:420`, `:485`) start it |
| `Tidy` | `Work::unfinished(folder, true)` removing a kept restore's folder |
| `UserWrite` | an editor or tool writing a whole file, making its folder; never an agent (`idle_in`, `checkpoints.rs:376`) |

## Scope

TLC explores the whole state space, 52,675 distinct states at the sync commit, for one restore with three paths: one deleted from a folder it leaves empty, one added in a folder that does not exist, one replaced. Keystone may stop up to 3 times (mid-restore, mid-undo, mid-put-back) and someone else may write up to 3 files. A pass covers the model, not all of Keystone.

Assumed, so a break there would not show:

- Each journal record is written whole and in order before the next call. That holds for a crash or a kill, not for power loss: the journal is never synced.
- Journal writes succeed. `record` keeps a step in memory before writing it, so a failed write is still undone in the same run; only a crash right after one would show the difference.
- Every write gives a file a new `Stamp`, and two different files never share one. `Stamp` includes the change time in nanoseconds, and a `Placing` check also compares the `digest`.
- Renames are within one file system. Across file systems `move_file` copies, and a file can be in both places for a moment.
- Someone else only writes whole files. They never delete files or folders, and never write while Keystone is moving files, except in the `Races = TRUE` runs.

Left out:

- Everything before `apply`: reading the tree, git's filters and `convert`, the checkpoint's ignore rules, left-out files, sparse checkouts, submodules and nested repositories (`not_ours`), and a file converted late in `apply`.
- Case-insensitive names (`stored`), folder modes (`Removing` records one; the model does not check it comes back), hard links, links.
- More than one restore folder, two Keystones at once (`hold` keeps them apart), and taking checkpoints (`record`, `snapshot`, `prune`, `expire`).
- Whether the user sees an error after a restart: `reported` means Keystone returned the error to the conversation, not that it was saved.
