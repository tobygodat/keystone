# Local sessions

Create a local Claude or Codex session and select it from the sidebar. These records do not start an agent process.

## Sub-features

- `session-welcome` creates a session from the centered welcome composer.
- `session-menu` creates a session from the sidebar plus menu.
- `session-keyboard` creates a session with a shortcut.
- `session-select` switches between existing sessions.

## How to get to it (user POV)

- In an empty workspace, click **Start with Claude Code** or **Start with Codex**.
- Click the plus beside **Workspace**, then **New Claude session** or **New Codex session**.
- Press Ctrl+N for Claude or Ctrl+Shift+N for Codex.
- Click a session row in the sidebar to select it.

## Driving it with keystone-verify

Preconditions:

- Doctor identifies the current owned window and binary.
- Start from an empty workspace for each welcome-provider test. Call `stop` then `start` to reset state.

- Call `wait_for` with `{"id":"welcome-claude"}`, then `click` with `{"id":"welcome-claude"}`. Call `wait_for` with `{"id":"session-1"}`. Capture `screenshot` and require the welcome buttons gone and **New Claude session** in the sidebar and tab.
- Call `workspace`. Require exactly one session with `provider:"Claude"`, `settled:false`, `draft:""`, and its ID selected.
- Type a short draft without an extra focus click. Require the composer and state file to contain it. This proves the welcome action focused the editor.
- For the keyboard entry, call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","shift","n"]}]}`. Require a second record with `provider:"Codex"` selected. Type a short draft without a focus click and require it in the composer and state file.
- For the menu entry, `click` `{"id":"new-session"}` and `wait_for` `{"id":"new-claude"}` or `{"id":"new-codex"}`. Capture the open menu, then `click` that id. Require exactly one additional record of that provider. Type a short draft without a focus click and require it in the composer and state file.
- `click` the first row by its `session-<id>` from `workspace`. Require its title in the tab, its provider chip, its restored draft, and its ID in `workspace.selected`.
- Preserve before/after screenshots and a state snapshot. Repeat the welcome journey in a fresh run for Codex before marking both provider buttons verified.

## Gotchas

- Duplicate titles are allowed. Use state IDs and the screenshot row position to distinguish sessions.
- The welcome chooser appears only when no session is selected. It is absent after creation.
- Named ids are `welcome-claude`, `welcome-codex`, `new-session`, `new-claude`, `new-codex`, `composer-field`, and `session-<id>`. The same ids serve GPUI simulated tests.
- Every creation path focuses the composer: the welcome buttons, the sidebar menu, Ctrl+N, and Ctrl+Shift+N.
- Use `wait_for` on a menu item before clicking it. With `headless:false`, the nested window must stay visible. A hidden window draws no frames, so a click on a newly opened menu lands on the previous frame and misses.
