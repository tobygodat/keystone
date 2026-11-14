# Search and sidebar navigation

Find local sessions by title, provider, account or worktree branch, and by the text of their messages, and recover the full list after clearing the search. Find inside one session is [find](find.md).

## Sub-features

- `search-focus` reaches search through the field or Ctrl+Shift+F (Ctrl+F is find in a session).
- `search-match` filters the current project's sessions in every sidebar section (Needs action, Working and Settled) by a case-insensitive match on title, provider, account label or worktree branch, without changing stored records.
- `search-messages` lists sessions whose saved messages hold the query under the title matches, under **In messages**: the title, then one muted line with the match marked. Each session shows its newest matching message; newest first. Settled sessions and sessions on other machines are included. Up and Down in the field move a ringed highlight over these rows; Enter or a click opens the session at the message with the find bar on the same words. A reply matches by the text it draws, not its Markdown source, so a link's address finds nothing. While a new query's scan runs, the older results fade and open nothing; Enter pressed then opens the new query's first result once it lands, unless another key, a click or focus leaving the field came first. A delivered result matches by the reply it draws, not its wrapper.
- `search-empty` displays an empty result state.
- `provider-filter` combines the provider choice with search over the window's project's sessions.
- `sidebar-toggle` hides and restores the session sidebar.

## How to get to it (user POV)

- Click **Search** in the sidebar, under the project switcher. Ctrl+Shift+F also opens the sidebar and focuses search. The top bar's panel button (`sidebar-toggle`) or Ctrl+B hides and shows the sidebar.
- Type a provider or title. Select all and delete to clear.
- Use the filter and pencil beside Search. Filter offers All sessions, Claude Code, and Codex. The pencil opens New session. Projects are in [projects](projects.md).
- Click the panel icon at the left of the main column's top bar to hide or restore the sidebar.

## Driving it with keystone-verify

Preconditions:

- At least one Claude session and one Codex session.
- Open the Settled section if a settled session should match.

- Call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","shift","f"]},{"action":"type","text":"Codex"}]}`. Capture and require only Codex rows. Read `workspace` and require both providers still exist.
- Send Ctrl+A while search has focus, then type `no-such-session`. Require **No matches** under **Working**.
- Send Ctrl+A then BackSpace through `keypress`. Capture and require the full list to return.
- `click` `{"id":"sidebar-toggle"}`. Call `wait_for` with `{"id":"search-field","present":false}`. Capture and require the whole left column to disappear, the main column to fill the window, and the composer to remain usable. Press Ctrl+Shift+F and require the sidebar and focused search to return.
- Type a word only a message holds. Require `message-results` and a `message-result-<session>` row whose second line marks the word. Press Down and require a ring on the first result, then Enter: require that session selected, `find-bar` open with the same words, the count `1 of N` and the message on screen. Click another `message-result-<session>` and require the same.
- `click` `{"id":"search-field"}`. Type `Claude` and require the Claude result. Clear it afterward. A click on the search icon focuses the field too; the icon has no name, so click it by coordinates from a fresh screenshot.

## Gotchas

- A collapsed Settled section hides matching settled rows. Its header count still includes them.
- Search text is view state and is not persisted in the workspace JSON.
- Message results read the saved log, so a reply still streaming is found once it is saved.
- Ctrl+A edits the currently focused field. Confirm focus before sending it.

## Project transitions

Switch projects as [projects](projects.md) describes. The draft is saved first. Cancel leaves the current presentation intact. A canonical alias of the same project changes nothing.

Apply distinct layouts in A and B. Stage another preview and type a distinct Customize prompt in each. Read earlier transcript rows in each, then return to A. Require the preview, prompt, Undo, and scroll to return independently. Changes that arrive while a project is parked must still appear on return. Widen after a narrow visit and require geometry to fit the current viewport.

A project switch must save the current draft first. Use private filesystem faults to prove a failed barrier leaves the source presentation and draft intact. Visiting a project must not create a saved layout entry. Explicit saved widths such as 302 px remain unchanged; 264 px is the new default.

Choose `filter`, then `filter-codex` or `filter-claude`. Require only the project's matching sessions, combined with search. Choose `filter-all` to restore them. An open filter menu blocks focused-latest read acknowledgement.
