# Questions: how Claude Code and Codex ask the user (TOB-296 probe)

Run on 2026-11-02 on an Apple Silicon Mac with Claude Code 2.1.316 (`--model haiku`) and
codex-cli 0.166.0-alpha.5.1 (`gpt-6-luna`, low effort), by `scripts/probe-questions.py`.
Each case ran one real turn in its own disposable git repository with the user's own
login, so the probe spends subscription usage and is never part of the test suite. The
probe gives each CLI a stdio MCP server whose one tool, `choose_plan`, asks its client a
form through `elicitation/create`. Replayable fixtures, made from the recordings:
[`crates/keystone-agent/tests/fixtures/questions/`](../../crates/keystone-agent/tests/fixtures/questions/README.md).

## Claude Code 2.1.316

- **AskUserQuestion** arrives as `can_use_tool` with `tool_name: "AskUserQuestion"` and
  `requires_user_interaction: true`. The input is
  `{questions: [{question, header, options: [{label, description}], multiSelect}]}`; the
  tool's schema says Claude adds an "Other" choice itself, so typed text is always allowed.
- Answering is `allow` with `updatedInput` set to the input plus
  `answers: {"<question text>": "<answer>"}`. Several choices are one string joined with
  ", ". The tool result reads
  `Your questions have been answered: "Which color do you prefer?"="Blue", ...`, and the
  turn went on.
- Declining is `deny` with a message. The tool result is an error with that message and
  `permission_decision: reject`, and Claude replied and ended the turn normally.
- **MCP elicitation** arrives as `control_request` subtype `elicitation`, with
  `mcp_server_name`, `message`, `mode: "form"`, `requested_schema` and `property_order`
  (the server's own property order). It comes after the MCP tool call's own
  `can_use_tool`. The answer is `{action: "accept", content: {...}}` or
  `{action: "decline"}`, and the server received exactly that.
- Claude echoes every `control_response` it receives back on stdout. The driver ignores
  the echo.

## codex-cli 0.166.0

- **request_user_input** is the server request `item/tool/requestUserInput` with
  `threadId`, `turnId`, `itemId`, `questions: [{id, header, question, isOther, isSecret,
  options: [{label, description}]}]`, `isBlocking` and `autoResolutionMs`. No item
  announces it.
- In Default mode the model sees no such tool and says "request_user_input is unavailable
  in Default mode", unless the config sets `features.default_mode_request_user_input`. Plan
  mode (`collaborationMode: {mode: "plan"}`) has it. Keystone never uses Plan mode (ADR 0001).
- Codex rejects a question without options ("request_user_input requires non-empty options
  for every question") or an option without a description, before anything reaches the
  client. There is no multiple choice. `isOther` adds typed text.
- Answering is `{answers: {"<id>": {answers: ["..."]}}}`. Declining is `{answers: {}}`;
  the model saw the empty answers and finished the turn. Both are followed by
  `serverRequest/resolved`.
- **MCP elicitation** is `mcpServer/elicitation/request` with `threadId`, `turnId`,
  `serverName`, `mode`, `message`, `requestedSchema` and `_meta`. Codex sends the schema's
  properties in alphabetical order.
- Codex asks before running an MCP tool through the same request: `message: "Allow the
  probe MCP server to run tool \"choose_plan\"?"`, an empty form, and `_meta:
  {codex_approval_kind: "mcp_tool_call", persist: ["session", "always"], tool_description,
  tool_params}`. Before TOB-296 Keystone answered this with an error, so an MCP tool that
  needed approval could not run. Accepting is `{action: "accept", content: {}}`.

## What Keystone does with it

See "Questions" in [provider integration](../design/providers.md#6-permissions-are-real).
T3 Code's `ClaudeAdapterV2.ts`, `CodexAdapterV2.ts` and `CodexMcpElicitation.ts` were read
for comparison. T3 Code treats an MCP form as consent only and fills it from schema
defaults. Keystone shows the form's fields instead, and treats only a form with no fields
as an approval.
