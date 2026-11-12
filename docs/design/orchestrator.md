# Local cross-provider orchestration

Keystone adds a `keystone` MCP server alongside each chat provider's native tools,
skills and configuration. It does not replace those tools or append instructions to
the provider's system prompt. The cross-session API takes design ideas from T3
Code's orchestrator, implemented independently in Rust.

The desktop owns one lazy server at `http://127.0.0.1:<ephemeral>/mcp`. The transport
uses `std::net` and a bounded thread per HTTP connection, with JSON request responses
rather than SSE. Id-less MCP notifications receive an empty HTTP 202 response,
as Codex's transport requires. It implements MCP initialization, tool listing and tool calls. It is not
a remote-control endpoint: no host setting, discovery, peer linking or browser
access is provided.

## Tools

| Tool | Behavior |
| --- | --- |
| `capabilities {}` | Lists both providers, their currently loaded model catalogs (for the caller's account on its own provider, otherwise the default account) and their accounts: `default` plus each added account's ID and name. A loading catalog can be queried again. |
| `session_launch {provider, account?, model?, mode?, cwd?, message, client_request_id}` | Creates and starts a session in the caller's project. `provider` is `claude` or `codex`; `account` is `default` (the default, the inherited environment) or an added account's ID such as `claude-work`; an unknown ID or another provider's account is refused. An omitted mode inherits the caller's mode. When the launched session's first turn ends, its result starts the caller's next turn (see Results of launched sessions). |
| `session_send {session, message, client_request_id}` | Steers an already running target immediately, or returns busy. It never starts an idle target or queues a follow-up. |
| `session_read {session, after?, limit?}` | Reads transcript rows, with zero-based positions. `after` is exclusive; `limit` defaults to 50 and is bounded to 1–200. `next` does not advance past a streaming row, so polling does not miss its completion. `handed_off_from` is `{session, second_opinion}`: the session a handoff or second opinion continued, so a session started from either can read the rows its handoff text left out. A second opinion, and a handoff or fork of one, only reads: its `session_launch`, `session_send` and `session_interrupt` calls are refused in every approval mode. |
| `session_wait {session, timeout?}` | Waits for the target's current work to end. The timeout defaults to 50 seconds and is capped at 50 seconds; `timed_out` distinguishes the cutoff from completion. Call again to keep waiting. A wait that sees a launched session's turn end marks its result observed, so it starts no turn. |
| `session_interrupt {session}` | Stops the current target turn through the same runtime path as the Stop button. Codex also cancels an initial message waiting for its thread to open. Work still queued on a connection worker returns busy rather than acknowledging a stop it cannot apply immediately. |
| `browser_*` | Open, read and drive the caller's project page in the Browser surface. See [browser preview](browser-preview.md). |
| `device_list`, `device_open`, `device_screenshot`, `device_launch`, `device_tap`, `device_swipe`, `device_type`, `device_close` | A simulator or emulator beside the caller's session, with screenshots as image content. `device_open` may take 50 seconds and the others 20. See [devices](devices.md). |

Every call requires a live Keystone caller process with a running turn. Every
target must have the caller's recorded project directory. `cwd`, when supplied,
must equal the canonical `project` returned by `capabilities`; a path is not a
cross-machine target.

Messages contain 1–262144 bytes and request IDs contain 1–128 bytes. Launch and
send request IDs are scoped to the caller session. Repeating an identical request
returns its original result; reusing the ID for different arguments is refused.
Use a new ID to retry a refused send. A dispatch timeout is not a refusal: the
write may already have happened, so retry it with the same ID. This app run retains at most 4096 request
receipts rather than evicting old IDs and accidentally replaying work.

A launch response includes the new session ID even if a later setup or initial
send fails (`sent: false`). Retrying its request ID does not create another child.
Creating an agent session does not intentionally change the user's selection.
Successful launch responses acknowledge admission, and steer responses acknowledge
local dispatch, not provider completion; later provider failures are exposed by `session_read` and
`session_wait`. Read responses also expose history-loading and log-saving errors.
Read pages have a 256 KiB encoded-byte budget as well as a row limit, and every
row, the first included, fits within it. Item text, command lines, approval actions
and turn errors keep their first and last 32 KiB with an omitted-byte marker, and the
row says `truncated: true`. A file change item carries its edit's diff, already kept to
1,000 lines when it was logged (see [Edit diffs](providers.md#edit-diffs)). A row still over the budget, such as a file change that
lists thousands of paths, is replaced by `{position, omitted_bytes, truncated: true}`,
so `next` still moves past it. The session-level `error`, `load_error` and
`save_error` in read and wait responses are capped the same way. This bounds the
HTTP response without modifying the saved transcript.

Wait obtains an authorized snapshot before returning target state. That first
dispatch has a five-second budget even for a shorter requested timeout, so a very
short wait can take longer than requested. The whole wait still has a 50-second
dispatch bound. Once a snapshot exists, a dispatch deadline at the cutoff returns
that last snapshot with `timed_out: true`; authorization and other runtime errors
remain errors. If the first snapshot cannot be obtained, no state is fabricated.

## Results of launched sessions

A launch saves a durable delivery record with the new session, in the same SQLite
transaction: the launched session (`child`), the session that launched it
(`parent`) and its state. `session_read` reports `created_by` from this record, so it
survives restarts. When the child's turn ends, its result is saved as `ready`: how the
turn ended (finished, stopped or failed with its error) and its last reply, with both
ends kept when long. The result then starts the parent's next turn as a message:

```
Result from session 12 (“Fix the flaky test”), which you launched.
It finished.
Read its whole transcript with session_read.

<the child's last reply>
```

The parent's transcript shows that message as a "Result from" card naming the child,
not as a user message. One record is delivered once, for the launch's first turn end;
later turns of the child, such as a user continuing it, deliver nothing more. So a
child that itself launches a session and ends its turn sends its parent that turn's
reply; the parent reads the child's later work with `session_read` or `session_wait`.

A result goes only to a parent that is idle: no running turn, no recovery holding its
next message, no approval request or widget waiting for the user, and not settled.
A parent busy with a turn gets the result once that turn ends, in a new turn, never as
a steer. A settled parent is never woken; the result waits until the user un-settles
it. Each held result is listed under the parent's last row with what it waits for.
Several results for one parent go one turn at a time. There is no queue UI.

A parent the user handed off is not woken: its results go to the newest session that
continues it, once that session has sent its first message (the handoff's context).
A second opinion continues nothing: it never receives its source's results, and since it
cannot launch a session, no result ever goes to it.

Delivery uses the same path as Send: a parent whose Claude process was evicted is
resumed with its own account, model and conversation; a parent whose history is not
loaded is loaded first, and recovery's gate (the check, then any acknowledgement) runs
before anything is sent. A send refused before it reaches the provider, such as a
missing folder or account, shows its error in the parent and keeps the result ready
for the next attempt. A provider that accepts the message and then fails to start the
turn (a process that cannot start, a resume that fails) shows the error after the
result card in the parent, as it does for a typed message; the result counts as
delivered and is not sent again, so the user continues from there.

If the parent's own `session_wait` sees the child's turn end while its result is ready,
the record becomes `observed` and no turn is started for it. A launch whose task was
never sent (`sent: false`) is `unsent` and delivers nothing.

`delivered` is saved with the message before the message is written to the provider,
so a crash or restart never sends a result twice. A crash in the moment between that
save and the provider write can leave a result marked delivered that the provider never
received and the parent's transcript never shows; the delivery record and the child's
own transcript still hold it. A ready result survives a restart and is delivered by the
next start once its parent is idle. A child whose turn end was saved but whose result
was not, because Keystone closed in between, has its result saved at the next start. A
child whose turn was cut off by a restart has no result until a later turn of it ends;
that turn, even one the user starts, then delivers as the first.

The transcript shows a message as a result card only when it is exactly the message a
delivery record holds as delivered to that parent, so text written to look like a
result stays an ordinary message. A result whose `delivered` save fails stays ready and
is tried again at the next update.

## Approval ceiling

For explicit modes, writes and launches obey
`read-only < ask < auto < full-access`. Checks run on runtime dispatch and again
under the provider connection lock, using the caller's current live-turn and mode
state. Launch admission captures permission to start the child even if the parent's
reply ends during initialization. Before the initial provider write, the caller's
process generation must still be live and its current mode must still permit the
child's current mode. Retiring the parent's process generation (its exit, a failed
start, a replacement, or connection or app shutdown) cancels that grant; merely
ending its turn does not. A narrowed mode can refuse the child before dispatch,
with the failure visible through read/wait. Grants belong to individual messages,
so cancelling or failing a launch cannot reject a later human Send. A strict steer
rejected after its target ends is not retried as a new turn.

**Provider settings are deliberately uncapped.** A Provider-settings caller may
launch or write to a target in any mode, including a more permissive explicit
mode. A Provider-settings target inherits its provider's own configuration,
regardless of the caller's explicit mode. Neither case is treated as Ask. This is
the product policy, not a claim that the app knows the native configuration's
effective permission level. A stricter policy could refuse explicit children
broader than Ask from Provider-settings callers; this version does not do that.

## Credentials and lifetime

Each session process generation gets a cryptographically random bearer token.
The shared registry keeps only its SHA-256 digest. A lease revokes it when the
process is retired, fails to start or exits; a restarted process gets a new token.
The token is sent only through the provider's stdin protocol, never argv, Keystone's
logs, transcript or saved session state. MCP-attached process exit diagnostics
retain exit codes but omit stderr tails, which might echo the configuration.
Tokens identify their own caller session,
not a target selected by request arguments.

Claude receives `mcp_set_servers` after initialization and before its first user
message. Only the six published `mcp__keystone__<tool>` session tools, the eight
`device_*` tools and the `browser_*` tools other than `browser_evaluate` are
auto-allowed (`mcp::auto_allowed`), and a browser tool only while the address it
loads is this machine's; native tools and other names retain normal approval
handling. Codex receives the server in its per-thread configuration. Catalog and
permission-mode probes, and the restricted layout helper, receive no orchestrator
credentials.

No-inference checks against Claude Code 2.1.301 and codex-cli 0.163.0 confirmed
dynamic injection and both Codex start/resume configuration. A static native MCP
server remained connected alongside Keystone in each case. Resume used an
explicitly synthetic metadata-only rollout; no real conversation or model turn
was used. Id-less notifications must receive HTTP 202 for Codex's handshake to
complete; a successful thread/start response alone does not prove its MCP server
connected.

## In-memory seam and deliberate limits

The runtime's small `Records` trait isolates idempotency receipts from the
transport. It is in memory; a restart loses them, and request IDs must not be
reused across app restarts as a durability guarantee. Parent relationships live in
the durable delivery records above.

`session_send` stays steer-only: it never starts an idle target or queues a
follow-up. Only a launched session's own result may start its parent's turn. There
is no queue UI, account switching or remote machine support here.
