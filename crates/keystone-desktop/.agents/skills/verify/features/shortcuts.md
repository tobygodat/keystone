# Shortcuts

Settings → Shortcuts rebinds any command-palette command. A rebinding is saved in the workspace preferences as `shortcuts` (`{"new-claude": "secondary-g"}`), binds at once, and comes back after a relaunch. Every hint for the command, in the palette, menus and tooltips, shows the new keys.

## Sub-features

- `shortcuts-record`: a click on a row's shortcut (`settings-shortcut-<id>`), or Enter or Space on it, shows "Press a shortcut…" and takes the next keystroke without running any command. Escape cancels. A key equal to the default removes the override.
- `shortcuts-refuse`: a command that runs while a text field has focus (the composer, search, a dialog's field such as Rename, the palette's query, or the Files editor, where **Save file** runs) refuses a key the field edits with: Select all, Cut, Copy, Paste, Undo and Redo, Enter, Shift+Enter and Ctrl+Enter, the arrows, Home, End and Page keys alone or with Shift, Alt, Ctrl or Cmd, Backspace and Delete, and Tab and Shift+Tab, which indent in the Files editor. Each is refused as `secondary-` (Ctrl on Linux, ⌘ on macOS) and as `cmd-`, which the field also binds, and arrows with Alt too, from `keystone_ui::text_input::editing_keys`, so a saved key means the same on macOS. A command that runs in the composer also refuses a key that types text, meaning one with neither Ctrl nor Cmd that isn't F1 to F35. No command takes a key that types a character: a letter, digit, punctuation mark or Space, alone or with Shift or Alt ("Text fields type that key, so it can't run this command."). That covers the transcript's commands too, because their context still holds in a text field inside the transcript, such as an agent widget's Summary, and a typed character reaches a field only after every binding. The row's `settings-shortcut-<id>-warning` says why ("Text fields use that key to select all, so it can't run this command."), and nothing is saved. The transcript's commands may take the field's editing keys, such as Up, Home or Ctrl+A: in a widget's field the field binds those under `TextInput`, deeper than `Conversation`, so the field's own key wins.
- `shortcuts-conflict`: a key the user bound that another binding also runs on the composer, transcript, sidebar or a dialog, or in a panel that binds the same command, shows "Also bound to …" under the row. Saving is not blocked.
- `shortcuts-unbound`: a palette-only command, such as Next model, shows "Not set" and can be given keys. Its reset removes them again.
- `shortcuts-reset`: `settings-shortcut-<id>-reset` restores one command's default, and `settings-shortcuts-reset-all` restores every default. Focus moves to a shortcut button.
- `shortcuts-hints`: the palette, the new-session menu, the welcome pills and tooltips read the current binding. View diff in the Changes list takes the keys of "Transcript: view the focused turn's or edit's changes", so its tooltip and the transcript's agree.
- `shortcuts-bad-config`: a saved entry naming no command, holding something other than text, holding keys that don't parse or name no key (such as `"secondary-"` or `""`), or holding a key the previous rule refuses is skipped, and the command keeps its default. Each one is printed to stderr as `Skipped a saved shortcut. …` and listed in `settings-shortcuts-skipped`. The app still starts and the other entries bind.

## How to get to it (user POV)

- Ctrl+Comma (⌘, on macOS), then Shortcuts. Click a shortcut and press the new keys.
- The reset button beside a changed shortcut, or Reset all.

## Driving it

- On macOS, `python3 scripts/macos-shortcuts.py` drives the whole journey: rebinding with the mouse, the new and old keys, the palette and menu hints, quit and relaunch, a refused key, reset, and a workspace seeded with bad entries.
- With keystone-verify: open Settings, `click` `settings-section-shortcuts` and then `settings-shortcut-new-claude`, and press Ctrl+G. Read `workspace` until `preferences.shortcuts` is `{"new-claude":"secondary-g"}`. Press Escape, require Ctrl+N to add no session and Ctrl+G to add a Claude one. Quit with Ctrl+Q, `launch`, and press Ctrl+G again.

## Gotchas

- A failed save shows "Not saved." in `settings-error`, pinned below the scrolled section, so it is in view from any row.
- Chords aren't recorded. A saved chord isn't one keystroke, so it's skipped.
- Each command keeps the context it runs in. There is no `when` editor.
- Keys that belong to one panel and aren't palette commands stay fixed: the diff viewer's N, P, C, Enter and arrows, the Files tree's and the Changes list's Space and copy keys. Their tooltips show the fixed key. The Files editor's Save is a palette command, **Save file**, and its tooltip follows its binding.
- An app-wide command outranks Save file in the editor, because GPUI ranks a `!Dialog` context at the focused element's depth. Moving Save file onto such a command's key, such as Capture screenshot's Ctrl+Alt+S, names the clash, and that command wins.

On Linux, `python3 scripts/shortcuts-audit-linux.py` screenshots every screenful of Settings → Shortcuts in dark and light, in the default state, after rebinding New Claude to Ctrl+G, Split terminal to Ctrl+K and Quit to Ctrl+P, and after resetting each, and checks that only Split terminal names a clash (Open palette). It also screenshots the palette's hints outside the terminal and inside a pane. Read the screenshots for the hints.
