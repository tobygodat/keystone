# Remote machines

Status: Design, not built (TOB-306). The decision and the alternatives are in [ADR 0012](../adr/0012-remote-machines-through-a-headless-host.md). This document says how it works and what each slice builds.

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
- **`Link`** (`keystone-agent`, `remote/link.rs`) is one connection to one machine. It owns the `ssh` child, writes frames to its stdin, reads frames on a thread, tracks the connection state, and reconnects. Every `RemoteConnection` for that machine shares it.
- **`RemoteConnection`** (`remote/connection.rs`) implements `Connection` for one machine, provider and account. `AppState` keys connections by `(machine, provider, account)` where it keys them by `(provider, account)` today. Everything else keyed by `(provider, account)` gains the machine too: the routing of `Catalog`, `Commands`, `Usage` and `McpStatus` in `pump`, and the check in `app_state.rs` that decides whether an MCP report still belongs to a live connection. A remote machine's default account must never overwrite the local default account's catalog or usage.
- **`keystone-host attach`** connects its stdin and stdout to the daemon's socket. It starts the daemon when the socket is missing or refuses the connection.
- **`keystone-host serve`** is the daemon. It holds one `ClaudeConnection` or `CodexConnection` per client, provider and account. The existing connections key their sessions by the app's bare `u64` id, so two workspaces linked to one machine each get their own connections and never share a session id. The daemon numbers the events, writes them to the journal, and sends them to the attached app.

## Linking a machine

1. The user runs **Link a machine** from the command palette or **Settings > Machines**, and types an SSH destination: a `Host` from `~/.ssh/config`, `user@host`, or a tailnet name.
2. Keystone runs `ssh -T -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=3 <destination> 'sh -lc "exec keystone-host attach"'`. The login shell gives the host the same `PATH` the user's terminal has, which is where `claude` and `codex` are found.
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

The daemon's socket is `host.sock` in `$XDG_RUNTIME_DIR/keystone-host/` when that variable is set, and in `~/.keystone-host/run/` otherwise. The directory is created with mode 0700 and checked on every start. If another user owns it or can open it, the daemon refuses to start. A file lock beside the socket keeps a second daemon from starting. The daemon trusts whatever connects to the socket. Other accounts on the machine can't reach it. Any process running as the same user can, which is the same reach that user's processes already have over `claude`, its logins and the repositories.

There is no listening TCP port, so nothing on the machine's network can talk to the host.

**Tailscale.** A tailnet name is an SSH destination like any other, and either the remote machine's sshd or Tailscale SSH accepts it. A later slice suggests destinations in the link dialog from `~/.ssh/config` and from the online peers in `tailscale status --json`. A direct TCP connection over the tailnet, without SSH, would need its own authentication and is not planned.

## Wire protocol

Each frame is one JSON object on one line, in UTF-8, at most 16 MiB. The field `t` names the frame type. A frame that doesn't parse ends the link with an error. That is a bug on one side, not something to skip.

**Handshake.**

```json
{"t":"hello","protocol":1,"client":"6c1f…","after":1041}
{"t":"welcome","protocol":1,"version":"0.4.0","os":"linux","arch":"x86_64","hostname":"desk","providers":[{"provider":"claude","version":"2.1.300"},{"provider":"codex","missing":"not on PATH"}],"oldest":980,"latest":1057}
{"t":"refused","reason":"protocol 2 is needed, this host speaks 1"}
```

`client` is a random id the workspace keeps, so two Keystone workspaces linked to one machine never see each other's sessions. `after` is the last event the app has received, or `null` on a first link and on each launch. With `null`, the host sends every event the app hasn't acknowledged.

**Calls.** One call per `Connection` method the slice supports, with a reply:

```json
{"t":"call","id":7,"op":"send","args":{"provider":"claude","account":null,"session":12,"cwd":"/home/dev/src/app","resume":null,"choice":{"model":null,"effort":null,"speed":null},"approval":{…},"text":"…"}}
{"t":"reply","id":7,"ok":null}
{"t":"reply","id":8,"err":"steer refused: no turn is running"}
```

The ops are `send`, `steer`, `interrupt`, `command` (which carries a serialized `Command`), `request_catalog`, `set_choice`, `set_approval`, `hold_follow_ups`, `release_follow_ups`, `evict_idle`, `decline_idle` and `quit`. Later slices add `set_fork`, `put_attachment`, `checkpoint`, `read_history`, `list_threads` and `list_folder`. A reply's `err` becomes the `Err` the `Connection` method returns.

**Calls run once.** A call's `id` counts up from 1 for the life of the app's `Link`, and the `Link` keeps every call until its reply arrives. The host runs each call and records its reply in the journal database, in the same transaction that marks the id as seen. When the link drops, the app can't know whether an unanswered call ran. On reconnect it sends every unanswered call again with the same id. The host runs a call it hasn't seen, and answers a call it has seen with the recorded reply without running it again. So a `send` whose reply was lost doesn't reach the provider twice, and an approval answer that was in flight still arrives. The host keeps recorded replies until the app acknowledges a later call.

`send` and `steer` return `Ok` locally only after the reply. While the machine isn't connected, `send` returns an error at once and the draft stays.

**Events.**

```json
{"t":"event","seq":1042,"provider":"claude","account":null,"session":12,"event":{"ItemDelta":{…}}}
{"t":"gap","from":1043,"to":1190,"sessions":[12,15]}
{"t":"synced","seq":1057}
{"t":"sessions","sessions":[{"session":12,"running":true,"approvals":[…]}]}
{"t":"quit","keep_running":true}
{"t":"ack","seq":1042,"calls":7}
{"t":"ping"}
{"t":"pong"}
```

`event` carries an `Event` as serde writes it. `provider` and `account` route it to the right `RemoteConnection`, because `AppState::pump` routes `Catalog`, `Commands`, `Usage` and `McpStatus` by them. `gap` says the journal no longer holds those events, and names the sessions they belonged to. `synced` ends the replay after a `hello`, and `sessions` follows it on a launch (see [Quit](#quit-idle-and-the-daemons-lifetime)). `ack` carries the event cursor and the highest call id whose reply the app has. Either side sends `ping` every 5 s and drops the link after 15 s with no frame of any kind.

**Received and drained are two cursors.** Frames arrive in order on one pipe, so the `Link` knows the highest sequence it has received, and drops a replayed event at or below it before routing. That is the `after` it sends in `hello`. Events received but not yet drained are in the app's memory, and a dropped link doesn't lose them. The `Link` acknowledges a different number: the highest sequence below which every event has been drained by its `RemoteConnection`. `pump` drains connections one at a time, so connection A can hand over sequences 10 and 12 while connection B still holds 11. The acknowledgement stays at 10 until B is drained.

**Session keys.** The host keys sessions by `(client, session)`, where `session` is the app's own `u64` id. It never makes up ids of its own.

**Serde.** `Event`, `Command`, `ApprovalState`, `RequestId`, `Approval`, `ModelInfo`, `Catalog`, `Commands` and the other payload types gain `Serialize` and `Deserialize`. Payloads that are already serde types keep their format. `Image` crosses as its media type and SHA-256, never as a local path, once attachments are supported.

## Connection states

The `Link` is in one state at a time, and the sidebar and the session header show it next to the machine's name:

- **Connecting.** The first attempt since launch or since linking.
- **Connected.** `welcome` arrived, and the replay is done.
- **Reconnecting.** The link dropped. Attempts back off 1, 2, 4, 8, 16 and then every 30 s, each with up to 20% jitter. A success resets the backoff.
- **Failed.** SSH refused the login or the host key, the host is missing, or the protocol differs. Retrying wouldn't help, so the link waits for the user to press **Retry**.

The state changes wake the pump like an event does. While a machine isn't connected, Send and the approval buttons in its sessions are disabled and say why, and the draft stays. Nothing is queued, because a message during work steers and there is no follow-up queue (ADR 0001). Calls already in flight when the link dropped are sent again on reconnect, as [Calls run once](#wire-protocol) describes. A call the host answers with an error after a reconnect, such as an approval answer for a request the provider no longer holds, shows in the session like any provider error.

## Reconnecting after a drop

When the network drops, `ssh` exits or goes quiet, and the local `Link` moves to Reconnecting. On the remote machine, `attach` ends when its stdin closes. The daemon keeps running, and so do the provider processes. A turn in progress finishes. An approval request waits until the user answers it.

The daemon numbers each event in one sequence per client, and writes it to its journal before sending it. The journal is a SQLite table at `$XDG_STATE_HOME/keystone-host/journal.sqlite3`, or `~/Library/Application Support/Keystone Host/journal.sqlite3` on macOS. `KEYSTONE_HOST_DIR` overrides both locations for tests.

Beside the journal, the daemon keeps a `sessions` table: for each client and session, the provider thread id, whether a turn is running, and the approval requests the provider is waiting on. It updates that row in the same transaction as the event that changed it, and never trims it with the journal.

**Replay is fenced.** One writer thread per daemon appends events and owns the attachment. On `hello` it does these steps in order, without appending anything in between:

1. Closes the previous attachment for that client, if one is open. A frame that arrives later on the old socket is ignored.
2. Makes the new attachment current and notes the journal's highest sequence, `H`.
3. Sends every journaled event after `after` up to `H`, then `synced`.
4. Sends live events, which all come after `H`, as they are appended.

Live events appended during the replay wait in the new attachment's queue, so none falls between the replay and live delivery. The `Link` drops any event at or below its received cursor, so an overlap applies each event once.

Each step is safe to repeat:

- **A second `attach` for the same client replaces the first.** After a drop, the old SSH session can stay open on the remote side for minutes. Step 1 closes it, and calls on the old socket that never got a reply are sent again on the new one with their ids.
- **The journal drops what the app acknowledged, and only item events when it is full.** It keeps at most 64 MiB of unacknowledged events per client. Past that it drops the oldest `ItemDelta` events first, because each item's `ItemCompleted` repeats its full text. If it has to drop more, it drops the oldest `ItemStarted` and `ItemCompleted` events and records a `gap` naming their sessions. It never drops `ThreadReady`, `TurnStarted`, `TurnCompleted`, `ApprovalRequested`, `ApprovalResolved` or `Failed`. Those are small, and a session can't resume or be answered without them. The app puts the existing gap marker into each named session's log, so the lost transcript is visible. So a drop loses nothing while the backlog fits in 64 MiB, and past that it loses transcript text but never a session's state.
- **A daemon crash doesn't lose the journal.** Each provider process runs in a process group of its own (`host.rs`), so it doesn't die with the daemon by signal. It loses its stdin when the daemon dies, and both CLIs exit on end of input. That has to be proven, for the CLI and the processes it started, with the fake providers and the real `claude` in slice 1. The next `attach` starts a new daemon, which reads the `sessions` table. For each session with a running turn, it appends the events the driver emits when its provider exits unexpectedly (`on_exit` in `claude.rs` and `codex.rs`), with the reason that the host restarted. That ends the turn and clears its approvals the way a local provider crash does. It is a host fact, not agent output. The next message resumes the provider thread with `--resume`, as it does locally. Tests cover a crash after the app acknowledged `TurnStarted`, and a crash while an approval waits.

**What isn't durable in the first slice.** The app's cursors live in memory. If the local app is killed after it acknowledged an event and before the log entry from that event was saved, that entry is lost, as it is for a local session today (see [storage](storage.md)). Slice 3 saves each session's cursor in the same transaction as its log rows, and the acknowledgement becomes the lowest saved cursor among the machine's sessions.

## Quit, idle and the daemon's lifetime

- **Quit** sends `{"t":"quit","keep_running":true}` after the logs are flushed and the last `ack` is sent. With `keep_running` set, the daemon keeps that client's provider processes, so a turn in progress finishes and an approval request waits for the next launch. With it unset, the daemon ends them, as Quit ends local ones. Each machine has a setting in **Settings > Machines**, **End sessions on this machine when Keystone quits**, off by default, that clears the flag. If the machine isn't connected at Quit, nothing can reach it, and its sessions keep running whatever the setting says.
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

1. **Host and protocol, no UI.** Serde on `Event`, `Command`, `ModelChoice` and their payloads. The `keystone-host` binary with `serve`, `attach`, the socket checks and the journal. `Link` and `RemoteConnection`, with reconnect. Tests run the real host binary with `keystone-fake-claude`, using `keystone-host attach` directly in place of `ssh`. Tests kill the transport during a turn and check that every event arrives once and in order after the reconnect, that a `send` whose reply was lost reaches the provider once, that events appended during a replay are neither lost nor doubled, and that a daemon crash ends a running turn and its approvals.
2. **Link a machine and run a Claude session there.** `Machine` in the workspace, **Link a machine**, **Settings > Machines** with the Quit setting, Quit that leaves remote sessions running and the catch-up on the next launch, a new session on a chosen machine, machine names and connection states in the sidebar and the session header, Send disabled while reconnecting, and every local-folder surface gated by `Session.machine`. Proven with two real machines, including pulling the network during a turn.
3. **Durable cursor.** Each session's cursor saves with its log rows, and `ack` follows the save.
4. **Codex, accounts and attachments.**
5. **Checkpoints, forks, recovery and import.**
6. **Files, Changes, pull requests, file mentions and the folder picker.**
7. **Orchestrator MCP tools for remote sessions.**
8. **Installing the host from Keystone, and Tailscale suggestions.**

TOB-306 is done when slice 2 is proven. Slices 3 to 8 get their own Linear issues.

## Testing

- Protocol and journal tests run in `keystone-agent` with temporary `KEYSTONE_HOST_DIR` directories and the fake providers. They never use the network or the user's real `~/.ssh`.
- The transport is a command line, so a test runs `keystone-host attach` as a local child, and the product runs it behind `ssh`. Everything above the byte pipe is the same code.
- The proof for slice 2 uses two real machines: the Mac and the Linux desktop on the tailnet. It links the Linux machine, runs a Claude session there, and turns off Wi-Fi during a turn. When Wi-Fi comes back, the turn's events are all there, in order, once.
