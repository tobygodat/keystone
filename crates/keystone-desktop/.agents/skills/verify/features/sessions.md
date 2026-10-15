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

## Driving it with native-driver

Preconditions:

- Doctor identifies the current owned window and binary.
- Start from an empty workspace for each welcome-provider test. Stop and start a new controller to reset state.

- Capture `{"kind":"call","tool":"screenshot","arguments":{}}` and inspect it. Click the observed center of **Start with Claude Code** using `input` with one `click` action, `button:"left"`, and the observed `x` and `y`. The welcome buttons disappear and **New Claude session** appears in the sidebar and tab.
- Send `{"kind":"workspace"}`. Require exactly one session with `provider:"Claude"`, `settled:false`, `draft:""`, and its ID selected.
- Type a short draft without an extra focus click. Require the composer and state file to contain it. This proves the welcome action focused the editor.
- For the keyboard entry, send `{"kind":"call","tool":"input","arguments":{"actions":[{"action":"keypress","keys":["ctrl","shift","n"]}]}}`. Require a second record with `provider:"Codex"` selected.
- For the menu entry, capture and click the sidebar plus. Capture the open menu, then click the observed **New Claude session** or **New Codex session** row. Require exactly one additional record of that provider.
- Capture and click the first session row. Require its title in the tab, its provider chip, its restored draft, and its ID in `workspace.selected`.
- Preserve before/after screenshots and a state snapshot. Repeat the welcome journey in a fresh run for Codex before marking both provider buttons verified.

## Gotchas

- Duplicate titles are allowed. Use state IDs and the screenshot row position to distinguish sessions.
- The welcome chooser appears only when no session is selected. It is absent after creation.
- GPUI IDs `welcome-claude`, `welcome-codex`, `new-claude`, and `session-1` are simulated-test selectors. They are not exported native selectors.
- A sidebar creation path does not promise composer focus. Use Ctrl+L before typing there.
