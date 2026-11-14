----------------------------- MODULE RemoteCursor -----------------------------
(***************************************************************************)
(* How the app saves what a remote machine's host sends, and when the host *)
(* may trim its journal (TOB-319, docs/design/remote.md, "Received, saved   *)
(* and acknowledged").                                                      *)
(*                                                                         *)
(* The host numbers events in one sequence per journal, journals each and  *)
(* sends it. A journal can be replaced at any point, as when the machine's *)
(* state folder is wiped, keystone-host is reinstalled, or the SSH name    *)
(* reaches another machine: the new one has a new identity and numbers    *)
(* from 1 again. An event is the pair <<journal, number>>. Each belongs to *)
(* one of two sessions and adds a log row or not (a streamed delta adds    *)
(* none), chosen by Init in every way. The app receives, drains, queues    *)
(* the rows on its log writer, saves them, hears that they are saved and   *)
(* acknowledges. The transport may drop and reconnect, and the app may be  *)
(* killed at any point and launched again, with only its database left.    *)
(*                                                                         *)
(* Each action names the Rust it stands for. Paths are under crates/.      *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    N,          \* how many events the host sends in each journal
    MaxResets,  \* how many times the host's journal is replaced
    MaxRewinds, \* how many times it goes back, as one restored from a backup
    GrowsBack,  \* whether a journal that went back may number new events before
                \* the app next connects, as its own restart events would
    MaxCrashes, \* how many times the host crashes or loses power, losing the commits
                \* the journal hadn't synced
    MaxKills,   \* how many times the app may be killed
    MaxDrops,   \* how many times the transport may drop
    Mutation    \* a deliberate break of the code, or "none"

Mutations == {"none", "ack-on-drain", "no-skip", "cursor-apart", "cross-journal",
              "no-rotate", "lossy-crash", "late-route"}
ASSUME Mutation \in Mutations

Sessions == {"a", "b"}
Journals == 1..(MaxResets + MaxRewinds + MaxCrashes + 1)
\* An event is <<journal, era, number>>: the era counts how often the journal went
\* back before the event was appended, so a number given again after a rewind
\* names a new event. The app sees only the journal and the number, a slot.
Eras == 0..(MaxRewinds + MaxCrashes)
Events == Journals \X Eras \X (1..N)
Slots == Journals \X (1..N)
Slot(e) == <<e[1], e[3]>>
NoPlace == <<0, 0>>
Places == Journals \X (0..N) \cup {NoPlace}
Max(S) == IF S = {} THEN 0 ELSE CHOOSE x \in S : \A y \in S : y <= x
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
Range(s) == {s[i] : i \in DOMAIN s}
\* The numbers of `S` in order, as a sequence.
Sorted(S) == LET n == Cardinality(S)
             IN [i \in 1..n |-> CHOOSE e \in S :
                    Cardinality({x \in S : x < e}) = i - 1]

VARIABLES
    of, row,      \* each event's session, and whether it adds a log row
    jid,          \* host: the journal it has now (journal.rs `Journal::id`)
    era,          \* ghost: how often the host's journal went back
    born,         \* ghost: the era each number of the journal was appended in
    latest,       \* host: events numbered in that journal (`clients.latest`)
    journal,      \* host: the numbers it still holds, not trimmed
    acked,        \* host: the cursor it was last acknowledged (`clients.acked`)
    trimmed,      \* ghost: every event the host trimmed on an acknowledgement
    up,           \* the app is running
    link,         \* the transport: "down" or "up"
    linked,       \* this run's link was welcomed once, so `hello` carries `after`
    ljournal,     \* the journal the link's places count in (State.journal), 0 for none
    wire,         \* events sent on the transport, not yet read by the app
    acks,         \* acknowledgements sent, not yet read by the host
    sentAck,      \* the last acknowledgement the link queued (State.sent_ack)
    received,     \* State.received, a number in `ljournal`
    inbox,        \* RouteState.inbox, in arrival order: the event and whether it
                  \* still has its place (a journal change takes the places away)
    unsaved,      \* State.unsaved: numbers in `ljournal` drained and not reported saved
    savedMem,     \* State.saved: each session's saved cursor as the link knows it
    queue,        \* PendingSession: each session's queued rows and its `through`
    late,         \* only under "cursor-apart": rows whose cursor was saved first
    reports,      \* Report::Through waiting for the next pump
    log,          \* database: log_entries, in commit order
    cursor,       \* database: remote_cursors, a place with its journal
    kills, drops, resets, rewinds, crashes,
    sentTo,       \* host: the highest number of its journal it has put on the wire
    fresh         \* ghost: the journal went back and the app hasn't connected since

vars == <<of, row, jid, era, born, latest, journal, acked, trimmed, up, link, linked, ljournal,
          wire, acks, sentAck, received, inbox, unsaved, savedMem, queue, late,
          reports, log, cursor, kills, drops, resets, rewinds, fresh, crashes, sentTo>>

NoQueue == [s \in Sessions |-> [rows |-> <<>>, through |-> NoPlace]]
Nowhere == [s \in Sessions |-> NoPlace]

TypeOK ==
    /\ of \in [Slots -> Sessions]
    /\ row \in [Slots -> BOOLEAN]
    /\ jid \in Journals /\ era \in Eras /\ born \in [1..N -> Eras] /\ ljournal \in {0} \cup Journals
    /\ latest \in 0..N /\ journal \subseteq 1..latest /\ acked \in 0..N
    /\ trimmed \subseteq Events
    /\ up \in BOOLEAN /\ linked \in BOOLEAN
    /\ link \in {"down", "up"}
    /\ received \in 0..N
    /\ unsaved \subseteq 1..N
    /\ savedMem \in [Sessions -> Places] /\ cursor \in [Sessions -> Places]
    /\ kills \in 0..MaxKills /\ drops \in 0..MaxDrops /\ resets \in 0..MaxResets
    /\ rewinds \in 0..MaxRewinds /\ fresh \in BOOLEAN
    /\ crashes \in 0..MaxCrashes /\ sentTo \in 0..N

Init ==
    /\ of \in [Slots -> Sessions]
    /\ row \in [Slots -> BOOLEAN]
    /\ jid = 1 /\ era = 0 /\ born = [n \in 1..N |-> 0] /\ latest = 0 /\ journal = {} /\ acked = 0 /\ trimmed = {}
    /\ up = TRUE /\ link = "down" /\ linked = FALSE /\ ljournal = 0
    /\ wire = <<>> /\ acks = <<>> /\ sentAck = 0
    /\ received = 0 /\ inbox = <<>> /\ unsaved = {}
    /\ savedMem = Nowhere /\ queue = NoQueue /\ late = NoQueue
    /\ reports = {} /\ log = <<>> /\ cursor = Nowhere
    /\ kills = 0 /\ drops = 0 /\ resets = 0 /\ rewinds = 0 /\ fresh = FALSE
    /\ crashes = 0 /\ sentTo = 0

Placed == {i \in DOMAIN inbox : inbox[i].placed}

(* The acknowledgement the link sends (link.rs `State::ack`): everything     *)
(* received, short of the first event still undrained or drained and not   *)
(* reported saved. Numbers in `ljournal`.                                   *)
(* An inbox counts only while its route's connection is registered (`ack`   *)
(* skips a route with no wake). "late-route" leaves session b's route        *)
(* unregistered until each event is drained, as an app that made a route's  *)
(* connection only when it next sent, rather than when it made the link,    *)
(* would at a launch that replays that route's turn.                        *)
AckValue ==
    LET counted == {i \in Placed : Mutation /= "late-route" \/ of[Slot(inbox[i].e)] /= "b"}
        pending == {inbox[i].e[3] : i \in counted} \cup unsaved
    IN IF pending = {} THEN received ELSE
       IF received < Min(pending) - 1 THEN received ELSE Min(pending) - 1

(* Whether a saved place covers event `e`: the same journal and not below.  *)
(* "cross-journal" compares the numbers alone, as round one of TOB-319 did. *)
Covers(p, e) ==
    IF Mutation = "cross-journal" THEN e[3] <= p[2]
    ELSE p[1] = e[1] /\ e[3] <= p[2]

(* The host journals the next event and, while the transport is up and its *)
(* replay done, sends it live (serve.rs `Daemon::pump`, journal.rs          *)
(* `Journal::append`).                                                      *)
HostAppends ==
    /\ latest < N /\ (GrowsBack \/ ~fresh)
    /\ latest' = latest + 1
    /\ journal' = journal \cup {latest + 1}
    /\ born' = [born EXCEPT ![latest + 1] = era]
    /\ wire' = IF link = "up" THEN Append(wire, <<jid, era, latest + 1>>) ELSE wire
    /\ sentTo' = IF link = "up" THEN latest + 1 ELSE sentTo
    /\ UNCHANGED <<of, row, jid, acked, trimmed, up, link, linked, ljournal, acks,
                   sentAck, received, inbox, unsaved, savedMem, queue, late, reports,
                   log, cursor, kills, drops, resets, era, rewinds, fresh, crashes>>

(* The link reads one event (link.rs `Shared::route`): a repeat of what it  *)
(* received is dropped, and so is one its session saved before a kill.     *)
Receive ==
    /\ up /\ link = "up" /\ wire /= <<>>
    /\ LET e == Head(wire) IN
       /\ wire' = Tail(wire)
       /\ IF e[3] <= received
            THEN UNCHANGED <<received, inbox>>
            ELSE /\ received' = e[3]
                 /\ inbox' = IF Covers(savedMem[of[Slot(e)]], e) THEN inbox
                             ELSE Append(inbox, [e |-> e, placed |-> TRUE])
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, acks, sentAck, unsaved, savedMem, queue, late, reports,
                   log, cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo>>

(* One connection drains: it hands over its own route's events in order     *)
(* (connection.rs `RemoteConnection::drain_placed`, link.rs `Shared::drain`), *)
(* each event is applied and its session committed with its place, if it   *)
(* still has one (app_state.rs `pump`, `receive_placed`, `commit`), which    *)
(* queues the rows and the place on the log writer (log.rs                  *)
(* `Logs::append_through`). Each session runs on a route of its own, as a    *)
(* Claude and a Codex session, or two logins, on one machine do, so `pump`   *)
(* drains one route while the link thread still receives into the other's  *)
(* inbox. Every route is registered before the link reads a frame          *)
(* (app_state.rs `open_link`, `remote_connection`), so each event has a    *)
(* connection to drain it. Draining both at once is one route's case.      *)
Drain(s) ==
    /\ up /\ \E i \in DOMAIN inbox : of[Slot(inbox[i].e)] = s
    /\ LET mine == SelectSeq(inbox, LAMBDA x : of[Slot(x.e)] = s)
           withRows == SelectSeq(mine, LAMBDA x : row[Slot(x.e)])
           rows == [i \in 1..Len(withRows) |-> withRows[i].e]
           placed == SelectSeq(mine, LAMBDA x : x.placed)
       IN /\ queue' = [queue EXCEPT ![s] =
                         [rows |-> queue[s].rows \o rows,
                          through |-> IF placed = <<>> THEN queue[s].through
                                      ELSE Slot(placed[Len(placed)].e)]]
          \* "ack-on-drain": the link acknowledges what it handed over, as before TOB-319.
          /\ unsaved' = IF Mutation = "ack-on-drain" THEN unsaved
                        ELSE unsaved \cup {placed[i].e[3] : i \in DOMAIN placed}
    /\ inbox' = SelectSeq(inbox, LAMBDA x : of[Slot(x.e)] /= s)
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, savedMem, late, reports,
                   log, cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo>>

(* The cursor a save leaves: the place saved with the rows. Round one raised *)
(* it by number and never let it fall, even into another journal.          *)
NewCursor(old, p) ==
    IF Mutation = "cross-journal"
      THEN IF p[2] > old[2] THEN p ELSE old
      ELSE p

(* The log writer saves a session's queued rows and its place in one        *)
(* transaction, then reports the place (log.rs `PendingSession::retry`,     *)
(* `Files::through`, database.rs `Database::append_logs`). With no rows,    *)
(* nothing is written and the place is reported. A failing save is this     *)
(* action not being taken: the rows wait in memory.                         *)
Save(s) ==
    /\ up /\ (queue[s].rows /= <<>> \/ queue[s].through /= NoPlace)
    /\ LET q == queue[s] IN
       IF Mutation = "cursor-apart" /\ q.rows /= <<>> /\ q.through /= NoPlace
         \* The cursor in a transaction of its own, before the rows.
         THEN /\ cursor' = [cursor EXCEPT ![s] = NewCursor(@, q.through)]
              /\ late' = [late EXCEPT ![s] = [rows |-> @.rows \o q.rows,
                                               through |-> q.through]]
              /\ UNCHANGED <<log, reports>>
         ELSE /\ log' = log \o q.rows
              /\ cursor' = IF q.rows = <<>> \/ q.through = NoPlace THEN cursor
                           ELSE [cursor EXCEPT ![s] = NewCursor(@, q.through)]
              /\ reports' = IF q.through = NoPlace THEN reports
                            ELSE reports \cup {<<s, q.through>>}
              /\ UNCHANGED late
    /\ queue' = [queue EXCEPT ![s] = [rows |-> <<>>, through |-> NoPlace]]
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, inbox, unsaved, savedMem,
                   kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo>>

(* Only under "cursor-apart": the rows' own transaction, after their cursor. *)
SaveLate(s) ==
    /\ up /\ late[s].through /= NoPlace
    /\ log' = log \o late[s].rows
    /\ reports' = reports \cup {<<s, late[s].through>>}
    /\ late' = [late EXCEPT ![s] = [rows |-> <<>>, through |-> NoPlace]]
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, inbox, unsaved, savedMem,
                   queue, cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo>>

(* The pump hands a report to the link (app_state.rs `receive_placed` on    *)
(* `Report::Through`, link.rs `Link::saved`), which ignores a place in a    *)
(* journal it no longer counts in.                                         *)
Report(r) ==
    /\ up /\ r \in reports
    /\ LET s == r[1] p == r[2] IN
       IF p[1] /= ljournal /\ Mutation /= "cross-journal"
         THEN UNCHANGED <<savedMem, unsaved>>
         ELSE /\ savedMem' = [savedMem EXCEPT ![s] =
                                 IF @[1] = p[1] /\ @[2] > p[2] THEN @ ELSE p]
              /\ unsaved' = {n \in unsaved : of[<<ljournal, n>>] /= s \/ n > p[2]}
    /\ reports' = reports \ {r}
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, inbox, queue, late, log,
                   cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo>>

(* The link queues an acknowledgement when it moved (link.rs `State::ack`).  *)
SendAck ==
    /\ up /\ link = "up" /\ AckValue /= sentAck
    /\ sentAck' = AckValue
    /\ acks' = Append(acks, AckValue)
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, received, inbox, unsaved, savedMem, queue, late,
                   reports, log, cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo>>

(* The host takes an acknowledgement and trims its journal through it      *)
(* (serve.rs on `Frame::Ack`, journal.rs `Journal::ack`).                   *)
HostAck ==
    /\ acks /= <<>>
    /\ LET a == Head(acks) IN
       /\ acked' = Max({acked, a})
       /\ trimmed' = trimmed \cup {<<jid, born[n], n>> : n \in {m \in journal : m <= a}}
       /\ journal' = {n \in journal : n > a}
    /\ acks' = Tail(acks)
    /\ UNCHANGED <<of, row, jid, latest, up, link, linked, ljournal, wire, sentAck,
                   received, inbox, unsaved, savedMem, queue, late, reports, log,
                   cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo>>

(* The network drops: whatever was in flight either way is lost            *)
(* (link.rs `Shared::connect_once` returning `Ended::Dropped`).            *)
Drop ==
    /\ link = "up" /\ drops < MaxDrops
    /\ link' = "down" /\ wire' = <<>> /\ acks' = <<>>
    /\ drops' = drops + 1
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, up, linked, ljournal,
                   sentAck, received, inbox, unsaved, savedMem, queue, late, reports,
                   log, cursor, kills, resets, era, born, rewinds, fresh, crashes, sentTo>>

(* The machine's journal is replaced: its daemon and every event and number *)
(* it held are gone, and the new one numbers from 1 (journal.rs             *)
(* `Journal::open` on a new file). The transport ends with the daemon.      *)
Reset ==
    /\ resets < MaxResets
    /\ jid' = jid + 1 /\ latest' = 0 /\ journal' = {} /\ acked' = 0 /\ sentTo' = 0
    /\ link' = "down" /\ wire' = <<>> /\ acks' = <<>>
    /\ resets' = resets + 1
    /\ UNCHANGED <<of, row, trimmed, up, linked, ljournal, sentAck, received, inbox,
                   unsaved, savedMem, queue, late, reports, log, cursor, kills, drops, era, born, rewinds, fresh, crashes>>

(* The machine's journal goes back, as one restored from a backup or damaged *)
(* would: the same identity, its latest number back at `b`, holding the      *)
(* events after the acknowledged cursor it had then, `a`. Numbers past `b`   *)
(* are given to new events from now on.                                    *)
Rewind ==
    /\ rewinds < MaxRewinds /\ latest > 0
    /\ \E b \in 0..(latest - 1), a \in 0..N :
         /\ a <= b /\ a <= acked
         /\ latest' = b /\ acked' = a
         /\ journal' = {n \in 1..b : n > a}
         /\ sentTo' = IF sentTo > b THEN b ELSE sentTo
    /\ link' = "down" /\ wire' = <<>> /\ acks' = <<>>
    /\ rewinds' = rewinds + 1 /\ era' = era + 1 /\ fresh' = TRUE
    /\ UNCHANGED <<of, row, jid, trimmed, up, linked, ljournal, sentAck, received, inbox,
                   unsaved, savedMem, queue, late, reports, log, cursor, kills, drops, resets,
                   born, crashes>>

(* The host crashes or loses power. Its daemon and provider die, as in a     *)
(* reboot, and a commit the journal hadn't synced may be gone: with `FULL`   *)
(* every commit is synced before it returns, and the daemon sends only what  *)
(* it committed (journal.rs `SYNCHRONOUS`, serve.rs `journal_events`), so    *)
(* only numbers it never sent can go back. The daemon that starts next       *)
(* numbers its restart events from there at once (serve.rs, the restart     *)
(* path through `enqueue`). "lossy-crash" lets the crash take back sent      *)
(* numbers too, as `NORMAL` did.                                            *)
Crash ==
    /\ crashes < MaxCrashes
    /\ \E b \in (IF Mutation = "lossy-crash" THEN 0 ELSE sentTo)..(latest - 1) :
         /\ latest' = b
         /\ journal' = {n \in journal : n <= b}
         /\ acked' = IF acked > b THEN b ELSE acked
         /\ sentTo' = IF sentTo > b THEN b ELSE sentTo
    /\ era' = era + 1
    /\ link' = "down" /\ wire' = <<>> /\ acks' = <<>>
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<of, row, jid, born, trimmed, up, linked, ljournal, sentAck, received,
                   inbox, unsaved, savedMem, queue, late, reports, log, cursor, kills,
                   drops, resets, rewinds, fresh>>

(* The places `hello` shows the host (link.rs `State::seen`): each saved      *)
(* cursor, and what was received in the journal the link took on.           *)
Seen == {savedMem[s] : s \in Sessions} \cup
        (IF ljournal = 0 THEN {} ELSE {<<ljournal, received>>})

(* The host takes a new identity when one of them is in its journal, past    *)
(* its latest number (serve.rs `Daemon::went_back`, journal.rs               *)
(* `Journal::rotate`). Its events keep their numbers under the new identity. *)
Rotates == Mutation \notin {"no-rotate", "cross-journal"}
           /\ \E p \in Seen : p[1] = jid /\ p[2] > latest

(* The link connects. The welcome names the host's journal, and the one rule *)
(* (link.rs `State::adopt`, run by `Shared::resend`) decides: another       *)
(* journal than the link's places count in, and every place is forgotten:   *)
(* received, unsaved, saved cursors from that journal and the inbox's       *)
(* numbers. `hello` carries `after` only with its journal, and the host     *)
(* ignores an `after` from another one (serve.rs, on `Frame::Hello`), so it *)
(* replays everything after its acknowledged cursor (`Daemon::replay`).     *)
(* "cross-journal" keeps every place and lowers the numbers to `latest`, as *)
(* round one did.                                                          *)
Connect ==
    /\ up /\ link = "down"
    /\ LET host == IF Rotates THEN jid + 1 ELSE jid
           behind == \E p \in Seen : p[1] = host /\ p[2] > latest
           replay(from) == [i \in 1..Len(Sorted({n \in journal : n > from})) |->
                              <<host, born[Sorted({n \in journal : n > from})[i]],
                                Sorted({n \in journal : n > from})[i]>>]
       IN /\ jid' = host
          /\ IF Mutation = "cross-journal"
               THEN /\ received' = IF received > latest THEN latest ELSE received
                    /\ savedMem' = [s \in Sessions |->
                                      IF savedMem[s][2] > latest
                                        THEN <<savedMem[s][1], latest>> ELSE savedMem[s]]
                    /\ UNCHANGED <<unsaved, inbox>>
                    /\ wire' = replay(IF linked THEN Max({received', acked}) ELSE acked)
               ELSE IF ljournal = host /\ ~behind
                 THEN /\ UNCHANGED <<received, savedMem, unsaved, inbox>>
                      /\ wire' = replay(IF linked THEN Max({received, acked}) ELSE acked)
                 \* Another journal, or the same one behind a place: every place goes.
                 ELSE /\ received' = 0 /\ unsaved' = {}
                      /\ savedMem' = [s \in Sessions |->
                                        IF savedMem[s][1] = host /\ ~behind
                                          THEN savedMem[s] ELSE NoPlace]
                      /\ inbox' = [i \in DOMAIN inbox |->
                                     [inbox[i] EXCEPT !.placed = FALSE]]
                      /\ wire' = replay(acked)
          /\ ljournal' = host
          /\ sentTo' = Max({IF host = jid THEN sentTo ELSE 0} \cup
                            {wire'[i][3] : i \in DOMAIN wire'})
    /\ link' = "up" /\ linked' = TRUE /\ sentAck' = N + 1 /\ fresh' = FALSE
    /\ UNCHANGED <<of, row, latest, journal, acked, trimmed, up, acks, queue, late,
                   reports, log, cursor, kills, drops, resets, era, born, rewinds, crashes>>

(* The app is killed (SIGKILL): everything but the database is gone.       *)
Kill ==
    /\ up /\ kills < MaxKills
    /\ up' = FALSE /\ link' = "down" /\ linked' = FALSE /\ ljournal' = 0
    /\ wire' = <<>> /\ acks' = <<>> /\ sentAck' = 0
    /\ received' = 0 /\ inbox' = <<>> /\ unsaved' = {} /\ savedMem' = Nowhere
    /\ queue' = NoQueue /\ late' = NoQueue /\ reports' = {}
    /\ kills' = kills + 1
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, log, cursor, drops,
                   resets, era, born, rewinds, fresh, crashes, sentTo>>

(* The next launch reads each session's saved cursor, with its journal, and  *)
(* gives it to the machine's link (app_state.rs `AppState::new`,              *)
(* `open_link`; link.rs `Link::with_cursors`). "no-skip" forgets them.        *)
Launch ==
    /\ ~up
    /\ up' = TRUE
    /\ savedMem' = IF Mutation = "no-skip" THEN Nowhere ELSE cursor
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, link, linked, ljournal,
                   wire, acks, sentAck, received, inbox, unsaved, queue, late, reports,
                   log, cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo>>

Next ==
    \/ HostAppends \/ Receive \/ (\E s \in Sessions : Drain(s)) \/ SendAck \/ HostAck
    \/ \E s \in Sessions : Save(s) \/ SaveLate(s)
    \/ \E r \in reports : Report(r)
    \/ Drop \/ Reset \/ Rewind \/ Crash \/ Connect \/ Kill \/ Launch

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* The rules.                                                              *)
(***************************************************************************)

\* No event that adds a row is both trimmed by the host and missing from the
\* app's saved log. Events a replaced journal took with it were lost by the host,
\* not trimmed; the app ends their turn with a note (app_state.rs `end_lost_turns`).
NothingLost ==
    \A e \in trimmed : row[Slot(e)] => e \in Range(log)

\* Each row is saved once.
SavedOnce ==
    \A i, j \in DOMAIN log : i /= j => log[i] /= log[j]

\* Each session's rows are saved in the order the host numbered them: by journal,
\* then by number within one.
SavedInOrder ==
    \A i, j \in DOMAIN log :
        (i < j /\ of[Slot(log[i])] = of[Slot(log[j])]) =>
            \/ log[i][1] < log[j][1]
            \/ log[i][1] = log[j][1] /\ log[i][3] < log[j][3]
=============================================================================
