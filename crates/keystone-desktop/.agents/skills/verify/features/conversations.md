# Conversations

Send a message in a Claude Code or Codex session, watch the reply stream, answer an approval, steer or stop a turn, and reopen the conversation after a restart. Both providers use the same composer, transcript, and approval card, so run each journey once per provider.

## Sub-features

- `chat-send` sends the composer text with the Send button or Ctrl+Enter and returns focus to the composer.
- `chat-stream` shows the reply as it arrives, with commands and their output, and "Working…" until the turn ends.
- `chat-approval` shows an approval card; Allow runs the command and Deny skips it. The decision is recorded.
- `chat-steer` sends a message during a turn to that turn. The button reads Steer while a turn runs.
- `chat-stop` ends the running turn. The partial reply stays, marked Stopped.
- `chat-markdown` renders replies as Markdown: headings, emphasis, inline code, lists, quotes, tables, and code blocks with a copy button. Completed replies have a copy button below them. Web links open in the browser.
- `chat-approval-mode` chooses the session's approval mode from the shield button in the composer. The mode is saved and applies to every message sent after it. Full access shows in the warning color.
- `chat-restore` reopens the conversation from its log after a restart, scrolled to the latest message, and resumes the same provider conversation.

## How to get to it (user POV)

- Click "Start with Claude Code" or "Start with Codex", or press Ctrl+N or Ctrl+Shift+N, type, then click Send or press Ctrl+Enter.
- Answer an approval card with Allow or Deny.
- While a turn runs, type and press Ctrl+Enter to steer, or click Stop.

## Driving it with keystone-verify

Use the scripted providers for repeatable runs. They make no model calls, label their replies as test data, and ask approval when a message contains "run". In the scripted Claude, a message containing "long" streams long enough to steer or stop:

```json
{"env": {"KEYSTONE_CODEX_PATH": "<workspace>/target/debug/keystone-fake-codex", "KEYSTONE_CLAUDE_PATH": "<workspace>/target/debug/keystone-fake-claude", "KEYSTONE_PROJECT_DIR": "/tmp/<disposable git repo>", "KEYSTONE_FAKE_CODEX_DELAY_MS": "90", "KEYSTONE_FAKE_CLAUDE_DELAY_MS": "90"}}
```

Pass that as `start`'s `env`. Build them first with `cargo build -p keystone-agent --bins`. Omit `KEYSTONE_CODEX_PATH` or `KEYSTONE_CLAUDE_PATH` to use the real `codex` or `claude`; that spends the user's usage, so keep prompts tiny and point `KEYSTONE_PROJECT_DIR` at a disposable repository. With the real `claude`, the default model may think for several seconds before any text streams, so wait longer before a steer or stop screenshot. Driver `wait` actions are capped at 3000 ms.

- `click` `welcome-claude` or `welcome-codex`, `wait_for` `send`, then `input` `{"actions":[{"action":"type","text":"Hello"}]}` and `click` `send`.
- Screenshot while `turn-status` is present to see streaming, then `wait_for` `turn-status` with `"present":false`.
- Type a message containing "run" and press Ctrl+Enter (`{"action":"keypress","keys":["ctrl","Return"]}`). `wait_for` `approval`, then `click` `approve` or `deny`.
- Steer: send a message, wait about 700 ms, type another, and press Ctrl+Enter. Stop: send a message, wait, and `click` `stop`.
- Steer refusal (Claude): point `KEYSTONE_CLAUDE_PATH` at a wrapper script that sets `KEYSTONE_FAKE_CLAUDE_NO_LIFECYCLE=1` and execs `keystone-fake-claude`. A steer during a "long" reply then shows the reason in the transcript and leaves the text in the composer. A missing binary (`KEYSTONE_CLAUDE_PATH=/nonexistent/claude`) shows "Could not start Claude Code at …" in the transcript a moment after Send.
- Quit with Ctrl+Q, `launch`, `wait_for` `conversation`, and inspect the restored rows. `workspace` shows the session's `provider_thread`.

Named elements: `send`, `stop`, `approval`, `approve`, `deny`, `turn-status`, `conversation`, `project`, `composer-field`, `transcript-status`, `transcript-row-N` for row N of the transcript, `approval-mode`, `approval-<mode>`, `copy-reply-<row>`, and `<markdown id>-copy-<n>`. The transcript is virtualized, so `elements` lists only the rows in view; a row far from the bottom is absent until it is scrolled to.

- Long sessions: create a Codex session, quit, write `sessions/<id>.jsonl` next to the run's `workspace.json` (the `state` path `start` returns), and `launch`. Logs without `seq` are the older format and must still open in file order.
- Restore order: steer while a reply streams, quit, and relaunch. The steer must follow the reply it interrupted.
- History that cannot be read: quit, `chmod 000` the session's log, and `launch`. The conversation shows a warning and no history. Restore the mode, send, quit, and relaunch: the new rows follow the history.
- History that never arrives: quit with the session selected, replace its log with a FIFO (`mkfifo`) that nothing writes to, and `launch`. Other sessions still send and save. Ctrl+Q must close the window in about two seconds. Put the log back before relaunching.
- Approval modes: `click` `approval-mode`, then `approval-read-only` (or `approval-provider`, `approval-ask`, `approval-auto`, `approval-full-access`). The scripted Codex ends each reply with ``**Approvals:** `<policy> / <sandbox>` ``, the settings its thread holds, and the scripted Claude with ``**Mode:** `<permission mode>` ``, so the next reply shows whether the mode took effect. `workspace` shows `approval_mode` and `approval_overridden`. With the real `codex` or `claude` in a disposable repository, ask it to create a file: Auto and Full access write it, and Read only asks or refuses. After a restart, switch to Provider settings: your own settings apply again.
- Markdown: start with `KEYSTONE_VISUAL_FIXTURE=1` in `env` for the sample transcript, or ask a real provider for a Markdown sample. Code block copy buttons are named `<markdown id>-copy-<n>`, such as `fixture-copy-1` or `reply-3-copy-1`; reply copy buttons are `copy-reply-<row>`. Read the clipboard with `wl-paste` on the run's Wayland socket.

## Not covered

- Wheel scrolling. Driver scroll actions reach no GPUI view (KS-012).
- A copy click before any key press in the run copies nothing (KS-013).
