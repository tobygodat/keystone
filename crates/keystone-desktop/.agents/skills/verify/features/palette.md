# Command palette

Open the palette, filter the app's commands by typing, and run one with Enter or a click. Each row shows the shortcut that runs the same command with the palette closed.

## Sub-features

- `palette-open` opens the palette with Ctrl+K or Ctrl+Shift+P. The same keys close it. A rename or full-access dialog keeps it closed.
- `palette-filter` keeps the commands whose label fuzzy-matches the query, best match first. Up and Down, or Tab and Shift+Tab, move the highlight.
- `palette-run` runs the highlighted command with Enter, or any row with a click. The palette closes first and focus returns to where it was, so the command acts as its shortcut would.
- `palette-dismiss` closes it with Escape or a press outside the card. Focus returns to where it was and nothing else changes.
- `palette-kbd` shows each row's shortcut. Rows and bindings come from one table in `src/commands.rs`, so a hint cannot drift from its binding.

The rows, in order: New Codex session, New Claude session, Toggle sidebar, Focus search, Focus composer, Rename session, Settle session, Open settings, Customize layout, Quit.

## How to get to it (user POV)

- Press Ctrl+K or Ctrl+Shift+P anywhere outside a dialog.
- Type part of a command name, such as "new c", and press Enter.
- Press Escape or click outside the card to close it without running anything.

## Driving it with keystone-verify

Preconditions:

- One local session, selected, unless the step says otherwise.
- No dialog open.

- Call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","k"]}]}`. `wait_for` `palette`. Require `palette-input` in `elements` and the input focused in a screenshot. Repeat with `["ctrl","shift","p"]`.
- Type "new c" with `input`, then press Enter (`{"action":"keypress","keys":["Return"]}`). Require `palette` gone, a new record with `provider:"Codex"` in `workspace`, and it selected.
- Open the palette, type "toggle sidebar", and press Enter. Capture and require the sidebar hidden. Press Ctrl+B and require it back.
- Open the palette, type "focus search", and press Enter. Type a word and require it in the sidebar search field.
- Type a draft in the composer, open the palette, and press Escape. Require `palette` gone, the composer focused, and the draft unchanged in `workspace`.
- Click a row with `click` `{"id":"palette-row-0"}`. Rows are numbered in display order, so read the screenshot first to know which command row 0 is.
- Press F2 to open the rename dialog, then Ctrl+K. Require `palette` absent.
- Press two rows' shortcuts with the palette closed, such as Ctrl+B and Ctrl+F. Require each to do what its row says.
- Type "quit" and press Enter. Capture before the window closes. Require `doctor` to report exit code 0.
- Preserve screenshots and a state snapshot for each exercised entry point.

## Gotchas

- The card is 560 px wide and narrows with the window, so it fits at 800 by 600.
- Ctrl+K inside the palette closes it. It does not reach the input.
- While the palette is open, the app's other shortcuts do nothing (Ctrl+N, Ctrl+Enter, Tab focus moves, and the Customize keys), so nothing changes behind it and focus cannot leave it. Ctrl+Q still quits.
- Escape with the palette open closes only the palette. Menus and the customize panel underneath stay as they were.
- Settle session refuses a session waiting for approval and shows the reason under the composer. See [Settlement](settlement.md).
- Open settings (`open-settings`, Ctrl+Comma) closes the palette and opens the settings page, or closes the page when it is open. See [Settings](settings.md).
