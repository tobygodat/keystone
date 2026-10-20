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
| `session_launch {provider, account?, model?, mode?, cwd?, message, client_request_id}` | Creates and starts a session in the caller's project. `provider` is `claude` or `codex`; `account` is `default` (the default, the inherited environment) or an added account's ID such as `claude-work`; an unknown ID or another provider's account is refused. An omitted mode inherits the caller's mode. |
| `session_send {session, message, client_request_id}` | Steers an already running target immediately, or returns busy. It never starts an idle target or queues a follow-up. |
| `session_read {session, after?, limit?}` | Reads transcript rows, with zero-based positions. `after` is exclusive; `limit` defaults to 50 and is bounded to 1–200. `next` does not advance past a streaming row, so polling does not miss its completion. |
| `session_wait {session, timeout?}` | Waits for the target's current work to end. The timeout defaults to 50 seconds and is capped at 50 seconds; `timed_out` distinguishes the cutoff from completion. Call again to keep waiting. |
| `session_interrupt {session}` | Stops the current target turn through the same runtime path as the Stop button. Codex also cancels an initial message waiting for its thread to open. Work still queued on a connection worker returns busy rather than acknowledging a stop it cannot apply immediately. |

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
row says `truncated: true`. A row still over the budget, such as a file change that
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
message. Only the six published `mcp__keystone__<tool>` names are auto-allowed;
native tools and other names retain normal approval handling. Codex receives the server in its per-thread configuration. Catalog and
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

The runtime's small `Records` trait isolates idempotency receipts and
`created_by: Session(parent)` relationships from the transport. V1 uses an
in-memory implementation; a restart loses these records, and request IDs must not
be reused across app restarts as a durability guarantee. Session creation, text
and turn state still use the existing runtime's storage methods. This unit does
not change the Store or neutral log implementation; the SQLite work can replace
the records implementation later.

There are no durable deliveries, completion wakes, idle-parent turns, queue UI,
account switching or remote machines here. A child finishing cannot start its
parent's next turn. Those require later units rather than implicit behavior in
`session_send`.
