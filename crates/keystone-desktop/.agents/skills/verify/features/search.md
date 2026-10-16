# Search and sidebar navigation

Find local sessions by title or provider and recover the full list after clearing the search.

## Sub-features

- `search-focus` reaches search through the field or Ctrl+F.
- `search-match` filters both sidebar sections by title or provider without changing stored records.
- `search-empty` displays an empty result state.
- `sidebar-toggle` hides and restores the session sidebar.

## How to get to it (user POV)

- Click **Search** under the app bar. Ctrl+F also opens the sidebar and focuses search.
- Type a provider or title. Select all and delete to clear.
- Click the panel icon at the left of the main column's top bar to hide or restore the sidebar.

## Driving it with keystone-verify

Preconditions:

- At least one Claude session and one Codex session.
- Open the Settled section if a settled session should match.

- Call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","f"]},{"action":"type","text":"Codex"}]}`. Capture and require only Codex rows. Read `workspace` and require both providers still exist.
- Send Ctrl+A while search has focus, then type `no-such-session`. Require **No sessions match this search.** under **Active**.
- Send Ctrl+A then BackSpace through `keypress`. Capture and require the full list to return.
- `click` `{"id":"sidebar-toggle"}`. Call `wait_for` with `{"id":"search-field","present":false}`. Capture and require the whole left column to disappear, the main column to fill the window, and the composer to remain usable. Press Ctrl+F and require the sidebar and focused search to return.
- `click` `{"id":"search-field"}`. Type `Claude` and require the Claude result. Clear it afterward.

## Gotchas

- A collapsed Settled section hides matching settled rows. Its header count still includes them.
- Search text is view state and is not persisted in the workspace JSON.
- Ctrl+A edits the currently focused field. Confirm focus before sending it.
