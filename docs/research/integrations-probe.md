# MCP servers, skills and plugins: where the tested CLIs keep them (TOB-68 probe)

Run on desk on 2026-10-21 with Claude Code 2.1.301 and Codex 0.163.0, by
`scripts/probe-integrations.py`. Raw output: [integrations-probe.json](integrations-probe.json).

No inference: each CLI ran under `unshare -rn` (no network) with a fresh `HOME`, a fixture
`CLAUDE_CONFIG_DIR` or `CODEX_HOME` and no credentials. Every run set those variables, so
what this note says about the defaults without them (`~/.claude.json`, `~/.codex`) comes
from the CLIs' documentation and binaries, not from a run. The CLIs' own `mcp add` and
`plugin` commands wrote the fixtures. Claude received only the `initialize` and
`mcp_status` control requests. Codex received `initialize`, list requests and one
ephemeral `thread/start`, which starts the thread's MCP servers. No user message was sent
and no turn started. The fixture values called `fixture-secret` are not real secrets; the
raw output keeps them because Claude's `mcp_status` echoes them (see below).

## Claude Code 2.1.301

- **MCP servers.** `claude mcp add -s user` writes `mcpServers` at the top of
  `.claude.json`; `-s local` writes `projects["<project>"].mcpServers` in the same file;
  `-s project` writes the project's `.mcp.json`. With `CLAUDE_CONFIG_DIR` set,
  `.claude.json` is inside that folder. Without it, it is `~/.claude.json`, beside
  `~/.claude`. The project key is the git root's absolute path, even when the command ran
  in a subfolder; Claude also reads `.mcp.json` from parent folders.
- Each entry has `type` (`stdio`, `http`, `sse`), `command`/`args`/`env` or
  `url`/`headers`, with the values as typed, secrets included.
- `/mcp` toggles write `projects["<project>"].disabledMcpServers`; refusing a project
  server writes `disabledMcpjsonServers` (both read from the binary). In `-p` mode an
  unapproved `.mcp.json` server is still started.
- **Plugins.** `claude plugin install -s user|project|local` writes
  `enabledPlugins["name@marketplace"] = true` in `<config>/settings.json`,
  `<project>/.claude/settings.json` or `<project>/.claude/settings.local.json`, and the
  install in `<config>/plugins/installed_plugins.json` (version 2: per plugin a list of
  `{scope, installPath, version, projectPath?}`). `disable` sets the value to `false`.
  Local overrides project, which overrides user.
- **Skills.** `<config>/skills/<name>/SKILL.md` (user) and
  `<project>/.claude/skills/<name>/SKILL.md` (project). A plugin's skills come with the
  plugin as `plugin:skill`. `~/.agents/skills` and `~/.claude/skills` are not read when
  `CLAUDE_CONFIG_DIR` is set.
- **Status.** `system/init` lists `mcp_servers: [{name, status}]`, but only after a user
  message, so this probe never received one: the shape and the `disabled` status come
  from the SDK's types and the binary, and Keystone reads each entry on its own, skipping
  one it does not understand. The `mcp_status` control request answers without one, with
  `{name, status, error, config, scope, source}` per server: `connected`, `failed`,
  `pending`, `needs-auth`. It includes servers no file Keystone reads declares, such as
  `plugin:fixture-plugin:fixture-plugin-server`. Its `config` echoes header values,
  secrets included, so anything that reads it must drop them.

## Codex 0.163.0

- **MCP servers.** `codex mcp add` writes `[mcp_servers.<name>]` in
  `$CODEX_HOME/config.toml`: `command`, `args`, `[…env]` or `url`,
  `bearer_token_env_var`. It has no project scope. A project's `.codex/config.toml`
  `[mcp_servers]` is used only once the project is trusted
  (`[projects."<path>"] trust_level = "trusted"`); until then Codex warns and starts only
  the user's servers. `enabled = false` turns a server off.
- **Plugins.** `codex plugin add` writes `[plugins."name@marketplace"] enabled = true` and
  `[marketplaces.<name>]` in `config.toml`, and installs under
  `$CODEX_HOME/plugins/cache/<marketplace>/<name>/<version>`.
- **Skills.** `skills/list` lists `$CODEX_HOME/skills` and `~/.agents/skills` as `user`,
  the project's `.agents/skills` and `.codex/skills` as `repo` (trusted or not), and its
  bundled `$CODEX_HOME/skills/.system` as `system`. `~/.codex/skills` is not read when
  `CODEX_HOME` points elsewhere. `[[skills.config]] path = "…/SKILL.md", enabled = false`
  turns one off.
- **Prompts.** Custom prompts were removed in Codex 0.117.0 (see
  [slash-commands-probe.md](slash-commands-probe.md)), so there is nothing to list.
- **Status.** After answering `thread/start`, Codex sends
  `mcpServer/startupStatus/updated` per server: `starting`, then `ready` or `failed` with
  an `error`. `mcpServerStatus/list` with the thread's ID gives the same as
  `runtimeStatus` (`connected`, `failed`). Without a thread ID it starts every configured
  server itself to list their tools and gives no `runtimeStatus`.

## What Keystone does with it

- Settings → Integrations reads the files above per account and project
  (`keystone_core::integrations`). Secret values are dropped while reading: a stdio
  server keeps its command and the number of its arguments, an env or header keeps only
  its name, and a URL keeps only its scheme, host and port.
- A declaration that another of the same name at a stronger scope replaces is marked
  overridden, following the order above for plugins and Claude's documented local over
  project over user for MCP servers.
- Status comes from running sessions only: Claude's `system/init` and Codex's
  `mcpServer/startupStatus/updated`, without Codex's `error` text. Keystone never starts a
  provider or an MCP server to ask, and never reads `mcp_status`'s `config`.

## Not verified

- Managed (enterprise) MCP and plugin settings, Claude's `claude.ai` connectors, and
  servers that need OAuth. The fixture had none.
- Whether Codex reads `.agents/skills` and `.codex/config.toml` in parent folders of the
  project up to the git root, as Keystone assumes, and whether Claude reads
  `.claude/skills` and `.claude/settings*.json` above the project, which Keystone does
  not. How far above the project Claude looks for `.mcp.json`: Keystone stops at the git
  root.
- Which cached version of a Codex plugin supplies its skills when several are cached:
  Keystone takes the highest version, comparing dot-separated numbers.
- Whether Codex trusts a subfolder through its git root's `[projects]` entry: Keystone uses
  the nearest entry, so a subfolder marked untrusted stays untrusted.
