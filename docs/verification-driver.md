# Native verification driver proof

For the corrected reusable wheel path, two-pane/composer proof and the explicit Sway maximize and keyboard Jump limitations, see the [2026-10-19 receipt](verification/native-wheel-main-3fe5ff6/README.md). Older runs below describe their own historical builds and drivers.

The project-local [verify skill](../crates/keystone-desktop/.agents/skills/verify/SKILL.md) drives the real Rust GPUI executable through computer-use-mcp and a private Sway desktop. It includes four feature maps for sessions, drafts and restart, settlement, and search. The `keystone-verify` MCP server in `.mcp.json` wraps the driver, records evidence, and resolves named elements.

## Verified journey

The clean run in `.audit/verify-nested-003/` exited 0. The app build SHA-256 was `11e764293c0e7026e1b6b3cf8ff8f61381bdbb17778506cb61ca71b06ddc2b34`. The driver came from upstream commit `7acd4518c0028f034e33dfd20bb5b159adf15be5`. Its local binary SHA-256 was `df8c5441065789bd8aa36557cb648ae24f0d8a33996380d39688a8afce7bd6ea`.

1. Launched from an empty disposable workspace. Doctor matched the live process executable to the build hash. Screenshot `0001.png` shows the empty sidebar and centered welcome composer.
2. Clicked the observed **Start with Claude Code** button. Typed `Verified draft.` without an extra focus action. The actual workspace file contained one selected Claude record with that exact draft. Screenshot `0002.png` shows the session and draft.
3. Quit with Ctrl+Q. Observed an empty window list and the original app PID exit. Relaunched with the same disposable state. Doctor matched the new PID to the same binary. Screenshot `0003.png` shows the restored draft.
4. Focused with Ctrl+L and appended ` Again.`. Both the visible composer in `0004.png` and the actual file contained `Verified draft. Again.`.
5. Quit through Ctrl+Q and stopped the driver. The app, compositor, and driver PIDs all exited. The temporary workspace was removed. All four PNGs, action records, and state snapshots survived cleanup.

`actions.jsonl` records each input and result. `result.json` records literal state assertions and the cleanup checks. The screenshots were visually inspected, not merely checked for existence. This is a native Wayland run on the target machine, not a web mock or simulated GPUI test.

## Visual comparison

The supplied MonoCode screenshot and this native capture both show a flat charcoal window, narrow activity rail, workspace sidebar, top tab, and centered heading above the composer. Keystone's capture has fewer rail actions and explicit provider-disconnection text, consistent with the implemented preview. It uses initial provider buttons instead of pretending a model selector is connected.

The test desktop was 1782 by 1075 pixels, assigned by Hyprland despite a 1280 by 900 request. The supplied reference was 2048 by 1234. This supports a structural visual comparison only. No pixel-difference score or exact visual parity is claimed. Small-window clipping is not covered by this run.

## Headless desktop and named elements

The first proof below ran the nested desktop as a Hyprland window. Verification now defaults to a headless Sway with the GLES2 renderer, from a local patch to the same driver commit. The app also reports the bounds of each named element when `KEYSTONE_AUTOMATION_PATH` is set in a debug build. The server maps them into desktop pixels for `elements`, `click`, and `wait_for`.

The run in `.audit/verify-20261015T172005-206124/` used only names for mouse input. It clicked **Start with Claude Code**, typed a draft, opened the new-session menu, and clicked **New Codex session** inside that popover. The saved workspace held both sessions and the typed draft. After Ctrl+Q and relaunch, `0002.png` shows both sessions restored. Cleanup reported no errors. The run came from an agent shell with no host Wayland display, which the previous driver could not use at all.

Element geometry is clip-aware but not occlusion-aware. While the new-session menu was open, the covered first session row still reported as visible. Under verification the session sidebar renders without view caching, because a reused cached view skips the prepaint that reports geometry.

Cua Driver was evaluated as an alternative and not adopted. Its semantic route on Linux is AT-SPI, which GPUI 0.2.2 does not expose. Its native Wayland lane was experimental as of upstream issue 1922. On Keystone it would offer the same pixel route this driver already has.

## Failed attempts and limitations

Run 001 could not connect to the host Wayland socket from the restricted shell. It cleaned up and retained the failure log. The permitted run used graphical-session and GPU access.

Run 002 captured the welcome and created-session states, then encountered two capture timeouts while the nested window was offscreen. Capture recovered after the owned host window was focused. That run exited 1 and remains failure evidence. Run 003 completed without tool errors while the nested window remained visible. The headless backend resolved this as KS-006.

Independent review corrected a nonexistent search-toolbar route in the map and a scratch-directory leak on driver execution failure. The failure regression attempt is retained in `.audit/verify-driver-exec-failure/`. A fresh review approved the resulting source and instructions.

Only the Claude welcome entry, short text, Ctrl+L, and Ctrl+Q restart journey were exercised in the clean run. The other mapped entry points remain for later runs. Provider execution, long-input stress, clipboard paste, IME composition, and exact screenshot parity are not proved here. Existing GPUI and core checks remain separate.

## Settings run, 2026-10-15

Run `.audit/verify-20261015T165545-settings` drove the settings menu natively with mouse input. Screenshots came from `grim` because of KS-011. It exercised these entry points:

| Feature | Entry point | Observed |
| --- | --- | --- |
| Sessions | "Start with Claude Code" welcome button | Session created, composer focused, typed draft shown (`grim-02`). |
| Settings | Rail gear | Menu opened upward with System, Default, and Default checked (`grim-03`). |
| Settings | Light, then Compact | Window, sidebar, and menu switched live (`grim-04`). |
| Settings | Noto Sans | Interface font changed (`grim-05`). |
| Settings | Escape | Menu closed (`grim-06`). |
| Sessions | Sidebar "+" menu, New Codex session | Second row appeared in the cached sidebar (`grim-07`, `grim-08`). |
| Sessions | Click first row | Selection moved and the Claude draft returned (`grim-09`). |
| Restart | Ctrl+Q, launch | Theme, density, font, sessions, selection, and draft restored (`grim-10`). |

The saved state snapshot recorded `{"theme": "Light", "density": "Compact", "ui_font": "Noto Sans"}`. Cleanup reported no errors and no retained scratch state. System theme switching, Spacious density, and the settled filter were not exercised in this run.

## Stability runs, 2026-10-15

`scripts/native-stress.py` drives the same server code without MCP. Each run gets a fresh headless desktop. It presses Ctrl+N through the driver to create a Claude session and types a long draft with `wtype -d 10` on the nested socket. It quits with a `wtype` Ctrl+Q and relaunches, then quits through the driver. It asserts the exact saved draft and captures `grim` screenshots after creation, typing, and restore.

| Batch | Runs | Result |
| --- | --- | --- |
| Unprimed: before the driver change, then `--no-prime` | 4 | All four relaunches panicked at `gpui-0.2.2/src/platform/linux/wayland/client.rs:1267` (KS-009). |
| 1,093 characters, before the frame check | 10 | 6 passed. 4 clicks missed, before any typing. |
| 1,093 characters | 12 | 12 passed. |
| 2,677 characters | 5 | 5 passed. |

The 4 missed clicks used the app's first frame. GPUI drew that frame at its default 1100 by 720 before Sway configured the window to 1280 by 900. The button then moved. `elements` now reports not ready until the frame's viewport matches the window rect, within an 8-pixel inset tolerance. It records each rejection as `elements_not_ready` in `actions.jsonl`. In the 12-run batch, 6 of 24 launches had that early frame, and every click waited it out.

`launch` now sends F24 through the driver's keyboard before starting the app, because of KS-009. Every primed run that reached the relaunch after `wtype` survived it, 31 of 31, counting short check runs not tabled here. KS-009 is now TOB-212. The KS-005 record is in git history (`git log -- docs/issues.md`).

## Maintenance

Use `/maintain-verification-skill` when the app's user paths change. Update the feature map and re-run the affected entries. Keep failed evidence alongside successful evidence.
