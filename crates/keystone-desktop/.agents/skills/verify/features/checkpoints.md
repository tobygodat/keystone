# Checkpoints and Restore to here (TOB-58)

In a git folder, each turn's end saves the session's files as a checkpoint, and Restore to here puts them back and continues the conversation from that turn in a fork. In a folder outside any repository, the latest turn's control says that checkpoints need one.

## Entry points

- The Restore to here button (`restore-<row>`, a counter-clockwise arrow) under each turn's last reply, with how many files that turn changed beside it (`checkpoint-files-<row>`) when it changed any.
- The command palette: "Transcript: restore to the focused turn", with the transcript focused on that reply.

## Expected behaviour

- As a turn ends, Keystone builds a tree of the working tree's tracked and untracked files without ignored ones, in an index file of its own in the repository's git folder (`GIT_INDEX_FILE`), and saves it as a commit named only by `refs/keystone/checkpoints/<store>/<session>/<turn>`. `<store>` is a random name the workspace keeps in its database, so two Keystone workspaces using one repository never share refs, and a saved ref is never replaced. Turns are numbered per session from 1. The user's index file, HEAD, branches and stash are never written. A worktree session checkpoints its own worktree; the refs are shared by the repository.
- Untracked nested repositories and untracked files over 20 MB stay out of checkpoints. The checkpoint records what it left out, and a restore leaves those paths as they are and lists them. In a sparse checkout, files outside the cone stay off disk.
- The file count compares the tree the turn started from, read before its first message went, with the one it ended with. A message sent during a turn that becomes the next turn waits until the ended turn's checkpoint is saved; that turn starts from it.
- Each session keeps its newest 50 checkpoints; older refs are deleted as each new one is saved. Debug builds take `KEYSTONE_TEST_CHECKPOINT_KEEP` for fewer.
- Restore to here asks first ("Restore to here": "Files go back to this turn, and the conversation continues in a fork.", Restore). Restore is refused with "A turn is running here. Stop it first." while any session's turn runs, a held follow-up waits, or a checkpoint is being saved in the same repository. A restore cannot be cancelled; a git command that never returns keeps the dialog open and messages to that repository refused until Keystone restarts. While it runs, a message to any session in that repository is not sent ("Not sent. Files are being restored."), and a launched session's result waits.
- A restore first saves the working tree as `<store>/<session>/before-restore`, then writes back every file the checkpoint has that differs, and deletes files created since, with folders they leave empty. The checkpoint's .gitignore files go back first, and nothing ignored by those rules or the current ones is read, written or deleted. Submodules and nested repositories are not touched. Paths it had to leave as they are (a folder or ignored file where the checkpoint has a file, a nested repository) are listed in the fork it opens: "Not restored: …", with the first three paths and how many more.
- After the files are back, the conversation forks from that reply as Fork from here does (see [fork](fork.md)), so the source session and its later turns stay as they were. The restore itself keeps every checkpoint.
- A session whose folder is in no repository shows the button only on the latest turn's last reply, with the tooltip "Checkpoints need a git repository."; pressing it, or the palette command, says the same in the conversation and asks nothing.
- Line endings follow git's own conversion: with `core.autocrlf` or `eol` attributes, a restored file has the line endings `git checkout` would give it.

## Native journey

Use a fixture repository with a commit, a staged file, a stash and ignored files, and the scripted Claude in store mode (`KEYSTONE_FAKE_CLAUDE_STORE=1`). Each line of a prompt reading `write <path>: <text>` or `delete <path>` makes the fake edit its working directory before replying. Give a session two turns that write, edit and delete files, then press Restore to here on turn one's reply and confirm. Check:

- `git for-each-ref refs/keystone` lists `refs/keystone/checkpoints/<store>/<session>/1` and `/2`, and `before-restore` after the restore.
- The working tree's files hash the same as `git ls-tree -r refs/keystone/checkpoints/<store>/<session>/1`, files created in turn two are gone, and ignored files are byte-for-byte unchanged.
- `.git/index`, `git rev-parse HEAD` and `git stash list` are unchanged.
- A new tab holds the fork with turn one only, linked by `forked_from`.
- With a turn held mid-reply (`KEYSTONE_FAKE_CLAUDE_DELAY_MS`), Restore is refused and no file changes.
- With `KEYSTONE_TEST_CHECKPOINT_KEEP=2`, a third turn removes `/1`.
