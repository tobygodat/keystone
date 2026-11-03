# Local sessions

Create a local Claude or Codex session and select it from the sidebar. These records do not start an agent process.

## Sub-features

- `session-welcome` creates a session from the centered welcome composer.
- `session-menu` creates a session from the sidebar plus menu.
- `session-keyboard` creates a session with a shortcut.
- `session-select` switches between existing sessions.
- `session-attention` lists sessions that need the user in a **Needs action (n)** section above **Working**. A session needs action while it waits for approval, after its last turn failed (including a turn cut off when Keystone closed, but not one the user stopped or that ended because they denied an approval), when a reply landed after the user last viewed it, or while an agent widget waits for an answer (**Waiting for your answer**). The second line names the reason.
- `session-title` titles a session from its first sent message: one line, cut near 48 characters with an ellipsis. Later messages leave it alone.
- `session-rename` renames the selected session from the top bar title or F2. A rename is never replaced by a message.
- `window-drag` moves the window by the top bar or the sidebar app bar, for window managers that draw no title bar. The sidebar app bar holds only the logo and name; pressing a control in the top bar still clicks it.
- `session-time` shows how long ago each session last changed: `now`, `5m`, `3h`, a weekday within a week, then a date such as `Oct 3`. Sessions saved before times were recorded show none.

## How to get to it (user POV)

- In an empty workspace, click the **Claude Code** or **Codex** pill in the welcome composer.
- Click the pencil (`new-session`) at the right of the search row, then **Claude Code** or **Codex**.
- Press Ctrl+N for Claude or Ctrl+Shift+N for Codex.
- Click a session row in the sidebar to select it.
- Click the session title in the top bar, or press F2, to rename it. Enter saves, Escape cancels.

## Driving it with keystone-verify

Preconditions:

- Doctor identifies the current owned window and binary.
- Start from an empty workspace for each welcome-provider test. Call `stop` then `start` to reset state.

- Call `wait_for` with `{"id":"welcome-claude"}`, then `click` with `{"id":"welcome-claude"}`. Call `wait_for` with `{"id":"session-1"}`. Capture `screenshot` and require the welcome pills and heading gone, **New Claude session** under **Working** with its **Settle** button at the right of its title (the selected or hovered row shows Settle where other rows show their time, such as `now`), and `<project> / New Claude session` in the top bar (`keystone / …` when no project is open).
- Call `workspace`. Require exactly one session with `provider:"Claude"`, `settled:false`, `draft:""`, and its ID selected.
- Type a short draft without an extra focus click. Require the composer and state file to contain it. This proves the welcome action focused the editor.
- For the keyboard entry, call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","shift","n"]}]}`. Require a second record with `provider:"Codex"` selected. Type a short draft without a focus click and require it in the composer and state file.
- For the menu entry, `click` `{"id":"new-session"}` and `wait_for` `{"id":"new-claude"}` or `{"id":"new-codex"}`. Capture the open menu, then `click` that id. Require exactly one additional record of that provider. Type a short draft without a focus click and require it in the composer and state file.
- `click` the first row by its `session-<id>` from `workspace`. Require its title in the top bar breadcrumb, its provider in the composer toolbar, its restored draft, and its ID in `workspace.selected`.
- Titles: in a Codex session, send `Fix the login bug` and require it as the row title, the top bar title, and `title` in `workspace`, with `title_source:"FirstMessage"`. `click` `{"id":"rename-session"}`, `wait_for` `{"id":"rename-dialog"}`, select all, type `Release notes`, and press Enter. Require the new title in the row, the top bar, and `workspace` with `title_source:"Renamed"`. Send another message and quit and `launch` again; require the title unchanged. F2 then Escape changes nothing.
- Window drag: under a floating window manager, press on empty top bar space and drag; require the window to move. Press `minimize`, `maximize`, or `sidebar-toggle` and drift a few pixels before release; require the click to act and the window not to move.
- Preserve before/after screenshots and a state snapshot. Repeat the welcome journey in a fresh run for Codex before marking both provider buttons verified.

Needs action, with the scripted Codex from [conversations](conversations.md) and `KEYSTONE_FAKE_CODEX_DELAY_MS` of `250`:

- Failed turn: in a Codex session, send a message, `wait_for` `turn-status`, and quit with Ctrl+Q while it runs. `launch` again. Require the row under **Needs action (1)** with **Turn failed** in the warning color, and `last_turn:"Running"` in `workspace`.
- Unread reply: create a Codex session, send `hello`, and create another session at once, before the reply finishes. After the turn ends, require the first row under **Needs action** with **New reply** and a dot. `click` its row; require it to stay under **Needs action** while selected. Select another row; require it to move under **Working**, and its `viewed_at` to be at least its `updated_at`.
- Approval: send a message containing `run`, `wait_for` `approval`, and select another session. Require **Waiting for approval** on that row. `click` it and `approve`; require the row to move to **Working** while the turn runs.
- Repeat these in a Claude Code session with the scripted Claude and `KEYSTONE_FAKE_CLAUDE_DELAY_MS` of `250`. A Stop leaves the row under **Working**; only a failure, an approval, or an unread reply moves it to **Needs action**. `KEYSTONE_FAKE_CLAUDE_SIGNED_OUT=1` makes every turn fail; `start`'s `env` refuses it, so set it in a wrapper script that execs `keystone-fake-claude` and pass that as `KEYSTONE_CLAUDE_PATH`.
- Times: require `now` on new rows, and the label to age to `1m` after a minute without any input. To see the older labels, quit, write a workspace file whose sessions carry `updated_at` values 5 minutes, 3 hours, 2 days, and 12 days old, and `launch`. Use fresh session IDs so no earlier session log is shown. A file without `_at` fields must show no times. Saving it again never adds `created_at` or `updated_at`; leaving a session or quitting records only its `viewed_at`.

## Gotchas

- Duplicate titles are allowed. Use state IDs and the screenshot row position to distinguish sessions.
- The welcome chooser appears only when no session is selected. It is absent after creation.
- Named ids are `welcome-claude`, `welcome-codex`, `welcome-heading`, `new-session`, `new-claude`, `new-codex`, `composer-field`, `sidebar-toggle`, `rename-session`, `rename-dialog`, `rename-field`, `rename-dialog-cancel`, `rename-dialog-confirm`, `minimize`, `maximize`, `close`, `session-<id>`, `settle-<id>`, `attention-section`, `working-section` (shown whenever Working has rows or an empty-state line), and `settled-section`. The same ids serve GPUI simulated tests.
- Every creation path focuses the composer: the welcome buttons, the sidebar menu, Ctrl+N, and Ctrl+Shift+N.
- Use `wait_for` on a menu item before clicking it. With `headless:false`, the nested window must stay visible. A hidden window draws no frames, so a click on a newly opened menu lands on the previous frame and misses.
