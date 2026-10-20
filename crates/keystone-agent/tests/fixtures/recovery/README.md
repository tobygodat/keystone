# Recovery fixtures (synthetic)

These files are **synthetic**: written by hand for Keystone's tests, not recorded from
a real provider store or a real user session. No inference produced them.

- `claude-2.1.300-store.jsonl` follows the session store layout Claude Code 2.1.300
  writes under `<config dir>/projects/<folder>/<session>.jsonl`: one JSON record per
  line, chained by `uuid`/`parentUuid`, with `sessionId`, `cwd`, `isSidechain`,
  `isMeta`, user prompts as strings, tool results as `tool_result` parts, and one
  assistant content block per record sharing the message `id`. It includes a
  sidechain record, a meta record, a thinking block, a summary line and a
  file-history snapshot, which recovery must skip, and ends mid-turn: the last tool
  call has no result.
- `codex-0.163-turns-list.json` is a `thread/turns/list` result with
  `itemsView: "full"`, newest turn first, in the shape the Codex app-server
  documentation describes.

Both pin the layout the readers expect. A store in another layout is reported as not
recovered rather than guessed at.
