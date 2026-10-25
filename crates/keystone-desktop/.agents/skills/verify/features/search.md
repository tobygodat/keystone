# Search and sidebar navigation

Find local sessions by title or provider and recover the full list after clearing the search.

## Sub-features

- `search-focus` reaches search through the field or Ctrl+F.
- `search-match` filters the current project's sessions in both sidebar sections by title or provider without changing stored records.
- `search-empty` displays an empty result state.
- `provider-filter` combines the provider choice with search over the window's project's sessions.
- `sidebar-toggle` hides and restores the session sidebar.

## How to get to it (user POV)

- Click **Search** under the app bar. Ctrl+F also opens the sidebar and focuses search.
- Type a provider or title. Select all and delete to clear.
- Use the filter and pencil beside Search. Filter offers All sessions, Claude Code, and Codex. The pencil opens New session. Projects are in [projects](projects.md).
- Click the panel icon at the left of the main column's top bar to hide or restore the sidebar.

## Driving it with keystone-verify

Preconditions:

- At least one Claude session and one Codex session.
- Open the Settled section if a settled session should match.

- Call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","f"]},{"action":"type","text":"Codex"}]}`. Capture and require only Codex rows. Read `workspace` and require both providers still exist.
- Send Ctrl+A while search has focus, then type `no-such-session`. Require **No matches** under **Working**.
- Send Ctrl+A then BackSpace through `keypress`. Capture and require the full list to return.
- `click` `{"id":"sidebar-toggle"}`. Call `wait_for` with `{"id":"search-field","present":false}`. Capture and require the whole left column to disappear, the main column to fill the window, and the composer to remain usable. Press Ctrl+F and require the sidebar and focused search to return.
- `click` `{"id":"search-field"}`. Type `Claude` and require the Claude result. Clear it afterward. A click on the search icon focuses the field too.

## Gotchas

- A collapsed Settled section hides matching settled rows. Its header count still includes them.
- Search text is view state and is not persisted in the workspace JSON.
- Ctrl+A edits the currently focused field. Confirm focus before sending it.

## Project transitions

Switch projects as [projects](projects.md) describes. The draft is saved first. Cancel leaves the current presentation intact. A canonical alias of the same project changes nothing.

Apply distinct layouts in A and B. Stage another preview and type a distinct Customize prompt in each. Read earlier transcript rows in each, then return to A. Require the preview, prompt, Undo, and scroll to return independently. Changes that arrive while a project is parked must still appear on return. Widen after a narrow visit and require geometry to fit the current viewport.

A project switch must save the current draft first. Use private filesystem faults to prove a failed barrier leaves the source presentation and draft intact. Visiting a project must not create a saved layout entry. Explicit saved widths such as 302 px remain unchanged; 264 px is the new default.

Choose `filter`, then `filter-codex` or `filter-claude`. Require only the project's matching sessions, combined with search. Choose `filter-all` to restore them. An open filter menu blocks focused-latest read acknowledgement.
