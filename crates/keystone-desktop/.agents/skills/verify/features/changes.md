# Changes

Stage and commit the selected session's repository from a Changes view, with the user's own `git`. There is no diff view.

## Sub-features

- `changes-surface` is a layout component like Files: at most one per project layout, beside a conversation, as a tab with Files, or a fitted tab in a narrow window. Its tab reads "Changes · N" once N files are changed.
- `changes-root` follows the selected session's recorded folder (a worktree session's worktree), else the current project. git runs in that folder's top level. A folder outside any repository shows git's own message.
- `changes-list` groups Conflicts, Staged, Changes and Untracked with counts. A file with staged and unstaged edits is listed in both. Rows show git's one-letter state (`?` untracked, `!` conflicted), the name and its folder.
- `changes-stage` stages (`git add --all`) or unstages (`git restore --staged`, or `git rm --cached` before the first commit) one row, or a whole group from its header. Staging a conflicted row marks it resolved, and is refused while the file still has `<<<<<<<` or `>>>>>>>` lines; Stage all on a header never touches conflicts.
- `changes-open` opens a row in `$VISUAL`, `$EDITOR` or `xdg-open`, as Files does. `changes-copy-path` copies its absolute path.
- `changes-commit` runs `git commit --file=-` with the user's hooks and signing. Success shows git's summary line and clears the message. Failure shows git's and the hooks' output and keeps the message.
- `changes-draft` saves the unfinished message per folder 250 ms after typing stops, and on Quit or window close, and restores it after a restart.
- `changes-refresh` re-reads status when the view opens or its folder changes, when the window regains focus, after an agent's file-change item or turn ends in that folder, after each stage or commit, and from the refresh button. Reads use `--no-optional-locks`.

## How to get to it (user POV)

- Press Ctrl+Shift+G, or choose **Toggle changes** in the command palette. With no Changes view it opens one as a tab with Files when Files is open, else 300 px wide right of the conversation, and focuses the message box. With one unfocused or behind another tab, it brings it forward. With focus inside it, it closes it.
- Click **+** on a tab strip and choose **Changes**. A fitted window draws no **+**.
- Click a row's + or −, or highlight a row and press Space. Enter or a click opens the file. Ctrl+Shift+C copies the highlighted row's path.
- Type a message and press Ctrl+Enter anywhere in the view, or click **Commit N files**. Ctrl+Enter in Changes never sends the composer.

## Driving it with keystone-verify

Preconditions: a disposable git repository as `KEYSTONE_PROJECT_DIR` with local `user.name`, `user.email`, `commit.gpgsign=false` and `core.hooksPath=.git/hooks`, so the run never uses the host's hooks or signing. Use the scripted Claude through a wrapper that sets a scratch `CLAUDE_CONFIG_DIR`.

Named elements: `changes-message`, `changes-commit`, `changes-branch`, `changes-sync`, `changes-refresh`, `changes-notice`, `changes-notice-dismiss`, `changes-output`, `changes-empty`, `changes-loading`, `changes-unavailable`, `changes-list-row-N`, `changes-all-<group>` and `changes-stage-<group>-<path>` with group `staged`, `unstaged`, `untracked` or `conflicted`. The set of `changes-stage-*` names is the drawn list, so compare it with `git status --porcelain`.

The journey that proves this feature: create a session, open Changes from **+** (1280) or Ctrl+Shift+G (720) and from the palette, compare the list with `git status --porcelain`, stage one file, commit "verify commit" and check `git log -1`, install a failing pre-commit hook and check its output and the kept message, read `commit_drafts` with `workspace`, then Quit and relaunch and check the restored view and draft.
