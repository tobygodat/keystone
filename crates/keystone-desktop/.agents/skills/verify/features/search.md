# Search and sidebar navigation

Find local sessions by title or provider and recover the full list after clearing the search.

## Sub-features

- `search-focus` reaches search through the field, rail icon, or Ctrl+F.
- `search-match` filters sessions by text without changing stored records.
- `search-empty` displays an empty result state.
- `sidebar-toggle` hides and restores the session sidebar.

## How to get to it (user POV)

- Click **Search conversations**, or the rail search icon. Ctrl+F also opens and focuses search.
- Type a provider or title. Select all and delete to clear.
- Click the sidebar icon at the top of the left rail to hide or restore the sidebar.

## Driving it with keystone-verify

Preconditions:

- At least one Claude session and one Codex session.
- Settled-only filtering is off.

- Call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","f"]},{"action":"type","text":"Codex"}]}`. Capture and require only Codex rows. Read `workspace` and require both providers still exist.
- Send Ctrl+A while search has focus, then type `no-such-session`. Require **No sessions match these filters**.
- Send Ctrl+A then BackSpace through `keypress`. Capture and require the full list to return.
- `click` `{"id":"rail-sidebar"}`. Call `wait_for` with `{"id":"search-field","present":false}`. Capture and require the session list to disappear and the composer to remain usable. Press Ctrl+F and require the sidebar and focused search to return.
- Test each mouse search entry separately: `click` `{"id":"search-field"}` and `click` `{"id":"rail-search"}`. Type `Claude` and require the Claude result. Clear after each route.

## Gotchas

- Search and the settled filter combine. An unexpected empty list can come from either.
- Search text is view state and is not persisted in the workspace JSON.
- Ctrl+A edits the currently focused field. Confirm focus before sending it.
