# Integrations

List the MCP servers, skills and plugins each provider account loads for the window's project, read from the providers' own files, with what running sessions in that project last reported about each server. Read only: nothing here writes.

## Sub-features

- `integrations-read` reads, per account (each provider's default and every added one) and the window's project: Claude's `.claude.json` (user `mcpServers`, local `projects["<git root>"].mcpServers`, `disabledMcpServers`, `disabledMcpjsonServers`), `.mcp.json` in the project and each folder above it up to its git root, `enabledPlugins` in `<config>/settings.json`, `.claude/settings.json` and `.claude/settings.local.json` with versions from `plugins/installed_plugins.json`, and skills in `<config>/skills`, `.claude/skills` and each loaded plugin's `skills` folder (named `plugin:skill`); Codex's `config.toml` (`[mcp_servers]`, `[plugins]`, `[[skills.config]]` and project trust), `.codex/config.toml` in the project and the folders above it up to the git root (shown "Project not trusted by Codex" until the nearest `[projects]` entry trusts it), and skills in `$CODEX_HOME/skills`, `~/.agents/skills`, `.agents/skills`, `.codex/skills` and each enabled plugin's highest cached version. The account's own files are never read a second time as the project's when the git root is the user's home. Hidden skill folders such as Codex's `.system` are skipped. Layouts are in `docs/research/integrations-probe.md`.
- `integrations-overrides` marks a server or plugin declared again at a stronger scope (local over project over user, and the nearest file at one scope) "Overridden by <scope> scope"; it shows no status.
- `integrations-secrets` keeps a stdio server's command (its first word only, if a whole command line was pasted there) and only the number of its arguments, the names of its env variables and headers, and only a URL's scheme, host and port (`/…` when it goes on; a URL that does not parse shows `•••`). Provider error text is never kept. A file that cannot be parsed is listed under the groups with the parser's message, never its contents.
- `integrations-status` shows each server's status from the newest report of a session whose process is still running, in the window's project folder itself: Claude's `system/init` `mcp_servers` and Codex's `mcpServer/startupStatus/updated`, without Keystone's own `keystone` and `keystone_ui` servers. A session's report replaces its last one, and a session that stops takes its report with it. An untrusted or overridden declaration gets no status. A server no file declares, such as a plugin's, is listed as reported by a running session. Before any running session reports, a server shows "Not running".
- `integrations-refresh` reads the files again with Refresh. Opening the section reads them each time, and only the newest read fills the page.

## How to get to it (user POV)

- Press Ctrl+Comma, then choose Integrations. The section lists MCP servers, then Skills, then Plugins, each with a count. A row shows the name (with Off, Overridden or Project not trusted when the provider does not use it), how it runs or its description or version, and provider · account · scope · source file (a plugin's skill names its plugin instead). A server's status sits on the right; its tooltip says when it was reported.

## Driving it with keystone-verify

- `start` gives the app an empty `HOME`, and empty `CLAUDE_CONFIG_DIR` and `CODEX_HOME` unless the server inherited them, whenever `env` names none. Name fixture homes for all three (Codex reads `~/.agents/skills`), and set `KEYSTONE_FAKE_MCP_CONFIG=1` so the scripted providers report the fixture's servers. Never run this section against the real `~/.claude`, `~/.claude.json`, `~/.codex` or `~/.agents`.
- `click` `{"id":"settings-section-integrations"}`, then `wait_for` a row: `integration-<provider>-<account>-<mcp|skill|plugin>-<user|local|project>-<name>`, such as `integration-claude-default-mcp-user-fixture-ok`. A second row with the same ID within one account ends in `-2`, and so on. A status element is the row's ID plus `-status-<state>`: `not-running`, `connected`, `starting`, `failed`, `needs-sign-in`, `off`, or `none` for a server the provider does not use. Reported-only servers use scope `reported`.
- The scripted providers report every server their fixture config turns on: a stdio server whose command is `false` fails, every other one connects. Send once in a Claude and a Codex session, reopen Integrations and require `-status-connected` and `-status-failed`.
- Read `elements` and OCR the screenshots, and require a fixture secret value in neither. Put the secret in an argument, a URL path and a URL password as well as an env value.

## Gotchas

- Status comes only from sessions that ran in this project's folder; a session started in a subfolder is not counted. Keystone never starts a provider or an MCP server to ask, so a server shows "Not running" until a session of that account starts here, and again once every such session has stopped.
- Claude sends `system/init` only once a turn starts, so a server that was pending then may show "Starting" until the next turn.
