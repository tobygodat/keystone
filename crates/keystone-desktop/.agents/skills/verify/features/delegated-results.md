# Delegated results

When an agent launches a session through Keystone's orchestrator and that session's turn ends, its result starts the launching session's next turn. See `docs/design/orchestrator.md`.

## Sub-features

- `result-idle` delivers a launched session's result to an idle parent as a new turn, shown as a **Result from "…"** card (`result-<row>`) followed by the parent's reply.
- `result-busy` holds a result while the parent's turn runs. The line under the last row (`result-waiting-<child>`) says it arrives when the turn ends; it is then delivered once, as the next turn, never as a steer.
- `result-settled` never wakes a settled parent. `result-waiting-<child>` says the result waits until the session is un-settled; un-settling delivers it.
- `result-restart` keeps a ready result across Quit and relaunch and delivers it once; a delivered result is never sent again.

## How to get to it (user POV)

An agent in a session calls `session_launch`. With the scripted providers, send one of these exact prompts in a Claude Code or Codex session:

- `orchestrator test: delegate claude` or `orchestrator test: delegate codex` launches a session on that provider and ends the turn at once; the launched session's reply streams for a few seconds, so its result reaches an idle parent.
- `orchestrator test: delegate claude while busy` (or `codex while busy`) gives the launched session a short task and keeps the parent's reply streaming, so the result arrives while the parent is busy.

The parent's scripted provider echoes the result message ("You wrote: Result from session N …"), which proves the message reached it.

## Driving it with keystone-verify

Start with both scripted providers and a disposable project, as in [conversations](conversations.md). Default delays (60 ms) are fine.

- Idle: `click` `welcome-codex`, type `orchestrator test: delegate claude`, `click` `send`. Wait for the parent's "Scripted delegation started" reply and `turn-status` gone, then `wait_for` the `result-<row>` card. Screenshot it. `workspace` shows `deliveries: [{child: 2, parent: 1, state: "delivered"}]`.
- Busy: send `orchestrator test: delegate claude while busy`. While `turn-status` is present, `wait_for` `result-waiting-<child>` and screenshot the "arrives when this turn ends" line. Then wait for the result card after the parent's streamed reply.
- Settled: send `orchestrator test: delegate claude`, then at once press Ctrl+Shift+S to settle the selected parent. After the launched session finishes, `result-waiting-<child>` says the session is settled and `workspace` shows the delivery `ready`. Quit with Ctrl+Q, `launch`, select the parent from **Settled**, and require the same line and state. Un-settle with Ctrl+Shift+S and require the result card, then `delivered`.
- After a delivery, Quit and relaunch: the parent's transcript has exactly one result card and no new turn starts.

## Gotchas

- The launched session is selected in no window; it appears in the sidebar.
- A parent whose own `session_wait` saw the launched session end gets no result turn (`state: "observed"`); the `orchestrator test: launch …` scenarios end that way.
