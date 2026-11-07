# Files

Browse the selected session's project folder in a Files view of the project layout, edit a file in place, and open a file in the user's own editor.

## Sub-features

- `files-surface` is a layout component like a conversation: at most one per project, placed beside a conversation or as a tab with one. Opening or closing it saves the project layout at once, outside Customize. Inside Customize it edits the draft instead.
- `files-strip` is the tab strip above a Files view, or above any tab group. The selected tab shows its close button unless it shows a status dot, and other tabs show theirs on hover. **+** opens the "Open a surface" menu listing what the layout still allows: **Files**, **Notes**, **Changes** and **Pull request**, each only while the layout has none, and **Conversation**. A lone conversation keeps its plain chrome and has no strip.
- `files-root` shows the selected session's recorded folder as the top row. A session without one falls back to the current project; with neither, the list explains why.
- `files-expand` lists one folder level at a time, folders first, without hidden files or anything the repository's `.gitignore` excludes. Expanding a folder again re-reads it.
- `files-open` opens a file with `$VISUAL`, then `$EDITOR`, then `xdg-open` (`open` on macOS). Terminal editors (vi, vim, nvim, nano, helix, `emacs -nw`, and others) are skipped because a detached editor has no terminal.
- `files-copy-path` copies a row's absolute path.
- `files-view` opens a file in the built-in editor in the tree's place, by Space or the pencil button (`view-file`, tooltip "Edit"). The editor is `file-editor`, with line numbers, syntax highlighting and undo. Typing shows **Edited** (`file-editor-edited`) in the header. Ctrl+S (Cmd+S on macOS; **Save file** in the palette and Settings → Shortcuts, where it can be rebound) or **Save** (`file-editor-save`, whose tooltip names the bound key) writes the file, and only while it still holds what Keystone last read or wrote. A save refreshes Changes for the file's repository, so the edit shows there and in the uncommitted diff opened from it. **Save** is disabled when there is nothing to save. A failed write shows its reason in `file-view-notice` and keeps the text. The editor checks the file every second. An outside change to an unedited file replaces the text at once, with no prompt. An outside change to an edited file, or a deleted file, shows the conflict bar (`file-editor-conflict`: "This file changed on disk." or "This file was deleted on disk."). **Reload** (`file-editor-reload`, only for a changed text file) takes the disk's text, and **Overwrite** (`file-editor-overwrite`) writes the editor's. A change that keeps the size and modification time is caught at Save, which then writes nothing and shows the bar. **Back** (`file-view-back`) or Escape returns to the tree, and with unsaved edits first asks "Discard changes?" (`file-editor-discard`, `file-editor-discard-cancel`, `file-editor-discard-confirm`). Cancel keeps the edits and refocuses the editor. Quit and closing the window are refused while a Files editor holds unsaved edits, with "Not quit. Unsaved changes in NAME. Save or discard them first." for Quit and "Unsaved changes in NAME. Save or discard them first." for the window. No shortcut can take a key the editor types or edits with, Tab included. Selected lines go to the agent ([Send to the agent](send-selection.md)). Files over 8 MiB say "Too large to edit here."; binary files, folders, pipes and devices "Not a text file." (`file-view-unreadable`).
- `files-mention` puts an `@` mention of a file row's path in the composer ([File mentions](mentions.md)).
- `files-error` shows a red row at the top of the tree when the editor cannot start or a folder cannot be read. The app stays up.

## How to get to it (user POV)

- Press Ctrl+Shift+E, or choose **Toggle files** in the command palette (Ctrl+K). With no Files view it opens one 300 px wide to the right of the conversation you last worked in and focuses the tree. With one behind another tab or unfocused, it brings Files forward and focuses the tree. With the tree focused, it closes Files.
- Click **+** on a tab strip and choose **Files** to open it as a tab in that group. Click the × on the Files tab to close it.
- Open **Customize** and click **Add files** to place it in the draft, then Apply.
- In a window too narrow for the layout, Files becomes one of the fitted tabs beside the conversations.
- Up and Down move, Right expands or steps into a folder, Left collapses or steps out, Enter or a click opens a file or toggles a folder.
- Hover or highlight a row and click its copy icon, or press Ctrl+Shift+C.
- Highlight a file and click its **@** icon, or press Ctrl+Shift+M, to mention it in the composer.
- Highlight a file and press Space, or click its pencil icon, to edit it. Type, then press Ctrl+S or click **Save**. Press Escape or click **Back** to return to the tree.

## Driving it with keystone-verify

Preconditions:

- Build a fixture with `python3 -I scripts/make-file-fixture.py /tmp/<run>/fixture 50000`. It is a git repository with `README.md`, `docs/`, `src/pkg-NN/mod-NN/part-NN/` files, and a `build/` folder that `.gitignore` excludes.
- A fake editor that logs its argument and stays running:

  ```sh
  #!/bin/sh
  echo "$1" >> /tmp/<run>/editor.log
  exec sleep 600
  ```

- Pass `{"env": {"KEYSTONE_PROJECT_DIR": "/tmp/<run>/fixture", "VISUAL": "/tmp/<run>/editor.sh"}}` to `start`, and create a session so it records that folder.

- Press Ctrl+Shift+E. `wait_for` `files-tree-row-1`. Require `files-tab-0`, the session list still present, and the fixture name in row 0, then `docs`, `src`, `README.md`, and no `build`.
- Press Down to reach `src`. Right expands the highlighted folder, and Right again steps into its first child. Expand `src`, `pkg-00`, and `mod-00` this way, waiting for each folder's rows before the next Right. Require `mod-00`'s `part-NN` folders and files.
- Highlight `README.md` and press Enter. Require `editor.log` to end with its absolute path.
- Highlight a file and `click` `copy-path`, or press Ctrl+Shift+C. Paste into search and require the absolute path.
- Restart with `VISUAL=/nonexistent` and no `EDITOR`. Open a file. Require `files-notice` to start with `Couldn't run /nonexistent: No such file or directory` and the window to keep responding.
- Open a file, then press Ctrl+Q. Require the `sleep` started by the fake editor to still be running (`pgrep -f 'sleep 600'`).
- Create a second session in another folder and select it. Require that folder's name in row 0. Select the first session again and require its expanded folders back.
- Press Ctrl+Shift+E twice. Require `files-tree-row-1` gone after the first and the same expanded folders after the second.
- `click` `surface-add`, then `surface-add-conversation`. Require the Files tab and a new conversation tab in one strip (`surface-tabs-N-tab-0` and `-tab-1`, where N is the Files view id), the new conversation shown, and the main `conversation` untouched. `click` `surface-tabs-N-tab-1`, then `surface-tabs-N-tab-1-close`. Require `files-tab-0` back.
- Restart. Require the Files view back from the saved layout.
- Add a file under an expanded folder on disk. Collapse and expand the folder. Require the new file.
- Highlight `README.md` and `click` `view-file`. `wait_for` `file-editor`. Type a word. Require `file-editor-edited`, and the file on disk unchanged. Press Ctrl+S. Require the word in the file on disk and `file-editor-edited` gone. Type again and `click` `file-editor-save`. Require the same.
- With the file unedited, write new text to it from a shell. Within two seconds require the editor to show it and no `file-editor-conflict`.
- Type a word, then write other text to the file from a shell. Require `file-editor-conflict`. Press Ctrl+S and require the file on disk unchanged. `click` `file-editor-reload` and require the shell's text in the editor and the bar gone. Type again, change the file again, and `click` `file-editor-overwrite`. Require the editor's text on disk.
- Delete the file from a shell. Require `file-editor-conflict` without `file-editor-reload`. `click` `file-editor-overwrite` and require the file back.
- Type a word and press Ctrl+Q. Require Keystone still running with the unsaved-changes message in `shutdown-error`. Press Escape, require `file-editor-discard`, `click` `file-editor-discard-cancel`, and require the word still there. Press Escape and `click` `file-editor-discard-confirm`. Require the tree.

Named elements: `files-tab-0` and `files-tab-0-close` (a lone Files view), `surface-tabs-N-tab-M` and `surface-tabs-N-tab-M-close` (a tab group whose first view is N; the close shows on the selected tab or on hover), `surface-add`, `surface-add-files`, `surface-add-conversation`, `component-add-files` (Customize), `files-tree-row-N`, `files-tree-chevron-N`, `copy-path` (only on the highlighted row), `mention-file` (only on a highlighted file row inside the selected session's recorded folder, so never while the tree falls back to the project folder) and `view-file` (only on a highlighted file row), `files-notice`, `files-empty`, and in the editor `file-editor`, `file-view-name`, `file-view-back`, `file-editor-edited`, `file-editor-save`, `file-editor-conflict`, `file-editor-reload`, `file-editor-overwrite`, `file-editor-discard` (with `-cancel` and `-confirm`), `file-view-notice`, `file-view-unreadable`, `file-view-send` and `file-view-open`.

## Perf

`cargo test -p keystone-desktop --features ui-tests --locked perf_files_first_rows -- --ignored --nocapture` builds the 50,000-file fixture and prints the time from the toggle, which also saves the layout, to the first real rows and the longest frame. Fail above 300 ms or 33 ms. Only one folder level is read, so the time does not grow with the fixture's total size.

## Not covered

- A copy click before any key press in the run copies nothing (KS-013, now TOB-212). Press any key first and record it.
