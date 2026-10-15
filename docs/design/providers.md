# Provider integration design

Status: proposal, 2026-10-15. Nothing here is implemented. Decisions marked **Decide** need approval before work starts.

Keystone drives the user's own installed `claude` and `codex` programs. This document says how, and where it deliberately differs from Zeron (MIT, Rust, studied at commit `3b2f780`) and T3 Code. Protocol facts were checked against Claude Code 2.1.300 and codex-cli 0.162.1 on 2026-10-15.

## What Keystone needs

From [scope.md](../scope.md): local execution, provider-native capabilities and configuration, steering-only messages during active work, native compaction, approvals that settlement never grants, detailed agent activity, checkpoints and rollback, cross-provider handoff and second opinions, recovery of interrupted work, and import of existing provider sessions. From the working preferences: low build time, measured performance, and native verification.

Keystone is one local process with one window owner. It does not need multi-device sync, a mobile client, or an RPC layer between its own parts.

## What the others do

| Concern | Zeron | T3 Code | Keystone (proposed) |
| --- | --- | --- | --- |
| Claude transport | `claude -p` stream-json, undocumented `--permission-prompt-tool stdio` | Official Agent SDK in Node | `claude -p` stream-json, documented `--permission-prompts host` |
| Codex transport | One `codex app-server` per chat, plus probe processes | `codex app-server` | One shared `codex app-server` for all sessions |
| Driver shape | Async tasks mixing IO, timers, and protocol logic; 1,500–2,500 lines per driver | Effect-TS adapters | Pure state machines with no IO; one shared process host |
| Per text delta | Line `String` → `Value` → typed struct → event → journal write → broadcast copy → fold | SDK objects → event store | Line bytes → one typed decode → append to the live item; nothing else |
| Persistence | Per-event journal, Loro document, full snapshot to SQLite every second | SQLite event store | Completed items only, appended once per item |
| Rebuild on update | Whole transcript re-materialized on every 120 ms commit | Projections | Only the live item changes; completed items are immutable |
| Tool output | Not stored for Claude or Codex | Stored | Stored, capped at head and tail |
| Approvals | Every Claude tool auto-allowed; Codex forced to `never` and full access | Surfaced | Surfaced as items; provider settings respected |
| Turn end | Native end plus lifecycle tracking, 3 s and 30 s fallback timers, retired watchdogs | Provider events | Native end only; process exit synthesizes a failed end |
| Version safety | `--version` read, no minimum enforced | SDK version pin | Tested range enforced as a warning, plus feature detection |
| Session import | None | Not checked | One-time import from provider transcripts |

## Design

### 1. Drivers are pure state machines

A driver turns provider output into Keystone events and Keystone commands into provider input. It never spawns, reads, writes, sleeps, or sets timers.

```rust
trait Driver {
    /// Feed one complete output line. Returns events and any lines to write back.
    fn on_line(&mut self, line: &[u8], out: &mut Outputs);
    /// Translate a user action into provider input lines.
    fn command(&mut self, command: Command, out: &mut Outputs);
    /// The process ended. Close any open turn with a terminal status.
    fn on_exit(&mut self, status: ExitStatus, out: &mut Outputs);
}
```

One `ProcessHost` owns every child process: spawning, line framing, stdin writes, stderr draining into a bounded tail, and process-tree teardown. Drivers are tested by replaying recorded transcripts, so the edit loop never starts a provider. This is the "scripted providers exercise the same protocol boundary" rule in [testing.md](../testing.md).

Zeron's drivers combine all of these concerns with fallback timers. Most of its documented bugs (doubled input requests, stale completions, mis-tagged task notifications) sit where IO and protocol state meet.

### 2. Items, not delta events

The model is a list of items per turn, each with a stable ID: `UserMessage`, `AgentMessage`, `Reasoning`, `ToolCall { kind, input, output, status }`, `Approval { request, decision }`, `Plan`, `Compaction`, and `Notice`. Both providers already speak in items: Codex has `item/started`, deltas, and `item/completed`; Claude has content blocks with partial-message stream events.

A delta appends to the live item's text in place. It does not become a message on a channel, a journal line, or a copy. The reader thread updates the session's live turn under a short lock and sets a dirty flag. The UI thread checks the flag once per frame and re-renders only the live item. That limits UI work to the frame rate however fast tokens arrive. It also attacks the cause suspected in KS-005: a backlog of synchronous redraws.

Completed items never change, so their layout can be cached. The transcript uses GPUI's virtualized `list`, so a long session costs only its visible rows.

### 3. Storage: the provider owns the conversation; Keystone keeps a neutral log

**Decide.** Each session gets one append-only file, `sessions/<id>.log`, holding completed items, approval decisions, turn boundaries, and usage. Nothing is written per delta. Each completed item is appended once, and the file is synced at turn end and after every approval decision. Tool output is kept with a cap: the first and last 32 KB plus the byte count. The existing `workspace.json` stays the small index of sessions and preferences.

Why keep a log when the providers keep transcripts:
- Claude's transcript format is "internal … and changes between versions" according to its own docs, so Keystone cannot depend on it at runtime.
- Handoffs and second opinions need one neutral representation across providers.
- Showing history should not require starting a provider process.

Provider transcripts are still the source of truth for the conversation itself. Resume uses the provider's session ID (`--resume` for Claude, `thread/resume` for Codex), never a replay of Keystone's log.

On startup, a session whose log has no turn end is marked interrupted. When the user resumes it, Keystone reconciles: it asks Codex for items after the last logged one (`thread/items/list`) or reads the tail of Claude's transcript. That reconciliation tolerates format changes because it is best effort and only fills gaps.

Import of existing sessions reuses the same readers. It is a versioned, best-effort, one-time conversion into a Keystone log, never a live dependency.

No SQLite and no CRDT. Both add build time and solve problems Keystone does not have, namely concurrent writers and sync.

### 4. Turns end only when the provider says so

The core runs one lifecycle per session: idle → running → (awaiting approval ↔ running) → ended with `completed`, `interrupted`, or `failed`.
- **Claude:** the turn ends at `result`. A `result` with `origin.kind = "task-notification"` is a turn the agent started itself, shown as such.
- **Codex:** the turn ends at `turn/completed`.
- **Process exit:** `on_exit` closes any open turn as failed with the stderr tail.

There are no watchdogs, heartbeats, or quiet-period timers. Zeron added and later retired several of these after "stuck working" incidents. Capabilities are data detected at startup (Claude's `system/init` capabilities, Codex's `initialize` response), not trait booleans.

### 5. Process topology

**Decide.**
- **Codex:** one shared `codex app-server` for the whole app. Threads are multiplexed by ID, model listing uses the same process, and probe processes go away. If it crashes, a supervisor restarts it and resumes the threads that were loaded. The docs imply concurrent turns on several threads but do not state it; the first spike verifies it before anything depends on it.
- **Claude:** one process per active session, as the protocol requires. Idle processes with no background tasks are stopped after 10 minutes and resumed lazily. When the user starts typing in a new Claude session, Keystone may start the process early to hide startup; measure before keeping that.

Children start in their own process group. Teardown sends the protocol interrupt, then SIGINT, SIGTERM, and SIGKILL with short grace periods, then kills descendants found through `/proc`. Agents put shells in separate groups, so the group alone is not enough. Zeron learned this; Keystone avoids spawning `ps` to do it.

### 6. Permissions are real

- **Claude:** Keystone uses the documented `--permission-prompts host` (2.1.259 and later). It does not use the undocumented `--permission-prompt-tool stdio`. Each `can_use_tool` control request becomes an `Approval` item.
- **Codex:** Keystone sends the user's own approval policy and sandbox settings. It does not force `never` and full access. Its approval requests also become `Approval` items.
- **Inbox:** an approval blocks its turn and appears in the attention inbox. Settlement never answers it, which the scope requires.
- **Unknown requests:** any control request or server request Keystone does not handle gets an explicit error response, so the agent never waits forever. Zeron leaves unknown Claude control requests unanswered.

### 7. Parsing

Each line is decoded once into typed, tagged structs that borrow from the line where possible. Fields Keystone does not show are skipped without being built. Large tool output is read through a capped field, so it is never fully allocated. Unknown message types are counted and, in debug builds, logged raw; they are never fatal.

Codex's `optOutNotificationMethods` turns off notifications Keystone does not render, so they are never sent at all.

The message types are written by hand for the subset Keystone uses. A test validates them against the JSON schema generated by the pinned Codex version.

### 8. Steering and interrupt

Steering follows the scope: a message sent during a turn steers that turn, and there is no follow-up queue.
- **Claude:** a user message with priority `now`, or `next` while a tool is running, because `now` aborts a running MCP call. Zeron found this rule; credit it.
- **Codex:** `turn/steer` with the expected turn ID. If the turn already ended, the message starts the next turn.

Interrupt uses Claude's `interrupt` with `cancel_queued` when the `interrupt_cancel_queued_v1` capability is present, or Codex's `turn/interrupt`. Interrupting a turn does not stop background tasks. Stopping the runtime does, and asks first when background work is live.

### 9. Version safety

A table lists the tested version range for each provider. Keystone reads `--version` once per binary and caches it on disk by path, size, and modification time. An untested version shows a warning in the UI and still runs. Recorded transcripts from each tested version live in `crates/keystone-agent/tests/fixtures/`. An opt-in `bash scripts/check.sh live` runs one tiny real turn per provider in a disposable repository and checks that the driver sees every message type it expects. It uses the user's subscription, so it never runs by default.

### 10. Crates and threads

A new `keystone-agent` crate holds items, drivers, the session lifecycle, the process host, and the log store. It does not depend on GPUI, so its tests compile without the graphics stack. The desktop crate only renders and sends commands.

The process host uses one blocking reader thread per child process for stdout and one for stderr, plus a writer thread with a bounded queue. That is a few threads per active session and no async runtime in Keystone's own code. `async-process` and `smol` are already compiled through GPUI if that changes.

## Terms

Keystone runs the user's own unmodified `claude` binary and never handles their credentials. That matches Anthropic's allowance for "an end user … signing in to the unmodified Claude Code binary with their own Claude subscription". The product name must not include "Claude". OpenAI says local and open-source apps can keep using app-server authentication, but it has "never been permitted for commercial or hosted services". Revisit both before any commercial release.

## Measurements

These are added to `scripts/perf.py` before the drivers are tuned:
- Decode throughput on recorded transcripts, in MB/s per provider.
- Delta-to-frame cost: draw time with a live item receiving 1,000 deltas per second, in the GPUI test platform.
- Bytes written to disk per turn.
- Cold start to first `system/init` for Claude, and `thread/start` latency on a warm Codex app-server.

## Plan

1. **Spike, about 1 day.** Confirm concurrent turns on one Codex app-server. Confirm that `--permission-prompts host` delivers `can_use_tool` on 2.1.300. Record one short transcript of each provider covering streaming, a tool call, an approval, an interrupt, and a steer. This consumes a small amount of the user's subscription. **Decide.**
2. **Codex first.** The `keystone-agent` crate, the Codex driver with fixture tests, the process host, the log store, and a desktop transcript with submit, stream, and interrupt.
3. **Claude driver,** against the same item model and fixtures.
4. **Approvals and steering** in the UI and inbox, then resume, crash reconciliation, and import.
5. **Handoff and second opinion** from the neutral log.

## Sources

- Claude Code headless, CLI reference, sessions, Agent SDK TypeScript reference, legal and compliance: code.claude.com/docs, fetched 2026-10-15.
- Codex App Server: learn.chatgpt.com/docs/app-server, fetched 2026-10-15. Schemas generated from codex-cli 0.162.1.
- Zeron: github.com/zeronsh/zeron at `3b2f780`, `crates/harness`, `crates/engine/src/sessions.rs`, `run_journal.rs`, `crates/doc`. Only design ideas are used, with credit; no code is copied.
