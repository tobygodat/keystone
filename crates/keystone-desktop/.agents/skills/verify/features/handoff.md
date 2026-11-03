# Handoff (TOB-55)

A handoff continues a session in a new one on another provider, account or model. The source session is never changed.

## Entry points

- The top bar's Hand off button (`hand-off`), at the right end of the top bar beside Second opinion.
- Ctrl+Shift+H.
- The command palette: "Hand off…".

## Expected behaviour

- A new session opens in the source's folder with the source's title (a placeholder title becomes "Continued from session N"), linked by `handed_off_from` in the `workspace` projection. When the active view has tabs, or the layout has two or more conversation views, it opens in a new tab beside the source, as New tab does; otherwise it follows the selection like New session.
- Its composer holds the handoff text, scrolled to the end where the note goes: the newest messages in full and one line per tool call, kept under a 16,000-token estimate (four bytes a token). The budget line, and any `Left out for the budget: #…` positions, sit at the end of the text, next to the cursor.
- The model selector opens on the other provider's default account. Choosing a provider, account or model works as for any unsent session.
- Send goes to the chosen provider and account like any first message. A note typed after the text is sent with it.
- "Continued from" (`handoff-from`) shows at the top of the new session's conversation, and "Continued in" (`handoff-to-<id>`) at the top of the source's once the continuation has sent its first message, so an abandoned handoff does not mark the source. Each opens the other session. Both survive a restart.
- The new session's agent can read left-out rows from the source with the orchestrator's `session_read`, whose reply names `handed_off_from`.
- Hand off is refused with an error when the source has not sent anything, has no folder, is still loading, or its saved history could not be read.

## Native journey

Use fixture homes and the scripted providers. Add a second Codex account in Settings → Accounts. Give a Claude session three turns, one of which runs a command. Set `KEYSTONE_FAKE_CLAUDE_REPLY` in the fake's wrapper (the driver's `env` refuses it, as it refuses every `KEYSTONE_FAKE_*` variable but `KEYSTONE_FAKE_MCP_CONFIG`, `KEYSTONE_FAKE_IMAGE_RECEIPT` and the two delay variables) to a reply of about 24 KB so three turns exceed the budget. In a tab, the link ids carry the view suffix, such as `handoff-from-view-13`. Hand off with Ctrl+Shift+H, choose the second account, type a note at the end, and Send. Then check:

- The fake Codex receipt (`KEYSTONE_FAKE_ENV_RECEIPT`) shows that account's `CODEX_HOME`.
- With `KEYSTONE_FAKE_CODEX_STORE=1`, the thread under that home holds the handoff text and the note.
- Both link rows show and open the other session.
- After Quit and relaunch, the links still show and `workspace` still holds `handed_off_from`.

The handoff text quotes earlier prompts, so a quoted "run" makes the fake Codex ask for approval.
