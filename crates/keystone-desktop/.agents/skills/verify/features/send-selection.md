# Send to the agent

Text selected in a Files editor or in a reply goes into the composer of the session the window shows, with the file's path and line numbers. It never sends.

## Sub-features

- `files-view` is the Files editor (`file-editor`), opened by Space on a highlighted file row or its pencil button (`view-file`). See [Files](files.md) for editing and saving. Select text with the mouse or the keyboard. The excerpt is every line the selection touches, whole, and a selection that ends at the start of a line leaves that line out. Lines send only while they match the file on disk: never while the file has unsaved edits, while the conflict bar shows, or after an outside change the editor has not taken yet. Then the button is disabled and the shortcut, menu and palette send nothing. **Open in editor** (`file-view-open`) runs the user's editor as `files-open` does. Click and Enter on a file row still open the user's editor.
- `selection-prompt` is what goes in: `<path> lines A-B:` (or `line A`) and the lines in a code fence longer than any backtick run in them; the path is relative to the Files root. A reply's selection goes in as a `> ` quote. Text over 64 KiB keeps its first and last 32 KiB with `… N bytes omitted …` between, as tool output does. It goes on lines of its own after the composer's cursor, the draft is saved and the composer focused.
- `selection-entry` reaches it four ways: the right-click menu's **Send to Claude Code** (`menu-send-selection`; Codex sessions say Codex), the palette's **Send selection to agent**, Ctrl+Shift+A (Cmd+Shift+A on macOS), and the view's **Send to Claude Code** button (`file-view-send`). The palette and the menu keep the selection they opened over, since a reply's selection clears when focus leaves it, and send it only while the window still shows the session it was taken for. A reply's menu offers only that reply's selection. Escape closes an open menu before anything else.
- `selection-target` is the session the window shows. With none, the button is hidden and the menu, palette row and shortcut do nothing. During a running turn the composer's usual steer rules apply to the prefilled text.

## How to get to it (user POV)

- Ctrl+Shift+E, highlight a file and press Space or click its pencil. Select lines, then right-click in the editor, press Ctrl+Shift+A, choose **Send selection to agent** in the palette, or click **Send to Claude Code**.
- Drag over a reply's text (or Ctrl+Shift+T, Tab to the reply, Ctrl+A), then the same right-click, shortcut or palette.

## Driving it with keystone-verify

`python3 scripts/send-selection-journey.py . OUT DRIVER` runs the whole journey with the scripted Claude and records it: lines 13-20 of a Rust file by the menu, a reply's first line by the palette, and a 135 KiB file by Space, Ctrl+A and Ctrl+Shift+A. Lines are selected with Ctrl+Home, Down and Shift+Down in the editor. Each draft is checked in `workspace`, `.fake-claude-messages` must stay unchanged until Send and then hold the draft exactly, and the dark theme is shot at the end, with Escape closing the menu and the button prefilling while a turn runs.

## Not covered

- Changes has no diff to select from yet. The diff viewer is in scope (docs/adr/0010-terminal-browser-scheduling-editor-diffs-remote-devices.md) but not built (TOB-305).
- Selections in tool output, user messages and Notes (Notes has its own Add to draft).
