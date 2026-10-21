What `scripts/probe-show-ui.py` saw on 2026-10-20 with Claude Code 2.1.301 and Codex
0.163.0, in disposable homes and with no message sent and no turn started. Claude
accepts the `keystone_ui` SDK server from `mcp_set_servers`, then runs `initialize`,
`notifications/initialized` and `tools/list` over `mcp_message`; Codex takes
`dynamicTools` on `thread/start` only after `initialize` sets `experimentalApi`.
Answers that list the user's own configuration are elided.
