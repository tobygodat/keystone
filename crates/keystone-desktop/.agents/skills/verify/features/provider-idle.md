# Claude idle shutdown

Idle eviction is off for every real Claude profile (`docs/design/providers.md`, Idle shutdown). Its native check uses the scripted Claude in idle mode and a debug build's `KEYSTONE_TEST_CLAUDE_IDLE_MS` interval, never a real provider, real inference, or the real `~/.claude`.

## Setup

Build the desktop and the scripted providers in debug from the exact head and record the SHA and binary hashes. Point `KEYSTONE_CLAUDE_PATH` at a wrapper that sets `KEYSTONE_FAKE_CLAUDE_IDLE=1`, `KEYSTONE_FAKE_CLAUDE_CHILD=1`, `KEYSTONE_FAKE_CLAUDE_ARGV_LOG`, `KEYSTONE_FAKE_ENV_RECEIPT` and `KEYSTONE_FAKE_CLAUDE_LOG_DIR` in scratch, then execs `keystone-fake-claude`. Create `.fake-claude-messages` in each session folder so the fake logs every message it reads. Launch with `KEYSTONE_TEST_CLAUDE_IDLE_MS=8000` in a private headless Sway instance. Give the default Claude account, or an added account, a scratch `CLAUDE_CONFIG_DIR`.

## Native acceptance

1. Idle eviction: create a Claude session on an added account with a requested model, send `hello`, and record the process ID from the env receipt and its `sleep` child from `.fake-claude-children`. Before the interval ends both run. After it, both are gone, and the transcript shows no failed-turn row, Interrupted badge or recovery card. The model selector keeps the requested model and the reported model goes back to unknown. The neutral log matches the rows shown.
2. Lazy resume: send `again`. A new process starts with `--resume <saved thread>`, the same folder, `--model <requested>`, the same approval flags and the account's `CLAUDE_CONFIG_DIR`. `.fake-claude-messages` holds each prompt exactly once. The reported model reappears only after the new process's `init`.
3. Held work: in separate sessions, hold each of these past the interval and prove its process survives: a reply streaming (`long` with a slow delay setting), an approval (`run it` in Ask first, `approval-ask`), a held tool (`hold tool`), a hook (`hook`), a background task (`background task`), an ambient task (`ambient task`), a message queued behind a start, an unproven check (`.fake-claude-quiesce` holding `missing`), and a conversation that is not being saved (hold a separate SQLite connection with `BEGIN IMMEDIATE` across the turn's end). Release each (`.fake-claude-release-tool`, `-hook`, `-background`, `-ambient`, approve, or close the lock and send once more) and show eviction only after a new full interval.
4. Two sessions: one session is evicted while another runs or waits for approval, and the other keeps its process.
5. Quit: Ctrl+Q exits 0 with a dormant session and a live one; no owned process is left.

Save the argv, env, message and PID receipts, the screenshots you inspected, the neutral-log checks and the cleanup receipt.

## Limits

The scripted Claude speaks the published Agent SDK frames as Keystone reads them; it does not prove the real CLI sends them. Real background-task and subagent ordering, and whether a resumed conversation keeps its context, need real inference and are not covered.
