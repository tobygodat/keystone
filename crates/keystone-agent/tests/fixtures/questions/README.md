Real provider output for questions, recorded on 2026-11-02 by `scripts/probe-questions.py`
with Claude Code 2.1.316 (`--model haiku`) and codex-cli 0.166.0-alpha.5.1 (`gpt-6-luna`,
low effort), each in a disposable git repository, and written here with
`python3 scripts/probe-questions.py --fixtures crates/keystone-agent/tests/fixtures/questions`.
`tests/question_drivers.rs` replays them. See `docs/research/questions-probe.md`.

- `claude-*-ask-*`: `AskUserQuestion` with a single and a multiple choice, answered
  (Blue; Apple, Cherry) and declined.
- `claude-*-elicit-*`: an MCP server's form (`elicitation` control request), after the
  tool call's own approval, accepted and declined.
- `codex-*-ask-*`: `item/tool/requestUserInput` with `features.default_mode_request_user_input`
  set, answered with typed text and declined with empty answers.
- `codex-*-elicit-*`: Codex's MCP tool-call approval (`mcpServer/elicitation/request` with
  an empty form) and then the server's form, accepted and declined.

Only what each provider printed is kept. Left out: Claude's `initialize` answer and the
inventory in `system/init` (they list the user's own skills and tools), Codex's handshake
answers, account reports and the user's own MCP servers. Each Codex recording's thread ID
is replaced with the one the tests open.
