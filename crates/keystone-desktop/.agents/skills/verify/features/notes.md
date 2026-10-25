# Notes

One note per project, written in a Notes surface of the layout. Notes are Keystone data kept in its database, never a file in the project's folder.

## Sub-features

- `notes-store` saves each project's note in the `notes` projection (schema 6, after `deliveries` in schema 5), keyed by the project's canonical folder, with a `note.upserted` event per change. Emptying a note removes its row with `note.removed`. The driver's `workspace` lists them under `workspace.notes` as `{"project": ..., "text": ...}`.
- `notes-surface` is a layout component like Files: at most one per project, beside a conversation or as a tab with one. Its strip is `notes-tab-0` when alone. The editor is `notes-editor`, the composer's multiline text input filling the pane; the footer shows the project's name and `notes-send`.
- `notes-autosave` saves 250 ms after typing stops, the composer's draft timer. Quit and closing a window save whatever the timer has not.
- `notes-project` shows the window's project's note. Switching project (Ctrl+O) shows the other project's note; typing not yet saved still saves to the project it was typed for. Two windows on one project show the same text.
- `notes-send` ("Add to draft", or Ctrl+Enter in the editor) puts the selection, or the whole note when nothing is selected, into the composer at its cursor, saves the draft and focuses the composer. It never sends. Without a selected session the button is disabled and Ctrl+Enter does nothing.

## How to get to it (user POV)

- Press Ctrl+Shift+J, or choose **Toggle notes** in the command palette (Ctrl+K). With no Notes view it opens one 320 px wide to the right of the conversation you last worked in and focuses the editor. With one behind another tab or unfocused, it brings it forward. With the editor focused, it closes Notes.
- Click **+** on a tab strip (for example above Files, or with session tabs) and choose **Notes**.

## Driving it with keystone-verify

- `start` with the scripted providers and `KEYSTONE_PROJECT_DIR`. Create a Claude session (Ctrl+N).
- Ctrl+Shift+J. `wait_for` `notes-editor`; require it beside `conversation`. `type` two lines (Enter between them). Wait 1 s, read `workspace` and require `notes` to hold the project's text with a literal newline.
- Ctrl+Q, `launch`. Require `notes-editor` back from the saved layout with the same text; click it and append text to prove a usable editor.
- Select part of the note (Shift+Left) and press Ctrl+Enter. Require the session's `draft` to equal the selection and the scripted Claude's `.fake-claude-messages` to stay empty. `click` `notes-send` with no selection and require the whole note inserted.
- Open a second project with Ctrl+O and a typed path. Require an empty note there; type, and require both projects' notes in `workspace`.
- Ctrl+Shift+E, `click` `surface-add`, `click` `surface-add-notes`; require Notes as a tab with Files.

## Not covered

- Markdown preview, sharing and notes as repository files are not in this unit.
- Customize has no "Add notes" button; Notes opens from the palette, the shortcut and the + menu.
