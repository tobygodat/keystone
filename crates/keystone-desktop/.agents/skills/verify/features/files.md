# Files

Browse the selected session's project folder in a Files view of the project layout and open a file in the user's own editor.

## Sub-features

- `files-surface` is a layout component like a conversation: at most one per project, placed beside a conversation or as a tab with one. Opening or closing it saves the project layout at once, outside Customize. Inside Customize it edits the draft instead.
- `files-strip` is the tab strip above a Files view, or above any tab group. Each tab has a close button on hover, and **+** opens the "Open a surface" menu listing what the layout still allows: **Files** when none is open, and **Conversation**. A lone conversation keeps its plain chrome and has no strip.
- `files-root` shows the selected session's recorded folder as the top row. A session without one falls back to the current project; with neither, the list explains why.
- `files-expand` lists one folder level at a time, folders first, without hidden files or anything the repository's `.gitignore` excludes. Expanding a folder again re-reads it.
- `files-open` opens a file with `$VISUAL`, then `$EDITOR`, then `xdg-open`. Terminal editors (vi, vim, nvim, nano, helix, `emacs -nw`, and others) are skipped because a detached editor has no terminal.
- `files-copy-path` copies a row's absolute path.
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

Named elements: `files-tab-0` and `files-tab-0-close` (a lone Files view), `surface-tabs-N-tab-M` and `surface-tabs-N-tab-M-close` (a tab group whose first view is N; the close shows on the selected tab or on hover), `surface-add`, `surface-add-files`, `surface-add-conversation`, `component-add-files` (Customize), `files-tree-row-N`, `files-tree-chevron-N`, `copy-path` (only on the highlighted row), `mention-file` (only on a highlighted file row), `files-notice`, `files-empty`.

## Perf

`cargo test -p keystone-desktop --features ui-tests --locked perf_files_first_rows -- --ignored --nocapture` builds the 50,000-file fixture and prints the time from the toggle, which also saves the layout, to the first real rows and the longest frame. Fail above 300 ms or 33 ms. Only one folder level is read, so the time does not grow with the fixture's total size.

## Not covered

- A copy click before any key press in the run copies nothing (KS-013). Press any key first and record it.
