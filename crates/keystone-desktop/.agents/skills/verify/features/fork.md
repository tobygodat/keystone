# Fork from here (TOB-57)

Fork from here starts a new session holding a conversation up to one of its messages, with the same provider, account, model, approval mode and folder. The source session is never changed.

## Entry points

- The Fork from here button (`fork-<row>`, a branch icon) beside Copy on a message the user sent (shown on hover or when the row is focused) and under a finished reply.
- The keyboard: focus the transcript (Ctrl+Shift+T), move to a message, Tab to its Fork from here action and press Enter; or press Ctrl+Shift+F on the focused row.
- The command palette: "Transcript: fork from the focused message".

## Expected behaviour

- Each finished turn saves the provider's own fork point in the session log (`fork_point` entries): for Claude Code the transcript UUID of the turn's last entry, for Codex the turn ID. Forking from any message in that turn uses it.
- A native fork opens a session titled "<source title> (fork)", linked by `forked_from` and holding `fork_point` in the `workspace` projection. It shows the source's rows through the end of that turn, and its composer is empty. It always opens in a tab beside the source, as New tab does, so the source stays one click away. Its link and row selectors carry the tab's view suffix, such as `fork-from-view-3`.
- Nothing reaches the provider until the fork's first message. That message opens the provider's fork of the source: Claude Code is launched with `--resume <source conversation> --fork-session --resume-session-at <entry UUID>`, and Codex receives `thread/fork` with `threadId` and `lastTurnId`. The fork then has its own conversation (`provider_thread`), and later messages resume that one.
- A turn with no saved fork point offers "Fork from here (handoff)" instead. It opens an unsent session titled "<source title> (handoff fork)", on the same provider, account and model with the conversation up to the end of that turn as handoff text in its composer, as Hand off does. It never starts a fresh conversation silently. A turn has no saved fork point when:
  - its history is from before this build, or its provider gave no IDs;
  - the provider exited during it;
  - it came before a Claude Code compaction, since Claude Code resumes a compacted conversation only from the compaction on;
  - the message is a shown but unechoed message (a `local:` row), such as one queued for the next turn.
- A message in the turn still running cannot be forked yet; the attempt says so and creates nothing.
- If the provider refuses the fork (for example, the source conversation is gone from its store, or Claude Code compacted it after the fork was made and before the fork's first message), the fork's first turn fails with "Could not fork this … conversation: <reason>", and the history stays shown.
- "Forked from" (`fork-from`) shows at the top of the fork, and "Forked in" (`fork-to-<id>`) at the top of the source once the fork has started (a native fork starts at once). Each opens the other session. Both survive a restart.

## Native journey

Use fixture homes and the scripted providers in store mode (`KEYSTONE_FAKE_CLAUDE_STORE=1`, `KEYSTONE_FAKE_CODEX_STORE=1`) with argument and method logs (`KEYSTONE_FAKE_CLAUDE_ARGV_LOG`, `KEYSTONE_FAKE_CODEX_METHOD_LOG`). For each provider, give a session three turns, then fork at the second turn's reply with the mouse, and at a message with the keyboard. Then check:

- The fork shows two turns and the source still shows three.
- After the fork's first Send, the argument log's last Claude launch ends with the fork flags naming the second turn's last entry, or the Codex method log holds one `thread/fork`; the source's store file is byte-for-byte unchanged.
- A Send in the fork reaches only the fork's process; the source's turn count is unchanged.
- Both link rows show and open the other session.
- After Quit and relaunch, both sessions, their rows and the links still show, and `workspace` still holds `forked_from`.
