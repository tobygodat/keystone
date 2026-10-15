# Hand-off: Keystone design system and performance tracking

Written 2026-10-15. Read this before you touch either checkout.

## Where things are

| Location | State |
| --- | --- |
| `~/Projects/keystone` on `main` | Holds the finished composer increment as **uncommitted** changes in 7 files. They were left uncommitted on purpose for review. Do not reset, stash, or overwrite them. |
| `wip/composer-increment` branch, commit `54ac1a6` | A snapshot commit of that uncommitted work, made with `git commit-tree`. `main` is unchanged. |
| `~/Projects/keystone-design-system` on `design-system` | A worktree with 12 commits on top of `54ac1a6`. All checks pass and the tree is clean at `32db123`. |
| `refs/keystone/pre-design-system` | A safety ref to the uncommitted state before this work started. |
| `.audit/` in the main checkout (gitignored) | The decision trail `design-system.tsv`, the todo list `design-system.todo.md`, the original sketch `design-system-sketch.md`, and native evidence in `verify-design-system-*/`. |

There is no git remote, so no PR exists. The `/tmp/keystone-*` worktrees are prunable leftovers from an earlier session.

## How to land it

1. Commit the composer increment on `main`. Its content matches `54ac1a6`.
2. Then rebase `design-system` onto `main` (`git rebase --onto main 54ac1a6 design-system`) or cherry-pick `54ac1a6..design-system`. It applies cleanly only after step 1.
3. Run `bash scripts/check.sh`, `bash scripts/check.sh ui`, and `bash scripts/check.sh build`.
4. Remove the worktree with `git worktree remove ../keystone-design-system`.

The first build after landing rebuilds every dependency, because dev builds now optimize them. That took 205 s for the app and 70 s for the test build.

## What the branch contains

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

1. Land the two increments as described above.
2. **Sidebar render cost.** Every composer keystroke redraws the whole window, including every session row. Make the sidebar a cached view or a virtualized list. Measure with `scripts/perf.py` before and after. This may also help KS-005, the rapid-typing stall, but that link is unproven.
3. **Pending tokens.** A light theme, density, and font choice are in scope but not built. Token names are semantic, so each is additive.
4. **Small polish.**
   - Escape does not close the popover yet.
   - A selected `ListItem` shows no extra hover state.
   - The color check misses `Hsla { .. }` struct literals.
5. **Prior open issues.** KS-002 partial, KS-004 partial, KS-005, KS-006, and KS-009 in `docs/issues.md`.

## Working preferences learned

- The user wants build time kept low and performance tracked in the repo. Run `scripts/perf.py` after changes that affect build or render speed, and commit the log row.
- Verify on the real window, not only in simulated tests. Two bugs this session were caught only natively: the "..." titles, and a focus fix that did nothing.
