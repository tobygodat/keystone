# Slash commands and skills: what the tested CLIs list (TOB-63 probe)

Run on desk on 2026-10-21 with Claude Code 2.1.301 and Codex 0.163.0, by
`scripts/probe-slash-commands.py`. Raw output: [slash-commands-probe.json](slash-commands-probe.json).

No inference: each CLI ran under `unshare -rn` (no network) with a fresh `HOME`, a fixture
`CLAUDE_CONFIG_DIR` or `CODEX_HOME`, and no credentials. Claude received only the
`initialize` control request. Codex received only `initialize`, `initialized` and list
requests. No user message was sent and no thread or turn started.

## Claude Code 2.1.301

- The `initialize` answer has `commands`: 53 entries, each `{name, description, argumentHint}`.
  Built-ins add `builtin: true` and sometimes `aliases`. It is the same answer Keystone's
  model-catalog probe already reads, so listing commands costs no extra process.
- User and project commands **and skills** are in the same list with no type field. The
  source is a suffix on the description: `Fixture user command (user)`,
  `Fixture project skill (project)`. A namespaced command is `fixture-ns:deep`. The
  argument hint comes from the `argument-hint` front matter (`[file]` arrives as `file`).
- Skills and commands are told apart only by where they live: `<config>/skills/<name>/` or
  `<cwd>/.claude/skills/<name>/`. Keystone checks for that `SKILL.md`.
- Only commands Claude accepts in this non-interactive mode are listed. The binary marks
  the rest `requires: {ink: true}` (they need the terminal UI). Among them are `/login`,
  `/logout`, `/resume`, `/permissions`, `/status`, `/theme`, `/hooks`, `/memory`,
  `/plugin`, `/add-dir`, `/export`, `/help`, `/ide`, `/terminal-setup` and `/statusline`.
  Keystone lists these as **Terminal only** and never sends them.
- Two built-ins are internal to server-launched sessions (`__remote-workflow`,
  `workflow-launch-exec`). Names starting with `__` are hidden.
- `system/init` (`slash_commands`, `skills`) only arrives after a user message, so it can't
  feed the menu before the first Send. The `initialize` list can.
- Claude can push `commands_changed` mid-session (skills found in a subdirectory). Keystone
  doesn't read it yet.
- Plugin commands were not in the fixture. The binary maps a plugin source to `plugin`, so
  Keystone treats a `(plugin…)` suffix as Plugin. Not observed.

## Codex 0.163.0

- Custom prompts (`$CODEX_HOME/prompts`) were removed in Codex 0.117.0. The app-server
  has no method that lists them (`prompts/list` and `customPrompts/list` are unknown
  variants), and the fixture prompt was not listed anywhere. Keystone does not read
  `$CODEX_HOME/prompts`.
- `skills/list {cwds: [cwd]}` lists skills for the folder: name, description, optional
  `shortDescription`, `scope` (`user`, `repo`, `system`, `admin`), `enabled` and
  `pluginId`. It found the fixture user skill in `$CODEX_HOME/skills`, both repo skills
  (`.codex/skills` and `.agents/skills`), and Codex's own system skills.
- Codex's turn input has a `skill` item (`{name, path}`) besides text. Its own TUI invokes
  a skill as `$name`. Keystone inserts `$name ` as text and sends it as typed.
- Codex's slash commands (`/model`, `/review`, `/compact`, `/init`, `/status`, …) belong to
  its TUI; the app-server takes them as plain text. Keystone lists the common ones as
  **Terminal only**.

## Not verified

- That Claude expands a custom command or skill sent as `/name args` in a stream-json user
  message, and that Codex injects a skill named only as `$name` in text. Both need a real
  turn. The scripted providers prove only that Keystone sends the text as typed.
- Plugin and managed (policy) commands.
