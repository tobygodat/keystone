# Keystone UI components plan

Keystone gets four component families in `crates/keystone-ui`. They are tabs with split panes and multiple windows, a command palette, a virtualized file tree, and settings controls. Each family lands first as a gallery component, then as a desktop feature.
The rule the program enforces is that shared pieces land before the features that use them. `TextInput` moves into keystone-ui and a keyboard-driven `VirtualList` lands before the palette, the select, and the tree. Sessions move to an app-level owner before any second window opens.
Component PRs never edit `crates/keystone-ui/src/markdown.rs` or `crates/keystone-desktop/src/transcript.rs`, which TOB-11 owns.
PR ids in stack order are UC-01, UC-02, UC-03, UC-04, UC-05, UC-06, UC-07, UC-08, UC-09, UC-10, UC-11, UC-12, UC-13, UC-14.

## How to read this

One box is one unit of work. Every box names the evidence that checks it. A nested box is a sub-step of the box above it. Check a box only when its evidence exists, a file, a log line, a screenshot, a test run, or a SHA. The body is a how-to. The appendices explain and record.

`<plugin root>` is `~/.claude/plugins/cache/t3-pstack/t3-pstack/2.0.0-rc.6`. The repo root is the Keystone checkout that the root chat runs from. Every cargo command runs after `export PATH=$HOME/.cargo/bin:$PATH`.

The program runs `<plugin root>/skills/poteto-mode/playbooks/autopilot-stack.md`. The work is sequenced, six PRs change an interaction the operator should see, and the operator keeps landing authority. Owners report STACK-READY. The root appends each clean PR to one linear stack on `main`. The operator lands the stack bottom-up. UC-04, UC-06, UC-08, UC-12, UC-13, and UC-14 are review-gated and stop at stack-ready until the operator approves them in chat. UC-14 also lands a commit in the private tGPUI fork, which the operator pushes.

Each PR names a Linear placeholder. The parent creates the real issue in Linear project `keystone`, team TOB, and replaces the placeholder before the owner opens the PR.

Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

### Verification tiers

The operator asked on 2026-10-16 for lighter checks on small changes. These tiers override the lane counts in each PR's **Verify, live** block and the swarm in **Verdict and merge**. Each **Verify, live** block is a menu of lanes. The owner runs the lanes its tier names and skips the rest.

- **Refactor (UC-01, UC-11).** Unit tests, `bash scripts/check.sh ui`, and one native smoke run by the owner covering lanes 1, 7, and 10. UC-01 gets one short diff review. UC-11 changes process teardown, so it gets a full GPT-6.1 Sol review.
- **Component (UC-02, UC-03, UC-05, UC-07, UC-09, UC-10).** Unit tests, the perf block, and gallery screenshots of the new component from the owner's own native run, covering at most four lanes. One GPT-6.1 Sol review of the diff.
- **Review-gated (UC-04, UC-06, UC-08, UC-12, UC-13, UC-14).** The full **Verify, live** block, run as a swarm, plus a GPT-6.1 Sol review. Each waits for the operator's review in chat.
- A fix round, rebase, or merge catch-up gets the PR's tests plus one verifier at the new head, never a new swarm.

## Program checklist

### Arm the program

- [x] State the protocol and this plan to the operator, then stop. Start execution only on the operator's explicit go. The operator said go on 2026-10-16 for UC-01 and UC-02.
- [ ] Read these at program start. Re-read them at every tick.
  - [ ] `cat <plugin root>/skills/poteto-mode/playbooks/autopilot-stack.md`
  - [ ] `cat <plugin root>/skills/poteto-mode/playbooks/autopilot-full.md`
  - [ ] `cat <plugin root>/skills/swarm/SKILL.md`
  - [ ] `cat crates/keystone-desktop/.agents/skills/verify/SKILL.md`
  - [ ] `cat crates/keystone-desktop/.agents/skills/verify/features/README.md`
  - [ ] `cat <plugin root>/skills/poteto-mode/playbooks/opening-a-pr.md`
  - [ ] `cat <plugin root>/skills/poteto-mode/playbooks/shipping.md`
  - [ ] `cat <plugin root>/skills/poteto-mode/playbooks/feature.md`
  - [ ] `cat <plugin root>/skills/poteto-mode/playbooks/refactoring.md`
  - [ ] `cat <plugin root>/skills/interrogate/SKILL.md`
  - [ ] `cat <plugin root>/skills/benchmark-checklist/SKILL.md`
  - [ ] `cat docs/design-system.md`
- [ ] On the operator's go, arm the audit tick as an hourly schedule_task bound to this thread with the tick prompt below. Never leave the cadence to memory.
- [ ] Use this tick prompt, verbatim. "Re-read the execution playbook from disk. Audit the operation against it and fix drift in this tick. Probe every active lane and judge progress by side effects only. Stand down a stuck lane and dispatch its replacement now. Then post a short status message to the operator in chat only when the audit found a tracked change that no earlier status message reported, such as a PR opened, a code-ready head, a round launched or closed, a verdict, a merge, a stuck agent and the action taken, a blocker added or cleared, or a decision only the operator can make. Name every such change and nothing else. Do not repeat a table, the merged list, or an unchanged blocker. If the audit found none, end the turn with no reply text. Either way, log this tick's row in your decision trail. The row names the items reported, or none."
- [ ] On the operator's hold or stand-down, send every owner a zero-writes order at once.

### Spawn owners

- [ ] Spawn one owner per PR with the full lifecycle the execution playbook names.
- [ ] Follow this dependency graph. Each owner branches from its parent's branch, because the execution playbook stacks.
  - [ ] UC-01, UC-02, UC-09, and UC-10 are independent and first. Each branches from `main`.
  - [ ] UC-03 after UC-02.
  - [ ] UC-04 after UC-03.
  - [ ] UC-05 after UC-01 and UC-02. It branches from UC-02's branch with UC-01 below it in the stack.
  - [ ] UC-06 after UC-05.
  - [ ] UC-07 after UC-02.
  - [ ] UC-08 after UC-07.
  - [ ] UC-11 after TOB-11 merges to `main`. TOB-11 edits the Markdown cache in `crates/keystone-desktop/src/main.rs`, and UC-11 moves the fields around it.
  - [ ] UC-12 after UC-09, UC-10, and UC-11.
  - [ ] UC-13 after UC-12.
  - [ ] UC-14 after UC-13. Its tGPUI commit can start any time, because it touches only `~/src/tGPUI`.
- [ ] Hold the file boundaries.
  - [ ] No PR touches `crates/keystone-ui/src/markdown.rs`, `crates/keystone-desktop/src/transcript.rs`, or the `markdown` field, `parsed()`, and `transcript_list` in `crates/keystone-desktop/src/main.rs`. TOB-11 owns them.
  - [ ] Component PRs (UC-02, UC-03, UC-05, UC-07, UC-09, UC-10) touch only `crates/keystone-ui/**`, `docs/design-system.md`, and `Cargo.lock`.
  - [ ] UC-01 touches only `crates/keystone-ui/src/text_input.rs`, `crates/keystone-ui/src/lib.rs`, `crates/keystone-ui/Cargo.toml`, `crates/keystone-ui/tests/components.rs`, the `text_input` import lines of `crates/keystone-desktop/src/main.rs`, and `crates/keystone-desktop/src/text_input.rs` (deleted).
  - [ ] Desktop PRs touch only the `main.rs` regions they name, plus their own new modules and feature files under `crates/keystone-desktop/.agents/skills/verify/features/`.
  - [ ] Only UC-12 and UC-13 touch `crates/keystone-core/src/lib.rs`.
  - [ ] Only UC-14 touches `~/src/tGPUI` and the tGPUI `rev` pins in the Cargo manifests.
- [ ] Hold the review gate. UC-04, UC-06, UC-08, UC-12, UC-13, and UC-14 change an interaction. They wait for the operator's review in chat with screenshots and a video before merge.

### PR mechanics, for every PR

- [ ] Resolve the forge once. Default to `gh`; if `command -v origin` succeeds and Origin can resolve the repository, use `origin pr` for every PR operation. Record any fallback to `gh`. Never require `gt`.
- [ ] Open the PR ready, never draft, per **Opening a PR**. Use the run's built-in PR tool when it has one, else `origin pr create --status open --base <base-branch>` or `gh pr create --base <base-branch>` according to the resolved forge. A stack child targets its parent branch.
- [ ] Call `link_pull_request` with the PR URL right after it opens.
- [ ] Run `bash scripts/check.sh ui` once before the PR-facing push. Push with hooks on.
- [ ] Run `/deslop` before each commit and `/no-comments` before review.
- [ ] Triage every Bugbot and security-reviewer comment per `<plugin root>/skills/poteto-mode/references/bugbot-triage.md`.
- [ ] Rebase onto current trunk before the code-ready report and babysit. Keep that merge base in fix rounds. Rebase again only at merge prep, on a `git merge-tree` conflict with trunk, or on a CI failure that comes from a change on trunk.
- [ ] Put an Apache-2.0 attribution line in the `## Sources` section of `docs/design-system.md` for every file that borrows from gpui-component 0.7.3. Keep the upstream copyright header in a copied file and mark it modified.

### Verdict and merge, for every PR

- [ ] Pick the checks from **Verification tiers**. For a review-gated PR, a full swarm runs once, at the code-ready head. Another full swarm runs only for new behavior, a redesign, or a large new diff. A rebase, a merge-conflict resolution, or a minor fix gets a single verifier at the new head. Run each full swarm per `<plugin root>/skills/swarm/SKILL.md`. One gates lane. The ten live lanes from the PR's **Verify, live** block. The perf lane from its **Verify, perf** block. Two or more audit lanes, each with its own focus, that read the diff and the receipts and distrust the PR body. The root audits the receipts in the merge-ready report before the verdict.
- [ ] Clean only when every lane is `PASS`. Findings go back to the owner, including a defect that a lane filed as a note. A new head gets a fresh verdict, except for results that stay valid under the patch-id rule in `playbooks/shipping.md`.
- [ ] Append each clean PR to the stack in dependency order with its base set to its parent's branch (`gh pr edit <pr> --base <parent-branch>`). Only the root rebases or retargets a link. A rebase that keeps every commit's patch-id keeps the verdict. A changed patch-id needs a single verifier at the new head. The operator lands the stack bottom-up through `gh`, and only the contiguous verified run from the root lands.

### Boot recipe, for every live lane

Each live lane runs in its own worktree at the PR head. Drive through the control skill above.

- [ ] `git fetch origin <head-branch> && git checkout <head SHA>`.
- [ ] Build with `cargo build -p keystone-desktop -p keystone-agent --bins --locked --offline -j 2`. A gallery lane also runs `cargo build -p keystone-ui --example gallery --locked --offline -j 2`.
- [ ] Start the keystone-verify server with `python3 scripts/native-driver.py --driver ~/keystone/.audit/tools/computer-use-mcp`. A gallery lane adds `--binary target/debug/examples/gallery`.
- [ ] Call `start` with `env` set to the scripted providers, `KEYSTONE_CODEX_PATH=<worktree>/target/debug/keystone-fake-codex`, `KEYSTONE_CLAUDE_PATH=<worktree>/target/debug/keystone-fake-claude`, `KEYSTONE_PROJECT_DIR=<disposable git repo>`, and both fake delays at `90`. A file-tree lane points `KEYSTONE_PROJECT_DIR` at the fixture the lane names.
- [ ] Call `doctor`, then `screenshot`. Ready means `binary_unchanged:true`, a live tracked PID, and the welcome composer on screen, or the gallery's first page for a gallery lane.
- [ ] Deliver input only through `click`, `input`, `wait_for`, and `call`. `elements`, `workspace`, `snapshot`, and `doctor` are the read-only diagnostics. Scroll with the keyboard or a scrollbar drag, because driver wheel events reach no GPUI view (KS-012).
- [ ] Quit with Ctrl+Q through `input` before any restart. Call `stop` at the end and require an empty `errors` list.
- [ ] Save every screenshot to `/tmp/swarm-<pr-id>/worker-<n>/<slug>.png` and return the paths with the report.

## Move the text input into keystone-ui (UC-01)

Linear placeholder `TOB-12`. Playbook is Refactoring. Behavior does not change.

**Depends on.** None.

**Files.**

- [ ] Create `crates/keystone-ui/src/text_input.rs` with `git mv` from `crates/keystone-desktop/src/text_input.rs`, so history follows the file.
- [ ] Edit `crates/keystone-ui/src/lib.rs`.
- [ ] Edit `crates/keystone-ui/Cargo.toml`.
- [ ] Edit `crates/keystone-desktop/src/main.rs`, import lines and `mod text_input;` only.
- [ ] Edit `crates/keystone-ui/tests/components.rs`.
- [ ] Edit `docs/design-system.md`.
- [ ] Delete `crates/keystone-desktop/src/text_input.rs`.

**Build.**

- [ ] Move `TextInput`, `InputMode`, and their events unchanged. Replace `keystone_ui::` paths inside the file with `crate::`.
- [ ] Add `unicode-segmentation` to `crates/keystone-ui/Cargo.toml` at the version in `Cargo.lock`. Export `TextInput` and `InputMode` from `lib.rs` and its prelude.
- [ ] Add a `TextInput::on_key_down_capture` hook, or document that hosts catch Up, Down, and Enter with `capture_key_down`. The palette in UC-05 needs one of the two. Pick the smaller diff.
- [ ] Add a `TextInput` row to the Contents table in `docs/design-system.md`.

**You see.**

- [ ] `git diff -M --stat main` shows the file as a rename with a similarity above 95 percent.
- [ ] The composer, the sidebar search, and Settings behave as on trunk.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `crates/keystone-ui/tests/components.rs` gains `single_line_input_rejects_newline` and `single_line_input_reports_changed`. Run `cargo test -p keystone-ui --features ui-tests --locked`.
- [ ] The desktop's existing composer and search tests pass unchanged. Run `bash scripts/check.sh ui`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Type a 200-character draft in the welcome composer, quit with Ctrl+Q, relaunch, and read the restored draft, at trunk and head. Save `draft-restore.png`. Pass when both runs restore the same literal draft and `workspace` holds it.
- [ ] Lane 2. Type into the sidebar search, then press Enter. Save `search-enter.png`. Pass when no newline appears and the session list filters to the matching titles.
- [ ] Lane 3. Paste multi-line text into the search field through `input`. Save `search-paste.png`. Pass when the field shows one line and the app does not crash.
- [ ] Lane 4. Type a three-line draft with Shift+Enter in the composer. Save `composer-multiline.png`. Pass when three lines render and the composer grows.
- [ ] Lane 5. Select a word with double-click, then press Ctrl+C and paste it into search. Save `copy-paste.png`. Pass when search shows the word.
- [ ] Lane 6. Press Home and End in a wrapped composer line. Save `home-end.png`. Pass when the caret lands at the visual row start and end, as on trunk.
- [ ] Lane 7. Send a message to the fake Codex provider. Save `send.png`. Pass when the transcript shows the sent text and the composer clears.
- [ ] Lane 8. Type an emoji and a combining accent, then press Backspace twice. Save `graphemes.png`. Pass when each Backspace removes one whole grapheme.
- [ ] Lane 9. Switch theme and density in Settings, then type in the composer. Save `theme-input.png`. Pass when the input uses the new font and colors.
- [ ] Lane 10. Run `python3 scripts/native-stress.py --runs 3`. Save `stress.png` from the last run. Pass when every run exits 0 and the draft persists within 1 s.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Median `perf_sidebar_draw` and `perf_transcript_draw` times, and the KS-005 persist lag from `native-stress.py`.
- [ ] Probe. `python3 scripts/perf.py --no-log` three times each at trunk and head, interleaved trunk, head, trunk, head. Then `native-stress.py --runs 3` at each.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a head median exceeds the trunk median by more than 20 percent, which is the documented noise floor, or when any persist lag exceeds 1 s.

**Review gate.** None. UC-01 is not review-gated.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-01 to the stack on `main`, and the operator lands it bottom-up.

## Add a keyboard-driven virtual list (UC-02)

Linear placeholder `TOB-13`. Playbook is Feature. This is the row list that the palette, the select, and the tree share.

**Depends on.** None.

**Files.**

- [ ] Create `crates/keystone-ui/src/virtual_list.rs`.
- [ ] Edit `crates/keystone-ui/src/lib.rs`.
- [ ] Edit `crates/keystone-ui/src/popover.rs`, `MenuItem` only.
- [ ] Edit `crates/keystone-ui/examples/gallery.rs`.
- [ ] Edit `crates/keystone-ui/tests/components.rs`.
- [ ] Create `crates/keystone-ui/tests/perf_lists.rs`.
- [ ] Edit `docs/design-system.md`.

**Build.**

- [ ] Write `VirtualListState { count, highlighted: Option<usize>, scroll: UniformListScrollHandle }` with `move_by(delta)`, `move_to(index)`, `page(delta)`, and `set_count(n)`. A move clamps to the range and calls `scroll_to_item` with `ScrollStrategy::Nearest`.
- [ ] Write `VirtualList::new(id, &state, render_row)` as a `RenderOnce` over `uniform_list`. Up, Down, PageUp, PageDown, Home, and End move the highlight in key context `VirtualList`. Enter calls `on_confirm(index)`. Pointer hover moves the highlight.
- [ ] Add `MenuItem::highlighted(bool)`, which paints the same selection wash as hover.
- [ ] Call `keystone_ui::automation::frame_writer()` in the gallery root, so keystone-verify can name gallery elements. Add a gallery page with a 50,000-row list.
- [ ] Row debug selectors are `{id}-row-{index}`.

**You see.**

- [ ] The gallery's list page shows "50000 rows" and a highlighted first row. Holding Down moves the highlight and keeps it on screen.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `components.rs` gains `virtual_list_keys_move_highlight`, `virtual_list_clamps_at_ends`, `virtual_list_enter_confirms`, and `virtual_list_set_count_keeps_highlight_in_range`. Run `cargo test -p keystone-ui --features ui-tests --locked`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Trunk has no virtual list, so record that. Run the desktop journey from `features/sessions.md` at trunk and head to show the app is unchanged, and gate the gallery list at head. Save `sessions-regress.png`. Pass when both desktop runs match and the head gallery shows 50,000 rows.
- [ ] Lane 2. Open the gallery list page and press Down five times. Save `down5.png`. Pass when `{id}-row-5` carries the highlight.
- [ ] Lane 3. Press End. Save `end.png`. Pass when `{id}-row-49999` is visible and highlighted.
- [ ] Lane 4. Press Home after End. Save `home.png`. Pass when `{id}-row-0` is visible and highlighted.
- [ ] Lane 5. Press PageDown three times. Save `pagedown.png`. Pass when the highlighted row is visible and its index grew by about three pages.
- [ ] Lane 6. Hover row 3 with the pointer, then press Down. Save `hover-then-key.png`. Pass when row 4 is highlighted.
- [ ] Lane 7. Press Enter on row 12. Save `confirm.png`. Pass when the gallery's status line reads "confirmed 12".
- [ ] Lane 8. Switch density to Compact, then Spacious. Save `density.png`. Pass when row heights change and the highlight stays on the same index.
- [ ] Lane 9. Switch the gallery theme to Light. Save `light.png`. Pass when the highlight wash is visible against the light surface.
- [ ] Lane 10. Hold Down for 3 s through repeated `input` keypresses. Save `hold-down.png`. Pass when the app stays responsive and the highlighted row is on screen.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Desktop `perf_sidebar_draw` and `perf_transcript_draw` medians at trunk and head. For the work the diff adds, the p95 frame time of `perf_virtual_list_50k`, which sweeps `scroll_to_item` across 50,000 rows in `perf_lists.rs`.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Then `cargo test -p keystone-ui --features ui-tests --locked --release perf_virtual_list_50k -- --ignored --nocapture` five times at head.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a desktop median grows more than 20 percent over trunk. Fail when the `perf_virtual_list_50k` p95 exceeds 8 ms, half the 16.7 ms frame budget.

**Review gate.** None. UC-02 is not review-gated.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-02 to the stack on `main`, and the operator lands it bottom-up.

## Add switch, slider, and select controls (UC-03)

Linear placeholder `TOB-17`. Playbook is Feature. Borrow structure from gpui-component 0.7.3 `switch.rs`, `slider.rs`, and `select.rs` with attribution.

**Depends on.** UC-02.

**Files.**

- [ ] Create `crates/keystone-ui/src/switch.rs`.
- [ ] Create `crates/keystone-ui/src/slider.rs`.
- [ ] Create `crates/keystone-ui/src/select.rs`.
- [ ] Edit `crates/keystone-ui/src/lib.rs`.
- [ ] Edit `crates/keystone-ui/examples/gallery.rs`.
- [ ] Edit `crates/keystone-ui/tests/components.rs`.
- [ ] Edit `docs/design-system.md`.

**Build.**

- [ ] Write `Switch::new(id, on)` with `on_change(Fn(bool, &mut Window, &mut App))`. Space and Enter toggle it when focused. Disabled ignores clicks and shows a not-allowed cursor.
- [ ] Write `Slider::new(id, value, range, step)` with `on_change`. Dragging the thumb and pressing Left, Right, Home, and End change the value. The value snaps to `step`.
- [ ] Write `Select::new(id, options, selected)` that opens a `PopoverMenu` anchored to the trigger and lists options in a `VirtualList` from UC-02. Typing a letter jumps to the next option that starts with it. Escape closes and returns focus to the trigger.
- [ ] Selectors are `{id}` for the trigger, `{id}-option-{n}` for options, and `{id}-thumb` for the slider thumb.

**You see.**

- [ ] The gallery's controls page shows a switch, a slider with its value, and a select in every density and theme.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `components.rs` gains `switch_toggles_on_click_and_space`, `switch_disabled_ignores_click`, `slider_keys_step_and_clamp`, `slider_drag_snaps_to_step`, `select_opens_and_confirms`, `select_escape_restores_focus`, and `select_type_to_jump`. Run `cargo test -p keystone-ui --features ui-tests --locked`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Trunk has no controls page, so record that. Run the Settings menu journey in the desktop at trunk and head, and gate the gallery controls at head. Save `settings-regress.png`. Pass when the desktop runs match and the head gallery shows all three controls.
- [ ] Lane 2. Click the switch. Save `switch-on.png`. Pass when it shows on and the gallery status reads "switch true".
- [ ] Lane 3. Focus the switch with Tab and press Space. Save `switch-space.png`. Pass when it toggles.
- [ ] Lane 4. Click the disabled switch. Save `switch-disabled.png`. Pass when nothing changes.
- [ ] Lane 5. Drag `{id}-thumb` to the right end with a press, move, and release through `call`. Save `slider-drag.png`. Pass when the value reads the range maximum.
- [ ] Lane 6. Focus the slider and press Left three times. Save `slider-keys.png`. Pass when the value drops by three steps.
- [ ] Lane 7. Click the select and choose the third option. Save `select-pick.png`. Pass when the trigger shows the third option's label.
- [ ] Lane 8. Open the select and press Escape. Save `select-escape.png`. Pass when the menu closes and the trigger shows the focus ring.
- [ ] Lane 9. Open the select and type "m". Save `select-jump.png`. Pass when the first option starting with "m" is highlighted.
- [ ] Lane 10. Switch the gallery to Light and Compact. Save `controls-light-compact.png`. Pass when every control resizes and stays legible.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Desktop `perf_sidebar_draw` and `perf_transcript_draw` medians at trunk and head. For the diff-added work, the first-frame time of a `Select` that opens with 500 options, from `perf_select_open_500` in `perf_lists.rs`.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Then `cargo test -p keystone-ui --features ui-tests --locked --release perf_select_open_500 -- --ignored --nocapture` five times at head.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a desktop median grows more than 20 percent over trunk. Fail when the open first frame exceeds 16.7 ms at p95.

**Review gate.** None. UC-03 is not review-gated.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-03 to the stack, and the operator lands it bottom-up.

## Replace the Settings menu with a settings pane (UC-04)

Linear placeholder `TOB-21`. Playbook is Feature.

**Depends on.** UC-03.

**Files.**

- [ ] Create `crates/keystone-desktop/src/settings_pane.rs`.
- [ ] Edit `crates/keystone-desktop/src/settings.rs`.
- [ ] Edit `crates/keystone-desktop/src/main.rs`, only `settings_menu` near line 623, the `Keystone` fields for the pane, the action list near line 32, and `bind_keys` near line 4946.
- [ ] Create `crates/keystone-desktop/.agents/skills/verify/features/settings.md`.
- [ ] Edit `crates/keystone-desktop/.agents/skills/verify/features/README.md`.

**Build.**

- [ ] Write `SettingsPane`, a dialog-style overlay with a theme `Select` (Light, Dark, System if `Appearance` gains it), a UI font `Select` and a mono font `Select` fed by `font_choices`, a density segmented choice of three `Button`s, and a UI text size `Slider`. Each change calls `update_preferences`, which already persists and calls `set_theme`.
- [ ] Open it from the existing Settings button and with Ctrl+Comma. Escape and the close button dismiss it and return focus to the composer.
- [ ] Remove the old `settings_menu` popover in the same PR.
- [ ] Add a `settings` feature file with sub-features `settings-open`, `settings-theme`, `settings-font`, `settings-density`, and `settings-persist`.

**You see.**

- [ ] Ctrl+Comma opens a pane titled "Settings". Changing density resizes the sidebar rows at once, and the change survives a restart.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] A `ui-tests` test in `settings_pane.rs`, `settings_changes_persist_preferences`, asserts that each control writes `Preferences` through the store. Run `cargo test -p keystone-desktop --features ui-tests --locked settings_`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Switch to the Light theme and Compact density through Settings at trunk (popover) and head (pane), quit, and relaunch. Save `theme-density-restart.png`. Pass when both relaunches show Light and Compact and `workspace` holds the same `preferences`.
- [ ] Lane 2. Press Ctrl+Comma. Save `open-shortcut.png`. Pass when `settings-pane` is visible.
- [ ] Lane 3. Click the Settings button in the sidebar. Save `open-click.png`. Pass when `settings-pane` is visible.
- [ ] Lane 4. Pick a different UI font. Save `font.png`. Pass when the sidebar text renders in the new font.
- [ ] Lane 5. Pick a different mono font. Save `mono.png`. Pass when inline code in a fake-provider reply uses it.
- [ ] Lane 6. Drag the text size slider to its maximum. Save `size-max.png`. Pass when text grows and nothing clips in the sidebar at 1100 by 720.
- [ ] Lane 7. Press Escape. Save `escape.png`. Pass when the pane closes and typing goes to the composer.
- [ ] Lane 8. Open the pane with a draft in the composer, change theme, and close. Save `draft-kept.png`. Pass when the draft is unchanged.
- [ ] Lane 9. Use only Tab, Space, and arrows to change density. Save `keyboard-only.png`. Pass when density changes without a pointer event.
- [ ] Lane 10. Resize the window to 800 by 600 with the pane open. Save `small-window.png`. Pass when every control stays reachable.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. The time from a density change to the next completed frame, at trunk through the popover and at head through the pane, from a new ignored test `perf_preferences_apply`.
- [ ] Probe. At head, `cargo test -p keystone-desktop --features ui-tests --locked perf_preferences_apply -- --ignored --nocapture`. At trunk, the same test body applied as a temporary patch that drives `update_preferences` directly. Run five times each, interleaved. Also `python3 scripts/perf.py --no-log` three times each.
- [ ] Baseline. Record the trunk numbers first.
- [ ] Rule. Fail when the head median exceeds trunk by more than 20 percent on any metric.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 1, 2, 4, and 6 screenshots into `.audit/review/uc-04-review-<slug>.png`.
- [ ] Record a 30 to 60 second video of the change on a lane VM, opening the pane and changing each control. Save it as `.audit/review/uc-04-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-04 to the stack, and the operator lands it bottom-up after approving it.

## Add the command palette component (UC-05)

Linear placeholder `TOB-18`. Playbook is Feature.

**Depends on.** UC-01 and UC-02.

**Files.**

- [ ] Create `crates/keystone-ui/src/fuzzy.rs`.
- [ ] Create `crates/keystone-ui/src/command_palette.rs`.
- [ ] Edit `crates/keystone-ui/src/lib.rs`.
- [ ] Edit `crates/keystone-ui/examples/gallery.rs`.
- [ ] Edit `crates/keystone-ui/tests/components.rs`.
- [ ] Edit `crates/keystone-ui/tests/perf_lists.rs`.
- [ ] Edit `docs/design-system.md`.

**Build.**

- [ ] Write `fuzzy::score(query, candidate) -> Option<Match { score, positions }>`, a case-insensitive subsequence scorer that rewards word starts and consecutive runs. No crate in the offline registry provides one, and Zed's `fuzzy` crate is GPL.
- [ ] Write `CommandPaletteState` with an `Entity<TextInput>` in single-line mode, a `VirtualListState`, and `Vec<PaletteItem { id, label, detail, kbd }>`. Typing rescores and sorts. Matched characters render in `Color::Strong`.
- [ ] Write `CommandPalette::new(id, &state)` as a centered overlay. Up, Down, and Enter reach the list while the input keeps focus, through the hook UC-01 chose. Escape and a press outside call `on_dismiss`.
- [ ] Selectors are `{id}`, `{id}-input`, and `{id}-row-{n}`.

**You see.**

- [ ] The gallery opens a palette with 2,000 sample commands. Typing "nwss" puts "New session" first with its letters marked.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `fuzzy.rs` gains table tests for ordering, case, word starts, and no match. Run `cargo test -p keystone-ui --locked fuzzy`.
- [ ] `components.rs` gains `palette_filters_as_you_type`, `palette_arrows_move_while_input_focused`, `palette_enter_confirms_highlighted`, and `palette_escape_dismisses`. Run `cargo test -p keystone-ui --features ui-tests --locked palette_`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Trunk has no palette, so record that. Run the composer draft journey at trunk and head to show the moved input still works, and gate the gallery palette at head. Save `palette-regress.png`. Pass when both desktop runs match and the head palette filters.
- [ ] Lane 2. Open the gallery palette and type "nwss". Save `nwss.png`. Pass when "New session" is row 0 and its matched letters are bold.
- [ ] Lane 3. Type "zzzz". Save `empty.png`. Pass when the list shows "No matching commands".
- [ ] Lane 4. Type "set", press Down twice, then Enter. Save `confirm.png`. Pass when the status line names the third match.
- [ ] Lane 5. Press Escape. Save `dismiss.png`. Pass when the palette is gone.
- [ ] Lane 6. Click outside the palette. Save `outside.png`. Pass when the palette is gone.
- [ ] Lane 7. Type a query, press Backspace until empty. Save `backspace.png`. Pass when all 2,000 items return in their original order.
- [ ] Lane 8. Click row 4 with the pointer. Save `click-row.png`. Pass when the status line names row 4.
- [ ] Lane 9. Switch to Light and Spacious. Save `light-spacious.png`. Pass when the palette resizes and stays centered.
- [ ] Lane 10. Type 20 characters quickly through one `type` action. Save `fast-type.png`. Pass when the input shows all 20 and the list matches the final query.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Desktop `perf_sidebar_draw` and `perf_transcript_draw` medians at trunk and head. For the diff-added work, the time to rescore and sort 10,000 items per keystroke, from `perf_palette_filter_10k`.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Then `cargo test -p keystone-ui --features ui-tests --locked --release perf_palette_filter_10k -- --ignored --nocapture` five times at head.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a desktop median grows more than 20 percent over trunk. Fail when the rescore p95 exceeds 4 ms.

**Review gate.** None. UC-05 is not review-gated.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-05 to the stack, and the operator lands it bottom-up.

## Open the command palette in the app (UC-06)

Linear placeholder `TOB-22`. Playbook is Feature.

**Depends on.** UC-05.

**Files.**

- [ ] Create `crates/keystone-desktop/src/commands.rs`.
- [ ] Edit `crates/keystone-desktop/src/main.rs`, only the action list near line 32, `bind_keys` near line 4946, the action handlers in `render` near lines 2120 to 2160, and one new `Keystone` field.
- [ ] Create `crates/keystone-desktop/.agents/skills/verify/features/palette.md`.
- [ ] Edit `crates/keystone-desktop/.agents/skills/verify/features/README.md`.

**Build.**

- [ ] Write one table in `commands.rs` that lists each palette command with its label, its action, and its key binding. `bind_keys` and the palette both read that table, so a shortcut shown in the palette cannot drift from the real binding.
- [ ] Start with New Codex session, New Claude session, Toggle sidebar, Focus search, Open settings (if UC-04 has landed below it), Settle session, and Quit.
- [ ] Bind Ctrl+Shift+P and Ctrl+K under `!Dialog`. Confirming dispatches the action to the focused window. Dismissing returns focus to where it was.

**You see.**

- [ ] Ctrl+K opens the palette over the session view. Typing "new c" and Enter starts a Codex session.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `commands.rs` gains `every_command_has_a_bound_action` and `palette_kbd_matches_binding`. A `ui-tests` test `palette_new_session_creates_session` drives the palette. Run `cargo test -p keystone-desktop --features ui-tests --locked palette_`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Create a Codex session at trunk through the sidebar menu and at head through Ctrl+K then "new codex" and Enter. Save `new-session.png`. Pass when both show a new selected Codex session and `workspace` lists it.
- [ ] Lane 2. Press Ctrl+Shift+P. Save `open-p.png`. Pass when `palette` is visible with the input focused.
- [ ] Lane 3. Run Toggle sidebar from the palette. Save `toggle-sidebar.png`. Pass when the sidebar hides.
- [ ] Lane 4. Run Focus search. Save `focus-search.png`. Pass when typing lands in the sidebar search.
- [ ] Lane 5. Open the palette with a composer draft and press Escape. Save `escape-focus.png`. Pass when the composer has focus and the draft is unchanged.
- [ ] Lane 6. Open a confirm dialog, then press Ctrl+K. Save `dialog-blocks.png`. Pass when the palette does not open.
- [ ] Lane 7. Type "quit" and Enter. Save `quit.png` before the window closes. Pass when `doctor` reports exit code 0.
- [ ] Lane 8. Check that each row's shortcut hint matches by pressing that shortcut with the palette closed for two commands. Save `kbd-match.png`. Pass when both shortcuts do what their rows say.
- [ ] Lane 9. Run Settle session on a session that waits for approval in the fake provider. Save `settle-approval.png`. Pass when the session stays unsettled, as `features/settlement.md` requires.
- [ ] Lane 10. Open the palette at 800 by 600. Save `small.png`. Pass when the palette fits the window.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Desktop `perf_sidebar_draw` and `perf_transcript_draw` medians at trunk and head. For the diff-added work, the first frame after the palette opens, from `perf_palette_open`.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Then `cargo test -p keystone-desktop --features ui-tests --locked perf_palette_open -- --ignored --nocapture` five times at head.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a desktop median grows more than 20 percent over trunk. Fail when the palette's first frame exceeds 16.7 ms at p95.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 1, 2, 3, and 5 screenshots into `.audit/review/uc-06-review-<slug>.png`.
- [ ] Record a 30 to 60 second video of the change on a lane VM, opening the palette, filtering, and running two commands. Save it as `.audit/review/uc-06-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-06 to the stack, and the operator lands it bottom-up after approving it.

## Add the virtualized tree component (UC-07)

Linear placeholder `TOB-19`. Playbook is Feature. Borrow structure from gpui-component 0.7.3 `tree.rs` with attribution.

**Depends on.** UC-02.

**Files.**

- [ ] Create `crates/keystone-ui/src/tree.rs`.
- [ ] Edit `crates/keystone-ui/src/lib.rs`.
- [ ] Edit `crates/keystone-ui/examples/gallery.rs`.
- [ ] Edit `crates/keystone-ui/tests/components.rs`.
- [ ] Edit `crates/keystone-ui/tests/perf_lists.rs`.
- [ ] Edit `docs/design-system.md`.

**Build.**

- [ ] Model the tree as `TreeModel { nodes: Vec<Node { parent, depth, name, kind, expanded, child_range }>, visible: Vec<u32> }`. `visible` is the flat list of rows that `VirtualList` draws. Expanding a folder splices its visible descendants into `visible` in one `Vec::splice`. Collapsing removes them in one drain. No render path walks the whole tree.
- [ ] Children load lazily through a caller callback `on_expand(node_id)`. The model holds a `Loading` row until the caller fills it.
- [ ] Write `Tree::new(id, &state, render_row)` on `VirtualList`, which is `uniform_list` underneath. The prototype in Appendix A measured `uniform_list` at 1.05 to 1.41 ms CPU per frame at p50 on 50,000 rows, and about 20 percent cheaper than `list`. Left collapses or moves to the parent. Right expands or moves to the first child. Enter calls `on_open(node_id)`. Rows show a chevron, an icon, and the name indented by `depth`.
- [ ] Selectors are `{id}-row-{index}` and `{id}-chevron-{index}`.

**You see.**

- [ ] The gallery's tree page shows a 50,000-node sample tree. Expanding a folder with 10,000 children shows them at once.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `tree.rs` gains model tests `expand_splices_visible_rows`, `collapse_removes_descendants`, `nested_expand_restores_inner_state`, and `lazy_children_replace_loading_row`. `components.rs` gains `tree_left_right_navigate`. Run `cargo test -p keystone-ui --features ui-tests --locked tree`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Trunk has no tree, so record that. Run the sessions journey at trunk and head, and gate the gallery tree at head. Save `tree-regress.png`. Pass when both desktop runs match and the head tree renders.
- [ ] Lane 2. Click the chevron on the 10,000-child folder. Save `expand-big.png`. Pass when its children show below it and the app stays responsive.
- [ ] Lane 3. Press End after that expand. Save `end.png`. Pass when the last visible row shows.
- [ ] Lane 4. Press Left on a child row. Save `left-parent.png`. Pass when the highlight moves to its parent.
- [ ] Lane 5. Press Left on the expanded parent. Save `collapse.png`. Pass when its children disappear.
- [ ] Lane 6. Press Right on a collapsed folder twice. Save `right-twice.png`. Pass when it expands, then the highlight moves to its first child.
- [ ] Lane 7. Expand a nested folder, collapse its parent, and expand the parent again. Save `nested-state.png`. Pass when the nested folder is still expanded.
- [ ] Lane 8. Expand the lazy folder. Save `lazy.png`. Pass when a "Loading" row shows, then the children replace it.
- [ ] Lane 9. Press Enter on a file row. Save `open.png`. Pass when the status line names the file.
- [ ] Lane 10. Switch to Light and Compact. Save `light-compact.png`. Pass when indentation and chevrons stay aligned.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Desktop `perf_sidebar_draw` and `perf_transcript_draw` medians at trunk and head. For the diff-added work, the p95 frame time while sweeping the 50,000-node tree and the time to expand a 10,000-child folder, from `perf_tree_50k` and `perf_tree_expand_10k`.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Then both `perf_tree_` tests five times at head with `--release -- --ignored --nocapture`.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a desktop median grows more than 20 percent over trunk. Fail when the sweep p95 exceeds 8 ms or the expand exceeds 16.7 ms including its frame.

**Review gate.** None. UC-07 is not review-gated.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-07 to the stack, and the operator lands it bottom-up.

## Browse project files and open them in an external editor (UC-08)

Linear placeholder `TOB-23`. Playbook is Feature.

**Depends on.** UC-07.

**Files.**

- [ ] Create `crates/keystone-desktop/src/files.rs`.
- [ ] Edit `crates/keystone-desktop/src/main.rs`, only the sidebar column near line 506, the action list, `bind_keys`, and one new `Keystone` field.
- [ ] Edit `crates/keystone-desktop/Cargo.toml` to add `ignore` 0.4.33 from the local registry.
- [ ] Edit `Cargo.lock`.
- [ ] Create `crates/keystone-desktop/.agents/skills/verify/features/files.md`.
- [ ] Edit `crates/keystone-desktop/.agents/skills/verify/features/README.md`.
- [ ] Create `scripts/make-file-fixture.py`.

**Build.**

- [ ] Write `FileBrowser` over the selected session's project directory. Read one directory level at a time with `ignore::WalkBuilder` limited to depth 1 on a background executor, so `.gitignore` applies and the main thread never blocks on disk.
- [ ] Open a file with `$VISUAL`, then `$EDITOR` in a terminal-free spawn when it is a GUI editor, then `xdg-open`. Spawn detached so closing Keystone never kills the editor. Show an inline error row when the spawn fails.
- [ ] Add a row action Copy path. Add a Files toggle beside Sessions in the sidebar and a palette command when UC-06 is below it.
- [ ] Write `scripts/make-file-fixture.py <dir> <count>` to build a git repo with nested folders, a `.gitignore`, and `<count>` files.

**You see.**

- [ ] The sidebar's Files toggle shows the project tree. Enter on a file runs the external editor, and a fake editor script logs the path it received.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `files.rs` gains `lists_one_level_respecting_gitignore`, `editor_command_prefers_visual_then_editor_then_xdg_open`, and `spawn_failure_reports_error`. Run `cargo test -p keystone-desktop --locked files`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe. Every lane sets `VISUAL` to a script that appends its argument to `/tmp/swarm-uc-08/editor.log`.

- [ ] Lane 1. Regression lane against trunk. Trunk has no file browser, so record that. Run the sessions journey at trunk and head, and at head open `README.md` from the Files view. Save `open-readme.png`. Pass when both sessions runs match and `editor.log` ends with the absolute path of `README.md`.
- [ ] Lane 2. Build a 50,000-file fixture and toggle Files. Save `big-fixture.png`. Pass when the first level shows within 1 s and the window stays responsive.
- [ ] Lane 3. Expand a folder that `.gitignore` excludes. Save `ignored.png`. Pass when the ignored folder does not appear.
- [ ] Lane 4. Expand three nested folders with Right. Save `nested.png`. Pass when the third level shows.
- [ ] Lane 5. Copy path on a file, then paste into search. Save `copy-path.png`. Pass when search shows the absolute path. If KS-013 blocks the first copy, press any key first and record it.
- [ ] Lane 6. Set `VISUAL` to `/nonexistent`, unset `EDITOR`, and open a file. Save `spawn-error.png`. Pass when an error row names the failed command and the app stays up.
- [ ] Lane 7. Open a file, then quit Keystone with Ctrl+Q. Save `editor-survives.png`. Pass when the fake editor process is still running after Keystone exits.
- [ ] Lane 8. Switch to another session with a different project directory. Save `switch-project.png`. Pass when the tree shows that project's root.
- [ ] Lane 9. Toggle back to Sessions and to Files. Save `toggle-back.png`. Pass when the tree keeps its expanded folders.
- [ ] Lane 10. Add a file to the fixture on disk, then collapse and expand its folder. Save `refresh.png`. Pass when the new file shows.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Desktop `perf_sidebar_draw` and `perf_transcript_draw` medians at trunk and head. For the diff-added work, the time from the Files toggle to the first rows on screen on a 50,000-file fixture, and the longest main-thread frame during that load.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Then `cargo test -p keystone-desktop --features ui-tests --locked perf_files_first_rows -- --ignored --nocapture` five times at head on the fixture.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a desktop median grows more than 20 percent over trunk. Fail when first rows take more than 300 ms or any frame during the load exceeds 33 ms.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 1, 2, 6, and 9 screenshots into `.audit/review/uc-08-review-<slug>.png`.
- [ ] Record a 30 to 60 second video of the change on a lane VM, browsing the fixture and opening a file. Save it as `.audit/review/uc-08-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-08 to the stack, and the operator lands it bottom-up after approving it.

## Add the tab bar component (UC-09)

Linear placeholder `TOB-14`. Playbook is Feature. Borrow structure from gpui-component 0.7.3 `tab/tab.rs` and `tab/tab_bar.rs` with attribution.

**Depends on.** None.

**Files.**

- [ ] Create `crates/keystone-ui/src/tabs.rs`.
- [ ] Edit `crates/keystone-ui/src/lib.rs`.
- [ ] Edit `crates/keystone-ui/examples/gallery.rs`.
- [ ] Edit `crates/keystone-ui/tests/components.rs`.
- [ ] Edit `docs/design-system.md`.

**Build.**

- [ ] Write `TabBar::new(id)` with `Tab::new(id, label)` children, `selected`, a close button, a dirty or working dot, and overflow scrolling. The caller owns the order and the selection.
- [ ] Add drag reorder with `on_drag(DraggedTab { bar_id, index })` and `on_drop` on each tab and on the empty end of the bar. A drop calls `on_move(from, to)`. Draw an insertion marker while a tab is over a slot.
- [ ] Middle-click closes. Ctrl+Tab and Ctrl+Shift+Tab are left to the host.
- [ ] Selectors are `{id}-tab-{n}` and `{id}-tab-{n}-close`.

**You see.**

- [ ] The gallery's tabs page shows eight tabs. Dragging tab 1 after tab 4 reorders them.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `components.rs` gains `tab_click_selects`, `tab_close_button_closes`, `tab_middle_click_closes`, and `tab_drag_reorders`, using `simulate_mouse_down`, `simulate_mouse_move`, and `simulate_mouse_up`. Run `cargo test -p keystone-ui --features ui-tests --locked tab_`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Trunk has no tabs, so record that. Run the sessions journey at trunk and head, and gate the gallery tab bar at head. Save `tabs-regress.png`. Pass when both desktop runs match and the head shows eight tabs.
- [ ] Lane 2. Click tab 3. Save `select.png`. Pass when tab 3 shows selected.
- [ ] Lane 3. Click `{id}-tab-2-close`. Save `close.png`. Pass when seven tabs remain.
- [ ] Lane 4. Middle-click tab 5. Save `middle.png`. Pass when tab 5 closes.
- [ ] Lane 5. Drag tab 1 onto tab 4 through `call` press, move, and release. Save `reorder.png`. Pass when the order reads 2, 3, 4, 1.
- [ ] Lane 6. Drag the last tab to the first slot. Save `to-front.png`. Pass when it becomes first.
- [ ] Lane 7. Start a drag and release outside the bar. Save `cancel.png`. Pass when the order is unchanged.
- [ ] Lane 8. Add tabs until the bar overflows. Save `overflow.png`. Pass when the bar scrolls and the selected tab stays visible.
- [ ] Lane 9. Mark tab 2 working. Save `working-dot.png`. Pass when its dot shows in the accent color.
- [ ] Lane 10. Switch to Light and Spacious. Save `light-spacious.png`. Pass when tab heights follow density.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Desktop `perf_sidebar_draw` and `perf_transcript_draw` medians at trunk and head. For the diff-added work, the draw time of a 50-tab bar during a drag, from `perf_tab_bar_drag_50`.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Then `perf_tab_bar_drag_50` five times at head with `--release -- --ignored --nocapture`.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a desktop median grows more than 20 percent over trunk. Fail when the drag frame p95 exceeds 4 ms.

**Review gate.** None. UC-09 is not review-gated.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-09 to the stack, and the operator lands it bottom-up.

## Add the resizable split component (UC-10)

Linear placeholder `TOB-15`. Playbook is Feature. Borrow structure from gpui-component 0.7.3 `resizable.rs` with attribution.

**Depends on.** None.

**Files.**

- [ ] Create `crates/keystone-ui/src/split.rs`.
- [ ] Edit `crates/keystone-ui/src/lib.rs`.
- [ ] Edit `crates/keystone-ui/examples/gallery.rs`.
- [ ] Edit `crates/keystone-ui/tests/components.rs`.
- [ ] Edit `docs/design-system.md`.

**Build.**

- [ ] Write `SplitState { axis, fractions: Vec<f32>, min_px }` and `Split::new(id, &state)` with any number of children. Dragging a divider moves the fraction between its two neighbours and clamps both to `min_px`. Double-click on a divider resets to equal parts.
- [ ] A drop zone API, `Split::on_drop_edge(Fn(Edge, payload))`, lets the host accept a dragged tab on a pane edge. It paints a half-pane highlight while a compatible drag hovers.
- [ ] Selectors are `{id}-divider-{n}` and `{id}-pane-{n}`.

**You see.**

- [ ] The gallery's split page shows three panes. Dragging a divider resizes its neighbours only.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `split.rs` gains `drag_moves_only_neighbours`, `drag_clamps_to_min`, and `double_click_resets`. `components.rs` gains `split_drop_edge_reports_edge`. Run `cargo test -p keystone-ui --features ui-tests --locked split`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Trunk has no split, so record that. Run the sessions journey at trunk and head, and gate the gallery split at head. Save `split-regress.png`. Pass when both desktop runs match and the head shows three panes.
- [ ] Lane 2. Drag `{id}-divider-0` 200 px right. Save `drag-right.png`. Pass when pane 0 widens by about 200 px and pane 2 keeps its width.
- [ ] Lane 3. Drag a divider past the window edge. Save `clamp.png`. Pass when the neighbour stops at its minimum width.
- [ ] Lane 4. Double-click a divider. Save `reset.png`. Pass when the panes are equal.
- [ ] Lane 5. Switch the gallery split to vertical and drag. Save `vertical.png`. Pass when heights change.
- [ ] Lane 6. Drag a gallery tab over the right edge of pane 1. Save `edge-highlight.png`. Pass when the right half of pane 1 highlights.
- [ ] Lane 7. Drop it there. Save `edge-drop.png`. Pass when the status line reads "drop right pane 1".
- [ ] Lane 8. Resize the window to 800 by 600. Save `small.png`. Pass when every pane keeps at least its minimum size.
- [ ] Lane 9. Hover a divider. Save `cursor.png`. Pass when the screenshot shows the resize cursor or the divider hover wash.
- [ ] Lane 10. Switch to Light. Save `light.png`. Pass when dividers use the hairline token.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Desktop `perf_sidebar_draw` and `perf_transcript_draw` medians at trunk and head. For the diff-added work, the frame time during a divider drag over three panes, from `perf_split_drag`.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Then `perf_split_drag` five times at head with `--release -- --ignored --nocapture`.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a desktop median grows more than 20 percent over trunk. Fail when the drag frame p95 exceeds 8 ms.

**Review gate.** None. UC-10 is not review-gated.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-10 to the stack, and the operator lands it bottom-up.

## Move session ownership out of the window view (UC-11)

Linear placeholder `TOB-16`. Playbook is Refactoring. Behavior does not change. This PR makes a second window possible.

**Depends on.** TOB-11 merged to `main`.

**Files.**

- [ ] Create `crates/keystone-desktop/src/app_state.rs`.
- [ ] Edit `crates/keystone-desktop/src/main.rs`, the `Keystone` struct near line 115, `save_before_exit` near line 342, the Quit handlers near lines 2133 and 5003, and `main` near lines 4980 to 5030.

**Build.**

- [ ] Move `store`, the `codex` and `claude` connections, `conversations`, and `logs` out of `Keystone` into one `Entity<AppState>` that `main` creates before any window. `Keystone` keeps a handle and its per-window view state, such as the composer, search, menus, and the transcript list.
- [ ] Make one Quit path. The global `Quit` action calls `AppState::save` then `cx.quit()`. Delete the view-level duplicate.
- [ ] Set `QuitMode::Explicit` with `set_quit_mode`. In this PR the single window's close still calls Quit, so behavior matches trunk.

**You see.**

- [ ] Every existing journey behaves as on trunk. `grep -n "codex\|claude\|conversations" crates/keystone-desktop/src/main.rs` shows only handles into `AppState`.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Existing desktop `ui-tests` pass unchanged. A new test, `quit_saves_once`, asserts one snapshot write per Quit. Run `bash scripts/check.sh ui`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Send a message to fake Codex, wait for the streamed reply, quit with Ctrl+Q, and relaunch, at trunk and head. Save `conversation-restore.png`. Pass when both relaunches restore the same transcript and `doctor` shows exit code 0.
- [ ] Lane 2. Run the `features/conversations.md` approval journey. Save `approval.png`. Pass when it matches the feature file's expected outcome.
- [ ] Lane 3. Steer a running fake Claude reply. Save `steer.png`. Pass when the steer appears in the transcript.
- [ ] Lane 4. Stop a running reply. Save `stop.png`. Pass when the reply ends and the session shows settled-ready state.
- [ ] Lane 5. Close the window through the window close path (`call` the compositor's close on the window). Save `close-window.png`. Pass when the app exits 0 and the state file holds the last draft.
- [ ] Lane 6. Run the drafts journey across two sessions. Save `drafts.png`. Pass when each session keeps its own draft after restart.
- [ ] Lane 7. Run the settlement journey. Save `settle.png`. Pass when it matches `features/settlement.md`.
- [ ] Lane 8. Run the search journey. Save `search.png`. Pass when it matches `features/search.md`.
- [ ] Lane 9. Start a reply and quit mid-stream. Save `quit-midstream.png`. Pass when the fake provider process has exited after quit and relaunch shows the interrupted turn.
- [ ] Lane 10. Run `python3 scripts/native-stress.py --runs 3`. Save `stress.png`. Pass when every run exits 0 and persists within 1 s.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Median `perf_sidebar_draw` and `perf_transcript_draw` times, and the persist lag from `native-stress.py`.
- [ ] Probe. `python3 scripts/perf.py --no-log` three times each at trunk and head, interleaved. Then `native-stress.py --runs 3` at each.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a head median exceeds trunk by more than 20 percent, or when any persist lag exceeds 1 s.

**Review gate.** None. UC-11 is not review-gated.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Interrogate panel run with no open Act on finding, because this PR changes process teardown and save-on-quit.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-11 to the stack, and the operator lands it bottom-up.

## Show sessions in tabs and split panes (UC-12)

Linear placeholder `TOB-20`. Playbook is Feature.

**Depends on.** UC-09, UC-10, and UC-11.

**Files.**

- [ ] Edit `crates/keystone-core/src/lib.rs`.
- [ ] Create `crates/keystone-desktop/src/layout.rs`.
- [ ] Edit `crates/keystone-desktop/src/main.rs`, only the region between `top_bar` and `main` near lines 2161 to 2166, the action list, `bind_keys`, and the `Keystone` fields for layout.
- [ ] Create `crates/keystone-desktop/.agents/skills/verify/features/tabs.md`.
- [ ] Edit `crates/keystone-desktop/.agents/skills/verify/features/README.md`.

**Build.**

- [ ] Add `Workspace::layout: Layout` with `#[serde(default)]`. `Layout { windows: Vec<WindowLayout> }`, `WindowLayout { root: PaneNode, active_pane }`, and `PaneNode = Split { axis, fractions, children } | Pane { tabs: Vec<SessionId>, active }`. A session id appears in at most one pane. Add `Store` setters that go through `commit`. An old state file loads as one window with one pane that holds the selected session.
- [ ] Render the active window's `PaneNode` with `Split` and one `TabBar` per pane. Each pane shows its active session's transcript and composer.
- [ ] Selecting a session in the sidebar opens it as a tab in the active pane, or focuses its existing tab.
- [ ] Drag a tab within a bar to reorder, to another pane's bar to move, and to a pane edge to split. Closing a pane's last tab removes the pane and collapses its split.
- [ ] Bind Ctrl+T new tab, Ctrl+W close tab, Ctrl+Tab and Ctrl+Shift+Tab to cycle, and Ctrl+Backslash to split right. Add these to the palette table when UC-06 is below.

**You see.**

- [ ] Two sessions open side by side in a vertical split, each with its own tab bar. After a restart the same split, tabs, and active tabs come back.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `keystone-core` gains `layout_old_state_loads_single_pane`, `layout_session_in_one_pane_only`, `close_last_tab_collapses_split`, and `layout_round_trips_through_snapshot`. Run `cargo test -p keystone-core --locked layout`.
- [ ] `layout.rs` gains `ui-tests` `drag_tab_to_edge_splits` and `sidebar_select_focuses_existing_tab`. Run `cargo test -p keystone-desktop --features ui-tests --locked layout`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Trunk has no tabs, so record that. Load a trunk state file at head and check it opens the selected session in one pane, then create two sessions and restart. Save `old-state.png`. Pass when the trunk state loads without loss and `workspace` gains a `layout` with one pane.
- [ ] Lane 2. Open two sessions and press Ctrl+Backslash. Save `split-right.png`. Pass when two panes show two different sessions.
- [ ] Lane 3. Drag a tab to the right edge of its own pane. Save `drag-split.png`. Pass when a new pane holds that tab.
- [ ] Lane 4. Drag a tab from pane 1's bar to pane 2's bar. Save `move-pane.png`. Pass when pane 2 lists it and pane 1 no longer does.
- [ ] Lane 5. Close the last tab in pane 2 with Ctrl+W. Save `collapse.png`. Pass when the split collapses to one pane.
- [ ] Lane 6. Quit and relaunch with two panes and three tabs. Save `restore.png`. Pass when panes, tab order, and active tabs match and each restored composer keeps its draft.
- [ ] Lane 7. Click a sidebar session that is already open in pane 2. Save `focus-existing.png`. Pass when pane 2 activates that tab and no duplicate opens.
- [ ] Lane 8. Send a message in pane 1 while pane 2 streams a fake Claude reply. Save `two-streams.png`. Pass when both transcripts update.
- [ ] Lane 9. Drag the split divider, quit, and relaunch. Save `fractions.png`. Pass when the divider position comes back.
- [ ] Lane 10. Cycle tabs with Ctrl+Tab three times. Save `cycle.png`. Pass when the active tab advances and wraps.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Median `perf_sidebar_draw` and `perf_transcript_draw` at trunk and head with one pane. For the diff-added work, the first frame after a tab switch and after a split, from `perf_tab_switch` and `perf_split_open`, and the launch-to-first-frame time with a three-pane layout.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Then both new `perf_` tests five times at head. Time launch to first `elements` frame with keystone-verify five times at trunk with one session and at head with three panes.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a one-pane median grows more than 20 percent over trunk. Fail when a tab switch or split first frame exceeds 16.7 ms at p95. Fail when three-pane launch exceeds the trunk launch by more than 150 ms.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 2, 3, 4, and 6 screenshots into `.audit/review/uc-12-review-<slug>.png`.
- [ ] Record a 30 to 60 second video of the change on a lane VM, splitting, dragging tabs between panes, and restarting. Save it as `.audit/review/uc-12-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Interrogate panel run with no open Act on finding, because this PR changes the persisted state format.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-12 to the stack, and the operator lands it bottom-up after approving it.

## Open multiple windows that outlive their work (UC-13)

Linear placeholder `TOB-24`. Playbook is Feature.

**Depends on.** UC-12.

**Files.**

- [ ] Edit `crates/keystone-desktop/src/main.rs`, only `main` near lines 4998 to 5030, the action list, and `bind_keys`.
- [ ] Edit `crates/keystone-desktop/src/layout.rs`.
- [ ] Edit `crates/keystone-desktop/src/app_state.rs`.
- [ ] Edit `crates/keystone-ui/src/automation.rs`.
- [ ] Edit `scripts/native-driver.py` if `elements` needs a window argument.
- [ ] Edit `crates/keystone-desktop/.agents/skills/verify/features/tabs.md`.
- [ ] Edit `crates/keystone-desktop/.agents/skills/verify/SKILL.md`.

**Build.**

- [ ] Open one GPUI window per `WindowLayout` at launch. Ctrl+Shift+N opens a new window with an empty pane.
- [ ] Move a tab to another window in two ways that need no fork change. Dropping a tab outside its window opens a new window that holds it, because the source window receives that mouse-up with the drag still active (Appendix A). A palette command "Move tab to window" lists open windows. Dragging a tab onto another window lands in UC-14.
- [ ] Closing a window removes its `WindowLayout` and keeps its sessions running in `AppState`. Those sessions stay in the sidebar of the remaining windows. Closing the last window keeps the process alive only while a turn runs, then exits after saving. Explicit Quit saves and ends everything.
- [ ] Make the automation frame file per window. `RECORDER` keys geometry by window id, and the frame file lists windows with their elements, so lanes can name elements in either window.

**You see.**

- [ ] Two Keystone windows show different sessions. Closing one while its fake reply streams leaves the reply finishing in the other window's sidebar.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] `layout.rs` gains `close_window_keeps_sessions`, `detach_tab_creates_window`, and `restore_opens_each_window`. `automation.rs` gains `frame_file_lists_each_window`. Run `bash scripts/check.sh ui`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Trunk has one window, so record that. At trunk and head, start a fake Codex reply with a 3 s delay and quit with Ctrl+Q. At head also close a second window mid-reply. Save `close-keeps-work.png`. Pass when Quit ends both runs with exit 0, and at head the reply finishes after its window closes and shows in the remaining window.
- [ ] Lane 2. Press Ctrl+Shift+N. Save `new-window.png`. Pass when `windows_list` shows two Keystone windows from one PID.
- [ ] Lane 3. Move a tab to the second window with the palette command "Move tab to window". Save `move-tab.png`. Pass when the second window shows the tab and the first does not.
- [ ] Lane 4. Resize the window to 800 by 600 so desktop space is free, drag a tab outside both windows, and release. Save `detach.png`. Pass when a third window holds the tab.
- [ ] Lane 5. Quit with two windows and relaunch. Save `restore-two.png`. Pass when two windows come back with their panes and tabs.
- [ ] Lane 6. Close the last window while a reply runs. Save `last-window.png` before closing. Pass when the process exits 0 only after the reply finishes and the state file holds the finished reply.
- [ ] Lane 7. Close the last window with nothing running. Save `last-idle.png` before closing. Pass when the process exits 0 within 2 s.
- [ ] Lane 8. Type a draft in window 2 and switch focus to window 1. Save `draft-per-window.png`. Pass when typing in window 1 does not change window 2's draft.
- [ ] Lane 9. Change the theme in window 1. Save `theme-both.png`. Pass when both windows redraw in the new theme.
- [ ] Lane 10. Press a key in window 2 right after it opens, then run `doctor`. Save `ks009.png`. Pass when no keymap panic appears in `app-NN.stderr.log` (KS-009).

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Median `perf_sidebar_draw` and `perf_transcript_draw` at trunk and head with one window. For the diff-added work, the time from Ctrl+Shift+N to the new window's first frame, and launch-to-first-frame with two restored windows.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Time the new window and the two-window launch with keystone-verify five times at head, and the one-window launch five times at trunk, interleaved.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a one-window median grows more than 20 percent over trunk. Fail when a new window takes more than 300 ms to its first frame. Fail when a two-window launch exceeds the trunk launch by more than 150 ms.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 1, 2, 3, and 5 screenshots into `.audit/review/uc-13-review-<slug>.png`.
- [ ] Record a 30 to 60 second video of the change on a lane VM, opening a window, moving a tab, closing a window mid-reply, and restarting. Save it as `.audit/review/uc-13-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Interrogate panel run with no open Act on finding, because this PR changes process lifetime.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-13 to the stack, and the operator lands it bottom-up after approving it.

## Drag tabs between windows (UC-14)

Linear placeholder `TOB-25`. Playbook is Feature. This PR has two parts, a tGPUI fork commit and a Keystone pin bump with the wiring. The operator pushes the fork commit, because the fork is private.

**Depends on.** UC-13.

**Files.**

- [ ] Edit `~/src/tGPUI/crates/gpui-pre/src/window.rs`, around `promote_external_drag_to_platform` near line 5765.
- [ ] Edit `~/src/tGPUI/crates/gpui-pre/src/elements/div.rs`, around `external_drag_payload` near line 640.
- [ ] Edit `~/src/tGPUI/crates/gpui-pre-linux/src/linux/wayland/client.rs`, around `start_drag` near lines 660 to 697 and the drop handler near line 2893.
- [ ] Edit the tGPUI `rev` in `crates/keystone-ui/Cargo.toml`, `crates/keystone-desktop/Cargo.toml`, and `Cargo.lock`.
- [ ] Edit `crates/keystone-desktop/src/layout.rs`.
- [ ] Edit `crates/keystone-desktop/.agents/skills/verify/features/tabs.md`.

**Build.**

- [ ] In tGPUI, add an in-app drag payload. When a drag that carries it leaves its window, offer a private MIME type such as `application/x-gpui-app-drag` with an opaque id instead of a file list. Keep the typed payload in `App`. A drop of that MIME type in a window of the same process hands back the original payload through `on_drop`. Other apps see no file list and cannot accept it.
- [ ] Give the Wayland `start_drag` an icon surface drawn from the drag preview, so the preview stays visible after the drag leaves its window.
- [ ] Commit the fork change as one commit on tGPUI with its own test in `gpui-pre`, then bump Keystone's pinned `rev`.
- [ ] In `layout.rs`, accept a `DraggedTab` from another window on any tab bar and pane edge. Keep detach-on-drop-outside from UC-13.

**You see.**

- [ ] Dragging a tab from window 1 onto window 2's tab bar moves it, and the drag preview follows the pointer the whole way.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] tGPUI gains a `gpui-pre` test, `app_drag_round_trips_typed_payload`. Run `cargo test -p gpui-pre --locked app_drag` in `~/src/tGPUI`.
- [ ] `layout.rs` gains `cross_window_drop_moves_tab`. Run `bash scripts/check.sh ui`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `claudeAgent/claude-opus-5-5 effort=medium` at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Drag a tab from window 1 onto window 2's tab bar at trunk (UC-13 below it) and at head. Save `cross-drop.png`. Pass when trunk leaves the tab in window 1 as the prototype recorded, and head moves it to window 2.
- [ ] Lane 2. Drag a tab onto the right edge of a pane in window 2. Save `cross-split.png`. Pass when window 2 splits and the new pane holds the tab.
- [ ] Lane 3. Take a screenshot mid-drag over window 2. Save `preview.png`. Pass when the tab preview is visible under the pointer.
- [ ] Lane 4. Drag a tab out of window 1 and release over empty desktop. Save `detach-still.png`. Pass when a new window holds the tab, as in UC-13.
- [ ] Lane 5. Drag a tab into window 2 and back into window 1. Save `round-trip.png`. Pass when it ends in window 1 at the drop slot.
- [ ] Lane 6. Drag the only tab of window 2 into window 1. Save `empty-source.png`. Pass when window 2's empty pane remains or the window closes as UC-13's rules say, and no session is lost.
- [ ] Lane 7. Drop a real file from a file manager or `wl-copy` drag source onto Keystone. Save `foreign-file.png`. Pass when Keystone ignores it without a crash.
- [ ] Lane 8. Quit after a cross-window move and relaunch. Save `restore.png`. Pass when the tab restores in window 2.
- [ ] Lane 9. Drag a tab whose session is streaming a fake reply into window 2. Save `streaming-move.png`. Pass when the reply keeps streaming in window 2.
- [ ] Lane 10. Run `doctor` after all drags. Save `doctor.png`. Pass when no panic appears in `app-NN.stderr.log`.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Median `perf_sidebar_draw` and `perf_transcript_draw` at trunk and head. For the diff-added work, the time from release over window 2 to window 2's first frame that shows the moved tab.
- [ ] Probe. `python3 scripts/perf.py --no-log` interleaved three times at trunk and head. Time five cross-window drops at head from the driver's release call to the first `elements` frame that lists the tab in window 2.
- [ ] Baseline. Record the trunk medians first.
- [ ] Rule. Fail when a median grows more than 20 percent over trunk. Fail when a drop takes more than 100 ms to show.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 1, 2, 3, and 4 screenshots into `.audit/review/uc-14-review-<slug>.png`.
- [ ] Record a 30 to 60 second video of the change on a lane VM, dragging tabs between two windows. Save it as `.audit/review/uc-14-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click, and for the operator to push the tGPUI commit.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] The tGPUI commit is on the fork's remote and the pinned `rev` resolves with `cargo fetch --locked`.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] The root appends UC-14 to the stack, and the operator lands it bottom-up after approving it.

## Close the program

- [ ] Every box above is checked with its evidence.
- [ ] `features/README.md` no longer lists split panes as future scope.
- [ ] Every Linear placeholder is replaced by its TOB id and each issue links its PR.
- [ ] Reply to the operator with the report the execution playbook names.

## Appendix A. Prototype evidence

Both prototypes are throwaway scratch crates outside git, so they have no branch or SHA. They built against tGPUI rev `766668f`, the rev Keystone pins, and ran on the keystone-verify headless Sway.

**`uniform_list` keeps 60 fps on a 50,000-row tree.** Scratch is `/tmp/proto-tree-50k` (`src/main.rs`, `res-uniform-*.txt`, `res-list-*.txt`, `shot-uniform.png`, `shot-list.png`). The run used a flat `Vec` of 50,000 rows, scrolled the whole list in 5 s, expanded a 10,000-child folder at 2.5 s, and collapsed it at 4.0 s. It did five alternating runs per element and dropped the first run of each, which the screenshot capture disturbed.

- Frame interval sat at 16.4 to 16.5 ms p50 for both elements. Sway's headless output paces frame callbacks at about 16.4 ms, so the interval measures the compositor, not GPUI.
- CPU from render start to paint end was 1.05 to 1.41 ms p50 and 3.80 ms worst p99 for `uniform_list`, and 1.36 to 1.45 ms p50 for `list`.
- The expand frame cost 2.0 to 2.6 ms CPU, and the collapse frame 1.2 to 1.7 ms. No frame near either toggle passed 16.7 ms.
- The plan picks `uniform_list` for UC-02 and UC-07. It is about 20 percent cheaper, its height is exact, and it reads rows straight from the `visible` vector without a second count to keep in step.

**Cross-window tab drag works only through the fork's Wayland platform drag.** Scratch is `/tmp/proto-tab-drag` (`log.txt`, `shots/plain/`, `shots/external/`, and `.audit/verify-20261016T151200-925250/` and `.audit/verify-20261016T151220-929767/` under it).

- In-window reorder passed, logged as `DROP-REORDER win=0 ... to_ix=1 via=gpui-drag`.
- In-window drop on a split zone passed, logged as `DROP-SPLIT win=0 ... via=gpui-drag`.
- Cross-window drop with plain `on_drag` failed. Wayland's implicit grab keeps the pointer on the source window, which logged `MOUSE-UP-OUTSIDE win=0 ... has_drag=true`, and window 2 logged nothing.
- Cross-window drop through `.external_drag_payload(...)` passed, logged as `PROMOTE-TO-PLATFORM win=0` then `DROP-SPLIT win=1 ... via=wl-data-device`. The tab travelled as a fake file path that any app could accept, and the preview vanished after it left the window.
- The plan ships in-window drag in UC-09 and UC-12, detach and a palette command in UC-13, and a typed in-app drag payload as a tGPUI change in UC-14.

**Still unproven.**

- Real Hyprland, X11, and macOS for either question. Both ran on headless Sway only.
- GPU present cost and refresh rates above 60 Hz.
- A drop outside both windows, because the two tiled windows filled the headless output. UC-13 lane 4 covers it.
- Native wheel scrolling, because of KS-012.
- Row cost with icons and git decorations, which the tree prototype did not draw.

## Appendix B. Alternatives rejected

- **Adopt gpui-kit.** The trial rejected it (draft PR #10, `docs/research/gpui-kit-trial.md` on `origin/gpui-kit-trial`). This plan borrows its Apache-2.0 source file by file, with attribution, and keeps Keystone's own tokens.
- **Use Zed's `ui`, `picker`, `fuzzy`, or `workspace` crates.** They are GPL-3.0-or-later, and Keystone does not take GPL code.
- **Build the palette on `PopoverMenu` and `MenuItem` as they stand.** `PopoverMenu` has no keyboard navigation, no focus, and no scrolling, and builds every child eagerly. UC-02 keeps `MenuItem`'s look and adds a highlighted state instead.
- **Add a fuzzy-matching crate.** None is in the offline registry, the builds run with `--offline`, and the palette needs only a few hundred lines of subsequence scoring.
- **Render the file tree as nested elements.** Nested flex nodes are what made taffy re-measure subtrees (see the tGPUI fork notes). A flat `visible` list on `uniform_list` keeps layout cost proportional to the rows on screen.
- **Cross-window drag with the file-list payload the fork already has.** It passed in the prototype, but any app could accept the fake file and the drag preview vanished. UC-14 adds a private MIME type instead.
- **Open a second window before moving session ownership.** Providers live inside the window's `Keystone` view today, so closing that window would kill running work, which scope.md forbids. UC-11 moves them first.
- **One PR for tabs, splits, and windows.** Each part has its own failure mode. Tabs fail on drag, splits on resize, and windows on process lifetime. Separate PRs give each one its own evidence.
- **Ship the settings controls inside the settings pane PR.** The select is also the base for later pickers, so it lands with its own gallery evidence first.

## Appendix C. Risks

- **TOB-11 collides with UC-11.** TOB-11 edits the Markdown cache inside `Keystone` in `main.rs`. UC-11 moves fields out of the same struct. UC-11 waits for TOB-11 to merge, and its owner rebases once at start.
- **`main.rs` is 5,000 lines and most desktop PRs edit it.** The stack is linear, each PR names its regions, and the root rebases links bottom-up. Owners watch for conflicts in the action list and `bind_keys`, which five PRs touch.
- **KS-012, driver wheel events reach no GPUI view.** Scroll lanes use keys or scrollbar drags. Native wheel scrolling of the tree and the palette stays unproven until KS-012 is fixed.
- **KS-013, click-only copy fails before the first key press.** It touches Copy path in UC-08. The lane records it rather than hiding it.
- **KS-009, a keymap panic on Wayland.** A second window may reach the same code path. UC-13 lane 10 watches for it.
- **The automation frame file is single-window.** `RECORDER` is a thread-local that one window's flush writes in full. UC-13 changes it, and until then lanes in UC-13 may see one window's elements only.
- **Drag through the driver.** Live drag lanes in UC-09, UC-10, UC-12, UC-13, and UC-14 use the computer-use driver's press, move, and release through `call`. The tab-drag prototype drove drags this way on headless Sway, so the method works there.
- **The tGPUI change in UC-14 is private and upstream does not have it.** Each future tGPUI rebase must carry the commit. The UC-14 owner adds it to the fork's patch list next to the KS-009 and taffy commits.
- **Review video capture on headless Sway.** The driver saves screenshots, not video. Owners record with `wf-recorder` on the nested output, or build the video from the lane's numbered screenshots with `ffmpeg`. The owner states which.
- **Verification runs render the sidebar uncached.** Perf numbers come from the `perf_` tests and `scripts/perf.py`, never from live lanes, per the verify skill.
- **External editor spawning.** `$EDITOR` is often a terminal editor. UC-08 spawns it only when it is a GUI command, and otherwise falls back to `xdg-open`. Owners watch for a terminal editor that hangs a detached spawn.
- **Restoration format.** UC-12 adds `layout` to the state file. An old file must load, and a new file must not break an older build that ignores unknown fields. The UC-12 owner checks that `Workspace` does not deny unknown fields.

## Appendix D. Links and reading list

- `docs/scope.md` lines 9, 13, and 17 for the product requirements.
- `docs/design-system.md` for the component rules and the Sources section.
- `docs/issues.md` for KS-009, KS-012, and KS-013.
- `docs/performance.md` and `scripts/perf.py` for the perf method and the 20 percent noise floor.
- `crates/keystone-desktop/.agents/skills/verify/SKILL.md` and `features/README.md` for live lanes.
- gpui-component 0.7.3 source at `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-component-0.7.3/src/` for `tab/`, `resizable.rs`, `select.rs`, `switch.rs`, `slider.rs`, and `tree.rs`.
- tGPUI at `~/src/tGPUI/crates/gpui-pre/src`, `elements/uniform_list.rs`, `elements/list.rs`, `elements/div.rs` for drag and drop, and `app.rs` for `active_drag`, `open_window`, and `QuitMode`.
- `<plugin root>/skills/how/SKILL.md` before UC-02, UC-11, UC-12, and UC-13, which set shapes later PRs build on.
- `<plugin root>/skills/interrogate/SKILL.md` for UC-11, UC-12, UC-13, and UC-14, which change process teardown, the state file, process lifetime, and the GPUI fork. The other PRs get one short diff-focused review, per the operator's preference for lighter reviews on small changes.
- The decision trail per `<plugin root>/skills/show-me-your-work/SKILL.md` lives at `.audit/ui-components.trail.md` and stays local.
