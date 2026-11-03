# Send to the agent

Text selected in a Files view or in a reply goes into the composer of the session the window shows, with the file's path and line numbers. It never sends.

## Sub-features

- `files-view` shows a file read-only in the Files view's place: Space on a highlighted file row, or its eye button (`view-file`). Lines are numbered and selected whole (`file-line-N`, 0-based): click, Shift+click, drag, Up/Down, Shift+Up/Down and Ctrl+A. Ctrl+C copies the lines in full. A line over 2,000 characters is drawn cut short with `…` but sent whole. When the file changed since it was shown, the next click, key, shortcut or Send in the view (except Ctrl+C, which copies the lines as shown) reads it again and clears the selection instead of sending, and the palette sends nothing. **Back** (`file-view-back`) or Escape returns to the tree. **Open in editor** (`file-view-open`) runs the editor as `files-open` does. Files over 8 MiB say "Too large to show."; binary files, folders, pipes and devices "Not a text file." (`file-view-unreadable`). Click and Enter on a file row still open the editor.
- `selection-prompt` is what goes in: `<path> lines A-B:` (or `line A`) and the lines in a code fence longer than any backtick run in them; the path is relative to the Files root. A reply's selection goes in as a `> ` quote. Text over 64 KiB keeps its first and last 32 KiB with `… N bytes omitted …` between, as tool output does. It goes on lines of its own after the composer's cursor, the draft is saved and the composer focused.
- `selection-entry` reaches it four ways: the right-click menu's **Send to Claude Code** (`menu-send-selection`; Codex sessions say Codex), the palette's **Send selection to agent**, Ctrl+Shift+A (Cmd+Shift+A on macOS), and the view's **Send to Claude Code** button (`file-view-send`). The palette and the menu keep the selection they opened over, since a reply's selection clears when focus leaves it, and send it only while the window still shows the session it was taken for. A reply's menu offers only that reply's selection. Escape closes an open menu before anything else.
- `selection-target` is the session the window shows. With none, the button is hidden and the menu, palette row and shortcut do nothing. During a running turn the composer's usual steer rules apply to the prefilled text.

## How to get to it (user POV)

- Ctrl+Shift+E, highlight a file and press Space or click its eye. Select lines, then right-click them, press Ctrl+Shift+A, choose **Send selection to agent** in the palette, or click **Send to Claude Code**.
- Drag over a reply's text (or Ctrl+Shift+T, Tab to the reply, Ctrl+A), then the same right-click, shortcut or palette.

## Driving it with keystone-verify

`python3 scripts/send-selection-journey.py . OUT DRIVER` runs the whole journey with the scripted Claude and records it: lines 13-20 of a Rust file by the menu, a reply's first line by the palette, and a 135 KiB file by Space, Ctrl+A and Ctrl+Shift+A. Line 11's number must not wrap. Each draft is checked in `workspace`, `.fake-claude-messages` must stay unchanged until Send and then hold the draft exactly, and the dark theme is shot at the end, with Escape closing the menu and the button prefilling while a turn runs.

## Not covered

- Changes has no diff to select from yet. The diff viewer is in scope (docs/adr/0010-terminal-browser-scheduling-editor-diffs-remote-devices.md) but not built (TOB-305).
- Selections in tool output, user messages and Notes (Notes has its own Add to draft).
- A drag past the view's edge does not scroll it.
