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
(* for the next pump. A pump runs only when something wakes it: a queued   *)
(* event, or a connection asking Keystone to check again.                  *)
(*                                                                         *)
(* A session on another machine (TOB-441) runs its turn there. Keystone    *)
(* learns of its end only while the machine's link is up, and the link may *)
(* drop and stay down. Quit leaves that turn running unless the user chose *)
(* Quit and the machine is set to end its sessions.                        *)
(*                                                                         *)
(* Paths are relative to crates/keystone-desktop/src. The coverage table  *)
(* in SKILL.md lists every function this model stands for.                 *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Parent,   \* a session whose turn launched Child, then ended
    Child,    \* the launched session; its result goes to Parent
    Task,     \* the target session of a scheduled task
    Mutation  \* a deliberate break, to prove a property can fail; "none" normally

Sessions == {Parent, Child, Task}

Mutations == {"none",
              "close-ignores-turns",      \* main.rs close_window skips its has_work() check
              "quit-skips-recheck",       \* quit_if_idle's deferred callback quits without checking again
              "turns-ignore-connections", \* has_work reads only Conversation.running
              "work-ignores-queued",      \* has_work skips a send the worker has not taken
              "work-ignores-result",     \* has_work skips a result waiting for its durable save
              "result-save-no-wake",     \* a durable Delivered save wakes no pump
              "work-ignores-steer",       \* has_work skips a steer Codex has not answered
              "steer-settles-silently",   \* an accepted late steer does not wake Keystone
              "scheduler-ignores-wait",   \* scheduled.rs quitting() forgets quit_when_idle
              "close-forgets-wait",       \* main.rs close_window never sets quit_when_idle
              "quit-forgets-who",         \* app_state.rs reserve_quit never sets idle_quit
              "work-counts-remote"}       \* has_work counts a remote session's turn

ASSUME Mutation \in Mutations

VARIABLES
    phase,         \* AppState.phase: "Running", "Saving" or "Exiting"
    quitWhenIdle,  \* AppState.quit_when_idle
    windows,       \* open Keystone windows (open_windows): 0 or 1
    running,       \* [session -> Conversation.running]
    asking,        \* [session -> the running turn awaits an approval answer]
    inbox,         \* [session -> provider event not yet drained: "none", "ask" or "end"]
    followUp,      \* [session -> where a message sent during a turn is:
                   \*  "none";
                   \*  "queued", accepted by send but not yet taken by the worker;
                   \*  "steer", a steer Codex has not answered;
                   \*  "held", kept by the provider until Keystone applies the turn's end;
                   \*  "start", a turn the connection started that no pump applied]
    woken,         \* a connection asked for a pump with no event to report
    delivery,      \* Child's DeliveryState: "none", "Working", "Ready" or "Delivered"
    deferred,      \* callbacks queued with cx.defer, oldest first
    cutOff,        \* sessions whose running turn a quit stopped that the user did not ask for
    scheduledWhileWaiting, \* a scheduled run started while Keystone had no window
    idleQuit,      \* AppState.idle_quit: the quit under way is not one the user chose
    remoteRunning, \* a turn runs on the other machine, whatever Keystone knows
    remoteShown,   \* the remote session's Conversation.running
    linkUp,        \* the machine's link is Connected
    endOnQuit,     \* the machine's "End sessions on this machine when Keystone quits"
    remoteCutOff   \* a quit the user did not choose ended the remote turn

remoteVars == <<remoteRunning, remoteShown, linkUp, endOnQuit, remoteCutOff>>

vars == <<phase, quitWhenIdle, windows, running, asking, inbox, followUp, woken,
          delivery, deferred, cutOff, scheduledWhileWaiting, idleQuit, remoteVars>>

\* Every action that leaves the remote session and the quit's reason alone.
Keep == UNCHANGED <<idleQuit, remoteVars>>

\* AppState::runs_turn: its conversation shows a turn, or its connection runs one
\* (Connection::is_running), which counts a held follow-up.
Runs(run, fu, s) == run[s] \/ fu[s] \in {"held", "start"}

\* Connection::has_work: what is_running counts, a send still queued for the worker
\* (`queued` in connection.rs), or a steer Codex may reject (codex.rs has_work).
HasWork(fu, s) ==
    \/ fu[s] \in {"held", "start"}
    \/ fu[s] = "queued" /\ Mutation # "work-ignores-queued"
    \/ fu[s] = "steer" /\ Mutation # "work-ignores-steer"

\* AppState::has_work. It leaves out a session on another machine (`is_remote`),
\* since Quit leaves its turn running; `rs` is that session's Conversation.running.
TurnRunning(run, fu, del, rs) ==
    \/ del \in {"Saving", "Saved"} /\ Mutation # "work-ignores-result"
    \/ \E s \in Sessions : run[s] \/ (HasWork(fu, s) /\ Mutation # "turns-ignore-connections")
    \/ rs /\ Mutation = "work-counts-remote"
Asking(run, ask) == \E s \in Sessions : run[s] /\ ask[s] \* AppState::asking

RequestQuitCb == [kind |-> "request_quit"]
ShowApprovalCb == [kind |-> "show_approval"]
SaveCb(by) == [kind |-> "save_and_teardown", by |-> by]

(***************************************************************************)
(* app_state.rs quit_if_idle, run by every publish/refresh. It works on    *)
(* the values an update has reached so far, so it takes them as arguments *)
(* and returns the new quit_when_idle and deferred queue.                  *)
(***************************************************************************)
QuitIfIdle(ph, qwi, run, fu, ask, del, d) ==
    IF ~qwi \/ ph # "Running" THEN [qwi |-> qwi, d |-> d]
    ELSE IF ~TurnRunning(run, fu, del, remoteShown)
         THEN [qwi |-> FALSE, d |-> Append(d, RequestQuitCb)]
    ELSE IF Asking(run, ask)
         THEN [qwi |-> FALSE, d |-> Append(d, ShowApprovalCb)]
    ELSE [qwi |-> qwi, d |-> d]

(***************************************************************************)
(* main.rs request_quit -> app_state.rs reserve_quit, called in an update. *)
(* Returns the new phase, deferred queue and idle_quit: reserve_quit sets  *)
(* idle_quit when the user did not choose Quit.                            *)
(***************************************************************************)
RequestQuit(ph, d, by) ==
    IF ph # "Running" THEN [ph |-> ph, d |-> d, iq |-> idleQuit]
    ELSE [ph |-> "Saving", d |-> Append(d, SaveCb(by)),
          iq |-> by # "user" /\ Mutation # "quit-forgets-who"]

Idle == deferred = <<>>   \* no deferred callback is waiting, so a new update may start

-----------------------------------------------------------------------------
(* The user. *)

\* main.rs close_window on the last window. A refusal changes nothing, so it is
\* left out. Saving the draft may fail and refuse too; that is left out as well.
CloseLastWindow ==
    /\ Idle /\ phase = "Running" /\ windows = 1
    /\ ~Asking(running, asking)                      \* "Answer the approval request..."
    /\ IF ~TurnRunning(running, followUp, delivery, remoteShown) \/ Mutation = "close-ignores-turns"
       THEN LET q == RequestQuit(phase, deferred, "keystone")
            IN /\ phase' = q.ph /\ deferred' = q.d /\ idleQuit' = q.iq
               /\ UNCHANGED <<quitWhenIdle, windows>>
       ELSE /\ windows' = 0
            /\ quitWhenIdle' = (Mutation # "close-forgets-wait")   \* main.rs close_window
            /\ UNCHANGED <<phase, deferred, idleQuit>>
    /\ UNCHANGED <<running, asking, inbox, followUp, woken, delivery, cutOff, scheduledWhileWaiting,
                   remoteVars>>

\* The Quit action (Ctrl+Q, the macOS menu or Dock): main.rs request_quit.
UserQuits ==
    /\ Idle /\ phase = "Running"
    /\ LET q == RequestQuit(phase, deferred, "user")
       IN phase' = q.ph /\ deferred' = q.d /\ idleQuit' = q.iq
    /\ UNCHANGED <<quitWhenIdle, windows, running, asking, inbox, followUp, woken, delivery, cutOff,
                   scheduledWhileWaiting, remoteVars>>

\* windows.rs reopen: a Dock click, a notification click, the quick composer or a
\* second launch while no window is open.
UserReopens ==
    /\ Idle /\ phase = "Running" /\ windows = 0
    /\ windows' = 1 /\ quitWhenIdle' = FALSE
    /\ UNCHANGED <<phase, running, asking, inbox, followUp, woken, delivery, deferred, cutOff,
                   scheduledWhileWaiting>>
    /\ Keep

\* app_state.rs send_message_from, from the composer: the turn shows as running at
\* once (conversation.rs push_sent). Its inner pump is left out: with a window
\* open, quit_when_idle is off, so that pump cannot start a quit.
UserSends(s) ==
    /\ Idle /\ phase = "Running" /\ windows = 1
    /\ s \in {Parent, Task} /\ ~Runs(running, followUp, s)
    /\ running' = [running EXCEPT ![s] = TRUE]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, asking, inbox, followUp, woken, delivery, deferred,
                   cutOff, scheduledWhileWaiting>>
    /\ Keep

\* send_message_from into Parent's running turn. The connection counts it in
\* `queued` (connection.rs send_inner) and hands it to its worker.
UserSteers ==
    /\ Idle /\ phase = "Running" /\ windows = 1
    /\ running[Parent] /\ followUp[Parent] = "none"
    /\ followUp' = [followUp EXCEPT ![Parent] = "queued"]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, inbox, woken, delivery,
                   deferred, cutOff, scheduledWhileWaiting>>
    /\ Keep

UserAnswers(s) ==
    /\ Idle /\ phase = "Running" /\ windows = 1 /\ asking[s]
    /\ asking' = [asking EXCEPT ![s] = FALSE]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, inbox, followUp, woken, delivery, deferred,
                   cutOff, scheduledWhileWaiting>>
    /\ Keep

-----------------------------------------------------------------------------
(* The providers, on their own threads. *)

ProviderEndsTurn(s) ==
    /\ phase # "Exiting" /\ running[s] /\ inbox[s] = "none"
    /\ inbox' = [inbox EXCEPT ![s] = "end"]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, followUp, woken, delivery, deferred,
                   cutOff, scheduledWhileWaiting>>
    /\ Keep

ProviderAsks(s) ==
    /\ phase # "Exiting" /\ running[s] /\ ~asking[s] /\ inbox[s] = "none"
    /\ inbox' = [inbox EXCEPT ![s] = "ask"]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, followUp, woken, delivery, deferred,
                   cutOff, scheduledWhileWaiting>>
    /\ Keep

\* Parent's provider still runs the turn the follow-up was sent into: no pump has
\* the turn's end to apply, so the provider has not ended it.
ProviderRuns == running[Parent] /\ inbox[Parent] # "end"

\* The connection's worker takes the queued follow-up (send_inner's job, which
\* unqueues it under the same lock as it hands it to the driver). Into a running
\* turn, Codex sends a steer and waits for its answer, and Claude adds it to the
\* turn. Once the turn ended, it starts the next turn, or waits for Keystone to
\* apply the end first when checkpoints hold follow-ups.
WorkerTakesFollowUp ==
    /\ phase # "Exiting" /\ followUp[Parent] = "queued"
    /\ \E next \in IF ProviderRuns THEN {"steer", "none"} ELSE {"held", "start"} :
          followUp' = [followUp EXCEPT ![Parent] = next]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, inbox, woken, delivery,
                   deferred, cutOff, scheduledWhileWaiting>>
    /\ Keep

\* Codex answers the steer (codex.rs on_response, Pending::Steer). While the turn
\* runs, it took the steer or failed with a warning. Codex reports turn/completed
\* before rejecting a steer for that turn, and the rejected message then starts
\* the next turn (send), or waits held. A steer accepted after the end changes
\* nothing and has no event, so it asks for a pump (CodexDriver::take_recheck).
CodexAnswersSteer ==
    /\ phase # "Exiting" /\ followUp[Parent] = "steer"
    /\ IF ProviderRuns
       THEN /\ followUp' = [followUp EXCEPT ![Parent] = "none"]
            /\ UNCHANGED woken
       ELSE \/ /\ \E next \in {"held", "start"} :
                     followUp' = [followUp EXCEPT ![Parent] = next]
               /\ UNCHANGED woken
            \/ /\ followUp' = [followUp EXCEPT ![Parent] = "none"]
               /\ woken' = (woken \/ Mutation # "steer-settles-silently")
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, inbox, delivery, deferred,
                   cutOff, scheduledWhileWaiting>>
    /\ Keep

\* The worker runs release_follow_ups, which checkpoints.rs turn_ended queued when a
\* pump applied the turn's end; the held message starts the next turn.
ProviderReleasesFollowUp ==
    /\ phase # "Exiting" /\ followUp[Parent] = "held"
    /\ ~running[Parent] /\ inbox[Parent] # "end"
    /\ followUp' = [followUp EXCEPT ![Parent] = "start"]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, inbox, woken, delivery,
                   deferred, cutOff, scheduledWhileWaiting>>
    /\ Keep

-----------------------------------------------------------------------------
(* Keystone's own updates. *)

\* Parent's running turn calls the orchestrator's session_launch (orchestrator.rs),
\* which saves a Working delivery and starts Child's turn.
ParentLaunchesChild ==
    /\ Idle /\ phase = "Running"
    /\ running[Parent] /\ delivery = "none" /\ ~running[Child]
    /\ delivery' = "Working"
    /\ running' = [running EXCEPT ![Child] = TRUE]
    /\ UNCHANGED <<phase, quitWhenIdle, windows, asking, inbox, followUp, woken, deferred, cutOff,
                   scheduledWhileWaiting>>
    /\ Keep

(***************************************************************************)
(* app_state.rs pump, one update, in its real order:                      *)
(*   1. receive applies the drained events in order: a turn end clears     *)
(*      running and asking (conversation.rs apply), then a follow-up's     *)
(*      start sets running. Child's end saves its result as Ready          *)
(*      (receive -> save_results -> Store, applied in memory).            *)
(*   2. receive ends with publish -> quit_if_idle.                         *)
(*   3. deliver_results (delivery.rs): when phase is Running and Parent    *)
(*      holds nothing back, it queues Delivered as Saving. A later pump   *)
(*      sends once WriterConfirmsDelivery makes it Saved.                 *)
(*   4. deliver_results ends with refresh -> quit_if_idle.                 *)
(* The pump returns at once when exiting. The store is assumed to save.   *)
(* It runs when woken: a connection queued an event (deliver wakes the UI  *)
(* when its inbox stops being empty) or asked for a check with none.      *)
(***************************************************************************)
Pump ==
    /\ Idle /\ phase # "Exiting"
    /\ woken \/ \E s \in Sessions : inbox[s] # "none" \/ followUp[s] = "start"
    /\ LET ended == {s \in Sessions : inbox[s] = "end"}
           asked == {s \in Sessions : inbox[s] = "ask"}
           started == {s \in Sessions : followUp[s] = "start"}
           run1 == [s \in Sessions |-> (running[s] /\ s \notin ended) \/ s \in started]
           ask1 == [s \in Sessions |-> (asking[s] \/ s \in asked) /\ s \notin ended]
           fu1 == [s \in Sessions |-> IF s \in started THEN "none" ELSE followUp[s]]
           del1 == IF Child \in ended /\ delivery = "Working" THEN "Ready" ELSE delivery
           q1 == QuitIfIdle(phase, quitWhenIdle, run1, fu1, ask1, del1, deferred)
           accept == phase = "Running" /\ del1 = "Ready"
                      /\ ~Runs(run1, fu1, Parent) /\ ~ask1[Parent]
           deliver == phase = "Running" /\ del1 = "Saved"
                      /\ ~Runs(run1, fu1, Parent) /\ ~ask1[Parent]
           run2 == IF deliver THEN [run1 EXCEPT ![Parent] = TRUE] ELSE run1
           del2 == IF deliver THEN "Delivered" ELSE IF accept THEN "Saving" ELSE del1
           q2 == QuitIfIdle(phase, q1.qwi, run2, fu1, ask1, del2, q1.d)
       IN /\ running' = run2
          /\ asking' = ask1
          /\ inbox' = [s \in Sessions |-> "none"]
          /\ followUp' = fu1
          /\ woken' = FALSE
          /\ delivery' = del2
          /\ quitWhenIdle' = q2.qwi
          /\ deferred' = q2.d
    /\ UNCHANGED <<phase, windows, cutOff, scheduledWhileWaiting>>
    /\ Keep

\* Store's watched Delivered revision becomes durable on the writer thread.
WriterConfirmsDelivery ==
    /\ phase # "Exiting" /\ delivery = "Saving"
    /\ delivery' = "Saved"
    /\ woken' = (woken \/ Mutation # "result-save-no-wake")
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, inbox, followUp,
                   deferred, cutOff, scheduledWhileWaiting>>
    /\ Keep

\* scheduled.rs check_schedules -> run_task -> start_run: runs a due task unless
\* quitting() (phase not Running, or quit_when_idle). The run starts a turn at once.
ScheduledRun ==
    /\ Idle /\ phase = "Running" /\ ~running[Task]
    /\ IF Mutation = "scheduler-ignores-wait" THEN TRUE ELSE ~quitWhenIdle
    /\ running' = [running EXCEPT ![Task] = TRUE]
    /\ scheduledWhileWaiting' = (scheduledWhileWaiting \/ windows = 0)
    /\ UNCHANGED <<phase, quitWhenIdle, windows, asking, inbox, followUp, woken, delivery, deferred,
                   cutOff>>
    /\ Keep

-----------------------------------------------------------------------------
(* Deferred callbacks, run one at a time when the update that queued them ends. *)

\* The callback quit_if_idle queues. The rest of its update may have started a
\* turn, as deliver_results does, so it checks again: while a turn runs it waits
\* to quit again, else it is main.rs request_quit.
RequestQuitDeferred(rest) ==
    IF TurnRunning(running, followUp, delivery, remoteShown) /\ Mutation # "quit-skips-recheck"
    THEN /\ quitWhenIdle' = TRUE /\ deferred' = rest
         /\ UNCHANGED <<phase, windows, cutOff, idleQuit, remoteVars>>
    ELSE LET q == RequestQuit(phase, rest, "keystone")
         IN /\ phase' = q.ph /\ deferred' = q.d /\ idleQuit' = q.iq
            /\ UNCHANGED <<quitWhenIdle, windows, cutOff, remoteVars>>

\* The callback quit_if_idle queues for a turn asking for approval:
\* notifications.rs show -> windows.rs reopen, which opens the saved windows and
\* clears quit_when_idle. Opening a window is assumed to succeed.
ShowApprovalDeferred(rest) ==
    /\ deferred' = rest
    /\ IF phase = "Running"
       THEN windows' = 1 /\ quitWhenIdle' = FALSE
       ELSE windows' = 0 /\ quitWhenIdle' = TRUE
    /\ UNCHANGED <<phase, cutOff>>
    /\ Keep

\* main.rs save_and_teardown. Either the save works and teardown stops every
\* provider (app_state.rs teardown: phase Exiting, connections cleared), or a save
\* fails: quit_failed sets phase Running and publishes, then windows.rs reopen
\* opens the windows again if none is open, and clears idle_quit. Teardown's
\* leave_machines tells a connected machine to end its sessions only when the
\* machine is set to and idle_quit is clear; one whose link is down hears nothing.
SaveAndTeardownDeferred(cb, rest) ==
    \/ LET ends == linkUp /\ remoteRunning /\ endOnQuit /\ ~idleQuit
       IN /\ phase' = "Exiting" /\ deferred' = rest
          /\ cutOff' = IF cb.by = "user" THEN cutOff
                       ELSE cutOff \cup {s \in Sessions : running[s] \/ followUp[s] # "none"
                                         \/ (s = Parent /\ delivery \in {"Saving", "Saved"})}
          /\ remoteRunning' = (remoteRunning /\ ~ends)
          /\ remoteCutOff' = (remoteCutOff \/ (ends /\ cb.by # "user"))
          /\ UNCHANGED <<quitWhenIdle, windows, idleQuit, remoteShown, linkUp, endOnQuit>>
    \/ LET q == QuitIfIdle("Running", quitWhenIdle, running, followUp, asking, delivery, rest)
       IN /\ phase' = "Running"
          /\ deferred' = q.d
          /\ windows' = 1
          /\ quitWhenIdle' = IF windows = 0 THEN FALSE ELSE q.qwi
          /\ idleQuit' = FALSE
          /\ UNCHANGED <<cutOff, remoteVars>>

RunDeferred ==
    /\ deferred # <<>>
    /\ LET cb == Head(deferred)
           rest == Tail(deferred)
       IN CASE cb.kind = "request_quit"      -> RequestQuitDeferred(rest)
            [] cb.kind = "show_approval"     -> ShowApprovalDeferred(rest)
            [] cb.kind = "save_and_teardown" -> SaveAndTeardownDeferred(cb, rest)
    /\ UNCHANGED <<running, asking, inbox, followUp, woken, delivery, scheduledWhileWaiting>>

-----------------------------------------------------------------------------
(* The session on another machine. *)

\* send_message_from to the remote session while its link is up. The turn runs on
\* the machine, and the conversation shows it running (push_sent).
UserSendsRemote ==
    /\ Idle /\ phase = "Running" /\ windows = 1 /\ linkUp
    /\ ~remoteShown /\ ~remoteRunning
    /\ remoteRunning' = TRUE /\ remoteShown' = TRUE
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, inbox, followUp, woken,
                   delivery, deferred, cutOff, scheduledWhileWaiting, idleQuit, linkUp,
                   endOnQuit, remoteCutOff>>

\* The machine's provider ends the turn, whether or not Keystone runs or hears it.
RemoteEndsTurn ==
    /\ remoteRunning /\ remoteRunning' = FALSE
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, inbox, followUp, woken,
                   delivery, deferred, cutOff, scheduledWhileWaiting, idleQuit, remoteShown,
                   linkUp, endOnQuit, remoteCutOff>>

\* The network drops or comes back (link.rs supervise). It may stay down.
LinkChanges ==
    /\ phase # "Exiting" /\ linkUp' = ~linkUp
    /\ UNCHANGED <<phase, quitWhenIdle, windows, running, asking, inbox, followUp, woken,
                   delivery, deferred, cutOff, scheduledWhileWaiting, idleQuit, remoteRunning,
                   remoteShown, endOnQuit, remoteCutOff>>

\* A pump drains the turn's end from the remote connection (pump's remote routes),
\* which reaches Keystone only while the link is up, then publishes.
RemotePump ==
    /\ Idle /\ phase # "Exiting" /\ linkUp /\ remoteShown /\ ~remoteRunning
    /\ remoteShown' = FALSE
    /\ LET q == [qwi |-> quitWhenIdle, d |-> deferred]
           r == IF ~quitWhenIdle \/ phase # "Running" THEN q
                ELSE IF ~TurnRunning(running, followUp, delivery, FALSE)
                     THEN [qwi |-> FALSE, d |-> Append(deferred, RequestQuitCb)]
                ELSE IF Asking(running, asking)
                     THEN [qwi |-> FALSE, d |-> Append(deferred, ShowApprovalCb)]
                ELSE q
       IN quitWhenIdle' = r.qwi /\ deferred' = r.d
    /\ UNCHANGED <<phase, windows, running, asking, inbox, followUp, woken, delivery, cutOff,
                   scheduledWhileWaiting, idleQuit, remoteRunning, linkUp, endOnQuit,
                   remoteCutOff>>

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
    /\ followUp = [s \in Sessions |-> "none"]
    /\ woken = FALSE
    /\ delivery = "none"
    /\ deferred = <<>>
    /\ cutOff = {}
    /\ scheduledWhileWaiting = FALSE
    /\ idleQuit = FALSE
    /\ remoteRunning = FALSE /\ remoteShown = FALSE
    /\ linkUp \in BOOLEAN /\ endOnQuit \in BOOLEAN
    /\ remoteCutOff = FALSE

Next ==
    \/ CloseLastWindow \/ UserQuits \/ UserReopens \/ UserSteers
    \/ \E s \in Sessions : UserSends(s) \/ UserAnswers(s)
    \/ \E s \in Sessions : ProviderEndsTurn(s) \/ ProviderAsks(s)
    \/ WorkerTakesFollowUp \/ CodexAnswersSteer \/ ProviderReleasesFollowUp
    \/ ParentLaunchesChild \/ Pump \/ WriterConfirmsDelivery \/ ScheduledRun
    \/ UserSendsRemote \/ RemoteEndsTurn \/ LinkChanges \/ RemotePump
    \/ RunDeferred \/ Exited

\* Turns end, providers take and answer follow-ups, the pump drains what wakes it,
\* and deferred callbacks run. The user and the scheduler are never forced to act,
\* and nothing forces a dropped link back up.
Fairness ==
    /\ WF_vars(WriterConfirmsDelivery)
    /\ WF_vars(Pump)
    /\ WF_vars(RunDeferred)
    /\ WF_vars(WorkerTakesFollowUp)
    /\ WF_vars(CodexAnswersSteer)
    /\ WF_vars(ProviderReleasesFollowUp)
    /\ \A s \in Sessions : WF_vars(ProviderEndsTurn(s))
    /\ WF_vars(RemoteEndsTurn)
    /\ WF_vars(RemotePump)

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* The rules. *)

TypeOK ==
    /\ phase \in {"Running", "Saving", "Exiting"}
    /\ quitWhenIdle \in BOOLEAN /\ windows \in {0, 1}
    /\ running \in [Sessions -> BOOLEAN] /\ asking \in [Sessions -> BOOLEAN]
    /\ inbox \in [Sessions -> {"none", "ask", "end"}]
    /\ followUp \in [Sessions -> {"none", "queued", "steer", "held", "start"}]
    /\ woken \in BOOLEAN
    /\ delivery \in {"none", "Working", "Ready", "Saving", "Saved", "Delivered"}
    /\ Len(deferred) <= 2 /\ cutOff \subseteq Sessions
    /\ idleQuit \in BOOLEAN /\ remoteRunning \in BOOLEAN /\ remoteShown \in BOOLEAN
    /\ linkUp \in BOOLEAN /\ endOnQuit \in BOOLEAN /\ remoteCutOff \in BOOLEAN

\* Keystone stops a running turn, or drops a message it accepted for one, only when
\* the user chooses Quit.
OnlyQuitStopsTurns == cutOff = {}

\* A turn on another machine is ended by Keystone only when the user chose Quit
\* and the machine is set to end its sessions. Closing the last window never does.
OnlyUserQuitEndsRemoteTurns == ~remoteCutOff

\* No scheduled run starts while Keystone has no window and waits to quit.
NoScheduledRunWhileWaiting == ~scheduledWhileWaiting

\* Whenever Keystone has no window and is not quitting, it is waiting to quit.
WindowlessMeansWaiting ==
    (Idle /\ windows = 0 /\ phase = "Running") => quitWhenIdle

\* With no window, Keystone eventually exits or shows a window again.
WindowlessEventuallyEnds ==
    (windows = 0 /\ phase = "Running") ~> (phase = "Exiting" \/ windows = 1)

=============================================================================
