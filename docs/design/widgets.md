# Agent-built widgets

Claude and Codex can put an interactive widget in a reply: a checklist, a short
form, a set of choices, a slider. Keystone hosts one tool, `show_ui`, whose input is
[A2UI](https://a2ui.org/) v0.9.1 messages against Keystone's own catalog. The widget
is drawn inline in the transcript with Keystone's components, keeps its state in its
own data model, and sends a Button press back to the agent. This is step 2 of
[the generative UI research](../research/diagrams-and-generative-ui.md)
(TOB-35). Computed values, diagrams in
widgets and diagram hit-testing are step 3 and are not built.

The provider's own tools, skills and configuration are untouched. Keystone adds one
tool beside them and no system prompt: the tool's description teaches the catalog.
The parser and data model live in `crates/keystone-agent/src/surface.rs`; drawing
lives in `crates/keystone-desktop/src/widget.rs`.

## How each provider gets the tool

**Claude Code** (checked on 2.1.301 with `scripts/probe-show-ui.py`, which sends no
message and starts no turn). After `initialize`, the Claude driver sends one
`mcp_set_servers` with Keystone's servers: the orchestrator's `keystone` HTTP server
when there is one, and an SDK-hosted server:

```json
{"keystone_ui": {"type": "sdk", "name": "keystone_ui", "timeout": 86400000, "disableAutoBackground": true}}
```

Claude then talks MCP to it through `mcp_message` control requests, which the driver
answers itself: `initialize`, `notifications/initialized`, `ping`, `tools/list`, and
`tools/call` for `show_ui`. Claude names the tool `mcp__keystone_ui__show_ui`; its
`can_use_tool` check is allowed without asking, because showing a widget changes
nothing but Keystone's own transcript. The 24 hour `timeout` keeps Claude waiting for
a press, and `disableAutoBackground` keeps the wait in the foreground. Messages wait
for the answer to `mcp_set_servers`, so the tool is there for the first turn. If Claude
refuses the widget server, it is left out and the turn goes on without it; each
server in the answer is judged on its own, so a refusal of the orchestrator server
still fails the message, as before. If Claude never answers, the orchestrator's server
is asked for again alone. Claude lists only the servers a request changed, so that
retry is ready when the orchestrator's server is not refused, even if it is not listed
as added.

**Codex** (checked on 0.163.0). `thread/start` takes
`dynamicTools: [{type: "function", name, description, inputSchema}]`, but only from a
client whose `initialize` set `capabilities.experimentalApi`. Codex then asks with the
`item/tool/call` server request (`{threadId, turnId, callId, tool, arguments}`), and
Keystone answers `{contentItems: [{type: "inputText", text}], success}`. The tool is
offered to new threads only: a resumed thread keeps the tools it started with. Only
the running turn's calls are shown; a call naming another turn, one that arrives with
no turn running or before `turn/started`, or a call ID the turn already used (answered
or not) is answered with `success: false`. If
Codex refuses the field (an error naming `dynamicTools`), the thread starts again
without it, and later threads are not offered it; any other `thread/start` error is
the thread's own and leaves the tool on. If Codex refuses the capabilities in `initialize`, the experimental opt-in
goes with them and no thread is offered the tool.

**Version gates.** Claude 2.1.300 and later and Codex 0.163.0 and later get the
tool (`widgets_from` in `version.rs`). An older or unreadable version gets none, and
the agent answers in plain text. Claude 2.1.300 is Keystone's minimum supported
version; the SDK server was probed on 2.1.301 only, so 2.1.300 is assumed to match.

**What the probe did not see.** The probe registers the servers and lists the tool;
it starts no turn. So the shapes of a real tool call were not observed on either CLI:
Claude's `tools/call` with `_meta.claudecode/toolUseId`, its `can_use_tool` name for
the tool, and its `control_cancel_request` on Stop; Codex's `item/tool/call`, the
`contentItems` answer, and `serverRequest/resolved` for a waiting call. These follow the
providers' published SDK and protocol schemas and are exercised against the scripted
providers only.

## The catalog

The catalog ID is `https://github.com/tobygodat/keystone/a2ui/catalogs/widgets/v1`, a
subset of A2UI's basic catalog. Every component has an `id` and `component`; children
are referenced by ID; `weight` sets flex grow inside a Row or Column.

| Component | Properties |
| --- | --- |
| Column, Row | `children`, `justify`, `align` |
| Card | `child` |
| Text | `text`, `variant` (h1–h5, caption, body) |
| Divider | `axis` |
| Button | `child` (a Text), `variant` (default, primary, borderless), `action.event` with `name` and `context` |
| CheckBox | `label`, `value` bound to a boolean |
| TextField | `label`, `value` bound to a string, `variant` (shortText, longText, number) |
| ChoicePicker | `label`, `options`, `value` bound to a string array, `variant` (mutuallyExclusive, multipleSelection), `displayStyle` (checkbox, chips) |
| Slider | `label`, `min`, `max`, `value` bound to a number |

Text properties are a literal or `{"path": "/pointer"}`. Inputs must bind `value` with
a path; a missing value starts empty (false, "", [], or `min`). Functions, `checks`,
templates and other components are refused.

One call builds one surface: `createSurface` first, then `updateComponents` and
`updateDataModel`. Anything Keystone cannot draw or bind is refused with A2UI's
`VALIDATION_FAILED` error naming the JSON Pointer of the problem in the tool's
arguments, so the agent can fix the call:

```json
{"version": "v0.9.1", "error": {"code": "VALIDATION_FAILED", "surfaceId": "todo", "path": "/messages/1/updateComponents/components/11/child", "message": "..."}}
```

Limits bound what one call can cost: 256 KiB of arguments, 64 messages, 200
components, a depth of 16, 50 options per picker, 8 KiB per text, 16 context entries
per Button, Slider bounds within ±1,000,000, and a data model of at most 64 KiB nested at most 32 deep (JSON Pointers
too). The data model limits hold for what the user types as well: an edit that would
pass them is not kept, and the conversation says why.

## Interaction

Inputs write to the surface's data model through their JSON Pointer bindings. A press
of a Button sends A2UI's `action` message with the button's `name`, the surface and
component IDs, a timestamp, and its `context` with every binding resolved; with
`sendDataModel` the whole data model rides along in `metadata`.

Every control works from the keyboard. With the widget's row selected in the transcript,
Tab and Shift+Tab walk its controls in document order after the row's own actions,
and past the last one return to the row; Enter or Space presses a button or ticks a
box, typing fills a focused field, and Left or Right moves a slider. A focused control
shows the focus ring and scrolls into view. Escape leaves for the composer from any
control. Up, Down, Home and End move between rows, except in a text field, where they
move the caret. A focused control that scrolls out of view gives focus back to the
transcript.

A surface with a Button **waits** for the user. While the provider's tool call waits
for it, the first press is the tool's result, and the agent goes on in the same turn.
A surface with no Button is answered at once with a note that no action will come.

When the provider no longer waits, the press is sent as a message instead: a steer
while a turn runs, a new turn otherwise. That happens after a later press of the same
surface, after the turn that asked has ended (the user stopped it, the provider
exited, or Claude withdrew the call), and after a restart. The message names the tool
call and carries the same action. Sending it leaves the composer's draft alone.

## Waiting on the user

A waiting surface puts its session in Needs action as "Waiting for your answer",
after an approval and before a failed turn. Like an approval, it blocks settling:
settling never answers a widget, and the user is asked to answer it first. The surface
says it is waiting, and once pressed it says which action was sent. When the provider
stops waiting without an answer, the surface is stopped and says a press now sends a
new message. That happens as soon as Claude withdraws the call
(`control_cancel_request`) or Codex resolves it (`serverRequest/resolved`), even while
the turn goes on, and otherwise when the turn that asked ends.

## Persistence

Each surface is saved whole in the session log as `Entry::Surface`, keyed by its tool
call ID: when it is shown, when it is pressed, when its turn ends, and a moment after
an edit (750 ms, so typing saves it once; quitting saves pending edits first). On
restore the latest entry for a call replaces earlier ones and keeps the row's place. A
surface restored as waiting has no live call to answer, so it is shown as stopped and a
press sends a message.

The tool call's own row is not shown: the surface stands for it.

## Testing

`keystone-fake-claude` and `keystone-fake-codex` script the tool (see
`crates/keystone-agent/src/bin/support/widgets.rs`). A prompt containing "show a widget"
and "checklist" or "form" shows that example and replies with what came back; "invalid"
first sends a call that fails validation; "withdraw" withdraws a call no one answers
within two seconds and keeps the turn running until it is stopped. Fake Claude takes
the SDK server, runs the MCP handshake, asks permission and calls `tools/call`; fake
Codex refuses
`dynamicTools` without `experimentalApi`, as Codex 0.163 does. Run fake Codex with
`KEYSTONE_FAKE_CODEX_VERSION=0.163.0`, since its default version predates the tool.
