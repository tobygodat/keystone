# Worktrees

A session can work in its own git worktree inside the project's repository. Keystone runs the user's own `git` with no credentials and contacts no remote.

## Sub-features

- **New session in worktree** opens `worktree-dialog`: a name field (`worktree-name`, prefilled with the first free `session-N`), a provider choice (`worktree-claude`, `worktree-codex`) and **Create** (`worktree-dialog-confirm`). It runs `git worktree add -b keystone/<name> <repo>/.keystone/worktrees/<name> <project HEAD>` off the UI thread and adds `/.keystone/worktrees/` to `.git/info/exclude`; the tracked `.gitignore` is never written. A refused name keeps the dialog open with the reason in `worktree-error`.
- The session records the worktree as its folder, plus the repository root and branch (`execution_context` version 2 in `workspace`; version 1 rows still load). Files and the provider process start there.
- The branch shows in the top bar (`worktree-branch`) and on the session row (`session-branch-N`), also while the row needs action. A worktree folder that is gone shows `worktree-missing` (**Worktree folder missing**) in the top bar and **Worktree missing** on the row; the conversation stays readable and Send keeps the draft with the reason.
- **Remove worktree…** opens `remove-worktree-dialog`. **Remove** refuses, with the reason in `worktree-error`, while the worktree has uncommitted, untracked or ignored files, hidden index flags, or commits on its branch that are neither pushed (in a remote-tracking ref) nor on another local branch, and while a turn runs in that folder or below it. It never passes `--force` and always keeps the branch. A sparse checkout marks files outside its cone as hidden, so its worktrees are always refused.
- The orchestrator's `session_launch` accepts `cwd` naming a registered worktree of the caller's repository, and `worktree: <name>` to create one only when the caller itself works in a worktree. Any other folder is refused.

## How to get to it (user POV)

- Welcome composer: the **In worktree** pill (`welcome-worktree`).
- Sidebar: the new-session button (`new-session`), then **In worktree…** (`new-worktree`).
- Ctrl+Alt+G, or **New session in worktree…** in the palette.
- Remove: right-click a worktree session's row and choose **Remove worktree…** (`menu-remove-worktree`), or Ctrl+Alt+Shift+G / the palette's **Remove worktree…** for the shown session.

## Driving it with keystone-verify

Make a disposable fixture repository with one commit and a temp bare remote (`git init --bare`, `git remote add origin`, push `main`). Point `KEYSTONE_CLAUDE_PATH` at a wrapper that logs `$PWD` and sets `KEYSTONE_FAKE_ENV_RECEIPT` and a scratch `CLAUDE_CONFIG_DIR`, and start with `KEYSTONE_PROJECT_DIR` set to the fixture.

- Click `welcome-worktree`, type a name, press Enter. Require `execution_context` version 2 with `worktree.repository` and `worktree.branch`, `git worktree list --porcelain` naming the path and branch, the exclude line, and no change to `git status` in the fixture. Send: the logged `$PWD` is the worktree. Ctrl+Shift+E: Files' root row is the worktree.
- Write a file in the worktree, press Ctrl+Alt+Shift+G and **Remove**: refused, file kept. Commit it, press Escape and reopen the dialog (no `worktree-error` yet), then **Remove**: refused again (unpushed). Push the branch to the bare remote and **Remove**: the folder is gone, the branch remains, the top bar shows `worktree-missing`.
- In a main-checkout session send `orchestrator test: launch in <worktree path>` (the scripted Claude calls `session_launch` with that `cwd`): a child session records the worktree. `orchestrator test: launch in <unrelated folder>` is refused, and `orchestrator test: launch in new worktree <name>` is refused unless the caller works in a worktree.
- Quit and `launch`: rows keep their branches; the removed one shows missing and Send keeps the draft. Kill the app with SIGKILL and `launch`: the surviving worktree session still shows its branch and its provider restarts in the worktree.

The scripted journey is `handoffs/archive-journey.py` in the Captain Drive.
