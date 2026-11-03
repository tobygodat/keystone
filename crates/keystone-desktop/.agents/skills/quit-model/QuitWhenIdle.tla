---------------------------- MODULE QuitWhenIdle ----------------------------
(***************************************************************************)
(* How Keystone quits on its own after its last window closes.            *)
(*                                                                         *)
(* Closing the last window while a turn runs keeps Keystone up with no    *)
(* window, waiting to quit (`AppState.quit_when_idle`). Every `publish`   *)
(* runs `quit_if_idle`, which defers `request_quit` once no turn runs.    *)
(* Meanwhile provider events arrive through `AppState::pump`, a launched  *)
(* session's result goes to its parent through `deliver_results`, and the *)
(* scheduler may start a run.                                              *)
(*                                                                         *)
(* Each action below is one GPUI update on the UI thread, so it is atomic. *)
(* `cx.defer` queues a callback that GPUI runs only after the outermost    *)
(* update ends (tGPUI `App::finish_update` -> `flush_effects`), in FIFO    *)
(* order and before any other update starts. `deferred` is that queue, and *)
(* every action but `RunDeferred` and the provider steps waits for it to   *)
(* be empty. Provider steps happen on other threads and only queue events  *)
(* for the next pump.                                                      *)
(*                                                                         *)
(* Paths are relative to crates/keystone-desktop/src. The coverage table  *)
(* in SKILL.md lists every function this model stands for.                 *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Parent,   \* a session whose turn launched Child, then ended
    Child,    \* the launched session; its result goes to Parent
    Task,     \* the target session of a scheduled task
    Fix,      \* TRUE: model a candidate fix (see RequestQuitDeferred)
    Mutation  \* a deliberate break, to prove a property can fail; "none" normally

Sessions == {Parent, Child, Task}

Mutations == {"none",
              "close-ignores-turns",     \* main.rs close_window skips its turn_running() check
              "scheduler-ignores-wait",  \* scheduled.rs quitting() forgets quit_when_idle
              "close-forgets-wait"}      \* main.rs close_window never sets quit_when_idle

ASSUME Fix \in BOOLEAN /\ Mutation \in Mutations

VARIABLES
    phase,         \* AppState.phase: "Running", "Saving" or "Exiting"
    quitWhenIdle,  \* AppState.quit_when_idle
    windows,       \* open Keystone windows (open_windows): 0 or 1
    running,       \* [session -> Conversation.running]
    asking,        \* [session -> the running turn awaits an approval answer]
    inbox,         \* [session -> provider event not yet drained: "none", "ask" or "end"]
    delivery,      \* Child's DeliveryState: "none", "Working", "Ready" or "Delivered"
    deferred,      \* callbacks queued with cx.defer, oldest first
    cutOff,        \* sessions whose running turn a quit stopped that the user did not ask for
    scheduledWhileWaiting  \* a scheduled run started while Keystone had no window

vars == <<phase, quitWhenIdle, windows, running, asking, inbox, delivery,
          deferred, cutOff, scheduledWhileWaiting>>

TurnRunning(run) == \E s \in Sessions : run[s]          \* AppState::turn_running
Asking(run, ask) == \E s \in Sessions : run[s] /\ ask[s] \* AppState::asking

RequestQuitCb == [kind |-> "request_quit"]
ShowApprovalCb == [kind |-> "show_approval"]
SaveCb(by) == [kind |-> "save_and_teardown", by |-> by]

(***************************************************************************)
(* app_state.rs quit_if_idle, run by every publish/refresh. It works on    *)
(* the values an update has reached so far, so it takes them as arguments *)
(* and returns the new quit_when_idle and deferred queue.                  *)
(***************************************************************************)
QuitIfIdle(ph, qwi, run, ask, d) ==
    IF ~qwi \/ ph # "Running" THEN [qwi |-> qwi, d |-> d]
    ELSE IF ~TurnRunning(run)
         THEN [qwi |-> FALSE, d |-> Append(d, RequestQuitCb)]
    ELSE IF Asking(run, ask)
         THEN [qwi |-> FALSE, d |-> Append(d, ShowApprovalCb)]
    ELSE [qwi |-> qwi, d |-> d]

(***************************************************************************)
(* main.rs request_quit -> app_state.rs reserve_quit, called in an update. *)
(* Returns the new phase and deferred queue.                               *)
(***************************************************************************)
RequestQuit(ph, d, by) ==
    IF ph # "Running" THEN [ph |-> ph, d |-> d]
    ELSE [ph |-> "Saving", d |-> Append(d, SaveCb(by))]

Idle == deferred = <<>>   \* no deferred callback is waiting, so a new update may start

-----------------------------------------------------------------------------
(* The user. *)

\* main.rs close_window on the last window. A refusal changes nothing, so it is
\* left out. Saving the draft may fail and refuse too; that is left out as well.
CloseLastWindow ==
    /\ Idle /\ phase = "Running" /\ windows = 1
    /\ ~Asking(running, asking)                      \* "Answer the approval request..."
    /\ IF ~TurnRunning(running) \/ Mutation = "close-ignores-turns"
       THEN LET q == RequestQuit(phase, deferred, "keystone")
            IN /\ phase' = q.ph /\ deferred' = q.d
               /\ UNCHANGED <<quitWhenIdle, windows>>
       ELSE /\ windows' = 0
            /\ quitWhenIdle' = (Mutation # "close-forgets-wait")   \* main.rs:350
            /\ UNCHANGED <<phase, deferred>>
    /\ UNCHANGED <<running, asking, inbox, delivery, cutOff, scheduledWhileWaiting>>

\* The Quit action (Ctrl+Q, the macOS menu or Dock): main.rs request_quit.
UserQuits ==
    /\ Idle /\ phase = "Running"
    /\ LET q == RequestQuit(phase, deferred, "user")
       IN phase' = q.ph /\ deferred' = q.d
    /\ UNCHANGED <<quitWhenIdle, windows, running, asking, inbox, delivery, cutOff,
                   scheduledWhileWaiting>>

\* windows.rs reopen: a Dock click, a notification click, the quick composer or a
\* second launch while no window is open.
UserReopens ==
    /\ Idle /\ phase = "Running" /\ windows = 0
    /\ windows' = 1 /\ quitWhenIdle' = FALSE
    /\ UNCHANGED <<phase, running, asking, inbox, delivery, deferred, cutOff,
                   scheduledWhileWaiting>>

\* app_state.rs send_message_from, from the composer: the turn shows as running at
\* once (conversation.rs push_sent). Its inner pump is left out: with a window
\* open, quit_when_idle is off, so that pump cannot start a quit.
UserSends(s) ==
    /\ Idle /\ phase = "Running" /\ windows = 1
    /\ s \in {Parent, Task} /\ ~running[s]
    /\ running' = [running EXCEPT ![s] = TRUE]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, asking, inbox, delivery, deferred,
                   cutOff, scheduledWhileWaiting>>

UserAnswers(s) ==
    /\ Idle /\ phase = "Running" /\ windows = 1 /\ asking[s]
    /\ asking' = [asking EXCEPT ![s] = FALSE]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, inbox, delivery, deferred,
                   cutOff, scheduledWhileWaiting>>

-----------------------------------------------------------------------------
(* The providers, on their own threads. *)

ProviderEndsTurn(s) ==
    /\ phase # "Exiting" /\ running[s] /\ inbox[s] = "none"
    /\ inbox' = [inbox EXCEPT ![s] = "end"]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, delivery, deferred,
                   cutOff, scheduledWhileWaiting>>

ProviderAsks(s) ==
    /\ phase # "Exiting" /\ running[s] /\ ~asking[s] /\ inbox[s] = "none"
    /\ inbox' = [inbox EXCEPT ![s] = "ask"]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, delivery, deferred,
                   cutOff, scheduledWhileWaiting>>

-----------------------------------------------------------------------------
(* Keystone's own updates. *)

\* Parent's running turn calls the orchestrator's session_launch (orchestrator.rs),
\* which saves a Working delivery and starts Child's turn.
ParentLaunchesChild ==
    /\ Idle /\ phase = "Running"
    /\ running[Parent] /\ delivery = "none" /\ ~running[Child]
    /\ delivery' = "Working"
    /\ running' = [running EXCEPT ![Child] = TRUE]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, asking, inbox, deferred, cutOff,
                   scheduledWhileWaiting>>

(***************************************************************************)
(* app_state.rs pump, one update, in its real order:                      *)
(*   1. receive applies the drained events; a turn end clears running and  *)
(*      asking (conversation.rs apply); Child's end saves its result as    *)
(*      Ready (receive -> save_results -> persist, saved at once).        *)
(*   2. receive ends with publish -> quit_if_idle.                         *)
(*   3. deliver_results (delivery.rs): when phase is Running and Parent    *)
(*      holds nothing back (result_hold), it saves Delivered and sends,    *)
(*      which starts Parent's turn at once (push_sent).                    *)
(*   4. deliver_results ends with refresh -> quit_if_idle.                 *)
(* The pump returns at once when exiting. The store is assumed to save.   *)
(***************************************************************************)
Pump ==
    /\ Idle /\ phase # "Exiting"
    /\ LET ended == {s \in Sessions : inbox[s] = "end"}
           asked == {s \in Sessions : inbox[s] = "ask"}
           run1 == [s \in Sessions |-> running[s] /\ s \notin ended]
           ask1 == [s \in Sessions |-> (asking[s] \/ s \in asked) /\ s \notin ended]
           del1 == IF Child \in ended /\ delivery = "Working" THEN "Ready" ELSE delivery
           q1 == QuitIfIdle(phase, quitWhenIdle, run1, ask1, deferred)
           deliver == phase = "Running" /\ del1 = "Ready"
                      /\ ~run1[Parent] /\ ~ask1[Parent]
           run2 == IF deliver THEN [run1 EXCEPT ![Parent] = TRUE] ELSE run1
           q2 == QuitIfIdle(phase, q1.qwi, run2, ask1, q1.d)
       IN /\ running' = run2
          /\ asking' = ask1
          /\ delivery' = IF deliver THEN "Delivered" ELSE del1
          /\ quitWhenIdle' = q2.qwi
          /\ deferred' = q2.d
    /\ inbox' = [s \in Sessions |-> "none"]
    /\ UNCHANGED <<phase, windows, cutOff, scheduledWhileWaiting>>

\* scheduled.rs check_schedules -> run_task -> start_run: runs a due task unless
\* quitting() (phase not Running, or quit_when_idle). The run starts a turn at once.
ScheduledRun ==
    /\ Idle /\ phase = "Running" /\ ~running[Task]
    /\ IF Mutation = "scheduler-ignores-wait" THEN TRUE ELSE ~quitWhenIdle
    /\ running' = [running EXCEPT ![Task] = TRUE]
    /\ scheduledWhileWaiting' = (scheduledWhileWaiting \/ windows = 0)
    /\ UNCHANGED <<phase, quitWhenIdle, windows, asking, inbox, delivery, deferred,
                   cutOff>>

-----------------------------------------------------------------------------
(* Deferred callbacks, run one at a time when the update that queued them ends. *)

\* The callback quit_if_idle queues: main.rs request_quit. With Fix, it first
\* checks again whether a turn runs, and if one does, waits to quit again.
RequestQuitDeferred(rest) ==
    IF Fix /\ TurnRunning(running)
    THEN /\ quitWhenIdle' = TRUE /\ deferred' = rest
         /\ UNCHANGED <<phase, windows, cutOff>>
    ELSE LET q == RequestQuit(phase, rest, "keystone")
         IN /\ phase' = q.ph /\ deferred' = q.d
            /\ UNCHANGED <<quitWhenIdle, windows, cutOff>>

\* The callback quit_if_idle queues for a turn asking for approval:
\* notifications.rs show -> windows.rs reopen, which opens the saved windows and
\* clears quit_when_idle. Opening a window is assumed to succeed.
ShowApprovalDeferred(rest) ==
    /\ deferred' = rest
    /\ IF phase = "Running"
       THEN windows' = 1 /\ quitWhenIdle' = FALSE
       ELSE windows' = 0 /\ quitWhenIdle' = TRUE
    /\ UNCHANGED <<phase, cutOff>>

\* main.rs save_and_teardown. Either the save works and teardown stops every
\* provider (app_state.rs teardown: phase Exiting, connections cleared), or a save
\* fails: quit_failed sets phase Running and publishes, then windows.rs reopen
\* opens the windows again if none is open.
SaveAndTeardownDeferred(cb, rest) ==
    \/ /\ phase' = "Exiting" /\ deferred' = rest
       /\ cutOff' = IF cb.by = "user" THEN cutOff
                    ELSE cutOff \cup {s \in Sessions : running[s]}
       /\ UNCHANGED <<quitWhenIdle, windows>>
    \/ LET q == QuitIfIdle("Running", quitWhenIdle, running, asking, rest)
       IN /\ phase' = "Running"
          /\ deferred' = q.d
          /\ windows' = 1
          /\ quitWhenIdle' = IF windows = 0 THEN FALSE ELSE q.qwi
          /\ UNCHANGED cutOff

RunDeferred ==
    /\ deferred # <<>>
    /\ LET cb == Head(deferred)
           rest == Tail(deferred)
       IN CASE cb.kind = "request_quit"      -> RequestQuitDeferred(rest)
            [] cb.kind = "show_approval"     -> ShowApprovalDeferred(rest)
            [] cb.kind = "save_and_teardown" -> SaveAndTeardownDeferred(cb, rest)
    /\ UNCHANGED <<running, asking, inbox, delivery, scheduledWhileWaiting>>

\* Keystone has exited. Nothing more happens; this keeps TLC from calling it a deadlock.
Exited == phase = "Exiting" /\ deferred = <<>> /\ UNCHANGED vars

-----------------------------------------------------------------------------

Init ==
    /\ phase = "Running"
    /\ quitWhenIdle = FALSE
    /\ windows = 1
    /\ running = [s \in Sessions |-> FALSE]
    /\ asking = [s \in Sessions |-> FALSE]
    /\ inbox = [s \in Sessions |-> "none"]
    /\ delivery = "none"
    /\ deferred = <<>>
    /\ cutOff = {}
    /\ scheduledWhileWaiting = FALSE

Next ==
    \/ CloseLastWindow \/ UserQuits \/ UserReopens
    \/ \E s \in Sessions : UserSends(s) \/ UserAnswers(s)
    \/ \E s \in Sessions : ProviderEndsTurn(s) \/ ProviderAsks(s)
    \/ ParentLaunchesChild \/ Pump \/ ScheduledRun
    \/ RunDeferred \/ Exited

\* Turns end, the pump drains what arrives, and deferred callbacks run. The user
\* and the scheduler are never forced to act.
Fairness ==
    /\ WF_vars(Pump)
    /\ WF_vars(RunDeferred)
    /\ \A s \in Sessions : WF_vars(ProviderEndsTurn(s))

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* The rules. *)

TypeOK ==
    /\ phase \in {"Running", "Saving", "Exiting"}
    /\ quitWhenIdle \in BOOLEAN /\ windows \in {0, 1}
    /\ running \in [Sessions -> BOOLEAN] /\ asking \in [Sessions -> BOOLEAN]
    /\ inbox \in [Sessions -> {"none", "ask", "end"}]
    /\ delivery \in {"none", "Working", "Ready", "Delivered"}
    /\ Len(deferred) <= 2 /\ cutOff \subseteq Sessions

\* Keystone stops a running turn only when the user chooses Quit.
OnlyQuitStopsTurns == cutOff = {}

\* No scheduled run starts while Keystone has no window and waits to quit.
NoScheduledRunWhileWaiting == ~scheduledWhileWaiting

\* Whenever Keystone has no window and is not quitting, it is waiting to quit.
WindowlessMeansWaiting ==
    (Idle /\ windows = 0 /\ phase = "Running") => quitWhenIdle

\* With no window, Keystone eventually exits or shows a window again.
WindowlessEventuallyEnds ==
    (windows = 0 /\ phase = "Running") ~> (phase = "Exiting" \/ windows = 1)

=============================================================================
