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
(* An event that adds no row is a part of an unfinished item, such as a    *)
(* streamed delta (TOB-453). The app keeps its session's unfinished items  *)
(* in memory, and saves them with the session's cursor, in the same        *)
(* transaction. A later row of the session completes them: its text is in  *)
(* that row. Each daemon run stamps the numbers it gives, and the app keeps *)
(* the stamp with each place, so a journal that went back and grew again   *)
(* is told by a stamp that differs (TOB-455).                              *)
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
    MaxFails,   \* how many saves may commit and then fail their directory barrier
    MaxCuts,    \* how many times the app's machine may lose power
    Mutation    \* a deliberate break of the code, or "none"

Mutations == {"none", "ack-on-drain", "no-skip", "cursor-apart", "cross-journal",
              "no-rotate", "lossy-crash", "late-route", "no-stamp", "ack-unsaved-part",
              "parts-apart", "no-restore", "report-unsynced"}
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
\* A place is <<journal, number, stamp>>. The stamp is the daemon run that gave the
\* number (serve.rs, `Journal::open` draws it), which the era stands for: a rewind or
\* a crash starts a new run, and a restored backup's events keep the stamps they had.
PlaceOf(e) == <<e[1], e[3], e[2]>>
NoPlace == <<0, 0, 0>>
Places == Journals \X (0..N) \X Eras \cup {NoPlace}
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
    fresh,        \* ghost: the journal went back and the app hasn't connected since
    recvStamp,    \* State.received's stamp, sent in `hello` with it
    open,         \* app: each session's unfinished items, as the parts applied to them
    dirty,        \* app: they changed since they were last queued (Conversation's flag)
    handed,       \* app: the last place handed to each session (AppState.handed)
    unfinished,   \* database: remote_cursors.unfinished, saved with the cursor
    done,         \* ghost: parts whose item a saved row completed
    durable,      \* database: what a power cut leaves of log, cursor, unfinished, done
    unsynced,     \* app: places whose save committed and whose barrier failed
    fails, cuts

vars == <<of, row, jid, era, born, latest, journal, acked, trimmed, up, link, linked, ljournal,
          wire, acks, sentAck, received, inbox, unsaved, savedMem, queue, late,
          reports, log, cursor, kills, drops, resets, rewinds, fresh, crashes, sentTo,
          recvStamp, open, dirty, handed, unfinished, done, durable, unsynced, fails, cuts>>

\* What a commit queues: rows, the place they reach, and, when `snap`, the session's
\* unfinished items at that place, with the parts the rows complete.
NoQueue == [s \in Sessions |-> [rows |-> <<>>, through |-> NoPlace, snap |-> FALSE,
                                parts |-> <<>>, done |-> {}]]
NoParts == [s \in Sessions |-> <<>>]
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
    /\ fails \in 0..MaxFails /\ cuts \in 0..MaxCuts
    /\ recvStamp \in Eras /\ dirty \in [Sessions -> BOOLEAN]
    /\ handed \in [Sessions -> Places] /\ done \subseteq Events

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
    /\ recvStamp = 0 /\ open = NoParts /\ dirty = [s \in Sessions |-> FALSE]
    /\ handed = Nowhere /\ unfinished = NoParts /\ done = {}
    /\ durable = [log |-> <<>>, cursor |-> Nowhere, unfinished |-> NoParts, done |-> {}]
    /\ unsynced = {} /\ fails = 0 /\ cuts = 0

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

partVars == <<recvStamp, open, dirty, handed, unfinished, done>>
barrierVars == <<durable, unsynced, fails, cuts>>

(* The host journals the next event and, while the transport is up and its *)
(* replay done, sends it live (serve.rs `Daemon::pump`, journal.rs          *)
(* `Journal::append`, which saves the run's stamp with the number).         *)
HostAppends ==
    /\ latest < N /\ (GrowsBack \/ ~fresh)
    /\ latest' = latest + 1
    /\ journal' = journal \cup {latest + 1}
    /\ born' = [born EXCEPT ![latest + 1] = era]
    /\ wire' = IF link = "up" THEN Append(wire, <<jid, era, latest + 1>>) ELSE wire
    /\ sentTo' = IF link = "up" THEN latest + 1 ELSE sentTo
    /\ UNCHANGED <<barrierVars, of, row, jid, acked, trimmed, up, link, linked, ljournal, acks,
                   sentAck, received, inbox, unsaved, savedMem, queue, late, reports,
                   log, cursor, kills, drops, resets, era, rewinds, fresh, crashes, partVars>>

(* The link reads one event (link.rs `Shared::route`): a repeat of what it  *)
(* received is dropped, and so is one its session saved before a kill.     *)
Receive ==
    /\ up /\ link = "up" /\ wire /= <<>>
    /\ LET e == Head(wire) IN
       /\ wire' = Tail(wire)
       /\ IF e[3] <= received
            THEN UNCHANGED <<received, recvStamp, inbox>>
            ELSE /\ received' = e[3] /\ recvStamp' = e[2]
                 /\ inbox' = IF Covers(savedMem[of[Slot(e)]], e) THEN inbox
                             ELSE Append(inbox, [e |-> e, placed |-> TRUE])
    /\ UNCHANGED <<barrierVars, of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, acks, sentAck, unsaved, savedMem, queue, late, reports,
                   log, cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo,
                   open, dirty, handed, unfinished, done>>

(* The parts of `q` (a sequence of inbox entries) as events, in order.     *)
PartsOf(q) == LET p == SelectSeq(q, LAMBDA x : ~row[Slot(x.e)])
              IN [i \in 1..Len(p) |-> p[i].e]
LastPlace(q) == LET p == SelectSeq(q, LAMBDA x : x.placed)
                IN IF p = <<>> THEN NoPlace ELSE PlaceOf(p[Len(p)].e)

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
(*                                                                         *)
(* A part joins its session's unfinished items, and a row completes them   *)
(* (conversation.rs `apply`: `ItemCompleted`, and `settle_unfinished` at a  *)
(* turn's end). A commit with rows queues the unfinished items as they are  *)
(* after the last row, with that row's place (`commit` takes them whenever  *)
(* it has writes). Parts after the last row leave the items changed, and    *)
(* their commits give no place: the place waits for `Flush`. That holds    *)
(* the acknowledgement at the first of them. "ack-unsaved-part" gives their *)
(* place at once, as the code before TOB-453 did.                          *)
Drain(s) ==
    /\ up /\ \E i \in DOMAIN inbox : of[Slot(inbox[i].e)] = s
    /\ LET mine == SelectSeq(inbox, LAMBDA x : of[Slot(x.e)] = s)
           withRows == SelectSeq(mine, LAMBDA x : row[Slot(x.e)])
           rows == [i \in 1..Len(withRows) |-> withRows[i].e]
           placed == SelectSeq(mine, LAMBDA x : x.placed)
           last == IF rows = <<>> THEN 0
                   ELSE CHOOSE i \in DOMAIN mine : row[Slot(mine[i].e)] /\
                          \A j \in DOMAIN mine : j > i => ~row[Slot(mine[j].e)]
           before == SubSeq(mine, 1, last)
           after == SubSeq(mine, last + 1, Len(mine))
           \* The place of the last row's commit: the last place handed up to it.
           rowPlace == IF LastPlace(before) = NoPlace THEN handed[s]
                       ELSE LastPlace(before)
           early == Mutation = "ack-unsaved-part"
       IN /\ handed' = [handed EXCEPT ![s] =
                          IF placed = <<>> THEN @ ELSE PlaceOf(placed[Len(placed)].e)]
          /\ IF last = 0
               THEN \* Parts only: the items change and nothing is queued for them.
                    /\ open' = [open EXCEPT ![s] = @ \o PartsOf(mine)]
                    /\ dirty' = [dirty EXCEPT ![s] = ~early]
                    /\ queue' = [queue EXCEPT ![s] =
                         [@ EXCEPT !.through = IF early /\ placed /= <<>>
                                                 THEN PlaceOf(placed[Len(placed)].e) ELSE @]]
                    /\ UNCHANGED done
               ELSE LET completed == Range(open[s]) \cup Range(PartsOf(before))
                        later == PartsOf(after)
                    IN /\ open' = [open EXCEPT ![s] = later]
                       /\ queue' = [queue EXCEPT ![s] =
                            [rows |-> @.rows \o rows,
                             through |-> IF early /\ LastPlace(after) /= NoPlace
                                           THEN LastPlace(after) ELSE rowPlace,
                             \* Only a commit with a place takes the items.
                             snap |-> @.snap \/ rowPlace /= NoPlace,
                             parts |-> IF rowPlace /= NoPlace THEN <<>> ELSE @.parts,
                             done |-> @.done \cup completed]]
                       \* Changed since: parts after the last row, or items no
                       \* commit with a place took.
                       /\ dirty' = [dirty EXCEPT ![s] =
                            ~early /\ (later /= <<>> \/ rowPlace = NoPlace)]
                       /\ UNCHANGED done
          \* "ack-on-drain": the link acknowledges what it handed over, as before TOB-319.
          /\ unsaved' = IF Mutation = "ack-on-drain" THEN unsaved
                        ELSE unsaved \cup {placed[i].e[3] : i \in DOMAIN placed}
    /\ inbox' = SelectSeq(inbox, LAMBDA x : of[Slot(x.e)] /= s)
    /\ UNCHANGED <<barrierVars, of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, savedMem, late, reports,
                   log, cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo,
                   recvStamp, unfinished>>

(* The pump's timer, or Quit's flush, commits a session whose unfinished    *)
(* items changed: it queues them with the last place handed to the session  *)
(* (app_state.rs `flush_unfinished`, `commit`). A session with no place to  *)
(* give waits.                                                              *)
Flush(s) ==
    /\ up /\ dirty[s] /\ handed[s] /= NoPlace
    /\ queue' = [queue EXCEPT ![s] = [@ EXCEPT !.through = handed[s], !.snap = TRUE,
                                                !.parts = open[s]]]
    /\ dirty' = [dirty EXCEPT ![s] = FALSE]
    /\ UNCHANGED <<barrierVars, of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, inbox, unsaved, savedMem, late,
                   reports, log, cursor, kills, drops, resets, era, born, rewinds, fresh,
                   crashes, sentTo, recvStamp, open, handed, unfinished, done>>

(* The cursor a save leaves: the place saved with the rows. Round one raised *)
(* it by number and never let it fall, even into another journal.          *)
NewCursor(old, p) ==
    IF Mutation = "cross-journal"
      THEN IF p[2] > old[2] THEN p ELSE old
      ELSE p

(* The log writer saves a session's queued rows, its place and its          *)
(* unfinished items in one transaction, then reports the place (log.rs      *)
(* `PendingSession::retry`, `Files::through`, database.rs                   *)
(* `Database::append_logs`). With neither rows nor items, nothing is        *)
(* written and the place is reported. A failing save is this action not    *)
(* being taken: the rows wait in memory. "parts-apart" saves the items in   *)
(* a transaction of their own that leaves the cursor where it was.          *)
(*                                                                         *)
(* A save is durable once the directory barrier after its commit succeeds  *)
(* (`Database::append_logs` returns `Durability::DirectorySyncPending`      *)
(* when it doesn't). When it fails, the commit stands, a power cut may take *)
(* it back, and the place waits in `unsynced` until `Barrier`: the writer   *)
(* keeps the last write, or the unfinished items with                      *)
(* `PendingSession.unfinished_unsynced`. "report-unsynced" reports it at   *)
(* once, as an items-only save did before TOB-453's second round.          *)
DB(l, c, u, d) == [log |-> l, cursor |-> c, unfinished |-> u, done |-> d]
Save(s) ==
    /\ up /\ (queue[s].rows /= <<>> \/ queue[s].through /= NoPlace)
    /\ \E ok \in BOOLEAN :
         LET q == queue[s]
             write == q.rows /= <<>> \/ (q.snap /\ Mutation /= "parts-apart")
             held == ~ok /\ q.through /= NoPlace /\ Mutation /= "report-unsynced"
         IN /\ ok \/ (fails < MaxFails /\ Mutation /= "cursor-apart")
            /\ fails' = IF ok THEN fails ELSE fails + 1
            /\ unfinished' = IF q.snap /\ q.through /= NoPlace
                               THEN [unfinished EXCEPT ![s] = q.parts] ELSE unfinished
            /\ unsynced' = IF held THEN unsynced \cup {<<s, q.through>>} ELSE unsynced
            /\ IF Mutation = "cursor-apart" /\ q.rows /= <<>> /\ q.through /= NoPlace
                 \* The cursor in a transaction of its own, before the rows.
                 THEN /\ cursor' = [cursor EXCEPT ![s] = NewCursor(@, q.through)]
                      /\ late' = [late EXCEPT ![s] = [@ EXCEPT !.rows = @ \o q.rows,
                                                               !.through = q.through,
                                                               !.done = @ \cup q.done]]
                      /\ UNCHANGED <<log, reports, done>>
                 ELSE /\ log' = log \o q.rows
                      /\ done' = done \cup q.done
                      /\ cursor' = IF ~write \/ q.through = NoPlace THEN cursor
                                   ELSE [cursor EXCEPT ![s] = NewCursor(@, q.through)]
                      /\ reports' = IF q.through = NoPlace \/ held THEN reports
                                    ELSE reports \cup {<<s, q.through>>}
                      /\ UNCHANGED late
            /\ durable' = IF ok THEN DB(log', cursor', unfinished', done') ELSE durable
    /\ queue' = [queue EXCEPT ![s] = NoQueue[s]]
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, inbox, unsaved, savedMem,
                   kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo,
                   recvStamp, open, dirty, handed, cuts>>

(* A retry of the directory barrier succeeds (`Logs::flush`, or the next   *)
(* append, retrying `SessionLog::sync`): everything committed is durable,  *)
(* and each place it held is reported.                                     *)
Barrier ==
    /\ up /\ unsynced /= {}
    /\ durable' = DB(log, cursor, unfinished, done)
    /\ reports' = reports \cup unsynced
    /\ unsynced' = {}
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, inbox, unsaved, savedMem,
                   queue, late, log, cursor, kills, drops, resets, era, born, rewinds,
                   fresh, crashes, sentTo, partVars, fails, cuts>>

(* Only under "cursor-apart": the rows' own transaction, after their cursor. *)
SaveLate(s) ==
    /\ up /\ late[s].through /= NoPlace
    /\ log' = log \o late[s].rows
    /\ done' = done \cup late[s].done
    /\ reports' = reports \cup {<<s, late[s].through>>}
    /\ late' = [late EXCEPT ![s] = NoQueue[s]]
    /\ durable' = DB(log', cursor, unfinished, done')
    /\ UNCHANGED <<unsynced, fails, cuts, of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, inbox, unsaved, savedMem,
                   queue, cursor, kills, drops, resets, era, born, rewinds, fresh, crashes,
                   sentTo, recvStamp, open, dirty, handed, unfinished>>

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
    /\ UNCHANGED <<barrierVars, of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, acks, sentAck, received, inbox, queue, late, log,
                   cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo,
                   partVars>>

(* The link queues an acknowledgement when it moved (link.rs `State::ack`).  *)
SendAck ==
    /\ up /\ link = "up" /\ AckValue /= sentAck
    /\ sentAck' = AckValue
    /\ acks' = Append(acks, AckValue)
    /\ UNCHANGED <<barrierVars, of, row, jid, latest, journal, acked, trimmed, up, link, linked,
                   ljournal, wire, received, inbox, unsaved, savedMem, queue, late,
                   reports, log, cursor, kills, drops, resets, era, born, rewinds, fresh,
                   crashes, sentTo, partVars>>

(* The host takes an acknowledgement and trims its journal through it      *)
(* (serve.rs on `Frame::Ack`, journal.rs `Journal::ack`).                   *)
HostAck ==
    /\ acks /= <<>>
    /\ LET a == Head(acks) IN
       /\ acked' = Max({acked, a})
       /\ trimmed' = trimmed \cup {<<jid, born[n], n>> : n \in {m \in journal : m <= a}}
       /\ journal' = {n \in journal : n > a}
    /\ acks' = Tail(acks)
    /\ UNCHANGED <<barrierVars, of, row, jid, latest, up, link, linked, ljournal, wire, sentAck,
                   received, inbox, unsaved, savedMem, queue, late, reports, log,
                   cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo,
                   partVars>>

(* The network drops: whatever was in flight either way is lost            *)
(* (link.rs `Shared::connect_once` returning `Ended::Dropped`).            *)
Drop ==
    /\ link = "up" /\ drops < MaxDrops
    /\ link' = "down" /\ wire' = <<>> /\ acks' = <<>>
    /\ drops' = drops + 1
    /\ UNCHANGED <<barrierVars, of, row, jid, latest, journal, acked, trimmed, up, linked, ljournal,
                   sentAck, received, inbox, unsaved, savedMem, queue, late, reports,
                   log, cursor, kills, resets, era, born, rewinds, fresh, crashes, sentTo,
                   partVars>>

(* The machine's journal is replaced: its daemon and every event and number *)
(* it held are gone, and the new one numbers from 1 (journal.rs             *)
(* `Journal::open` on a new file). The transport ends with the daemon.      *)
Reset ==
    /\ resets < MaxResets
    /\ jid' = jid + 1 /\ latest' = 0 /\ journal' = {} /\ acked' = 0 /\ sentTo' = 0
    /\ link' = "down" /\ wire' = <<>> /\ acks' = <<>>
    /\ resets' = resets + 1
    /\ UNCHANGED <<barrierVars, of, row, trimmed, up, linked, ljournal, sentAck, received, inbox,
                   unsaved, savedMem, queue, late, reports, log, cursor, kills, drops, era,
                   born, rewinds, fresh, crashes, partVars>>

(* The machine's journal goes back, as one restored from a backup or damaged *)
(* would: the same identity, its latest number back at `b`, holding the      *)
(* events after the acknowledged cursor it had then, `a`, with the stamps    *)
(* they had. Numbers past `b` are given to new events from now on, by a new *)
(* daemon run with a stamp of its own.                                     *)
Rewind ==
    /\ rewinds < MaxRewinds /\ latest > 0
    /\ \E b \in 0..(latest - 1), a \in 0..N :
         /\ a <= b /\ a <= acked
         /\ latest' = b /\ acked' = a
         /\ journal' = {n \in 1..b : n > a}
         /\ sentTo' = IF sentTo > b THEN b ELSE sentTo
    /\ link' = "down" /\ wire' = <<>> /\ acks' = <<>>
    /\ rewinds' = rewinds + 1 /\ era' = era + 1 /\ fresh' = TRUE
    /\ UNCHANGED <<barrierVars, of, row, jid, trimmed, up, linked, ljournal, sentAck, received, inbox,
                   unsaved, savedMem, queue, late, reports, log, cursor, kills, drops, resets,
                   born, crashes, partVars>>

(* The host crashes or loses power. Its daemon and provider die, as in a     *)
(* reboot, and a commit the journal hadn't synced may be gone: with `FULL`   *)
(* every commit is synced before it returns, and the daemon sends only what  *)
(* it committed (journal.rs `SYNCHRONOUS`, serve.rs `journal_events`), so    *)
(* only numbers it never sent can go back. The daemon that starts next       *)
(* numbers its restart events from there at once (serve.rs, the restart     *)
(* path through `enqueue`), with its own stamp. "lossy-crash" lets the crash *)
(* take back sent numbers too, as `NORMAL` did, and compares numbers alone, *)
(* since the stamps would catch it as they catch a restored backup.         *)
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
    /\ UNCHANGED <<barrierVars, of, row, jid, born, trimmed, up, linked, ljournal, sentAck, received,
                   inbox, unsaved, savedMem, queue, late, reports, log, cursor, kills,
                   drops, resets, rewinds, fresh, partVars>>

(* The places `hello` shows the host (link.rs `State::seen`): each saved      *)
(* cursor, and what was received in the journal the link took on, each with *)
(* its stamp.                                                               *)
Seen == {savedMem[s] : s \in Sessions} \cup
        (IF ljournal = 0 THEN {} ELSE {<<ljournal, received, recvStamp>>})

(* The host takes a new identity when one of them is in its journal, past    *)
(* its latest number, or past its acknowledged cursor with another stamp     *)
(* than the one the host has for that number (serve.rs `Daemon::went_back`,  *)
(* journal.rs `Journal::stamp`, `Journal::rotate`). Its events keep their    *)
(* numbers under the new identity. "no-stamp" compares numbers alone, as the *)
(* code before TOB-455 did.                                                 *)
Rotates == Mutation \notin {"no-rotate", "cross-journal"}
           /\ \E p \in Seen :
                /\ p[1] = jid
                /\ \/ p[2] > latest
                   \/ /\ Mutation \notin {"no-stamp", "lossy-crash"}
                      /\ p[2] > acked /\ born[p[2]] /= p[3]

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
                                        THEN <<savedMem[s][1], latest, savedMem[s][3]>>
                                        ELSE savedMem[s]]
                    /\ UNCHANGED <<unsaved, inbox, recvStamp>>
                    /\ wire' = replay(IF linked THEN Max({received', acked}) ELSE acked)
               ELSE IF ljournal = host /\ ~behind
                 THEN /\ UNCHANGED <<received, savedMem, unsaved, inbox, recvStamp>>
                      /\ wire' = replay(IF linked THEN Max({received, acked}) ELSE acked)
                 \* Another journal, or the same one behind a place: every place goes.
                 ELSE /\ received' = 0 /\ unsaved' = {} /\ recvStamp' = 0
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
    /\ UNCHANGED <<barrierVars, of, row, latest, journal, acked, trimmed, up, acks, queue, late,
                   reports, log, cursor, kills, drops, resets, era, born, rewinds, crashes,
                   open, dirty, handed, unfinished, done>>

(* The app is killed (SIGKILL): everything but the database is gone.       *)
Kill ==
    /\ up /\ kills < MaxKills
    /\ up' = FALSE /\ link' = "down" /\ linked' = FALSE /\ ljournal' = 0
    /\ wire' = <<>> /\ acks' = <<>> /\ sentAck' = 0
    /\ received' = 0 /\ inbox' = <<>> /\ unsaved' = {} /\ savedMem' = Nowhere
    /\ queue' = NoQueue /\ late' = NoQueue /\ reports' = {}
    /\ recvStamp' = 0 /\ open' = NoParts /\ dirty' = [s \in Sessions |-> FALSE]
    /\ handed' = Nowhere /\ unsynced' = {}
    /\ kills' = kills + 1
    /\ UNCHANGED <<durable, fails, cuts, of, row, jid, latest, journal, acked, trimmed, log, cursor, drops,
                   resets, era, born, rewinds, fresh, crashes, sentTo, unfinished, done>>

(* The app's machine loses power: as a kill, and the database keeps only   *)
(* what was durable, so a commit whose directory barrier failed is gone.    *)
PowerCut ==
    /\ up /\ cuts < MaxCuts
    /\ up' = FALSE /\ link' = "down" /\ linked' = FALSE /\ ljournal' = 0
    /\ wire' = <<>> /\ acks' = <<>> /\ sentAck' = 0
    /\ received' = 0 /\ inbox' = <<>> /\ unsaved' = {} /\ savedMem' = Nowhere
    /\ queue' = NoQueue /\ late' = NoQueue /\ reports' = {}
    /\ recvStamp' = 0 /\ open' = NoParts /\ dirty' = [s \in Sessions |-> FALSE]
    /\ handed' = Nowhere /\ unsynced' = {}
    /\ log' = durable.log /\ cursor' = durable.cursor
    /\ unfinished' = durable.unfinished /\ done' = durable.done
    /\ cuts' = cuts + 1
    /\ UNCHANGED <<of, row, jid, latest, journal, acked, trimmed, kills, drops, resets,
                   era, born, rewinds, fresh, crashes, sentTo, durable, fails>>

(* The next launch reads each session's saved cursor, with its journal and  *)
(* stamp, and gives it to the machine's link (app_state.rs `AppState::new`,  *)
(* `open_link`; link.rs `Link::with_cursors`). It puts each session's saved  *)
(* unfinished items back after its restored rows, but not an item a saved   *)
(* row already completed (conversation.rs `restore_resuming`).               *)
(* "no-skip" forgets the cursors; "no-restore" leaves the items out.        *)
Launch ==
    /\ ~up
    /\ up' = TRUE
    /\ savedMem' = IF Mutation = "no-skip" THEN Nowhere ELSE cursor
    /\ open' = IF Mutation = "no-restore" THEN NoParts
               ELSE [s \in Sessions |-> SelectSeq(unfinished[s], LAMBDA e : e \notin done)]
    \* Opening the workspace syncs its folder before anything reads it, and fails
    \* the launch if it can't (database.rs `Database::open`), so whatever a killed
    \* run committed is durable before the link starts from it.
    /\ durable' = DB(log, cursor, unfinished, done)
    /\ UNCHANGED <<unsynced, fails, cuts, of, row, jid, latest, journal, acked, trimmed, link, linked, ljournal,
                   wire, acks, sentAck, received, inbox, unsaved, queue, late, reports,
                   log, cursor, kills, drops, resets, era, born, rewinds, fresh, crashes, sentTo,
                   recvStamp, dirty, handed, unfinished, done>>

Next ==
    \/ HostAppends \/ Receive \/ (\E s \in Sessions : Drain(s)) \/ SendAck \/ HostAck
    \/ \E s \in Sessions : Save(s) \/ SaveLate(s) \/ Flush(s)
    \/ \E r \in reports : Report(r)
    \/ Drop \/ Reset \/ Rewind \/ Crash \/ Connect \/ Kill \/ Launch
    \/ Barrier \/ PowerCut

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* The rules.                                                              *)
(***************************************************************************)

\* No event is both trimmed by the host and missing from what the app saved: a row
\* from the saved log, a part from its session's saved unfinished items, or from the
\* saved row that completed its item. Events a replaced journal took with it were
\* lost by the host, not trimmed; the app ends their turn with a note (app_state.rs
\* `end_lost_turns`).
NothingLost ==
    \A e \in trimmed :
        IF row[Slot(e)] THEN e \in Range(log)
        ELSE e \in done \cup Range(unfinished[of[Slot(e)]])

\* Each row is saved once, and each part is applied once: an unfinished item holds no
\* part twice, nor one whose item a saved row already completed.
SavedOnce ==
    /\ \A i, j \in DOMAIN log : i /= j => log[i] /= log[j]
    /\ \A s \in Sessions :
        /\ \A i, j \in DOMAIN open[s] : i /= j => open[s][i] /= open[s][j]
        /\ Range(open[s]) \cap done = {}

\* Each session's rows are saved, and its parts applied, in the order the host
\* numbered them: by journal, then by number within one.
InOrder(a, b) == a[1] < b[1] \/ (a[1] = b[1] /\ a[3] < b[3])
SavedInOrder ==
    /\ \A i, j \in DOMAIN log :
        (i < j /\ of[Slot(log[i])] = of[Slot(log[j])]) => InOrder(log[i], log[j])
    /\ \A s \in Sessions : \A i, j \in DOMAIN open[s] : i < j => InOrder(open[s][i], open[s][j])
=============================================================================
