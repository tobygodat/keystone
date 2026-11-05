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
- `chat-model` chooses the draft session's provider and model from the one `<Provider> · <model>` button in the composer. The menu has a provider column and a model column read from the provider's own catalog. A third column offers the model's Effort and Speed from its catalog entry. A provider choice saves at once, resets the model, effort and speed to Default, retitles a placeholder, and keeps the menu open; a model, effort or speed choice saves and closes. After the first Send the button shows the model the provider reported, or the requested one with "not reported yet", and still opens the menu, without the provider column: the model, effort and speed change for the next turns, a field that has a value offers no Default row, and each change adds a muted "Next turns: …" line to the transcript. The provider column also lists each account (`account-<id>`, `account-<provider>-default`), and with an account chosen the button reads `<Provider> · <account> · <model>`.
- `chat-model-keys` changes the model choice from the keyboard: Next effort level (Ctrl+Alt+E, ⌥⌘E on macOS) cycles the model's efforts, Toggle fast mode (Ctrl+Alt+F, ⌥⌘F) switches between its standard and fast speed, and Next model (palette only) takes the next catalog model. Each saves like the menu, or shows why it did nothing.
- `chat-restore` reopens the conversation from its log after a restart, scrolled to the latest message, and resumes the same provider conversation.

## How to get to it (user POV)

- Click the **Claude Code** or **Codex** welcome pill, or press Ctrl+N or Ctrl+Shift+N, type, then click the round Send button or press Ctrl+Enter. Send is filled only when the draft has text.
- Click the **Codex · Provider default** (or **Claude Code · …**) button in the composer toolbar to choose the provider on the left (before the first Send only), the model, and its effort and speed on the right.
- Press Ctrl+Alt+E or Ctrl+Alt+F, or run Next effort level, Toggle fast mode or Next model from the palette.
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

Named elements: `send`, `stop`, `approval`, `approve`, `deny`, `turn-status`, `conversation`, `project` (the small conversation-folder button, 20 px at default density; its tooltip shows its saved folder or required action), `composer-field`, `transcript-status`, `transcript-row-N` for row N of the transcript, `approval-mode`, `approval-<mode>`, `model-selector`, `provider-claude`, `provider-codex`, `model-default`, `model-<id>` for each catalog row (the provider's own model ID, such as `model-fake-fast`), `model-loading`, `model-failed`, `model-retry`, `choice-column`, `effort-default`, `effort-<value>`, `speed-default`, `speed-<value>`, `choice-unknown`, `note-<row>` for a transcript note, `copy-reply-<row>`, and `<markdown id>-copy-<n>`. The transcript is virtualized, so `elements` lists only the rows in view; a row far from the bottom is absent until it is scrolled to.

- Long sessions: generate repeated scripted turns, then quit and relaunch. For a bounded legacy-import fixture, instantiate the Python driver's `Run`, stage `workspace.json` and nested `sessions/<id>.jsonl` under its scratch directory before calling `start`, then inspect the imported history. Logs without `seq` must still open in file order. JSON/JSONL files become read-only backups after the first import; editing them after the SQLite database exists must not change live state. Fixture staging proves import, not user creation.
- Restore order: steer while a reply streams, quit, and relaunch. The steer must follow the reply it interrupted.
- History that cannot be read: after startup and history load, obstruct the disposable SQLite database or its parent and inspect retryable save feedback. Repair before cleanup. Making the whole database unreadable before startup tests startup failure, not a per-session history warning. Use the GPUI reader seam for isolated unreadable-history behavior.
- History that never arrives: use the GPUI reader gate to stall one session while another sends and saves, and assert the two-second Quit fallback. Replacing a legacy JSONL file with a FIFO after import no longer injects a live read failure.
- Approval modes: `click` `approval-mode`, then `approval-read-only` (or `approval-provider`, `approval-ask`, `approval-auto`, `approval-full-access`; Full access first asks in a "Turn on Full access?" dialog, so also `click` `full-access-dialog-confirm`). The scripted Codex ends each reply with ``**Approvals:** `<policy> / <sandbox>` ``, the settings its thread holds, and the scripted Claude with ``**Mode:** `<permission mode>` ``, so the next reply shows whether the mode took effect. `workspace` shows `approval_mode` and `approval_overridden`, and leaves both out while they hold their defaults (Provider settings, false). With the real `codex` or `claude` in a disposable repository, ask it to create a file: Auto and Full access write it, and Read only asks or refuses. After a restart, switch to Provider settings: your own settings apply again.
- Model: in a draft, `click` `model-selector`, then `provider-claude` or `provider-codex`, then `model-default` or a catalog row. The scripted Codex lists `model-fake-default` and `model-fake-fast` (a hidden third entry never shows); the scripted Claude lists `model-fake-opus` and `model-fake-haiku`. Rows show names only, no descriptions. Each scripted reply ends with ``**Model:** `<model>` `` (`fake-default` or `fake-opus` when nothing was chosen), so the next reply shows which model the turn ran on, and after it the button reads that model: by its catalog label (`Fake Opus`, `Fake Fast`) when this run has read the catalog, else by the reported ID. `workspace` shows `model`, `effort`, `speed` and `model_locked`.
- Effort and speed: the scripted Codex lists efforts `low` and `high` (default `high`) and speeds `default` (Standard) and `priority` (Fast) on both its models; the scripted Claude lists `low`, `medium`, `high`, `max` and Standard and Fast (`standard`, `fast`) on `fake-opus` and none on `fake-haiku`, where the column says "This model has no effort or speed choices". Once a turn sets them, a scripted reply adds ``**Effort:** `<effort>` `` and, for Codex, ``**Tier:** `<tier>` `` or, for Claude, ``**Fast:** `on` ``. After the first Send, choose `model-fake-fast` and then `effort-high` from the menu, or press Ctrl+Alt+E and Ctrl+Alt+F; each adds a `note-<row>` line, and the next reply names the new settings. A Claude working directory holding `.fake-claude-set-model` with `error` refuses a model change, which fails the next turn with nothing sent; one holding `.fake-claude-fast-mode` with `disabled` answers fast mode off for the account, which shows "Claude Code did not turn on fast mode (extra_usage_disabled).". Quit and relaunch: the choice and the notes come back. With `KEYSTONE_FAKE_CODEX_NO_CATALOG=1` in `env`, the Codex model column shows `model-failed` with "scripted catalog failure" and a `model-retry` row, and `model-default` still works. A Codex wrapper that exports `KEYSTONE_FAKE_CODEX_EXTRA_MODELS=<n>` adds `model-fake-extra-1` to `model-fake-extra-<n>`, enough rows to choose one below the fold; reopening the menu must show it. The real `claude` lists its catalog from a short-lived process that saves no session; the real `codex` answers `model/list` from the shared app-server.
- Markdown: start with `KEYSTONE_VISUAL_FIXTURE=1` in `env` for the sample transcript, or ask a real provider for a Markdown sample. Code block copy buttons are named `<markdown id>-copy-<n>`, such as `fixture-copy-1` or `reply-3-copy-1`; reply copy buttons are `copy-reply-<row>`. Read the clipboard with `wl-paste` on the run's Wayland socket.

## Not covered

- Wheel scrolling requires the corrected driver and separate [native wheel journey](wheel-and-maximize.md); older headless-only drivers omit discrete data (KS-012).
- A copy click before any key press in the run copies nothing (KS-013).

## Repeated conversations and unread

All conversation occurrences follow the active session, with independent retained scroll and focus. The default occurrence keeps the selectors above. Other occurrences append `-view-{ViewId}` to transcript, status, approval, and jump selectors. Read the actual element list after a helper or Borrow replacement. See [Customize](customize.md) for the complete selector map and fitting behavior.

Only a visible, focused conversation showing the latest reply in an active native window acknowledges that reply. Composer focus and unfocused visibility do not count. Selection, navigation, Send, Stop, and completion do not implicitly observe replies. Send scrolls only the most recently focused visible occurrence, then focuses the composer.

## Conversation folders

New conversations record their creation directory. Choose another layout project with the sidebar folder button, then Send from the existing conversation. Its folder tooltip and execution directory must remain the recorded directory. Repeat after restart, Codex process loss, and a Claude permission change that replaces the process. Log scripted requests and process working directories to establish these paths without real inference.

A restored conversation with no recorded folder retains its draft and refuses Send and Ctrl+Enter. Click the existing composer folder icon. Require a real rendered OS folder chooser. Cancel keeps the draft and unknown state. Choose an existing directory and require the canonical saved context with origin `chosen`. Choosing must not send. The button now opens that recorded folder. Send remains an explicit next action.

A failed SQLite commit keeps the context missing. A directory-sync warning after a successful commit keeps the context recorded and the Open conversation folder action. The warning ends once any later successful save confirms the folder's snapshot, such as an unrelated layout Apply, and a later unrelated save failure must not bring it back. A subsequent explicit Send retries sync. While sync fails, Send refuses with `Not sent. Could not finish saving changes.` followed by the error, whatever write left the sync pending, the draft stays and no provider starts. After repairing the fault, another explicit Send proceeds. Stop and pending approvals remain available. The folder button exposes its tooltip text as its accessible name on the same element that owns its click and focus, so an AT-SPI invoke opens the chooser or file manager. The disabled unsupported button keeps its tooltip and exposes no action.

Unsupported raw context disables the folder action with the explanation `Saved conversation folder is not supported by this version`. It cannot open a replacement picker. Send retains the draft and refuses dispatch. Removed recorded directories also refuse dispatch without adopting the layout project.

Native chooser evidence must show the OS window, selection or cancellation, resulting Keystone UI, and saved file. Simulated completions test ticket policy only. They do not prove the OS chooser.
