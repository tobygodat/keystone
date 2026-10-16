# Provider integration design

Status: Codex is implemented as described, in `crates/keystone-agent` (2026-10-15). Claude Code is not built yet. The spike results below have been folded in. Log IO, history loading, and provider startup run off the UI thread, and the transcript is virtualized (2026-10-16; see sections 3 and 10).

Keystone drives the user's own installed `claude` and `codex` programs. This document says how, and where it deliberately differs from Zeron (MIT, Rust, studied at commit `3b2f780`) and T3 Code. Protocol facts were checked against Claude Code 2.1.300 and codex-cli 0.162.1 on 2026-10-15.

## What Keystone needs

From [scope.md](../scope.md): local execution, provider-native capabilities and configuration, steering-only messages during active work, native compaction, approvals that settlement never grants, detailed agent activity, checkpoints and rollback, cross-provider handoff and second opinions, recovery of interrupted work, and import of existing provider sessions. From the working preferences: low build time, measured performance, and native verification.

Keystone is one local process with one window owner. It does not need multi-device sync, a mobile client, or an RPC layer between its own parts.

## What the others do

| Concern | Zeron | T3 Code | Keystone (proposed) |
| --- | --- | --- | --- |
| Claude transport | `claude -p` stream-json with `--permission-prompt-tool stdio` | Official Agent SDK in Node | `claude -p` stream-json with the same flags the official SDKs pass |
| Codex transport | One `codex app-server` per chat, plus probe processes | `codex app-server` | One shared `codex app-server` for all sessions |
| Driver shape | Async tasks mixing IO, timers, and protocol logic; 1,500–2,500 lines per driver | Effect-TS adapters | Pure state machines with no IO; one shared process host |
| Per text delta | Line `String` → `Value` → typed struct → event → journal write → broadcast copy → fold | SDK objects → event store | Line bytes → one typed decode → append to the live item; nothing else |
| Persistence | Per-event journal, Loro document, full snapshot to SQLite every second | SQLite event store | Completed items only, appended once per item |
| Rebuild on update | Whole transcript re-materialized on every 120 ms commit | Projections | Only the live item changes; completed items are immutable |
| Tool output | Not stored for Claude or Codex | Stored | Stored, capped at head and tail |
| Approvals | Every Claude tool auto-allowed; Codex forced to `never` and full access | Surfaced | Surfaced as items; provider settings respected |
| Turn end | Native end plus lifecycle tracking, 3 s and 30 s fallback timers, retired watchdogs | Provider events | Native end only; process exit synthesizes a failed end |
| Version safety | `--version` read, no minimum enforced | SDK version pin | Tested range shown as a warning, plus capability detection |
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

Completed items never change, so their layout can be cached. The transcript uses GPUI's virtualized `list` with bottom alignment, so a long session costs only its visible rows. The list has one entry per row plus a status entry (approval card, "Working…", errors) after the last row. The conversation reports which rows it added or changed, and only those are remeasured; the status entry is remeasured on every change. The list follows new rows while it is scrolled to the bottom. Once the user scrolls up, it keeps the row at the top of the view at the same offset, even while that row streams or a row above it changes. Sending a message or selecting a session returns to the bottom. Row content comes from one function, `transcript::row`, so a change to how rows look, such as Markdown, stays out of the list code.

### 3. Storage: the provider owns the conversation; Keystone keeps a neutral log

**Decide.** Each session gets one append-only file, `sessions/<id>.log`, holding completed items, approval decisions, turn boundaries, and usage. Nothing is written per delta. Each completed item is appended once, and the file is synced at turn end and after every approval decision. Tool output is kept with a cap: the first and last 32 KB plus the byte count. The existing `workspace.json` stays the small index of sessions and preferences.

Why keep a log when the providers keep transcripts:
- Claude's transcript format is "internal … and changes between versions" according to its own docs, so Keystone cannot depend on it at runtime.
- Handoffs and second opinions need one neutral representation across providers.
- Showing history should not require starting a provider process.

Provider transcripts are still the source of truth for the conversation itself. Resume uses the provider's session ID (`--resume` for Claude, `thread/resume` for Codex), never a replay of Keystone's log.

**Order.** Each item, decision, and shown turn end carries `seq`, its place in the transcript as it was shown: the order in which its row first appeared. A restore sorts rows by it. Completion order is wrong for steering: a steer is shown, and logged, while the reply it interrupts is still streaming, and that reply is logged only when it completes. Logs written before `seq` existed have none; their rows keep file order, and rows written later are placed after them. Each placed entry also carries `run`, which identifies the run of Keystone that wrote it, and its `seq` counts from the first row that run added. A restore places each run after every row before it. So a run never needs the saved history to log in the right place: when the history cannot be read, or has not arrived when Keystone closes, what happens is still saved and later restores after the history.

**Off the UI thread.** The desktop's conversation state does no IO. It collects entries in an outbox, and `log::Logs` writes them on one worker thread for all sessions, in the order they were queued, so a session's entries stay in order. Each history load reads on a thread of its own, so a read that never returns, such as from a FIFO or a stalled network disk, holds up neither writes nor other sessions' loads. A session's history is requested once, the first time it is needed, before anything is written for it, and arrives as a report; events and sent messages that arrive meanwhile wait and are applied after the history. If the read fails, the conversation goes on without the history and shows a warning for the rest of the session; what was sent meanwhile is shown and saved. A message sent meanwhile has already gone to the provider, so the turn shows as running, with Stop, at once. A write failure is reported to the conversation and shown until a later write succeeds; the next write reopens the file. Quitting and closing the window first wait up to two seconds for history still loading and apply it, so messages waiting on it are logged. A history that has not arrived by then is given up, which logs what was waiting on it; the read is left running and is not joined. Then they wait for every queued write, without a bound, so a disk that stalls writes still holds up quitting. `worker::Worker` is the general piece and does not know about Codex.

On startup, a session whose log has no turn end is marked interrupted. When the user resumes it, Keystone reconciles: it asks Codex for items after the last logged one (`thread/items/list`) or reads the tail of Claude's transcript. That reconciliation tolerates format changes because it is best effort and only fills gaps.

Import of existing sessions reuses the same readers. It is a versioned, best-effort, one-time conversion into a Keystone log, never a live dependency.

No SQLite and no CRDT. Both add build time and solve problems Keystone does not have, namely concurrent writers and sync.

### 4. Turns end only when the provider says so

The core runs one lifecycle per session: idle → running → (awaiting approval ↔ running) → ended with `completed`, `interrupted`, or `failed`.
- **Claude:** the turn ends at `result`. A `result` with `origin.kind = "task-notification"` is a turn the agent started itself, shown as such.
- **Codex:** the turn ends at `turn/completed`.
- **Process exit:** `on_exit` closes any open turn as failed with the stderr tail.

The driver also reports whether the user ended the turn: with Stop, or with a denial the provider ends the turn on. Codex's `decline` lets the turn go on and its `cancel` ends it; the driver sends `decline` when Codex offers it. Only the driver knows which turn an interrupt or a `cancel` reached, so it carries this on that turn's end (`stopped`), and the desktop counts an interrupted turn as a failure unless it was stopped. A crash or an explicit failure stays a failure.

There are no watchdogs, heartbeats, or quiet-period timers. Zeron added and later retired several of these after "stuck working" incidents. Capabilities are data detected at startup (Claude's `system/init` capabilities, Codex's `initialize` response), not trait booleans.

### 5. Process topology

**Decide.**
- **Codex:** one shared `codex app-server` for the whole app. Threads are multiplexed by ID, model listing uses the same process, and probe processes go away. If it crashes, a supervisor restarts it and resumes the threads that were loaded. The spike confirmed two threads running turns at the same time in one process. Memory decides it: an idle app-server with one thread used about 650 MB across 4 processes, and each extra thread added about 150 MB (three helper processes from the user's Codex configuration). Six sessions measured 1.4 GB shared, against about 3.9 GB estimated for six separate processes. Startup is not a reason; a cold app-server answered `initialize` in 0.09 s. Keystone unsubscribes threads that have been idle, so Codex unloads them and their helpers.
- **Claude:** one process per active session, as the protocol requires. An idle process measured 256 MB and answered `initialize` in 0.29 s. Idle processes with no background tasks are stopped after 10 minutes and resumed lazily. When the user starts typing in a new Claude session, Keystone may start the process early to hide startup; measure before keeping that.

Children start in their own process group. Teardown sends the protocol interrupt, then SIGINT, SIGTERM, and SIGKILL with short grace periods, then kills descendants found through `/proc`. Agents put shells in separate groups, so the group alone is not enough. Zeron learned this; Keystone avoids spawning `ps` to do it.

### 6. Permissions are real

- **Claude:** Keystone passes `--permission-prompts host --permission-prompt-tool stdio`. The spike showed that `--permission-prompts host` alone does not reach Keystone: Claude denied the tool itself and emitted `system/permission_denied`. The `stdio` value is undocumented, but the official Python SDK passes it whenever a permission callback is set (`types.py:2288-2304`), so Anthropic's own SDKs depend on it. Each `can_use_tool` control request becomes an `Approval` item.
- **Codex:** Keystone sends the user's own approval policy and sandbox settings. It does not force `never` and full access. Its approval requests also become `Approval` items.
- **Inbox:** an approval blocks its turn and appears in the attention inbox. Settlement never answers it, which the scope requires.
- **Approval modes:** each session has a mode the user can change at any time, mapped to the provider's own settings. Claude takes `--permission-mode` at launch (`manual`, `acceptEdits`, `auto`, `dontAsk`, `plan`, `bypassPermissions` on 2.1.300) and the `set_permission_mode` control request mid-session. Codex takes `approvalPolicy` (`untrusted`, `on-request`, `never`) and a sandbox on every `turn/start`, so a change applies from the next turn. Keystone shows an approval only when the chosen mode asks for one. The default is whatever the user's own Claude and Codex configuration says.
- **Unknown requests:** any control request or server request Keystone does not handle gets an explicit error response, so the agent never waits forever. Zeron leaves unknown Claude control requests unanswered.

### 7. Parsing

Each line gets a cheap first pass that reads only its method and ID, skipping the body, then one typed decode of the parts Keystone uses. Fields Keystone does not show are skipped without being built. Large tool output is read through a capped field, so it is never fully allocated. Unknown message types are counted and, in debug builds, logged raw; they are never fatal.

Codex's `optOutNotificationMethods` turns off notifications Keystone does not render, so they are never sent at all. The spike saw `remoteControl/status/changed`, `account/updated`, and per-thread `mcpServer/startupStatus/updated` alongside the turn traffic.

The message types are written by hand for the subset Keystone uses. A test validates them against the JSON schema generated by the pinned Codex version.

### 8. Steering and interrupt

Steering follows the scope: a message sent during a turn steers that turn, and there is no follow-up queue.
- **Claude:** a user message with priority `now`, or `next` while a tool is running, because `now` aborts a running MCP call. Zeron found this rule; credit it. In the spike, a `now` steer ended the current response with a `result` (`terminal_reason: aborted_streaming`), then a second `result` followed with the steered reply. Keystone shows both as one user-visible turn.
- **Codex:** `turn/steer` with the expected turn ID. In the spike, the same turn continued and completed with the steered reply. If the turn already ended, the message starts the next turn.

Interrupt uses Claude's `interrupt` with `cancel_queued` when the `interrupt_cancel_queued_v1` capability is present, or Codex's `turn/interrupt`. In the spike, Claude ended with `result` subtype `error_during_execution` and stayed running; Codex ended with `turn/completed` status `interrupted`. Interrupting a turn does not stop background tasks. Stopping the runtime does, and asks first when background work is live.

### 9. Version safety

A table lists the tested version range for each provider. Keystone reads `--version` once per binary and caches it on disk by path, size, and modification time. An untested version shows a warning in the UI and still runs. Recorded transcripts from each tested version live in `crates/keystone-agent/tests/fixtures/`. An opt-in `bash scripts/check.sh live` runs one tiny real turn per provider in a disposable repository and checks that the driver sees every message type it expects. It uses the user's subscription, so it never runs by default.

### 10. Crates and threads

A new `keystone-agent` crate holds items, drivers, the session lifecycle, the process host, and the log store. It does not depend on GPUI, so its tests compile without the graphics stack. The desktop crate only renders and sends commands.

The process host uses one blocking reader thread per child process for stdout and one for stderr, plus a writer thread with a bounded queue. That is a few threads per active session and no async runtime in Keystone's own code. `async-process` and `smol` are already compiled through GPUI if that changes.

The UI thread never blocks on a provider or the disk. Provider commands go through a `worker::Worker` per connection, in order, so finding the program and spawning it happen there. A start failure arrives as a `Failed` event for the session that needed it. The connection keeps its event inbox under its own lock, so draining events never waits for a process to start. A Claude connection can use the same worker and the same `log::Logs`.

## Spike results, 2026-10-15

Run in a disposable repository with Claude Code 2.1.300 (`--model haiku`) and codex-cli 0.162.1 (`effort: low`). Raw logs are in `.audit/spike-20261015/`. They stay out of git because the Claude `initialize` response lists the user's own skills.

| Question | Result |
| --- | --- |
| Concurrent turns on one Codex app-server | Yes. Both turns started at 0.79 s and completed at 3.34 s and 3.61 s. |
| Codex approval round trip | `item/commandExecution/requestApproval` arrived under `untrusted`; accepting created the file. |
| Codex steer and interrupt | Steer kept the turn and produced `STEERED`. Interrupt ended with status `interrupted`. |
| Claude permission with `--permission-prompts host` only | No. The tool was denied without a `can_use_tool` request. |
| Claude permission with `--permission-prompt-tool stdio` added | Yes. `can_use_tool` arrived for Bash; allowing it created the file. |
| Claude steer | Two `result` messages: the aborted response, then the steered reply. |
| Claude interrupt | `error_during_execution`, process still running. |
| Claude capabilities on 2.1.300 | `interrupt_receipt_v1`, `interrupt_cancel_queued_v1`, `interrupt_send_now_v1`, `msg_lifecycle_v1`, `request_marker_lists_v1`, `sdk_mcp_tools_list_changed`, `sdk_mcp_manifests`, `mcp_read_resource_v1`, `mcp_tool_ui_meta_v1`, `ui_surface_v1`. |
| Footprint | Codex: 650 MB idle with one thread, plus about 150 MB per extra thread. Claude: 256 MB per idle session process. |

## Limitations

These are the costs of the choices above, and what would make each one wrong.

1. **Claude's control protocol has no published spec.** Keystone uses the same flags and messages as the official SDKs, which lowers the risk but does not remove it. A Claude Code release can change behavior with no notice to third parties. The official SDKs absorb that because they ship in lockstep with the CLI; Keystone does not. Mitigation: capability detection, a tested version range, recorded fixtures, and the opt-in live check. If this proves too fragile, the fallback is a small Node helper around the official Agent SDK, as T3 Code does, at the cost of a Node runtime.
2. **Pure drivers still need time-based behavior somewhere.** Interrupt escalation and idle shutdown need timers. They live in the process host, not the drivers. A driver cannot say "no answer within 3 seconds", which Zeron uses after a stop request. Keystone accepts a stop as pending until the provider ends the turn, and offers a forced stop after a visible delay.
3. **One shared Codex process is one failure domain.** If the `codex app-server` process itself dies (a Codex bug, out of memory, or killed), every Codex turn running at that moment stops. Conversations are not lost: Codex has saved them, and Keystone restarts the process and resumes each thread, but the user has to resend the interrupted turns. Failures inside a session, such as a failed shell command, a crashed MCP helper, or a model error, stay in that session. A hang is worse than a crash, because Keystone must detect it before restarting. One process also means one global Codex configuration: anything that differs per session must go through `thread/start` and `turn/start` parameters. If crashes prove common, Keystone can cap threads per process or give each project its own app-server.
4. **Claude memory scales with sessions.** At 256 MB each, ten active Claude sessions use about 2.5 GB. Idle shutdown limits this, but resuming then costs a fresh process start.
5. **Keystone's log is a partial copy.** It stores completed items and capped tool output, not every provider event. Detail the provider keeps but Keystone does not render (for example raw thinking signatures or hook events) is only in the provider's own transcript. Claude deletes transcripts after 30 days by default, so for old sessions Keystone's log may be the only record left, and resume will fail.
6. **Two copies can disagree.** If the user also continues a session in the plain CLI, the provider's transcript moves ahead of Keystone's log. Reconciliation fills the gap on resume, but for Claude it reads an internal format that may have changed. Keystone must show when reconciliation was partial.
7. **Coalescing hides per-token timing.** Rendering once per frame is invisible at 60 Hz, but Keystone keeps no record of when each delta arrived. A future "replay this turn" feature would need provider timestamps.
8. **Capped tool output loses the middle.** A 10 MB build log keeps its first and last 32 KB. That covers most needs; the full output exists only in the provider's records, if anywhere.
9. **Not using the official SDK has a policy dimension.** Anthropic's terms are written around the Agent SDK. Driving the CLI directly with the user's own login matches the allowance for the unmodified binary, but it is a judgment call, and Codex says app-server authentication was never permitted for commercial services. Both are fine for a local, personal tool and need review before any commercial release.
10. **Approvals depend on the chosen mode.** In a manual mode, sessions pause until the user answers, and the inbox must make that obvious. Auto modes avoid the pauses. Keystone never auto-approves on the provider's behalf; it changes the provider's own mode, so the provider's safety checks still apply.
11. **Linux first.** Process-tree teardown through `/proc` and the measurements above are Linux-specific. See the platform section below.

## Platforms

Keystone targets Linux first, with macOS and Windows later. GPUI supports all three. The provider layer keeps platform code in one module of the process host so the drivers never change.

| Concern | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Kill a whole process tree | Own process group, then descendants from `/proc` | Own process group, then descendants from `libproc` | Job object with kill-on-close; this covers every descendant |
| Find `claude` and `codex` | `PATH`, plus common install directories | Apps launched from Finder do not get the shell's `PATH`; read the login shell's `PATH` once | `.cmd` and `.ps1` npm shims, `%APPDATA%\npm`, and native installers |
| Graceful interrupt | Protocol interrupt, then SIGINT | Same | Protocol interrupt, then terminate; there is no SIGINT for pipes |
| Provider data | `~/.claude`, `~/.codex` | Same | `%USERPROFILE%\.claude`, `%USERPROFILE%\.codex` |

Other platform work outside the provider layer: the workspace store syncs its parent directory after writes (KS-001), which Windows does not allow, so that step needs a Windows path. Native verification uses nested Sway and is Linux-only; macOS and Windows need their own checks. There is no CI yet. A build and test matrix on all three platforms should come before the first non-Linux release.

## Signing in

Keystone does not handle credentials by default. It uses whatever login the user's `claude` and `codex` already have.

OpenAI's **Sign in with ChatGPT** is an option for Codex. The user signs in through the browser and grants "ChatGPT plan usage". Keystone receives OAuth tokens and passes the access token to `codex app-server` in an environment variable, with a `model_provider` configuration pointing at the Responses API ([codex app-server guide](https://developers.openai.com/siwc/token-sharing-open-source/codex-app-server)). It is available now for open-source projects and personal projects that run locally; paid or hosted apps join a waitlist ([cookbook, 2026-09-28](https://developers.openai.com/cookbook/articles/sign-in-with-chatgpt)). It needs a Plus or Pro plan.

What it would change:
- Users would not need to run `codex login`, and their usage appears under ChatGPT settings for the app.
- Keystone would hold refresh tokens, so it must store them in the OS keychain and own the browser sign-in flow (PKCE, a loopback callback, and an installation host ID). OpenAI's devkit is TypeScript, so Keystone would implement this in Rust.
- A new access token requires restarting `codex app-server`. With one shared process, Keystone starts a new process with the new token and moves each thread over when its turn ends, so no running turn is cut off.
- `model/list` returns a bundled catalog in this mode, so a model is confirmed only by a successful turn.

Claude has no equivalent. Anthropic does not allow third-party apps to offer claude.ai login, so Claude always uses the user's own `claude` login.

Recommendation: ship with the existing CLI logins, and add Sign in with ChatGPT as an optional Codex account choice once the core works. It is also the sanctioned path if Keystone is ever distributed.

## Terms

Keystone runs the user's own unmodified `claude` binary and never handles their credentials. That matches Anthropic's allowance for "an end user … signing in to the unmodified Claude Code binary with their own Claude subscription". The product name must not include "Claude". OpenAI says local and open-source apps can keep using app-server authentication, but it has "never been permitted for commercial or hosted services". Revisit both before any commercial release.

## Measurements

These are added to `scripts/perf.py` before the drivers are tuned:
- Decode throughput on recorded transcripts, in MB/s per provider.
- Delta-to-frame cost: draw time with a live item receiving 1,000 deltas per second, in the GPUI test platform.
- Bytes written to disk per turn.
- Cold start to first `system/init` for Claude, and `thread/start` latency on a warm Codex app-server.

## Plan

1. **Spike, done 2026-10-15.** See the results above. Redacted fixtures still need to be cut from the raw logs.
2. **Codex first.** The `keystone-agent` crate, the Codex driver with fixture tests, the process host, the log store, and a desktop transcript with submit, stream, and interrupt.
3. **Claude driver,** against the same item model and fixtures.
4. **Approvals and steering** in the UI and inbox, then resume, crash reconciliation, and import.
5. **Handoff and second opinion** from the neutral log.

## Sources

- Claude Code headless, CLI reference, sessions, Agent SDK TypeScript reference, legal and compliance: code.claude.com/docs, fetched 2026-10-15.
- Codex App Server: learn.chatgpt.com/docs/app-server, fetched 2026-10-15. Schemas generated from codex-cli 0.162.1.
- Zeron: github.com/zeronsh/zeron at `3b2f780`, `crates/harness`, `crates/engine/src/sessions.rs`, `run_journal.rs`, `crates/doc`. Only design ideas are used, with credit; no code is copied.
