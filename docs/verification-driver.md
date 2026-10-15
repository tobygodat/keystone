# Native verification driver proof

The project-local [verify skill](../crates/keystone-desktop/.agents/skills/verify/SKILL.md) drives the real Rust GPUI executable through computer-use-mcp and a nested Sway desktop. It includes four feature maps for sessions, drafts and restart, settlement, and search. The controller uses stdio and needs no global MCP registration.

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

## Failed attempts and limitations

Run 001 could not connect to the host Wayland socket from the restricted shell. It cleaned up and retained the failure log. The permitted run used graphical-session and GPU access.

Run 002 captured the welcome and created-session states, then encountered two capture timeouts while the nested window was offscreen. Capture recovered after the owned host window was focused. That run exited 1 and remains failure evidence. Run 003 completed without tool errors while the nested window remained visible. KS-006 tracks background-capture reliability.

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

## Maintenance

Use `/maintain-verification-skill` when the app's user paths change. Update the feature map and re-run the affected entries. Keep failed evidence alongside successful evidence.
