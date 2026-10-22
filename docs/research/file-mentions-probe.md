# File mentions: do the tested CLIs expand `@path`? (TOB-62 probe)

Run on desk on 2026-10-21 with Claude Code 2.1.301 and Codex 0.163.0, by
`scripts/probe-file-mentions.py`. Raw output: [file-mentions-probe.json](file-mentions-probe.json).

No inference: the whole probe ran under `unshare -rn`, whose only network is a private
loopback. Each CLI's model API pointed at a fake server on that loopback, which recorded
each request and answered it with a 400 error. Every CLI ran with a fresh `HOME`, a fresh
`CLAUDE_CONFIG_DIR` or `CODEX_HOME`, and a dummy key that only the fake server accepts.

Each CLI got one message, sent the way Keystone sends it:
`Summarise @notes.txt, @src/deep/file.rs and @"my notes/a b.txt"`. Each file held a
sentinel string. A sentinel in a model request means the CLI read the file for the model.

## Claude Code 2.1.301 (stream-json)

- **It expands mentions itself.** The user's text reaches the model unchanged, and Claude
  Code adds a context message before it: `Called the Read tool with the following input:
  {"file_path":"<cwd>/notes.txt"}`, followed by the file's numbered lines. All three
  sentinels were in all three requests (two retries after the 400).
- Paths are relative to the session's folder. A path with spaces works when quoted,
  `@"my notes/a b.txt"`.
- No extra request was made for the mention; the read happened locally before the first
  model call.

## Codex 0.163.0 (app-server)

- **It does not expand mentions.** The `text` input reached the model as typed in one
  `input_text` block, with no sentinel anywhere in the request. The model sees `@path`
  and reads the file with its own tools if it chooses to.
- `turn/start` also accepts a `mention` input item (`{type, name, path}`). Sending one per
  file beside the text changed nothing: no sentinel reached the model. That item is not a
  file-attachment mechanism in this version.

## What Keystone does

Keystone sends the composer's text exactly as typed: `@relative/path`, or
`@"relative/path"` when the path has whitespace. Claude reads the file through its own
mention handling. Codex gets the path and uses its own tools. Keystone never reads or
inlines file contents, and never adds input items for mentions.
