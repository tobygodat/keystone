# File mentions

Mention a file of the session's folder in the composer with `@`, or from a Files row. The message goes as typed: Keystone never reads the file. Claude Code reads a mentioned file itself and Codex reads it with its own tools (`docs/research/file-mentions-probe.md`).

## Sub-features

- `mention-picker`: typing `@` at the start of a word opens a picker above the composer over the files of the selected session's recorded folder, without hidden files, anything `.gitignore` excludes, or names with a `"`. It opens only in the window whose composer is being typed in, so not for a draft shown again on returning to its session, and not over the palette or the project list. A session without a recorded folder (a legacy session) has no picker. The folder is listed on a worker, the first 200,000 files the walk finds, and listed again whenever the open picker redraws with a listing older than 15 s. Typing after `@` fuzzy-filters by relative path, ranked on a worker; the best 50 matches show, shallowest first for a bare `@`.
- `mention-keys`: Up and Down move the highlight while the caret stays in the draft. Enter (not Shift+Enter) or Tab chooses the highlighted row; while the rows shown were ranked for an earlier query, it waits for this query's and chooses its best match, unless the caret has left that `@word` by then. A click chooses the row clicked. Escape closes the picker until the draft is edited again.
- `mention-token`: choosing replaces the `@word` with `@relative/path` and a space, drawn in the accent colour. A path with whitespace is written `@"relative/path"`. Backspace right after a token removes it whole, and Delete right before one does too. Typing inside it or against its edges makes it plain text.
- `mention-restore`: a draft loaded from storage marks its `@path` words that name existing files as tokens again, without opening the picker. A draft shared with another window keeps its tokens.
- `files-mention`: a highlighted file row in Files whose path can be mentioned (the session has a recorded folder and the path has no `"`) shows an **@** button ("Mention in composer", Ctrl+Shift+M) that inserts the same token at the composer's caret (after the token the caret is in, if any) and focuses the composer.

## How to get to it (user POV)

- In a session's composer, type `@` at the start of a word, then part of a path.
- In Files, highlight a file and click its **@** button, or press Ctrl+Shift+M.

## Driving it with keystone-verify

Preconditions:

- Build a fixture with `python3 -I scripts/make-file-fixture.py /tmp/<run>/fixture 50000`. Create `/tmp/<run>/fixture/.fake-claude-messages` (empty) so the scripted Claude logs each message it reads there.
- Pass `{"env": {"KEYSTONE_PROJECT_DIR": "/tmp/<run>/fixture", "KEYSTONE_CLAUDE_PATH": "<target>/debug/keystone-fake-claude"}}` to `start` and create a Claude session.

Steps:

- `click` `composer-field` and type `@mod-03`. `wait_for` `mention-menu-list-row-0`. Require the rows to be files under `src/pkg-NN/mod-03/`, none under `build/`, with the matched characters highlighted. Screenshot at 1280 and 720 wide.
- Press Down, then Up. Require the highlight to move and the draft to stay `@mod-03`.
- Press Enter. Require the draft to read `@src/pkg-00/mod-03/file-0000.txt ` with the path in the accent colour, `mention-menu` gone, and the saved draft (`workspace`) the same.
- Press Backspace twice. Require the draft to be empty: the first removes the space and the second the whole token.
- Type `@mod-03`, press Escape. Require `mention-menu` gone; type a character and require it back.
- Choose a file again, type ` please`, press Ctrl+Enter. Require the last line of `.fake-claude-messages` to be the exact text with `@src/...`.
- Press Ctrl+Shift+E. Highlight `README.md` with Down. `click` `mention-file` (or press Ctrl+Shift+M). Require the draft to end with `@README.md ` and the composer focused.
- Quit with Ctrl+Q and relaunch. Require the draft back with its token in the accent colour; Backspace at its end removes it whole.

Named elements: `mention-menu`, `mention-menu-status`, `mention-menu-list-row-N`, `mention-file` (only on the highlighted file row), `composer-field`.

## Perf

`cargo test -p keystone-desktop --features ui-tests --locked perf_mention_rows -- --ignored --nocapture` types `@mod-03` in the 50,000-file fixture and prints the time to ranked rows and the longest frame. The test executor runs the workers' listing and ranking on the test thread, so only the frame bound (33 ms) speaks for the UI; `perf_index_and_rank_50k` prints the worker times.

## Not covered

- Mentioning folders. Files' **@** shows on file rows only and the picker lists files only.
- Inlining file contents, and sending text selected in a file.
