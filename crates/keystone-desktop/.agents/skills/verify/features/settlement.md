# Settle sessions

Move a session out of active work, find it in the Settled section, and return it to active work.

## Sub-features

- `settle-row` settles or un-settles one session from its sidebar row without changing the selection.
- `settle-shortcut` toggles the selected session with Ctrl+Shift+S.
- `settled-section` shows settled sessions in their own sidebar section. It starts collapsed.

## How to get to it (user POV)

- Point at a session row and click **Settle**. The selected row always shows the button. A settled row shows **Un-settle**.
- Press Ctrl+Shift+S to toggle the selected session.
- Click the **Settled (n)** header at the bottom of the sidebar to open or close the section.

## Driving it with keystone-verify

Preconditions:

- Two local sessions, both active. Select the second.
- Clear search so it cannot hide the expected row.

- Move the pointer over the first row with `input` `{"actions":[{"action":"move","x":..,"y":..}]}` at the `session-1` center from `elements`. Require `settle-1` to appear in `elements`.
- `click` `{"id":"settle-1"}`. Require `settled:true` for session 1 and the second session still in `workspace.selected`. Capture and require session 1 gone from **Working** and a collapsed **Settled (1)** header at the bottom.
- `click` `{"id":"settled-section"}`. Capture and require the settled row below the header, with a filled wedge, a muted title, and **Un-settle** on hover.
- Hover the settled row and `click` `{"id":"settle-1"}`. Require `settled:false` and the row back under **Working**.
- Select a row, then call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","shift","s"]}]}`. Require `settled:true` for the selected record and **Settled** after its title in the top bar. Press it again to return it.
- Preserve screenshots and a state snapshot for each exercised entry point.

## Gotchas

- A row's Settle button exists only while that row is hovered or selected. `elements` lists it only when it is drawn, so hover before clicking.
- Settling the selected session keeps it selected. With the section collapsed, its row is hidden while its conversation stays open.
- The section header counts the settled sessions that match the current search.
- Settling a session waiting for approval is refused from every entry point (row button, Ctrl+Shift+S, and the palette), including a request that arrived while the session's history was still loading. The session stays active and the reason shows under the composer. Un-settling is always allowed.
- A settled session stays in Settled even when it needs action, such as a failed turn. Settling is how the user puts a session away.
