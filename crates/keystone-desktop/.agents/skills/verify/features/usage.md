# Usage

Show each account's usage-limit windows and the session's context, as the provider reports them. Keystone shows windows, percentages, reset times and how old each reading is. It never shows an email address, organisation or plan.

## Sub-features

- `usage-decode` reads Claude's `rate_limit_event` (each of `unifiedWindows`, plus the limiting window it names), the answer to the `get_usage` each Claude chat process is asked once it answers `initialize` (each window of `rate_limits` with a `rate_limit_event` ID; nothing for an error or `rate_limits_available: false`), and Codex's `account/rateLimits/updated` (primary and secondary windows). Each report replaces the windows it carries and keeps the others. A window that reset more than a day ago is forgotten.
- `usage-context` reads the context the conversation fills: Claude's last main-conversation call's token usage with the `result`'s `modelUsage.contextWindow`, and Codex's `thread/tokenUsage/updated` (`last.totalTokens`, `modelContextWindow`). It is kept for the run only.
- `usage-save` saves each account's readings as a `usage.recorded` event and a `usage/<provider>/<account>` row in `metadata`, and loads them at launch.
- `usage-meter` appears in the composer footer only when the session's own account has a window at 80% or more that has not reset, or the session's context is at 80% or more. Its tooltip lists every window on its own line, with its age when older than the newest, and the context. A reading older than 15 minutes shows its age on the meter itself. Clicking it, or Enter or Space while it has focus, opens Settings → Accounts.
- `usage-accounts` lists each account's windows under its row in Settings → Accounts, with a bar, the percentage, the reset time, "Updated …", and the age of any older window. A window past its reset shows "reset" without a percentage.

## How to get to it (user POV)

- Send in a Claude or Codex session. After the provider reports usage, open Settings → Accounts (Ctrl+Comma, then Accounts) to see each account's windows.
- When the session's account nears a limit, the footer shows a small meter beside the approval mode. Click it to open Settings → Accounts.

## Driving it with keystone-verify

- Use the scripted providers. A prompt containing `usage NN` makes the fake end its turn with synthetic, labelled usage: Claude a `rate_limit_event` with the 5-hour window at NN% and the weekly one at 20%, plus token usage filling half of NN% of a 200,000-token context; Codex the matching `account/rateLimits/updated` and `thread/tokenUsage/updated`.
- Send `usage 40` in a Claude session and require no `usage-meter`. Send `usage 85` and `wait_for` `{"id":"usage-meter"}`.
- In a Codex session send `usage 30` and require no `usage-meter`: the meter follows the session's own account.
- `click` `{"id":"usage-meter"}` and require `settings-accounts` with `account-usage-<row>-<window>` elements, such as `account-usage-claude-default-five_hour` and `account-usage-codex-default-codex/primary`.
- Read the run's database read-only and require `metadata` rows `usage/claude/default` and `usage/codex/default` and `usage.recorded` events.
- Quit, wait over a minute, relaunch. Require the meter on the Claude session and "Updated 1m ago" (or older) in Settings → Accounts.
- `get_usage`, as separate runs, each with fresh state: write `85` to `.fake-claude-get-usage` in the `KEYSTONE_PROJECT_DIR` folder (or export `KEYSTONE_FAKE_CLAUDE_GET_USAGE` in a wrapper passed as `KEYSTONE_CLAUDE_PATH`; `start` does not accept it in `env`), set a long `KEYSTONE_FAKE_CLAUDE_DELAY_MS`, and send `hello` in a new Claude session. `usage-meter` appears while the reply streams, and Settings → Accounts shows `account-usage-claude-default-five_hour`, `-seven_day` and `-seven_day_sonnet` (no `model_scoped`). With `error`, `{"rate_limits_available":false,"rate_limits":null}` or no setting, the turn completes and no `account-usage-claude-default-*` window appears.

## Gotchas

- Ages and reset states are computed when the view draws. While any reading exists the window redraws once a minute, so a label or a reset can lag by up to a minute.
- Saves run off the UI thread, the newest reading per account. A reading that arrives in the last moment before Quit may not be saved; the account's next report saves every window again.
- A Claude process starts at a session's first Send, so the `get_usage` reading arrives as that turn starts, not when the session opens. The scripted Claude answers it like a home with no login (`rate_limits_available: false`) unless `KEYSTONE_FAKE_CLAUDE_GET_USAGE` (or a working directory's `.fake-claude-get-usage`) is `NN` for synthetic windows with the 5-hour one at NN%, `error`, or a JSON answer.
- Codex reports rate limits after a turn's responses. Keystone does not ask `account/rateLimits/read` at startup.
