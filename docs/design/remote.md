# Remote machines

Status: Slice 1, the host and its protocol, is built (TOB-306). The app doesn't use it yet: slices 2 to 8 are not built. The decision and the alternatives are in [ADR 0012](../adr/0012-remote-machines-through-a-headless-host.md). This document says how it works and what each slice builds.

A session can run on another machine. Its `claude` or `codex` runs there, in a folder on that machine, with that machine's logins and configuration. The local window shows and drives it like any other session.

## Parts

```
local machine                                   remote machine
┌──────────────────────────────┐   ssh -T host  ┌─────────────────────────────────────┐
│ AppState                     │  keystone-host │ keystone-host attach                │
│  ├ ClaudeConnection (local)  │     attach     │   stdin/stdout <-> host.sock        │
│  └ RemoteConnection ─ Link ──┼────────────────┼─▶ keystone-host serve (daemon)      │
│ Store, Logs (workspace.db)   │  JSON Lines    │     ├ ClaudeConnection, CodexConn.  │
└──────────────────────────────┘                │     └ journal.sqlite3               │
                                                └─────────────────────────────────────┘
```

- **`Machine`** (`keystone-core`) is a linked machine: an id, a label, and the SSH destination the user typed. It holds no secret. The workspace keeps machines in a `machines` projection, added in the next free schema version.
- **`Session.machine`** and **`Project.machine`** are `Option<MachineId>`. `None` is this machine. A session's machine is fixed when it is created, like its provider. A project on another machine is identified by the machine and the path the host reported, never by a local `canonicalize`.
- **`Link`** (`keystone-agent`, `remote/link.rs`) is one connection to one machine. It owns the `ssh` child, reads frames on a thread, tracks the connection state, and reconnects. Nothing the app calls waits on the transport: each frame goes into a queue, and one writer thread per transport writes the queue to `ssh`'s stdin. A transport that stops reading holds up only that thread, until the link's watchdog ends the transport. Every `RemoteConnection` for that machine shares the link.
- **`RemoteConnection`** (`remote/connection.rs`) implements `Connection` for one machine, provider and account. `AppState` keys connections by `(machine, provider, account)` where it keys them by `(provider, account)` today. Everything else keyed by `(provider, account)` gains the machine too: the routing of `Catalog`, `Commands`, `Usage` and `McpStatus` in `pump`, and the check in `app_state.rs` that decides whether an MCP report still belongs to a live connection. A remote machine's default account must never overwrite the local default account's catalog or usage.
- **`keystone-host attach`** connects its stdin and stdout to the daemon's socket. It starts the daemon when the socket is missing or refuses the connection.
- **`keystone-host serve`** is the daemon. It holds one `ClaudeConnection` or `CodexConnection` per client, provider and account. The existing connections key their sessions by the app's bare `u64` id, so two workspaces linked to one machine each get their own connections and never share a session id. The daemon numbers the events, writes them to the journal, and sends them to the attached app.

## Linking a machine

1. The user runs **Link a machine** from the command palette or **Settings > Machines**, and types an SSH destination: a `Host` from `~/.ssh/config`, `user@host`, or a tailnet name.
2. Keystone runs `ssh -T -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=3 -- <destination> 'sh -lc "exec keystone-host attach"'` (`remote::ssh`). The login shell gives the host the same `PATH` the user's terminal has, which is where `claude` and `codex` are found. A destination that is empty, starts with `-` or holds whitespace is refused before `ssh` runs.
3. The app sends `hello`. The host answers `welcome` with its protocol, version, OS, architecture, hostname, and the provider CLIs it found with their versions.
4. Keystone saves the `Machine` and shows its status: connected, with the providers found.

Each failure says what to do and doesn't save the machine:

| What happened | What the user sees |
| --- | --- |
| `ssh` exits 255 with "Permission denied" | SSH refused the login. Keystone uses keys and the SSH agent only, so the user adds a key. |
| `ssh` exits 255 with "Host key verification failed" | Keystone never accepts a host key. The user runs `ssh <destination>` once in a terminal to accept it. |
| The remote shell exits 127 | `keystone-host` isn't installed there, with the install command. |
| `welcome` has another protocol | The host's version, the version Keystone needs, and the install command. |
| No frame within 15 s | The machine didn't answer. |

**Installing the host.** In the first slice the user installs it on the remote machine from a checkout of the same commit: `cargo install --locked --path crates/keystone-agent --bin keystone-host`. Later, Keystone copies its own `keystone-host` over `scp` when the remote OS and architecture match its own, and uses a release archive once there is one. T3 Code's launcher, which downloads a pinned archive and checks its hash, is the model for that step.

## Transport and authentication

SSH carries every byte and does all the authentication. The `ssh` options above make it fail fast and never prompt. A password-only host is not supported in the first slice. T3 Code's askpass prompt can be ported later.

The daemon's socket is `host.sock` in `$XDG_RUNTIME_DIR/keystone-host/` when that variable is set, and in `~/.keystone-host/run/` otherwise. The directory is created with mode 0700 and checked on every start, by `serve` and by `attach`. The folder that holds the journal gets the same check, and the journal and its `-wal` and `-shm` files are mode 0600. If another user owns it or can open it, the daemon refuses to start, and `attach` answers with a `refused` frame naming the reason, so the app shows it instead of retrying. `attach` sends `refused` only for a cause that retrying can't fix. When it fails to reach a daemon for any other reason, such as one that doesn't start within 5 s, it exits with the reason on stderr, and the link tries again as it does after a drop. A file lock beside the socket, `host.lock`, keeps a second daemon from starting and holds the daemon's process ID. Beside it, each provider process has a watcher's lock file (see [Reconnecting after a drop](#reconnecting-after-a-drop)). A new daemon waits up to 5 s for the lock, so one started while the last is still exiting, after it removed its socket, takes over instead of giving up. `attach` starts the daemon in a process group of its own, with its output in `host.log` beside the journal. The daemon trusts whatever connects to the socket. Other accounts on the machine can't reach it. Any process running as the same user can, which is the same reach that user's processes already have over `claude`, its logins and the repositories.

There is no listening TCP port, so nothing on the machine's network can talk to the host.

**Tailscale.** A tailnet name is an SSH destination like any other, and either the remote machine's sshd or Tailscale SSH accepts it. A later slice suggests destinations in the link dialog from `~/.ssh/config` and from the online peers in `tailscale status --json`. A direct TCP connection over the tailnet, without SSH, would need its own authentication and is not planned.

## Wire protocol

Each frame is one JSON object on one line, in UTF-8, at most 16 MiB. The field `t` names the frame type. A frame that doesn't parse ends the link with an error. That is a bug on one side, not something to skip. The frames are `remote/frame.rs`. A provider is written as its existing serde name, `"Claude"` or `"Codex"`.

**Handshake.**

```json
{"t":"hello","protocol":1,"client":"6c1f…","after":1041}
{"t":"welcome","protocol":1,"version":"0.4.0","os":"linux","arch":"x86_64","hostname":"desk","providers":[{"provider":"Claude","version":"2.1.300 (Claude Code)"},{"provider":"Codex","missing":"not on PATH"}],"oldest":980,"latest":1057}
{"t":"refused","reason":"protocol 2 is needed, this host speaks 1"}
```

`client` is a random id the workspace keeps, so two Keystone workspaces linked to one machine never see each other's sessions. `after` is the last event the app has received, or `null` on a first link and on each launch. With `null`, the host sends every event the app hasn't acknowledged. A `null` also starts a new run of the app's call ids, so the host forgets the client's recorded calls (see Calls run at most once).

**Calls.** One call per `Connection` method the slice supports, with a reply. The call names its connection with `provider` and `account`, as an event does:

```json
{"t":"call","id":7,"provider":"Claude","account":null,"op":"send","args":{"session":12,"cwd":"/home/dev/src/app","resume":null,"choice":{"model":null,"effort":null,"speed":null},"approval":{…},"text":"…"}}
{"t":"reply","id":7,"ok":null}
{"t":"reply","id":8,"err":"steer refused: no turn is running"}
```

The ops are `send`, `command` (which carries a serialized `Command`), `request_catalog`, `set_choice`, `set_approval`, `hold_follow_ups`, `release_follow_ups`, `evict_idle` and `decline_idle`. As in the `Connection` trait, `send` steers a running turn and `command` carries `Interrupt` and approval answers, so there are no separate `steer` and `interrupt` ops. Quit is the `quit` frame, not a call. Later slices add `set_fork`, `put_attachment`, `checkpoint`, `read_history`, `list_threads` and `list_folder`. A reply's `err` shows in the call's session: for `send`, as a warning while a turn runs and as a failure otherwise, the way a local `send` reports a message that fails after it was accepted; for `request_catalog`, as a failed catalog; for the rest, as a warning. The first slice's host runs Claude only, as the machine's default login: a call for Codex or another account gets an `err` that says so.

**Calls run at most once.** A call's `id` counts up from 1 for the life of the app's `Link`, and the `Link` keeps every call until its reply arrives. When the link drops, the app can't know whether an unanswered call ran, so on reconnect it sends every unanswered call again with the same id. The host handles a call in three steps. It records the id as accepted, in one transaction, and only then runs the call. Then it records the reply. If it can't record the acceptance, it doesn't run the call and replies with an error saying so. A call sent again gets its recorded reply without running again. One that was accepted and has no reply, because the daemon stopped between the steps or the reply couldn't be recorded, isn't run again either: its reply is an error saying it may or may not have run. So a `send` whose reply was lost never reaches the provider twice, and an approval answer that was in flight still arrives. The host keeps recorded calls until the app acknowledges a later call.

`send` returns `Ok` as soon as the call is queued, without waiting for the host. While the machine isn't connected, `send` returns an error at once and the draft stays. Those are the only errors it returns, since any later one would come after the message may have gone out. A steer the host refuses, or a reply saying the message may not have run, arrives later in the session as described above. A call whose reply hasn't arrived stays queued and is sent again on every reconnect until it is answered.

**Events.**

```json
{"t":"event","seq":1042,"provider":"Claude","account":null,"session":12,"event":{"ItemDelta":{…}}}
{"t":"gap","provider":"Claude","account":null,"from":1043,"to":1190,"sessions":[12,15]}
{"t":"warning","provider":"Claude","account":null,"session":12,"message":"…"}
{"t":"synced","seq":1057}
{"t":"sessions","sessions":[{"provider":"Claude","account":null,"session":12,"running":true,"approvals":[…]}]}
{"t":"quit","keep_running":true}
{"t":"ack","seq":1042,"calls":7}
{"t":"ping"}
{"t":"pong"}
```

`event` carries an `Event` as serde writes it. `provider` and `account` route it to the right `RemoteConnection`, because `AppState::pump` routes `Catalog`, `Commands`, `Usage` and `McpStatus` by them. `gap` says the journal no longer holds the item events of those sessions between `from` and `to`. It names the connection it belongs to, and its place in the sequence is `to`, so a link that received events up to some point inside the range still gets it. Until slice 2 puts the log's gap marker in each named session, `RemoteConnection` reports a gap as a warning in each one. `warning` is a warning for one session outside the sequence. The host doesn't journal it or send it again. `synced` ends the replay after a `hello`, and `sessions` follows it on a launch (see [Quit](#quit-idle-and-the-daemons-lifetime)). `ack` carries the event cursor and the highest call id whose reply the app has. Either side sends `ping` every 5 s and drops the link after 15 s with no frame of any kind.

**Received and drained are two cursors.** Frames arrive in order on one pipe, so the `Link` knows the highest sequence it has received, and drops a replayed event at or below it before routing. That is the `after` it sends in `hello`. Events received but not yet drained are in the app's memory, and a dropped link doesn't lose them. The `Link` acknowledges a different number: the highest sequence below which every event has been drained by its `RemoteConnection`. `pump` drains connections one at a time, so connection A can hand over sequences 10 and 12 while connection B still holds 11. The acknowledgement stays at 10 until B is drained. Events for a provider and account with no `RemoteConnection` registered don't hold the acknowledgement back. They wait in the `Link` until a connection registers and drains them. Otherwise a connection the app never makes would stop the host trimming its journal for every other one. Like any received event, they are lost if the app is killed first, which slice 3's durable cursor addresses. A `RemoteConnection` that replaces another for the same provider and account takes over its events, and dropping the replaced one leaves the new one registered.

**Session keys.** The host keys sessions by `(client, session)`, where `session` is the app's own `u64` id. It never makes up ids of its own.

**Serde.** `Event`, `Command`, `ApprovalState`, `RequestId`, `Approval`, `ModelInfo`, `Catalog`, `Commands` and the other payload types gain `Serialize` and `Deserialize`. Payloads that are already serde types keep their format. `Image` crosses as its media type and SHA-256, never as a local path, once attachments are supported.

## Connection states

The `Link` is in one state at a time, and the sidebar and the session header show it next to the machine's name:

- **Connecting.** The first attempt since launch or since linking.
- **Connected.** `welcome` arrived, and the replay is done.
- **Reconnecting.** The link dropped. Attempts back off 1, 2, 4, 8, 16 and then every 30 s, each with up to 20% jitter. A success resets the backoff.
- **Failed.** SSH refused the login or the host key, the host is missing, or the protocol differs. Retrying wouldn't help, so the link waits for the user to press **Retry**.

The state changes wake the pump like an event does. While a machine isn't connected, Send and the approval buttons in its sessions are disabled and say why, and the draft stays. Nothing is queued, because a message during work steers and there is no follow-up queue (ADR 0001). Calls already in flight when the link dropped are sent again on reconnect, as [Calls run at most once](#wire-protocol) describes. A call the host answers with an error after a reconnect, such as an approval answer for a request the provider no longer holds, shows in the session like any provider error.

## Reconnecting after a drop

When the network drops, `ssh` exits or goes quiet, and the local `Link` moves to Reconnecting. On the remote machine, `attach` ends when its stdin closes. The daemon keeps running, and so do the provider processes. A turn in progress finishes. An approval request waits until the user answers it.

The daemon numbers each event in one sequence per client, and writes it to its journal before sending it. The journal is a SQLite table at `$XDG_STATE_HOME/keystone-host/journal.sqlite3`, or `~/Library/Application Support/Keystone Host/journal.sqlite3` on macOS. `KEYSTONE_HOST_DIR` overrides both locations for tests.

Beside the journal, the daemon keeps a `sessions` table: for each client and session, the provider thread id, whether a turn is running, and the approval requests the provider is waiting on. It updates that row in the same transaction as the event that changed it, and never trims it with the journal. Claude reports no event when the app answers an approval, so the host removes the approval from the row when it runs the answer, and the app's `Link` does the same when it sends one.

**Replay is fenced.** One writer thread per daemon appends events and owns the attachment. On `hello` it does these steps in order:

1. Closes the previous attachment for that client, if one is open. A frame that arrives later on the old socket is ignored.
2. Makes the new attachment current, but not live: events appended now are journaled and not sent to it.
3. Queues every journaled event after `after` through the journal's end as it is at this step, then `synced`.
4. Makes the attachment live. Events appended from then on, which all come after the replay's last, are queued behind it as they are appended.

Nothing is appended between steps 3 and 4, so every event reaches the new attachment once: through the replay if it was appended before step 3, live if after. An append that lands during step 2 is still safe, because step 3 reads the journal after it, and a test forces one there. The `Link` drops any event at or below its received cursor, so an overlap would still apply each event once.

Each step is safe to repeat:

- **A second `attach` for the same client replaces the first.** After a drop, the old SSH session can stay open on the remote side for minutes. Step 1 shuts its socket down at once, dropping what was queued for it, and calls on the old socket that never got a reply are sent again on the new one with their ids.
- **A reader that falls behind is disconnected.** Each attachment may have at most the journal's cap plus two frames of live output queued and unwritten, 96 MiB by default. Its replay doesn't count, however large, so a reader that keeps up with live events is never disconnected over the size of its replay. A reader that lets more pile up, such as an SSH session that stopped reading, is shut down, and its writer thread and queue go with it. So is one whose input ends or sends a frame that doesn't parse. Only a `refused` reply is written out before its socket closes. A reader catches up from the journal when it reconnects, as after any drop. The socket's two halves are shut down one at a time: once the other side has closed its half, macOS refuses to shut down both together, and the write half alone still ends a blocked writer.
- **The journal drops what the app acknowledged, and only item events when it is full.** It keeps at most 64 MiB of unacknowledged events per client. Past that it drops the oldest `ItemDelta` events first, because each item's `ItemCompleted` repeats its full text. If it has to drop more, it drops the oldest `ItemStarted` and `ItemCompleted` events and records a `gap` naming their sessions. A run of lost items is one gap: when nothing was kept for that connection since its last gap, that gap grows to cover the new loss. It never drops `ThreadReady`, `TurnStarted`, `TurnCompleted`, `ApprovalRequested`, `ApprovalResolved` or `Failed`. Those are small, and a session can't resume or be answered without them. The app puts the existing gap marker into each named session's log, so the lost transcript is visible. So a drop loses nothing while the backlog fits in 64 MiB, and past that it loses transcript text but never a session's state.
- **A daemon crash doesn't lose the journal, and leaves nothing running into the next turn.** Each provider process runs in a process group of its own (`host.rs`), so it doesn't die with the daemon by signal. It loses its stdin when the daemon dies, and both CLIs exit on end of input. That alone isn't enough: Claude Code 2.1.317 first waits for a tool command it is running, which it starts in a session of its own, and after the daemon's SIGKILL a 57 s shell command kept it and the shell running until the command ended. A provider that exits at once can leave that shell behind, under init.

  So each provider has a watcher, `keystone-host reap` (`remote/reap.rs`). The daemon starts it from the connection's spawn hook, before the provider gets any input, in a session of its own, so a signal to the daemon's group or session doesn't reach it. It runs the daemon's own binary. On Linux that is `/proc/self/exe`, which still reaches it after an install replaces or deletes the file. macOS can't run a binary through a descriptor (`execve` of `/dev/fd/N` is refused), so there it is the path the daemon started from, which an install replaces by renaming a new binary into place. The watcher writes one byte on a pipe once it holds the lock and has pinned the provider, and the hook waits up to 2 s for it. Without it, the watcher is stopped, its file is removed, the provider is stopped before it gets any input, and its start fails, so no provider runs unwatched. The watcher holds an exclusive lock on `reap-<pid>-<start>.lock` in the run folder for its whole life: the daemon creates and locks the file, writes the session's client, ID, provider and account in it, and hands the locked descriptor to the watcher. Every 250 ms the watcher pins the provider's tree, in any group or session, by PID and start time, and keeps every process it has seen while that process lives, so a shell whose parent exited stays watched under init. Pins are pidfds on Linux, and on macOS a start-time check just before each signal, as local teardown uses (`host.rs`).

  The daemon's death is the watcher's parent changing: the daemon started it, so the kernel gives it another parent the moment the daemon exits, whatever process later takes the daemon's PID. A pipe the daemon holds open would be less certain, since a provider started at the same instant could inherit its write end and keep it open. While the daemon lives, the watcher ends nothing. When the provider exits, the daemon's teardown ends the rest, as for a local session: SIGTERM, and SIGKILL 2 s later. The watcher keeps looking and holds its lock until every process it has pinned is gone, so a tool shell that ignores SIGTERM in a session of its own is still held if the daemon dies within those 2 s. A process a tool left running in the background, such as a server, is held the same way, so it ends when the daemon exits, including after the daemon was idle for its full time. When the daemon dies at any point first, the watcher freezes every process it holds with SIGSTOP, rescans for children of what it froze until a scan finds nothing new, sends SIGKILL, waits until all of it has exited, and only then removes its file and lets the lock go. If something doesn't exit, it tries again every 250 ms and never lets go, and notes in its file the processes it still waits for. A process Keystone may not signal, such as one run through `sudo`, counts as running until it exits on its own: on Linux only `ESRCH`, not `EPERM`, means a pinned process is gone, and macOS, which refuses to read another user's process in full, counts whatever holds the pinned PID.

  The next `attach` starts a new daemon. It listens first, so `attach` isn't kept waiting, then looks for the last daemon's watcher files. A watcher removes its file before it lets go, so a file still there whose lock nobody holds lost its watcher before its tree was gone. If the provider it names still runs, with the same PID and start time, the daemon starts a new watcher for it, which ends its tree at once, since the daemon that started it is gone, and holds the session until that watcher lets go. If the provider is gone, what it started can't be found: the session gets a warning that its last run's watcher stopped unexpectedly and that processes its Claude Code started may still be running, naming that process, the log says so, and the session's turn ends. The daemon waits up to 3 s for held files, then holds each session a held lock names: a call that would start a turn in it, `send` or a `command` that sends or opens, is answered with an error saying the machine is still stopping the processes of the session's last run, and so are an answer, a widget action and an interrupt, which would reach no process. Quit with `keep_running` unset ends a held session's turn as Quit does, once its watchers let go. Once all of a session's watchers let go, and for every other session at once, the daemon reads the session's state, from the `sessions` table followed by any events still waiting for the journal, and if a turn ran or approvals waited, it appends the events the driver emits when its provider exits unexpectedly (`on_exit` in `claude.rs` and `codex.rs`), with the reason that the host restarted. That ends the turn and clears its approvals the way a local provider crash does. It is a host fact, not agent output. A session still held 60 s after the daemon started (`KEYSTONE_HOST_HOLD_MS` in tests) gets a warning naming the processes that haven't stopped, the log says so, and the session stays held. The next message resumes the provider thread with `--resume`, as it does locally, and nothing of the old turn runs beside it.

  Tests cover a crash after the app acknowledged `TurnStarted`, a crash while an approval waits, a crash with providers whose child is in their group, in a group of its own, and in a session of its own under a provider that exits on end of input, a provider that exits while the daemon lives and leaves a child that ignores SIGTERM, a watcher that can't end what it holds, one that fails before it takes over, one killed before its daemon while the provider still runs, a file whose provider is gone, Quit during a hold, and a binary replaced while the daemon runs. Gaps remain. A process that left the tree between two of the watcher's scans, born and orphaned within 250 ms, is out of reach, as it is when a local provider stops. That includes a child started just after a scan when the daemon dies and the provider exits on end of input before the next one. The tests wait until the watcher has pinned the child before they kill a daemon, so they don't exercise that case. Such a child keeps running after the daemon dies, whether it's detached or still in the provider's group, and the next message can resume beside it with no warning. The 250 ms is nominal, and under load the gap between scans is longer. A daemon that dies after starting a provider and before starting its watcher leaves that provider unwatched too. A process the user can't signal, such as one run through `sudo`, holds its session until it exits. On macOS a watcher sees such a process only if it pinned it before it changed user, since macOS doesn't let it read another user's process in full. A watcher killed by something else after its provider exited leaves what it held out of reach, and its session says so.

  Each watcher scans the whole process table every 250 ms: on macOS `proc_listallpids` and one `proc_pidinfo` per process, on Linux one read of `/proc/<pid>/stat` per process. On this Mac, with about 1,480 processes, one idle watcher of the release build used 330 to 350 ms of CPU per 30 s over three runs, about 1.1% of one core, nearly all of it in `proc_pidinfo`. The cost grows with the number of processes and with the number of providers running, so five sessions with live providers cost about 5.5% of one core on such a machine. Linux wasn't measured.

**A journal that fails to write keeps a session's state.** When appending a connection's events fails, the daemon keeps them in memory in order and tries again every 250 ms, at most 256 events per try. Nothing of that connection goes out live past them until the journal takes them. What waits is bounded by the journal's cap, and past it the same rule applies: the oldest `ItemDelta` events go first, then the oldest `ItemStarted` and `ItemCompleted` events, with a `gap` in their place, and never a lifecycle event. The attached app gets a `warning` in each session the waiting events belong to, once per attachment, and the daemon's log says why once per failure. Events then arrive late, never out of order or twice. Quit with `keep_running` unset decides which turns to end from the `sessions` table followed by the waiting events, and queues their ends behind them. Lifecycle events, catalogs and warnings are never dropped, so a long outage can still grow the backlog. Once every backlog together holds more than twice the cap, the daemon refuses `send` and the commands that send or open with an error naming the journal's failure, so no turn starts, and its log says so once. Running turns still end. Twice the cap leaves a full cap of lifecycle events beyond what the drop rule keeps, which is hours of busy sessions, and keeps the daemon's memory for waiting events within twice the cap. A daemon idle for its full time exits even with events waiting, and its log says how many it never journaled; the next daemon ends any turn they leave running, as after a crash.

**What isn't durable in the first slice.** The app's cursors live in memory. If the local app is killed after it acknowledged an event and before the log entry from that event was saved, that entry is lost, as it is for a local session today (see [storage](storage.md)). Slice 3 saves each session's cursor in the same transaction as its log rows, and the acknowledgement becomes the lowest saved cursor among the machine's sessions.

## Quit, idle and the daemon's lifetime

- **Quit** sends `{"t":"quit","keep_running":true}` after the logs are flushed and the last `ack` is sent. With `keep_running` set, the daemon keeps that client's provider processes, so a turn in progress finishes and an approval request waits for the next launch. With it unset, the daemon ends them, as Quit ends local ones, and a turn running in one ends as interrupted, its approvals resolved. Each machine has a setting in **Settings > Machines**, **End sessions on this machine when Keystone quits**, off by default, that clears the flag. If the machine isn't connected at Quit, nothing can reach it, and its sessions keep running whatever the setting says.
- **The next launch catches up.** The app reconnects to each machine with `after` set to `null`, and the host replays every event the app hasn't acknowledged. A clean Quit acknowledged everything before it, so that replay is exactly what happened while Keystone was closed. After `synced`, the host sends one `sessions` frame with each session's state from its `sessions` table: whether a turn is running, and each approval request the provider is waiting on, in full. The app applies it on top of the restored log, so a turn that started before Quit shows as running and its waiting approval shows its card.
- **Closing every window** isn't quitting, so remote sessions keep running, as local ones do.
- **No client attached.** While no app is attached, the daemon ends a provider process once its turn ends and no approval waits. That frees the machine. The next message resumes the provider thread with `--resume`, as it does after idle eviction.
- **The daemon exits** after 10 minutes with no attached client and no provider process.
- **Idle eviction** of Claude processes works through `evict_idle` and `decline_idle`, as it does locally. It stays off by default, as it is locally.

This is the one place remote sessions differ from local ones. Locally, Quit still ends every session, as ADR 0001 says. A remote machine is where work should keep going while the laptop is closed, so Quit leaves it running unless the user turns that off.

## What stays local and what moves

| Concern | First slice | Later |
| --- | --- | --- |
| Workspace, drafts, neutral log, inbox | Local `Store` and `Logs`, unchanged | |
| Provider process, binary lookup, version check | Host | |
| Provider's own conversation files | Remote machine, owned by the provider | |
| Accounts | The remote machine's default login only | `Account` gains a machine, and the host builds the `LaunchProfile` |
| Picking the folder | The user types a path, and the host checks it exists | `list_folder` gives a folder picker |
| Attachments | Refused for remote sessions, with the reason | `put_attachment` uploads the bytes by hash into the host's cache |
| Checkpoints and rollback | Off for remote sessions | `run_in_order` closures become a typed `checkpoint` op |
| Forks | Off for remote sessions | `set_fork` |
| Orchestrator MCP tools | Reported as unavailable on remote machines | The host serves a loopback MCP endpoint on the remote machine and forwards each request over the link to the local server, with the local `Issuer`'s token |
| Crash recovery, import | Off for remote sessions | `read_history` and `list_threads` run the existing readers on the host |
| Files, Changes, pull request, worktrees, file mentions, Open in editor | Say they aren't available for sessions on another machine | Host ops per surface |
| Codex | Not offered on remote machines | The host runs `CodexConnection`. Its history and import calls become ops. |

Every surface that reads a session's folder checks `Session.machine` first. A remote path is never passed to a local filesystem call. Some of those calls run without the user opening anything, and slice 2 gates each one:

- `dispatch_directory` checks the folder with `std::fs::metadata` and `worktree::is_intact` before every send. For a remote session the host checks the folder instead, as part of `send`.
- `begin_checkpoint` reads the git tree in the folder before a turn. It does nothing for a remote session.
- Crash recovery reads `~/.claude/projects` through `claude_root`. It doesn't run for a remote session.
- The MCP status check asks the local `claude` and `codex` maps whether a session is open. It asks the session's own connection instead.
- `ProjectId::new` and `WorkingDirectory::new` canonicalize the path locally. A remote project or session takes the path the host reported.

A test in slice 2 creates a remote session whose folder exists locally and one whose folder doesn't, and checks that no local filesystem call sees either path.

## Devices on other machines (TOB-307)

A device belongs to a machine, the way a session does. The iOS Simulator and the Android Emulator run where the app is built, which for a remote session is the remote machine, and the agent installs and drives builds from there.

- TOB-307 gives its device record a `machine: Option<MachineId>`, matching `Session.machine`. The device panel beside a session lists only devices on that session's machine.
- The `device_*` orchestrator tools act on devices on the calling session's machine. An agent on a remote machine never gets a local simulator it can't install a build on.
- Until a later slice carries device frames, the device panel beside a remote session says devices on other machines aren't supported yet. It doesn't offer a local device in their place.
- Device frames will be a separate kind of frame on the same link. They are not journaled, and they are dropped while the link is down, because only the newest frame matters. Input goes the other way as calls. Device frames may need their own SSH channel so a video stream can't delay session events. That choice belongs to the slice that builds them.
- The iOS Simulator needs macOS, so a Linux machine offers only the Android Emulator.

## Slices

1. **Host and protocol, no UI.** Serde on `Event`, `Command`, `ModelChoice` and their payloads. The `keystone-host` binary with `serve`, `attach`, the socket checks and the journal. `Link` and `RemoteConnection`, with reconnect. Tests run the real host binary with `keystone-fake-claude`, using `keystone-host attach` directly in place of `ssh`. Tests kill the transport during a turn and check that every event arrives once and in order after the reconnect, that a `send` whose reply was lost reaches the provider once, that events appended during a replay are neither lost nor doubled, and that a daemon crash ends a running turn and its approvals only once nothing of its provider still runs.
2. **Link a machine and run a Claude session there.** `Machine` in the workspace, **Link a machine**, **Settings > Machines** with the Quit setting, Quit that leaves remote sessions running and the catch-up on the next launch, a new session on a chosen machine, machine names and connection states in the sidebar and the session header, Send disabled while reconnecting, and every local-folder surface gated by `Session.machine`. Proven with two real machines, including pulling the network during a turn.
3. **Durable cursor.** Each session's cursor saves with its log rows, and `ack` follows the save.
4. **Codex, accounts and attachments.**
5. **Checkpoints, forks, recovery and import.**
6. **Files, Changes, pull requests, file mentions and the folder picker.**
7. **Orchestrator MCP tools for remote sessions.**
8. **Installing the host from Keystone, and Tailscale suggestions.**

TOB-306 is done when slice 2 is proven. Slices 3 to 8 get their own Linear issues.

## Testing

- Protocol and journal tests run in `keystone-agent` (`tests/remote_host.rs`) with temporary `KEYSTONE_HOST_DIR` directories and the fake providers. They never use the network or the user's real `~/.ssh`. `KEYSTONE_HOST_DIR` holds the socket folder (`run/`) as well as the journal, so a test never touches the user's runtime folder. More variables exist only for tests, and an empty one counts as unset: `KEYSTONE_HOST_JOURNAL_CAP` sets the cap in bytes, `KEYSTONE_HOST_IDLE_MS` how long the daemon waits idle before it exits, `KEYSTONE_HOST_HOLD_MS` how long a session stays held before it is told why, and `KEYSTONE_HOST_QUEUE_LIMIT` how many bytes of live output an attachment may have queued, at least 64 KiB. `KEYSTONE_HOST_FAULTS` names a folder where a file turns on a fault by its name. Faults exist only in a debug build, which `cargo test` makes and `cargo build --release` and `cargo install` don't; the tests that need one are ignored in a release build. `crash-before-reply` makes the daemon kill itself after it runs a call and before it records the reply, `append` makes every journal append fail, `append-during-replay` appends what the client's connections reported in the middle of a replay, `reap-hold` keeps a watcher from signalling anything, as if what it holds can't be ended, and `reap-refuse` makes a watcher exit before it says it holds the provider. The scripted Claude's `child-term` setting of `ignore` makes its child ignore SIGTERM. On the app's side, `Link::set_silence` shortens how long a link waits for a silent transport.
- The transport is a command line, so a test runs `keystone-host attach` as a local child, and the product runs it behind `ssh`. Everything above the byte pipe is the same code.
- The proof for slice 2 uses two real machines: the Mac and the Linux desktop on the tailnet. It links the Linux machine, runs a Claude session there, and turns off Wi-Fi during a turn. When Wi-Fi comes back, the turn's events are all there, in order, once.
