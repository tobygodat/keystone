# 0012. Run sessions on other machines through a headless host over SSH

Status: Proposed, 2026-11-02 (TOB-306). Builds on [ADR 0010](0010-terminal-browser-scheduling-editor-diffs-remote-devices.md), which brings remote environments into scope.

## Context

TOB-306 ports T3 Code's remote environments: a session runs on another machine, with that machine's own `claude`, `codex` and repositories, and the user drives it from the local window. The session has to survive a network drop. A hosted relay and a mobile client are out of scope.

Keystone is one local process with no RPC between its parts. `AppState` owns every session and talks to providers through the `Connection` trait (`crates/keystone-agent/src/connection.rs`). `ClaudeConnection` and `CodexConnection` spawn the CLIs as children, and the UI drains their `Event`s on a wake. `Store` and `Logs` keep the workspace and the neutral conversation log in one local SQLite database.

T3 Code runs its whole server on the remote machine. The desktop reaches it through `ssh -L` to a loopback port, pairs with a one-time code, and holds a bearer token. The remote database owns projects and threads. That fits T3, which already splits a server from its clients. Keystone has no such split.

## Decision

**A headless host owns the provider processes on the remote machine.** `keystone-host` is a new binary in `keystone-agent`, with no GPUI. It runs the existing `ClaudeConnection` and `CodexConnection` on that machine, one set per linked workspace, and forwards their events. It holds no workspace, no drafts and no neutral log.

**The local app stays the owner of Keystone's state.** Sessions, projects, drafts, the neutral log and the attention inbox stay in the local database. The provider's own conversation stays where the provider writes it, which for a remote session is the remote machine. A `Machine` record in the workspace names each linked machine, and a session or project on another machine records which one.

**The cut is the `Connection` trait.** `RemoteConnection` implements `Connection` locally and sends each call to the host as a frame. Events come back the same way and land in the same inbox, so `Conversation` doesn't change. `AppState` keys connections, and the catalog, usage and MCP status they report, by machine as well as provider and account. `Event`, `Command` and their payloads gain serde derives. Calls that can't cross the wire, such as the `run_in_order` closures checkpoints use, become named host operations in a later slice.

**SSH is the only transport.** Keystone runs the system `ssh` with `BatchMode=yes` and keep-alives, and runs `keystone-host attach` on the remote. `attach` pipes its stdin and stdout into a long-lived `keystone-host serve` daemon through a Unix socket in a directory only the user can open, and starts that daemon when none is running. SSH does all the authentication. Keystone adds no tokens, pairing codes or listening ports. Tailscale is a network path, not a second transport: the user links a tailnet name, and plain sshd or Tailscale SSH carries the connection.

**The protocol is newline-delimited JSON over that pipe.** It has a versioned handshake, request and reply frames for `Connection` calls, and sequenced event frames. The same framing is used by both providers' own protocols, and `serde_json` is already a dependency.

**The host keeps an event journal, so a network drop loses nothing that fits in it.** The daemon numbers every event and appends it to a SQLite journal on the remote machine before sending it. The app acknowledges what it has drained. After a drop the app reconnects with backoff, asks for every event after the last one it received, and sends again every call that had no reply. The host runs each call id once, so a resent call never reaches the provider twice. A backlog past 64 MiB drops transcript text, with a visible gap marker, and never a session's turn or approval state. The provider keeps running through the drop, and an approval request waits for the user. Keystone never answers it on the user's behalf.

**The first slice is Claude sessions only.** The user links a machine by its SSH name, starts a Claude session in a folder on it, and sends, steers, interrupts and answers approvals. The session survives a network drop. Quit leaves the machine's sessions running. Codex, accounts on the remote machine, attachments, checkpoints and rollback, forks, the orchestrator MCP tools, import, crash recovery from provider files, and the Files, Changes and pull request surfaces come in later slices. Until then they say they aren't available for sessions on another machine. They never act on a local path by mistake. The [remote machines design](../design/remote.md) lists the slices.

### Alternatives considered

- **Run the provider over plain SSH** (`ssh host claude …` as the provider's pipe). This is the least code, but the turn dies when the TCP connection does, and nothing buffers the events in between. It fails the network-drop requirement.
- **Run all of Keystone's backend on the remote machine** (T3's model). The desktop would become a client of a remote `Store`, `Logs` and `AppState`. Keystone would need an RPC layer over every store operation and a second database to reconcile, and a machine going offline would take its sessions' history with it. This costs far more than the feature needs.
- **Forward a loopback TCP port and pair with a token** (T3's transport). A loopback port on the remote machine is open to every account on that machine, so it needs pairing and a token store. A Unix socket in a private directory gets the same isolation from the operating system.
- **Run the whole app remotely and forward the window** (waypipe, as `scripts/laptop-preview` does). That isn't a product feature, it doesn't work from macOS, and it needs a graphical session on the remote machine.

## Consequences

- Keystone is no longer one process with no RPC. `docs/ARCHITECTURE.md` changes in the first build PR.
- `keystone-agent` ships a second binary, `keystone-host`, which has to be installed on each remote machine. In the first slice the user installs it from a checkout with `cargo install`. A version check in the handshake refuses a host with a different protocol and says why.
- Every surface that reads the session's folder must check the session's machine. Local paths and remote paths never mix.
- The workspace schema gains a `machines` projection and a machine on sessions and projects. TOB-183 also changes storage, so the build PR rebases on whichever lands first.
- A device in the device panel (TOB-307) belongs to a machine, the way a session does. The design doc says what that means for TOB-307.
- Quit leaves remote sessions running. The `quit` message carries a `keep_running` flag, and a per-machine setting, off by default, ends them instead. Locally, Quit still ends every session as ADR 0001 says. The next launch replays what happened while Keystone was closed and restores each remote session's running turn and waiting approvals.
