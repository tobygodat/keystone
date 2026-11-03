---------------------------- MODULE RestoreJournal ----------------------------
(***************************************************************************)
(* How a checkpoint restore moves the user's files, and how it is undone:  *)
(* by a cancel or a failure in the same run, or by a put back after       *)
(* Keystone stopped mid-way. Paths are relative to                         *)
(* crates/keystone-agent/src/checkpoint.rs unless named otherwise.         *)
(*                                                                         *)
(* One restore with one change of each kind, in the order `restore` makes *)
(* them (deletes first, then writes in diff-tree path order):              *)
(*   x  d/x, deleted; the only file in folder d, so d is removed too       *)
(*   y  n/y, added; folder n does not exist yet, so it is made             *)
(*   r  r,   replaced                                                      *)
(*                                                                         *)
(* Each action is one file system call, or one journal record, of the     *)
(* code. A crash can come between any two. Someone else (the user, an      *)
(* editor) can write a file: with Races = FALSE only while Keystone is not *)
(* in the middle of moving files (waiting on `keep`, stopped, or done);    *)
(* with Races = TRUE between any two calls as well.                        *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Races,          \* someone else may write between any two of Keystone's calls
    Fix,            \* candidate fixes for the findings (see SKILL.md), not the code
    Mutation,       \* a deliberate break of the code a rule guards, or "none"
    MaxCrashes,     \* how many times Keystone may stop mid-way
    MaxUserWrites   \* how many files someone else may write

Mutations == {"none", "undo-forgets-aside", "placed-undo-ignores-stamp",
              "placing-not-undone", "made-not-undone", "unfinished-ignores-aside",
              "notice-after-journal"}
ASSUME Mutation \in Mutations

Paths   == {"x", "y", "r"}
Folders == {"d", "n"}
Parent  == [x |-> "d", y |-> "n", r |-> "root"]

\* A file is told apart by its `Stamp` (inode, length, modified and changed time),
\* and by its `digest` for a `Placing` step. Every write gives a new stamp, and a
\* rename keeps it, so a file here is a record that a rename moves whole and a
\* write replaces.
None  == [who |-> "none", id |-> 0]
OrigR == [who |-> "orig", id |-> 1]   \* the user's r before the restore
OrigX == [who |-> "orig", id |-> 2]   \* the user's d/x before the restore
CkptR == [who |-> "ckpt", id |-> 3]   \* the checkpoint's r, converted into stage
CkptY == [who |-> "ckpt", id |-> 4]   \* the checkpoint's n/y, converted into stage
FirstUserId == 5
Files == {OrigR, OrigX, CkptR, CkptY} \cup
         [who: {"user"}, id: FirstUserId..(FirstUserId + MaxUserWrites)]
NoFiles == [p \in Paths |-> None]

\* A journal record (`Step::fields`, `KEPT`). `path` is a path or a folder.
Steps == [kind: {"placing", "placed"}, path: Paths, stamp: Files]
    \cup [kind: {"made", "removing", "removed"}, path: Folders, stamp: {None}]
    \cup [kind: {"kept"}, path: {"root"}, stamp: {None}]
Step(kind, path, stamp) == [kind |-> kind, path |-> path, stamp |-> stamp]

VARIABLES
    tree,        \* the working tree: what stands at each path
    dirs,        \* the folders of the working tree that exist (the top one always does)
    stage,       \* `Work.stage`: the checkpoint's files, converted before any file moves
    aside,       \* `Work.aside`: the files of the working tree a restore moved away
    folder,      \* the restore's folder in the git folder exists
    journalFile, \* its `journal` file exists
    journal,     \* the records in it; the in-memory `Journal.steps` are the same
    pc,          \* where Keystone is: a label below, "ask", "down" or "done"
    here,        \* `apply`: whether r stood there when its write began
    idx,         \* `undo`: the step being undone, newest first
    placed,      \* `undo`: paths of the `Placed` steps undone so far
    removed,     \* `undo`: folders of the `Removed` steps undone so far
    failed,      \* `undo`: a step failed or a file stays aside
    pending,     \* `put_aside_back`: aside entries not yet looked at
    target,      \* the path a stat just looked at, for the call that follows it
    unchanged,   \* `Journal.unchanged`: the stamp read for each path, or None
    owned,       \* ghost: every file someone must be able to get back
    lastUser,    \* ghost: the newest file someone else wrote at each path
    result,      \* ghost: how the last restore or put back ended
    reported,    \* Keystone told the user where files stay aside: it returned the
                 \* error, or with Fix, saved a notice that every launch shows
    crashes, userWrites, nextId

vars == <<tree, dirs, stage, aside, folder, journalFile, journal, pc, here, idx,
          placed, removed, failed, pending, target, unchanged, owned, lastUser,
          result, reported, crashes, userWrites, nextId>>

ApplyLabels == {"x_check", "x_aside", "x_removing", "x_rmdir", "x_removed",
                "y_check", "y_mkdir", "y_made", "y_placing", "y_move", "y_placed",
                "r_check", "r_aside", "r_placing", "r_move", "r_placed"}
UndoLabels  == {"u_step", "u_unlink", "u_aside", "u_move", "u_holds",
                "u_notice", "u_rmjournal", "u_end"}
KeepLabels  == {"k_kept", "k_rm"}
Labels == ApplyLabels \cup UndoLabels \cup KeepLabels \cup {"ask", "down", "done"}

TypeOK ==
    /\ tree \in [Paths -> Files \cup {None}]
    /\ dirs \subseteq Folders
    /\ stage \in [Paths -> Files \cup {None}]
    /\ aside \in [Paths -> Files \cup {None}]
    /\ folder \in BOOLEAN /\ journalFile \in BOOLEAN
    /\ journal \in Seq(Steps) /\ Len(journal) <= 9
    /\ pc \in Labels
    /\ here \in BOOLEAN
    /\ idx \in 0..9
    /\ placed \subseteq Paths /\ removed \subseteq Folders
    /\ failed \in BOOLEAN
    /\ pending \subseteq Paths
    /\ target \in Paths \cup {"none"}
    /\ unchanged \in [Paths -> Files \cup {None}]
    /\ owned \subseteq Files
    /\ lastUser \in [Paths -> Files \cup {None}]
    /\ result \in {"none", "kept", "ok", "err"}
    /\ reported \in BOOLEAN
    /\ crashes \in 0..MaxCrashes /\ userWrites \in 0..MaxUserWrites

----------------------------------------------------------------------------
InTree(f)  == \E p \in Paths : tree[p] = f
InAside(f) == \E p \in Paths : aside[p] = f
AsideHolds == \E p \in Paths : aside[p] # None
FilesIn(f) == {p \in Paths : Parent[p] = f /\ tree[p] # None}
Kept(j)    == \E i \in 1..Len(j) : j[i].kind = "kept"

\* `changed_since_read`: something stands there other than what the restore read.
ChangedSinceRead(p) == tree[p] # None /\ unchanged[p] # tree[p]

\* `Work::unfinished(folder, false)`, which `list` reports and the conversation
\* offers to put back: a journal not kept, with steps or files aside.
Offered == /\ folder /\ journalFile /\ ~Kept(journal)
           /\ \/ journal # <<>>
              \/ AsideHolds /\ Mutation # "unfinished-ignores-aside"

Record(step) == journal' = Append(journal, step)

\* `restore` has read the tree, saved before-restore, converted every file into
\* stage and opened the journal: nothing in the working tree has changed yet.
Init ==
    /\ tree = [x |-> OrigX, y |-> None, r |-> OrigR]
    /\ dirs = {"d"}
    /\ stage = [x |-> None, y |-> CkptY, r |-> CkptR]
    /\ aside = NoFiles
    /\ folder = TRUE /\ journalFile = TRUE /\ journal = <<>>
    /\ pc = "x_check"
    /\ here = FALSE /\ idx = 0 /\ placed = {} /\ removed = {} /\ failed = FALSE
    /\ pending = {} /\ target = "none"
    \* `unchanged_since_read` over the deletes and the replaced files.
    /\ unchanged = [x |-> OrigX, y |-> None, r |-> OrigR]
    /\ owned = {OrigX, OrigR}
    /\ lastUser = NoFiles
    /\ result = "none" /\ reported = FALSE
    /\ crashes = 0 /\ userWrites = 0 /\ nextId = FirstUserId

Goto(l) == pc' = l

----------------------------------------------------------------------------
(* `Journal::apply`, the deletes: `delete` of d/x.                         *)

XCheck == /\ pc = "x_check"
          /\ Goto(IF ChangedSinceRead("x") THEN "y_check" ELSE "x_aside")
          /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal, here>>

\* `set_aside`: a rename; a file already gone is not an error.
XAside == /\ pc = "x_aside"
          /\ IF tree["x"] # None
               THEN /\ aside' = [aside EXCEPT !["x"] = tree["x"]]
                    /\ tree' = [tree EXCEPT !["x"] = None]
               ELSE UNCHANGED <<aside, tree>>
          /\ Goto("x_removing")
          /\ UNCHANGED <<dirs, stage, folder, journalFile, journal, here>>

\* The folder loop: stops where the folder is gone; `Removing` is recorded first.
XRemoving == /\ pc = "x_removing"
             /\ IF "d" \in dirs
                  THEN Record(Step("removing", "d", None)) /\ Goto("x_rmdir")
                  ELSE UNCHANGED journal /\ Goto("y_check")
             /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, here>>

\* `fs::remove_dir` fails on a folder that holds a file: the loop stops.
XRmdir == /\ pc = "x_rmdir"
          /\ IF FilesIn("d") = {}
               THEN dirs' = dirs \ {"d"} /\ Goto("x_removed")
               ELSE UNCHANGED dirs /\ Goto("y_check")
          /\ UNCHANGED <<tree, stage, aside, folder, journalFile, journal, here>>

XRemoved == /\ pc = "x_removed"
            /\ Record(Step("removed", "d", None))
            /\ Goto("y_check")
            /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, here>>

----------------------------------------------------------------------------
(* The writes: n/y added, then r replaced. Both were staged up front, so   *)
(* neither is converted again in `apply`.                                  *)

\* `taken`: an added path where something stands now is left.
YCheck == /\ pc = "y_check"
          /\ Goto(IF tree["y"] # None THEN "r_check" ELSE "y_mkdir")
          /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal, here>>

\* `place`: each missing folder is made, then recorded `Made`. With Fix, the
\* record comes first, as `Removing` does.
YMkdir == /\ pc = "y_mkdir"
          /\ IF "n" \in dirs
               THEN UNCHANGED <<dirs, journal>> /\ Goto("y_placing")
             ELSE IF Fix
               THEN Record(Step("made", "n", None)) /\ UNCHANGED dirs /\ Goto("y_made")
               ELSE dirs' = dirs \cup {"n"} /\ UNCHANGED journal /\ Goto("y_made")
          /\ UNCHANGED <<tree, stage, aside, folder, journalFile, here>>

YMade == /\ pc = "y_made"
         /\ IF Fix
              THEN dirs' = dirs \cup {"n"} /\ UNCHANGED journal
              ELSE Record(Step("made", "n", None)) /\ UNCHANGED dirs
         /\ Goto("y_placing")
         /\ UNCHANGED <<tree, stage, aside, folder, journalFile, here>>

YPlacing == /\ pc = "y_placing"
            /\ Record(Step("placing", "y", stage["y"]))
            /\ Goto("y_move")
            /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, here>>

\* `move_file`: `fs::rename`, which replaces whatever stands there now.
YMove == /\ pc = "y_move"
         /\ "n" \in dirs   \* else the rename fails, and `Fail` undoes
         /\ tree' = [tree EXCEPT !["y"] = stage["y"]]
         /\ stage' = [stage EXCEPT !["y"] = None]
         /\ Goto("y_placed")
         /\ UNCHANGED <<dirs, aside, folder, journalFile, journal, here>>

\* `Stamp::of` the placed file, then the `Placed` record.
YPlaced == /\ pc = "y_placed"
           /\ tree["y"] # None
           /\ Record(Step("placed", "y", tree["y"]))
           /\ Goto("r_check")
           /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, here>>

RCheck == /\ pc = "r_check"
          /\ here' = (tree["r"] # None)
          /\ Goto(IF ChangedSinceRead("r") THEN "ask" ELSE "r_aside")
          /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal>>

RAside == /\ pc = "r_aside"
          /\ IF here /\ tree["r"] # None
               THEN /\ aside' = [aside EXCEPT !["r"] = tree["r"]]
                    /\ tree' = [tree EXCEPT !["r"] = None]
               ELSE UNCHANGED <<aside, tree>>
          /\ Goto("r_placing")
          /\ UNCHANGED <<dirs, stage, folder, journalFile, journal, here>>

RPlacing == /\ pc = "r_placing"
            /\ Record(Step("placing", "r", stage["r"]))
            /\ Goto("r_move")
            /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, here>>

RMove == /\ pc = "r_move"
         /\ tree' = [tree EXCEPT !["r"] = stage["r"]]
         /\ stage' = [stage EXCEPT !["r"] = None]
         /\ Goto("r_placed")
         /\ UNCHANGED <<dirs, aside, folder, journalFile, journal, here>>

RPlaced == /\ pc = "r_placed"
           /\ tree["r"] # None
           /\ Record(Step("placed", "r", tree["r"]))
           /\ Goto("ask")
           /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, here>>

ApplyStep == XCheck \/ XAside \/ XRemoving \/ XRmdir \/ XRemoved
          \/ YCheck \/ YMkdir \/ YMade \/ YPlacing \/ YMove \/ YPlaced
          \/ RCheck \/ RAside \/ RPlacing \/ RMove \/ RPlaced

ApplyAction ==
    /\ ApplyStep
    /\ UNCHANGED <<idx, placed, removed, failed, pending, target, unchanged, owned,
                   lastUser, result, reported, crashes, userWrites, nextId>>

----------------------------------------------------------------------------
(* The end of `restore`: `keep` is asked once every file is in place.      *)

BeginUndo ==
    /\ idx' = Len(journal)
    /\ placed' = {} /\ removed' = {} /\ failed' = FALSE /\ target' = "none"
    /\ Goto("u_step")

\* The calls in `apply` that can fail and end it with `?`: a rename (`set_aside`,
\* `move_file`, also into a folder that is gone), `fs::create_dir`, `digest` of the
\* staged file before its `Placing`, `Stamp::of` the placed file before its
\* `Placed`. Journal writes are assumed to succeed: `record` keeps a step in memory
\* before writing it, so a failed write would still be undone in the same run.
FailLabels == {"x_aside", "y_placing", "y_move", "y_placed",
               "r_aside", "r_placing", "r_move", "r_placed"}
              \cup (IF Fix THEN {"y_made"} ELSE {"y_mkdir"})

\* A failed call, or a cancel or "no" from `keep`: `restore` undoes from its
\* in-memory steps, the same as the journal's.
Fail ==
    /\ pc \in FailLabels \cup {"ask"}
    /\ BeginUndo
    /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal, here,
                   pending, unchanged, owned, lastUser, result, reported, crashes,
                   userWrites, nextId>>

\* `keep` says yes: `KEPT` is written before the folder goes. From here the
\* originals aside are given up by the user's choice; a file someone else wrote
\* is not.
Keep == /\ pc = "ask"
        /\ Goto("k_kept")
        /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal, here,
                       idx, placed, removed, failed, pending, target, unchanged,
                       owned, lastUser, result, reported, crashes, userWrites, nextId>>

KeptRecord == /\ pc = "k_kept"
              /\ Record(Step("kept", "root", None))
              /\ owned' = {f \in owned : f.who = "user" \/ InTree(f)}
              /\ Goto("k_rm")
              /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, here, idx,
                             placed, removed, failed, pending, target, unchanged,
                             lastUser, result, reported, crashes, userWrites, nextId>>

\* `Work`'s drop: the journal, then the folder with stage and aside.
KeptRemove == /\ pc = "k_rm"
              /\ folder' = FALSE /\ journalFile' = FALSE /\ journal' = <<>>
              /\ stage' = NoFiles /\ aside' = NoFiles
              /\ result' = "kept"
              /\ Goto("done")
              /\ UNCHANGED <<tree, dirs, here, idx, placed, removed, failed, pending,
                             target, unchanged, owned, lastUser, reported, crashes,
                             userWrites, nextId>>

----------------------------------------------------------------------------
(* `Journal::undo`: every step newest first, then `put_aside_back`.        *)

UStep ==
    /\ pc = "u_step"
    /\ IF idx = 0
         THEN /\ pending' = {p \in Paths : aside[p] # None}   \* `fs::read_dir(aside)`
              /\ Goto("u_aside")
              /\ UNCHANGED <<tree, dirs, idx, placed, removed, target>>
         ELSE LET s == journal[idx]
                  \* Placed: removed only while its stamp is the one placed.
                  \* Placing: a crash came as the file moved; there, it is the staged
                  \* file, unless a Placed step for it was undone already.
                  unlink ==
                    \/ /\ s.kind = "placed"
                       /\ \/ tree[s.path] = s.stamp
                          \/ Mutation = "placed-undo-ignores-stamp" /\ tree[s.path] # None
                    \/ /\ s.kind = "placing"
                       /\ s.path \notin placed /\ tree[s.path] = s.stamp
                       /\ Mutation # "placing-not-undone"
              IN
              /\ placed' = IF s.kind = "placed" THEN placed \cup {s.path} ELSE placed
              /\ removed' = IF s.kind = "removed" THEN removed \cup {s.path} ELSE removed
              /\ dirs' =
                   CASE s.kind = "made" /\ s.path \in dirs /\ FilesIn(s.path) = {}
                          /\ Mutation # "made-not-undone" -> dirs \ {s.path}
                          \* `fs::remove_dir`: gone or not empty is fine.
                     \* `DirBuilder::create` with the mode; one that is there is fine.
                     [] s.kind = "removing" -> dirs \cup {s.path}
                     [] OTHER -> dirs
              /\ IF unlink
                   THEN target' = s.path /\ Goto("u_unlink") /\ UNCHANGED idx
                   ELSE idx' = idx - 1 /\ UNCHANGED <<target, pc>>
              /\ UNCHANGED <<tree, pending>>
    /\ UNCHANGED <<stage, aside, folder, journalFile, journal, failed>>

\* `fs::remove_file` after the stat matched; gone by now is an error.
UUnlink == /\ pc = "u_unlink"
           /\ IF tree[target] # None
                THEN tree' = [tree EXCEPT ![target] = None] /\ UNCHANGED failed
                ELSE failed' = TRUE /\ UNCHANGED tree
           /\ idx' = idx - 1
           /\ target' = "none"
           /\ Goto("u_step")
           /\ UNCHANGED <<dirs, stage, aside, folder, journalFile, journal, placed,
                          removed, pending>>

\* `put_aside_back`: each file aside goes back where nothing stands; one that
\* cannot stays aside.
UAside == /\ pc = "u_aside"
          /\ IF pending = {}
               THEN Goto("u_holds") /\ UNCHANGED <<pending, target>>
               ELSE \E p \in pending :
                      /\ pending' = pending \ {p}
                      /\ IF tree[p] # None
                           THEN UNCHANGED target /\ Goto("u_aside")
                           ELSE target' = p /\ Goto("u_move")
          /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal, idx,
                         placed, removed, failed>>

\* `create_dir_all` then `move_file`: a rename, which replaces what stands there now.
UMove == /\ pc = "u_move"
         /\ tree' = [tree EXCEPT ![target] = aside[target]]
         /\ aside' = [aside EXCEPT ![target] = None]
         /\ dirs' = (dirs \cup {Parent[target]}) \ {"root"}
         /\ target' = "none"
         /\ Goto("u_aside")
         /\ UNCHANGED <<stage, folder, journalFile, journal, idx, placed, removed,
                        failed, pending>>

UHolds == /\ pc = "u_holds"
          /\ failed' = (failed \/ (AsideHolds /\ Mutation # "undo-forgets-aside"))
          /\ Goto(IF Fix /\ Mutation # "notice-after-journal" THEN "u_notice"
                  ELSE "u_rmjournal")
          /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal, idx,
                         placed, removed, pending, target>>

\* Fix only, the chosen fix for TOB-352: while a file stays aside, a notice saying
\* where is saved before the journal goes, and every launch shows it until the
\* user deals with the file.
UNotice == /\ pc = "u_notice"
           /\ reported' = (reported \/ failed)
           /\ Goto(IF Mutation = "notice-after-journal" THEN "u_end" ELSE "u_rmjournal")
           /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal, idx,
                          placed, removed, failed, pending, target, here, unchanged,
                          owned, lastUser, result, crashes, userWrites, nextId>>

\* The journal goes.
URmJournal == /\ pc = "u_rmjournal"
              /\ journalFile' = FALSE /\ journal' = <<>>
              /\ Goto(IF Fix /\ Mutation = "notice-after-journal" THEN "u_notice"
                      ELSE "u_end")
              /\ UNCHANGED <<tree, dirs, stage, aside, folder, idx, placed, removed,
                             failed, pending, target>>

\* Failed: stage goes, aside stays and its path is returned to the user.
\* Otherwise `Work`'s drop removes the folder.
UEnd == /\ pc = "u_end"
        /\ IF failed
             THEN /\ stage' = NoFiles
                  /\ UNCHANGED <<aside, folder>>
             ELSE /\ folder' = FALSE /\ stage' = NoFiles /\ aside' = NoFiles
        /\ Goto("done")
        /\ UNCHANGED <<tree, dirs, journalFile, journal, idx, placed, removed, failed,
                       pending, target>>

UndoStep == UStep \/ UUnlink \/ UAside \/ UMove \/ UHolds \/ URmJournal

UndoAction ==
    \/ /\ UndoStep
       /\ UNCHANGED <<here, unchanged, owned, lastUser, result, reported, crashes,
                      userWrites, nextId>>
    \/ UNotice
    \* The undo returns: Ok, or Err with the folder that keeps files aside.
    \/ /\ pc = "u_end"
       /\ UEnd
       /\ result' = IF failed THEN "err" ELSE "ok"
       /\ reported' = (reported \/ failed)
       /\ UNCHANGED <<here, unchanged, owned, lastUser, crashes, userWrites, nextId>>

----------------------------------------------------------------------------
(* Keystone stops, and the next launch.                                    *)

\* A crash, a kill or a power-cut-free stop: what is on disk stays, what was in
\* memory goes. Journal writes reach the file in order (no power loss).
Crash ==
    /\ pc \notin {"down", "done"}
    /\ crashes < MaxCrashes
    /\ crashes' = crashes + 1
    /\ Goto("down")
    /\ target' = "none" /\ pending' = {}
    /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal, here, idx,
                   placed, removed, failed, unchanged, owned, lastUser, result,
                   reported, userWrites, nextId>>

\* `put_back`: the user takes the offer. Its `Journal` starts from the steps the
\* journal file holds.
PutBack ==
    /\ pc = "down"
    /\ Offered
    /\ BeginUndo
    /\ UNCHANGED <<tree, dirs, stage, aside, folder, journalFile, journal, here,
                   pending, unchanged, owned, lastUser, result, reported, crashes,
                   userWrites, nextId>>

\* `Work::unfinished(folder, true)` on a kept restore's folder removes it.
Tidy ==
    /\ pc = "down"
    /\ folder /\ journalFile /\ Kept(journal)
    /\ folder' = FALSE /\ journalFile' = FALSE /\ journal' = <<>>
    /\ stage' = NoFiles /\ aside' = NoFiles
    /\ UNCHANGED <<tree, dirs, pc, here, idx, placed, removed, failed, pending, target,
                   unchanged, owned, lastUser, result, reported, crashes, userWrites,
                   nextId>>

----------------------------------------------------------------------------
(* Someone else writes a whole file at p, making its folder if needed: an  *)
(* editor's save, a formatter, the user's shell. Keystone refuses to       *)
(* restore while a turn runs in the repository, so this is never an agent. *)

UserMayWrite == Races \/ pc \in {"ask", "down", "done"}

UserWrite(p) ==
    /\ UserMayWrite
    /\ userWrites < MaxUserWrites
    /\ LET f == [who |-> "user", id |-> nextId] IN
         /\ tree' = [tree EXCEPT ![p] = f]
         /\ owned' = (owned \ {tree[p]}) \cup {f}
         /\ lastUser' = [lastUser EXCEPT ![p] = f]
    /\ dirs' = (dirs \cup {Parent[p]}) \ {"root"}
    /\ userWrites' = userWrites + 1
    /\ nextId' = nextId + 1
    /\ UNCHANGED <<stage, aside, folder, journalFile, journal, pc, here, idx, placed,
                   removed, failed, pending, target, unchanged, result, reported,
                   crashes>>

\* Stopped with nothing offered, or done: nothing more happens.
Rest == pc \in {"down", "done"} /\ UNCHANGED vars

Next ==
    \/ ApplyAction
    \/ Fail \/ Keep \/ KeptRecord \/ KeptRemove
    \/ UndoAction
    \/ Crash \/ PutBack \/ Tidy
    \/ \E p \in Paths : UserWrite(p)
    \/ Rest

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* The rules.                                                              *)

\* No file is lost: every original the user has not given up by keeping the
\* restore, and every file someone else wrote, is in the working tree or aside.
NothingLost == \A f \in owned : InTree(f) \/ InAside(f)

\* A file someone else wrote stays where they wrote it whenever Keystone is not
\* in the middle of moving files.
OthersWritesStay ==
    pc \in {"ask", "down", "done"} =>
        \A p \in Paths : lastUser[p] # None => tree[p] = lastUser[p]

\* A cancel or put back that reports success leaves every file as it was before
\* the restore, except those someone else wrote since.
Expected(p) == IF lastUser[p] # None THEN lastUser[p]
               ELSE [x |-> OrigX, y |-> None, r |-> OrigR][p]
PutBackRestoresFiles ==
    (pc = "done" /\ result = "ok") => \A p \in Paths : tree[p] = Expected(p)

\* ... and every folder: those it removed are back, and those it made are gone
\* unless someone else wrote a file into them.
ExpectedDirs == ({"d"} \cup {Parent[p] : p \in {q \in Paths : lastUser[q] # None}})
                \ {"root"}
PutBackRestoresFolders ==
    (pc = "done" /\ result = "ok") => dirs = ExpectedDirs

\* Files left aside are never forgotten: while Keystone is stopped or done, any
\* file aside that someone must get back is offered for a put back, or Keystone
\* told the user where it is.
AsideNeverForgotten ==
    pc \in {"down", "done"} =>
        ((\E f \in owned : InAside(f)) => (Offered \/ reported))
=============================================================================
