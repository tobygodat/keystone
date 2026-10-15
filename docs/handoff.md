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

- Native checks use the nested Sway driver in `crates/keystone-desktop/.agents/skills/verify/`. The driver binary is at `.audit/tools/computer-use-mcp` in the main checkout. Run the controller with `--root` pointing at the checkout whose binary you built.
- **The nested window must be visible.** A hidden window times out screenshots and also stops GPUI from drawing frames. A click on a menu that just opened then lands on the previous frame and misses. Run each journey as one scripted burst with the nested window focused, then restore the user's previous Hyprland focus.
- Find the nested window with `pgrep -x sway` and match that PID in `hyprctl clients -j`. `pgrep -f` matches your own shell.
- The final native run (`.audit/verify-design-system-20261015T153837`) had zero failures. Earlier runs confirmed restart restoration, the tooltip, menu hover, and the gallery.
- SSH to `desk`, which the user suggested for heavy work, was blocked by the permission classifier. Everything ran locally on 22 cores.

## Open work, in priority order

1. **KS-005 confirmation.** The sidebar is now a cached view (see `docs/performance.md`). The 10 ms native typing check passed on 2026-10-15 both before and after that change, so the dependency optimization is the more likely fix. Repeat it several times before closing KS-005.
2. **Pending tokens.** A light theme, density, and font choice are in scope but not built. Token names are semantic, so each is additive.
3. **Small polish.**
   - Escape does not close the popover yet.
   - A selected `ListItem` shows no extra hover state.
   - The color check misses `Hsla { .. }` struct literals.
4. **Prior open issues.** KS-002 partial, KS-004 partial, KS-005, KS-006, and KS-009 in `docs/issues.md`.

## Working preferences learned

- The user wants build time kept low and performance tracked in the repo. Run `scripts/perf.py` after changes that affect build or render speed, and commit the log row.
- Verify on the real window, not only in simulated tests. Two bugs this session were caught only natively: the "..." titles, and a focus fix that did nothing.
