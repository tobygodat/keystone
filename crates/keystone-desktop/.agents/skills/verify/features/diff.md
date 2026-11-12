# Diff viewer (TOB-305)

A read-only diff shown in place of a conversation view: what one turn changed, one agent edit (TOB-310), or the uncommitted changes of the session's repository. Lines are selected as in a Files view, comments are left on them, and the comments go to the session as one message. The composer and its draft stay as they were.

## Entry points

- The "N files" button under a turn's last reply (`view-changes-<row>`, inside `checkpoint-files-<row>`), shown when the turn changed files.
- Ctrl+Shift+D with the transcript on that reply, or the palette's "Transcript: view the focused turn's or edit's changes".
- On an edit's row, Ctrl+Shift+D, or an open edit's Open full edit (`edit-open-<row>`, also reached by Tab from the row's toggle), shows that edit alone: "Changes in this edit", read only, with no Comment button, and "This edit is too long to keep whole. The rest isn't shown." at the end of a cut edit.
- The Changes header's View uncommitted changes button (`changes-diff`), the highlighted row's View diff button (`changes-diff-file`) and Ctrl+Shift+D in the Changes list, which open at that file.
- Ctrl+Alt+U, or the palette's "View uncommitted changes", for the selected session's folder.

## Expected behaviour

- A turn's diff runs from the tree the turn started from to its checkpoint, so edits made after the turn are not in it. Each checkpoint records its start (`Start:` in its message) and keeps it from gc with a parent commit. A checkpoint saved before starts were kept says "This turn's starting point wasn't saved, so its changes can't be shown."
- The uncommitted diff compares the working tree, untracked files included and ignored files left out, with HEAD, or with nothing in a repository without commits. It never touches the index, HEAD or stash.
- The patch is read and laid out on a background thread and stops after 200,000 lines with "This diff is too long. The rest isn't shown." Only the rows in view are drawn, and a line shows its first 2,000 characters.
- Each file has a header with its path, Added, Deleted or Renamed, and its added and removed counts. Binary files say so. Removed lines show the old number and added lines the new one.
- Keys in the diff (`DiffView`): Up and Down move between lines, Shift extends within one file, N and P move to the next and previous file, C or Enter opens the comment box for the selected lines, Ctrl+Enter sends, Escape returns to the conversation. In the comment box (`DiffComment`), Enter adds the comment, Escape drops it and Ctrl+Enter adds it and sends. The header's arrows (`diff-previous-file`, `diff-next-file`) and Back (`diff-back`), and the footer's Comment (`diff-comment`), Add comment (`diff-comment-save`) and Send (`diff-send`) do the same with the mouse. A click selects a line, Shift+click or a drag extends.
- Saved comments show under their last line (`diff-comment-<n>`) with a delete button. Send sends "Comments on your changes:" (or "Comments on the uncommitted changes:") with each comment's path, lines and the quoted diff lines in a fence, then says "Sent N comments." and clears them. A message the session can't take now keeps them and says "Not sent." The message goes as its own; the composer and its saved draft stay as they were. Closing a diff with comments not yet sent keeps it, and opening the same diff again brings them back.

## Native journey

On macOS, `python3 scripts/macos-diff.py` runs the whole journey on the automation inbox: a fixture repository with a 120,000-line file, one scripted turn that edits three files, the turn's diff opened by mouse and by Ctrl+Shift+D, comments by keyboard and by mouse, both sends checked in the workspace database, and the uncommitted diff from the Changes button. Screenshots and `report.json` go to `.audit/macos-diff-*/`. On Linux, drive the same steps through the verify skill with the scripted Claude in store mode.
