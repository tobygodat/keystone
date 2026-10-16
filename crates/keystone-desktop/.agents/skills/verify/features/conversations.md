# Codex conversations

Send a message in a Codex session, watch the reply stream, answer an approval, steer or stop a turn, and reopen the conversation after a restart. Claude sessions do not send yet.

## Sub-features

- `chat-send` sends the composer text with the Send button or Ctrl+Enter and returns focus to the composer.
- `chat-stream` shows the reply as it arrives, with commands and their output, and "Working…" until the turn ends.
- `chat-approval` shows an approval card; Allow runs the command and Deny skips it. The decision is recorded.
- `chat-steer` sends a message during a turn to that turn. The button reads Steer while a turn runs.
- `chat-stop` ends the running turn. The partial reply stays, marked Stopped.
- `chat-restore` reopens the conversation from its log after a restart, scrolled to the latest message, and resumes the same Codex thread.

## How to get to it (user POV)

- Click the **Codex** welcome pill or press Ctrl+Shift+N, type, then click the round Send button or press Ctrl+Enter. Send is filled only when a Codex draft has text.
- Answer an approval card with Allow or Deny.
- While a turn runs, type and press Ctrl+Enter to steer, or click Stop.

## Driving it with keystone-verify

Use the scripted Codex for repeatable runs. It makes no model calls, labels its replies as test data, and asks approval when a message contains "run":

```json
{"env": {"KEYSTONE_CODEX_PATH": "<workspace>/target/debug/keystone-fake-codex", "KEYSTONE_PROJECT_DIR": "/tmp/<disposable git repo>", "KEYSTONE_FAKE_CODEX_DELAY_MS": "90"}}
```

Pass that as `start`'s `env`. Build it first with `cargo build -p keystone-agent --bins`. Omit `KEYSTONE_CODEX_PATH` to use the real `codex`; that spends the user's Codex usage, so keep prompts tiny and point `KEYSTONE_PROJECT_DIR` at a disposable repository.

- `click` `welcome-codex`, `wait_for` `send`, then `input` `{"actions":[{"action":"type","text":"Hello"}]}` and `click` `send`.
- Screenshot while `turn-status` is present to see streaming, then `wait_for` `turn-status` with `"present":false`.
- Type a message containing "run" and press Ctrl+Enter (`{"action":"keypress","keys":["ctrl","Return"]}`). `wait_for` `approval`, then `click` `approve` or `deny`.
- Steer: send a message, wait about 700 ms, type another, and press Ctrl+Enter. Stop: send a message, wait, and `click` `stop`.
- Quit with Ctrl+Q, `launch`, `wait_for` `conversation`, and inspect the restored rows. `workspace` shows the session's `provider_thread`.

Named elements: `send`, `stop`, `approval`, `approve`, `deny`, `turn-status`, `conversation`, `project`, `composer-field`.

## Not covered

- Claude Code sessions, which show "not connected yet".
- Approval modes; Keystone uses the user's own Codex approval settings.
- Markdown rendering; replies show as plain text.
