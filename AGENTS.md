# Keystone

A native Rust and GPUI desktop workspace for running Claude Code and Codex sessions side by side, for developers who use both.

- Issues: https://github.com/tobygodat/keystone/issues. Every change starts from an issue there.

## Read first

1. The issue you're working on.
2. `docs/ARCHITECTURE.md` for where things live.
3. `docs/CONVENTIONS.md` before you add a file or a test.
4. `PRODUCT.md` and `DESIGN.md` before you change anything a user sees.
5. `docs/adr/` before you reverse a decision. Supersede an ADR with a new one, and never edit an accepted one.

## Before asking to merge

Put cargo on `PATH` first: `export PATH=$HOME/.cargo/bin:$PATH`.

- `cargo test --workspace --locked`. Plain `cargo test` runs only `keystone-core`.
- `cargo clippy --workspace --locked` exits 0.
- `bash scripts/check.sh` for formatting, the color and shortcut checks, and the UI string inventory. Add `bash scripts/check.sh ui` when the desktop or `keystone-ui` changed.
- Prove the change in the real app: `cargo run -p keystone-desktop --locked` with `KEYSTONE_STATE_PATH` set to a scratch file, or drive it headless through the verify skill (`crates/keystone-desktop/.agents/skills/verify/SKILL.md`). Do what a user would do and attach screenshots to the PR for any visible change:
  - Create a Claude session (Ctrl+N), type a draft, quit with Ctrl+Q, relaunch: the session and its draft come back.
  - For a changed surface, click through it with the mouse as well as its shortcut, and check the screenshot, not only the named element.

## Rules

- Colors only in `crates/keystone-ui/src/theme.rs`, and shortcut hints only through `keystone_ui::shortcut_text`. `check.sh` fails otherwise.
- After changing interface text, run `python3 scripts/ui-strings.py` and commit the updated `docs/ui-strings.txt`.
- `keystone-core` and `keystone-agent` never depend on GPUI.
- Tests use temporary directories and the fake providers. Never touch real provider config, real repositories or the real workspace database. Live provider checks are opt-in.
- Never fabricate agent output or show a provider as connected when it isn't.
- Settlement never approves a request, and the Needs action grouping isn't extended (ADR 0001).
- No GPL code (ADR 0003). Credit borrowed files in `THIRD_PARTY_NOTICES.md`.
- GPUI comes from the pinned tGPUI fork (ADR 0004). Bump every `rev` together.
- Call `Keystone::refresh`, not `cx.notify()`, for anything the sidebar shows (ADR 0007).
- After a change that could affect build or render speed, run `python scripts/perf.py --note "..."` and commit the log row.
- After changing the composer or text layout, run `python3 scripts/native-stress.py --runs 10`.
- After changing how Keystone quits, closes its last window, pumps provider events, delivers launched sessions' results or starts scheduled runs, follow the quit-model skill (`crates/keystone-desktop/.agents/skills/quit-model/SKILL.md`). `check.sh --drift` there says whether a change touches the code it models.
- After changing how a remote session's events are received, drained, saved, acknowledged or replayed, follow the cursor-model skill (`crates/keystone-desktop/.agents/skills/cursor-model/SKILL.md`). `check.sh --drift` there says whether a change touches the code it models.
- After changing how a checkpoint restore moves files, writes its journal, undoes, or puts back an unfinished restore, follow the restore-model skill (`crates/keystone-agent/.agents/skills/restore-model/SKILL.md`). `check.sh --drift` there says whether a change touches the code it models.
- Each worktree keeps its own `target/`. Don't share one or point another worktree at it.
- Remaining work and known bugs go in GitHub issues, not in repo files.
