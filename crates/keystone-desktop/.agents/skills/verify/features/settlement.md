# Settle sessions

Mark a session settled, find settled sessions, and return a session to draft status.

## Sub-features

- `settle-toggle` changes the selected session's settled state.
- `settle-filter` shows settled sessions through the sidebar filter or inbox rail icon.

## How to get to it (user POV)

- Click **Settle** in the composer header or press Ctrl+Shift+S.
- Click **Un-settle** to reverse the state, or press the same shortcut.
- Click the funnel beside search, or the inbox icon on the left rail.

## Driving it with keystone-verify

Preconditions:

- Two local sessions. Keep one unsettled and select the other.
- Clear search so it cannot hide the expected row.

- `click` `{"id":"settle"}`. Capture `screenshot` and require **Un-settle**, a **Settled** row label, and `settled:true` for the selected record in `workspace`.
- `click` `{"id":"filter"}`. Capture and require only the settled row to remain. The state file must still contain both sessions.
- `click` `{"id":"filter"}` again. Require both rows to return.
- Call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","shift","s"]}]}`. Require `settled:false`, **Settle**, and the **Draft** row label.
- Test the inbox entry separately with `click` `{"id":"rail-settled"}` and confirm the settled filter in a screenshot. Preserve screenshots and a state snapshot for each exercised entry point.

## Gotchas

- The inbox icon sets the settled filter. It does not display an approval inbox in this preview.
- Filtering hides rows without deleting records.
- A selected session can remain in the composer while its row is hidden by the filter.
