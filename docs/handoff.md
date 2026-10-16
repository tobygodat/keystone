# Hand-off: Keystone design system and performance tracking

Written 2026-10-15, updated after landing. Read this before starting new work.

## Status

The composer increment and the 12 design-system commits are on `main` and pushed to `origin` (`1d10977`). The `design-system` branch is merged and can be deleted. The `.audit/` directory, the Sway driver binary, and the worktrees named in earlier notes existed only on the original machine.

After a fresh clone, the first build rebuilds every dependency, because dev builds optimize them. That took 205 s for the app and 70 s for the test build.

## What landed

- **`crates/keystone-ui`.** A new component library with theme tokens (`Theme`, `Colors`, `Color`, `TextSize`, `Elevation`, `spacing`, `radius`), typed icons, `Button`, `IconButton`, `ListItem`, `PopoverMenu`, `MenuItem`, `MenuSeparator`, `Tooltip`, `Label`, and `Kbd`. See `docs/design-system.md`.
- **Gallery.** `cargo run -p keystone-ui --example gallery` shows every component in every state.
- **Desktop migration.** `crates/keystone-desktop/src/main.rs` is built from the components and has no color literals. `icons.rs` was deleted, and `text_input.rs` reads colors from the theme.
- **Behavior changes, all intended.**
  - The "+" opens an anchored overlay menu with shortcut hints.
  - Icon buttons have tooltips.
  - The welcome buttons, menu items, Ctrl+N, and Ctrl+Shift+N all create a session and focus the composer.
  - The empty sidebar says "No sessions yet" when there are no sessions, and "No sessions match these filters" when a filter hides them all.
- **Color check.** `scripts/check.sh` fails on GPUI color constructors (`rgb(`, `hsla(`, `white(`, and so on) outside `crates/keystone-ui/src/theme.rs`.
- **Tests.** 6 desktop GPUI tests pass, including a new regression test for typing after menu creation. 2 component tests pass. 1 ignored benchmark exists.
- **Performance tracking.**
  - `python scripts/perf.py --note "..."` appends to `docs/perf-log.tsv`. `docs/performance.md` explains the columns.
  - Dev builds now optimize dependencies (`[profile.dev.package."*"] opt-level = 2`). Measured on this machine, this cut the 200-session draw benchmark from 52–101 ms to 8–11 ms per frame. Incremental rebuilds fell from about 1.5 s to about 1.1 s.
- **Docs.** README check list, `docs/design-system.md`, `docs/performance.md`, KS-010 in `docs/issues.md`, and updated focus guidance in `crates/keystone-desktop/.agents/skills/verify/features/sessions.md`.

## Decisions to keep

- **Licensing.** Zed's `ui`, `theme`, and `component` crates are GPL-3.0-or-later. No Zed code was copied, only patterns. Zeron (`github.com/zeronsh/zeron`) is MIT, and only design values were adapted from it. Keep it that way unless the project accepts the GPL.
- **GPUI version.** GPUI stays pinned at crates.io `=0.2.2`, the latest published. Zed and Zeron run newer GPUI, and Zeron uses a fork with blur and edge-fade features. Their components need porting, not copying.
- **Ellipsis workaround.** `Label::truncate` clips without an ellipsis. GPUI 0.2.2 caches the first measured size of non-wrapping text, so `text_ellipsis` rendered titles as a bare "...". This is logged as KS-010. Recheck it on a GPUI upgrade.
- **Popover open and close.** `PopoverMenu` opens on mouse down and closes on any press outside the card. That includes the trigger itself, so a click-based toggle would reopen the menu. The caller owns the `open` state.

## Verification notes

- Native checks use the `keystone-verify` MCP server (`scripts/native-driver.py`, registered in `.mcp.json`). See `crates/keystone-desktop/.agents/skills/verify/`. The server drives the `target/debug` binary of the checkout that contains the script, and defaults `--driver` to that checkout's `.audit/tools/computer-use-mcp`. In a worktree without its own driver, pass `--driver` with the main checkout's path.
- `start` is headless by default. The private Sway desktop renders offscreen at the requested size, so host focus and visibility no longer matter (KS-006). Named elements from the app's frame drive `click` and `wait_for`. With `headless:false`, the nested window must stay visible, or GPUI stops drawing frames and clicks on new menus miss.
- Find the nested window with `pgrep -x sway` and match that PID in `hyprctl clients -j`. `pgrep -f` matches your own shell.
- The final native run (`.audit/verify-design-system-20261015T153837`) had zero failures. Earlier runs confirmed restart restoration, the tooltip, menu hover, and the gallery.
- SSH to `desk`, which the user suggested for heavy work, was blocked by the permission classifier. Everything ran locally on 22 cores.
- On this machine, Sway 1.12 is installed and the driver was rebuilt at the pinned commit. Driver screenshots fail with `unsupported SHM pixel format` (KS-011), so capture with `grim` against the nested socket. Mouse actions need `"button":"left"`.
- An agent shell may not inherit the Hyprland session. Set `HYPRLAND_INSTANCE_SIGNATURE` from `/run/user/1000/hypr/` and `WAYLAND_DISPLAY=wayland-1` before running native checks.
- The settings run (`.audit/verify-20261015T165545-settings`) verified the settings menu, mouse session creation, and restart natively. See `docs/verification-driver.md`.

## Open work, in priority order

0. **Providers.** Codex and Claude Code work end to end (`crates/keystone-agent`, `docs/design/providers.md`). Both start on a worker, and their events go through the same `Conversation`, `log::Logs`, and virtualized transcript. Claude follow-ups are listed under "Not built yet" in the design: idle shutdown of Claude processes, a version check, and transcript reconciliation. Markdown replies and the per-session approval mode are built for both providers (see `docs/design-system.md` and "Approval modes" in the provider design). The mode reaches each provider through `Connection::send` and `Connection::set_approval`; the connection keeps it across process restarts. Claude restarts its process, idle, to enter or leave Full access and to return to Provider settings. Full access is one click with no confirmation, which is still an open product question. The Codex review follow-ups are done (2026-10-16): session logs and history loading run on a worker (`log::Logs`), Codex and Claude are found and started on a worker, a restored transcript keeps the order it was shown in (`seq` in the log), and the transcript is a virtualized GPUI `list`. Known limits: theme, density, or font changes leave the cached heights of off-screen rows stale until they scroll into view, which can make the scroll extent briefly inaccurate. Native wheel scrolling is unproven because the driver's wheel events reach no GPUI view (KS-012); the GPUI test covers the wheel.
1. **KS-005 confirmation.** The sidebar is now a cached view (see `docs/performance.md`). The 10 ms native typing check passed on 2026-10-15 both before and after that change, so the dependency optimization is the more likely fix. Repeat it several times before closing KS-005.
2. **Appearance follow-ups.** The light theme, density, and font settings are built and saved in the workspace file. The interface font list only offers installed fonts from a fixed candidate list. Text size and the monospace font are not configurable yet. System mode follows GPUI's reported window appearance. On Hyprland with the portal reporting `prefer-dark`, it opened dark. A light desktop setting and switching while the app runs have not been tested.
3. **Small polish.** Escape closes menus, selected rows have a hover state, and the color check catches `Hsla { .. }` literals. Nothing else is queued.
4. **Prior open issues.** KS-002 partial, KS-004 partial, KS-005, and KS-009 in `docs/issues.md`.

## Review follow-ups landed 2026-10-16

- **Workers.** `keystone_agent::worker::Worker` runs blocking jobs in order on one thread. `log::Logs` uses one for every session's log: appends, syncs, and history loads, with results as `Report`s drained by the UI's pump. `CodexConnection` and `ClaudeConnection` each use one for commands, so a start failure arrives as `Event::Failed`. Both implement `Connection`; its `send` returns an error only for a steer the provider refuses (Claude without `command_lifecycle` frames, or before it has reported any), and `command` returns nothing.
- **Conversation.** `Conversation` (desktop crate) does no IO. Writes collect in an outbox (`take_writes`), and `Keystone::commit` hands them to `Logs` and splices the changed rows into the list. A conversation starts `loading()` and buffers events and sent messages until `restore`; a message sent meanwhile marks it running at once. A failed load (`restore(Err)`) applies what was buffered without the history and keeps `load_error` shown. `flush_logs` (quit, window close) waits up to `LOAD_WAIT` (2 s) for outstanding loads, gives up on the rest the same way, then waits for every queued write. `commit` splices only the rows that changed and keeps a scrolled-up reader's top entry and offset, the status entry included. `Keystone::clamp_top` runs before the list lays out and moves past a top row that shrank below the kept offset.
- **Order.** Log entries carry `seq` and `run`; `seq` counts from the first row of the run that wrote it, and a restore places each run after the rows before it. Old logs still read in file order.
- **Reads.** Each history load reads on its own thread (`Logs::load`), never joined, so a hung read holds up neither writes nor quitting. `Logs::with_reader` takes a stand-in reader for tests.
- **Transcript.** `Keystone::transcript_list` builds the list; `transcript::row` decides each row's content. Rows are named `transcript-row-N` and the status entry `transcript-status`, so native `elements` lists only the rows in view.
- **Evidence.** Native run `.audit/verify-20261015T210533-412732` (local only): real Codex reply, a steer mid-reply, restart with the steer after the interrupted reply, resume, a 1,000-turn old-format log drawing 7 rows, session switching, and a save failure shown then cleared. Benchmark: 115 ms per frame before, under 1 ms after.

## Working preferences learned

- The user wants build time kept low and performance tracked in the repo. Run `scripts/perf.py` after changes that affect build or render speed, and commit the log row.
- Verify on the real window, not only in simulated tests. Two bugs this session were caught only natively: the "..." titles, and a focus fix that did nothing.
