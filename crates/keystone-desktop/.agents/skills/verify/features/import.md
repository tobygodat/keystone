# Import sessions (TOB-74)

Import sessions… brings a conversation Claude Code or Codex already keeps for the window's project folder into Keystone as a new session that continues it. Nothing is written to the provider's store.

## Entry points

- The command palette: "Import sessions…" (Ctrl+Alt+I).
- The new-session menu (`new-session`): "Import…" (`import-sessions`).

## Expected behaviour

- The list (`import`, rows `import-row-<n>`) first offers each provider account: "Claude Code" and "Codex", with the account's name after a dot once the user has added accounts for that provider.
- Choosing one lists that account's conversations whose recorded working directory is the project folder, newest first, at most 50. Each row is the conversation's title (Codex's name, else its first message; Claude Code's first message) with when it last changed and its turn count. A count with a plus counted only part of a long history.
  - Claude Code: `<config dir>/projects/*/<id>.jsonl`, the config dir being the account's home or, for the default account, `CLAUDE_CONFIG_DIR` or `~/.claude`. A conversation the recovery reader cannot follow (branched, or another folder) is not listed. A folder recorded through a symlink, such as macOS's `/tmp`, matches.
  - Codex: `thread/list` with the folder as `cwd` and `useStateDbOnly`, then `thread/turns/list` with `itemsView: "notLoaded"` per thread to count turns, on the account's own app-server. Codex matches the folder path exactly. A thread with no turns is not listed; one Codex cannot count shows no count.
- A conversation some Keystone session already continues shows "Imported". Choosing it opens that session; it is never imported twice.
- Choosing another imports it: a new session, selected, on that provider and account, with `provider_thread` set to the provider's ID and the provider fixed. Its rows are the newest 50 turns, read as crash recovery reads them. When older turns are left out, the first row says "Older messages weren't imported."
- The next Send resumes the provider's conversation: Claude Code launches with `--resume <id>` under the account's `CLAUDE_CONFIG_DIR`; Codex receives `thread/resume` with that `threadId` on the account's `CODEX_HOME` app-server.
- With no folder, a failed read or a provider that cannot start, the list says why, or the window shows "Not imported. <reason>". A conversation that changed after it was listed (branched, emptied or now in another folder) shows "Not imported. It changed since it was listed." and adds no session.
- Switching project or closing the window closes the list; nothing it was waiting for is imported later.

## Native journey

Give the run fixture homes as `CLAUDE_CONFIG_DIR` and `CODEX_HOME`, with `KEYSTONE_FAKE_ENV_RECEIPT`, `KEYSTONE_FAKE_CLAUDE_ARGV_LOG` and `KEYSTONE_FAKE_CODEX_THREAD_LOG` set and `KEYSTONE_PROJECT_DIR` naming the folder the fixtures ran in. The Claude home holds two labelled sessions in that folder (one past 50 turns) and one elsewhere; the Codex home holds `keystone-fake-threads/<id>.json` files with a `thread` record (`name`, `preview`, `cwd`, `updatedAt`) beside their `turns`. Hash every fixture file first.

- Open the list from the palette, choose Claude Code: two rows with titles, times and counts. Import the long one: the older-history line and the newest 50 turns render.
- Open it from the new-session menu, choose Codex: import the thread; its rows render.
- Send in each. The argument log's last Claude launch holds `--resume <id>`, and the env receipt for that process names the fixture `CLAUDE_CONFIG_DIR`; the thread log holds `thread/resume <id>`, and the Codex process's receipt names the fixture `CODEX_HOME`.
- Reopen the list for each account: the imported rows say "Imported", and choosing one selects the existing session without adding one to `workspace`.
- Quit and relaunch: both sessions and their rows restore.
- Every fixture file's hash is unchanged.
