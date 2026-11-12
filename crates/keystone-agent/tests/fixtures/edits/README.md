Real provider output for file edits, recorded on 2026-11-12 by `scripts/probe-edits.py`
with Claude Code 2.1.322 (`--model haiku`) and codex-cli 0.167.1 (`gpt-6-luna`, low
effort), each in a disposable git repository, and written here with
`python3 scripts/probe-edits.py --fixtures crates/keystone-agent/tests/fixtures/edits`.

- `claude-*-edits.jsonl`: three `Read`s, then `Edit` on `a.txt`, two `Edit`s on `b.txt`,
  `Write` creating `c.txt` and `Write` replacing `d.txt`, each allowed through
  `can_use_tool`. Claude Code 2.1.322 has no `MultiEdit` tool: it is missing from
  `system/init`'s tool list, and the model said so and used `Edit` twice. Each result's
  `tool_use_result` carries `structuredPatch` hunks; a created file's is empty, with the
  file in `content` and `type: "create"`.
- `codex-*-edits.jsonl`: one `fileChange` item from `apply_patch` that updates `a.txt`,
  adds `c.txt` and deletes `d.txt`. An update's `diff` is unified hunks without file
  headers; an add's or delete's is the file's whole text.

Only what each provider printed is kept. Left out: Claude's `initialize` answer, the
inventory in `system/init` and its usage report, and Codex's handshake answers and account
reports. The repository path is `/tmp/probe`, and the Codex thread ID is the one the tests
open.
