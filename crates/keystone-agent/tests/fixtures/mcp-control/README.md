# No-inference MCP control probe

Captured on 2026-10-19 with Claude Code 2.1.301 and codex-cli 0.163.0 in isolated,
unsigned-in homes. `scripts/probe-orchestrator.py` sends no user messages, starts no
turns and denies tool calls. Codex's model provider points at the loopback probe
server, not an inference endpoint. Tokens and temporary roots are sanitized.

Claude accepted `mcp_set_servers`, connected with its `headers.Authorization`
bearer, listed tools and reported the dynamic server connected. Codex accepted
per-thread `config.mcp_servers.keystone` with `http_headers.Authorization` on both
`thread/start` and `thread/resume`; both made authorized MCP initialize and
tools/list requests. `mcpServerStatus/list` must name the thread to see its server.

A genuinely empty Codex thread has no persisted rollout, so ordinary live and cold
resume return `no rollout found`. The successful resume probe explicitly created
a synthetic session_meta-only rollout in its disposable CODEX_HOME. It proves
resume configuration and MCP connection, not real-conversation resumption.

Run `python3 scripts/probe-orchestrator.py --claude /path/to/claude --codex
/path/to/codex`. Output is kept under `~/.cache/keystone/work/keystone-mcp-probe/` by default,
or the `KEYSTONE_PROBE_DIR` directory. No installed app or saved provider sessions
are touched. The fixture records protocol observations, not model behavior.
