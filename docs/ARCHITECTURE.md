# Architecture

## Overview

Keystone is one local process with one GPUI application that owns every window. It drives the user's installed `claude` and `codex` CLIs as child processes, keeps workspace state and a neutral copy of each conversation in one SQLite database, and serves a local MCP server so sessions can launch and read each other. The app has no server, sync, or RPC between its own parts. It runs on Linux (Wayland and X11) and Apple Silicon macOS.

This describes what is built. [ADR 0010](adr/0010-terminal-browser-scheduling-editor-diffs-remote-devices.md) brings seven unbuilt features into scope, and some will change it. Remote environments (TOB-306) run sessions on another machine through `keystone-host`, a second binary that speaks a JSON Lines protocol over SSH ([ADR 0011](adr/0011-remote-machines-through-a-headless-host.md)). The host and the protocol are built. The app doesn't use them yet, so it is still one process. The browser preview (TOB-302) and device panel (TOB-307) will add orchestrator MCP tools. The terminal (TOB-301) and editor (TOB-304) are bound by ADR 0003: permissive crates only. Whether scheduled tasks (TOB-303) need a background process is open. Each design lands here when it's built.

## Code map

- `crates/keystone-core`: GPUI-free domain and storage. Sessions, projects, accounts, tabs, windows and layout, prompts, notes, attachments, worktrees, pull requests, usage, and integrations inventory and sync. `database.rs` is the SQLite store. Its tests never compile the graphics stack.
- `crates/keystone-agent`: provider integration, also GPUI-free. Drivers (`claude.rs`, `codex.rs`, `driver.rs`) turn each wire protocol into `Event`s and do no IO. `host.rs` owns child processes. `connection.rs` is the `Connection` trait both providers implement. `question.rs` is the neutral shape of a question an agent or MCP server asks the user. `worker.rs` runs blocking jobs in order on one thread. `log.rs` is the per-session log. `mcp/` is the orchestrator server. `browser/` runs headless Chrome over the DevTools protocol for the browser preview. `setup.rs` runs a project's worktree setup script in its own session and process group. `git.rs`, `github.rs`, `checkpoint.rs`, `diff.rs`, `import.rs`, `recovery*`, `idle.rs` and `version.rs` cover the rest. `remote/` runs sessions on other machines: `frame.rs` is the wire protocol, `serve.rs` the host daemon with its SQLite `journal.rs`, `reap.rs` the watcher it starts beside each provider, `attach.rs` the bridge SSH runs, and `link.rs` and `connection.rs` the app's end (`Link`, `RemoteConnection`). `src/bin/` holds `keystone-host`, and `keystone-fake-claude` and `keystone-fake-codex`, the scripted stand-ins for tests.
- `crates/keystone-ui`: the component library. Theme and tokens, icons, and controls, including Markdown, diagrams, a virtual list, a tree, splits, tabs and the command palette. `examples/` has the gallery. Colors live only in `src/theme.rs`. See `DESIGN.md`.
- `crates/keystone-desktop`: the app binary. `main.rs` holds the `Keystone` window view and `bind_keys`. `commands.rs` is the one table of palette commands and their default keys, and `keymap.rs` reads the user's saved overrides of them (Settings → Shortcuts, `settings_shortcuts.rs`). `app_state.rs` owns sessions and providers at app level, so closing a window doesn't stop work. `conversation.rs`, `transcript.rs` and `transcript_keyboard.rs` draw a session. Feature modules (`changes.rs`, `diff_view.rs`, `pull_request.rs`, `questions.rs`, `files.rs`, `quick.rs`, `orchestrator.rs`, `browser.rs`, `settings*.rs`, and others) each own one surface. `setup.rs` holds each new worktree session's setup script, its card, and the send it holds until the script succeeds or is skipped. `terminal.rs` and `terminal/` run each session's shells, kept at app level like sessions, in a drawer below the main area. `.agents/skills/verify/` is the native verification skill.
- `scripts/`: `check.sh` lanes, the native driver MCP server (`native-driver.py`, registered in `.mcp.json`), smoke, stress and journey scripts, perf and timing tools, provider probes, and `rustc-wrapper.sh`.

## Data

**Storage.** One bundled SQLite database holds an append-only `events` log and the projections the app reads: `sessions`, `workspace` and `log_entries`. `Store` applies changes to its in-memory read model and queues ordered commits on the database writer, and a stale writer gets a reload-required error instead of overwriting. It lives at `$XDG_DATA_HOME/keystone/workspace.sqlite3` (macOS: `~/Library/Application Support/Keystone/`), or at `KEYSTONE_STATE_PATH`. A file lock keeps two processes off one workspace. Attached images are copied into `attachments/` beside it. Details, durability and backups: [storage](design/storage.md).

**Conversations.** The provider owns the real conversation. Keystone keeps a neutral log of completed items with capped tool output, so it can restore, hand off, fork and offer second opinions.

- `Conversation` (desktop crate) does no IO. Writes collect in an outbox (`take_writes`), and `Keystone::commit` hands them to `Logs` and splices only the changed rows into the list. It keeps a scrolled-up reader's top entry and offset, the status entry included.
- A conversation starts `loading()` and buffers events and sent messages until `restore`. A message sent meanwhile marks it running at once. A failed load (`restore(Err)`) applies what was buffered without the history and keeps `load_error` shown.
- `log::Logs` runs every session's appends, syncs and history loads on a worker, with results as `Report`s drained by the UI's pump. Each history load reads on its own thread, never joined, so a hung read holds up neither writes nor quitting. `Logs::with_reader` takes a stand-in reader for tests.
- Log entries carry `seq` and `run`. `seq` counts from the first row of the run that wrote it, and a restore places each run after the rows before it. Old logs still read in file order.
- `flush_logs` (quit, window close) waits up to `LOAD_WAIT` (2 s) for outstanding loads, gives up on the rest, then waits for every queued write.

**Providers.** `CodexConnection` and `ClaudeConnection` each run commands on a worker, so a start failure arrives as `Event::Failed`. `Connection::send` errors only for a steer the provider refuses, and `command` returns nothing. One shared `codex app-server` serves every Codex session. Each Claude session runs its own `claude` process, stopped when idle and resumed on the next send. Approval modes reach each provider through `Connection::send` and `Connection::set_approval`. Details: [providers](design/providers.md).

**Rendering.** The sidebar is a cached view, and the transcript is a virtualized `list` built by `Keystone::transcript_list`. `transcript::row` decides each row's content. Rows are named `transcript-row-N` and the status entry `transcript-status`, so native `elements` lists only the rows in view. See [ADR 0007](adr/0007-cached-sidebar-and-virtualized-transcript.md).

## External services

- **`claude` and `codex`**, the user's own binaries and logins, found on `PATH` or through `KEYSTONE_CLAUDE_PATH` and `KEYSTONE_CODEX_PATH`. Keystone never holds provider credentials.
- **`git` and `gh`**, the user's own, for Changes, push and pull requests. Keystone never holds a GitHub token.
- **The orchestrator MCP server**, at `http://127.0.0.1:<ephemeral>/mcp`, local only, offered to each provider beside its own tools. See [orchestrator](design/orchestrator.md).
- **A Chromium-family browser** (Chrome, Chromium, Edge or Brave, or `KEYSTONE_BROWSER_PATH`), run headless for the browser preview with its own profile in `browser/` beside the database. See [browser preview](design/browser-preview.md).
- **Desktop tools**: `slurp` and `grim` (Wayland) or `screencapture` (macOS) for screenshots, the notification daemon, and `xdg-open` or `$EDITOR` for files.

## Reference docs

- [Provider integration](design/providers.md): protocols, approvals, versions, limitations.
- [Workspace storage](design/storage.md): transactions, durability, import, attachments.
- [Orchestration](design/orchestrator.md): the `keystone` MCP server and its tools.
- [Agent-built widgets](design/widgets.md): the `show_ui` tool and its A2UI catalog.
- [Browser preview](design/browser-preview.md): the embedding decision, the Browser surface and the `browser_*` tools.
- [Scheduled tasks](design/scheduled-tasks.md): when tasks fire, catch-up at launch, and what a run does.
- [Integrated terminal](design/terminal.md): the emulator, its license audit, shell ownership, shortcuts and drawing cost.
- [Remote machines](design/remote.md): running sessions on another machine through `keystone-host` over SSH. The host and protocol are built, and the app doesn't use them yet.
- [macOS](macos.md): what differs from Linux.
- [Performance](performance.md) and [development iteration timing](dev-iteration.md): how build and render costs are measured.
- `research/`: provider probes and design research, each with its raw output.
- `verification/` and [the verification driver proof](verification-driver.md): dated native evidence for past changes.
