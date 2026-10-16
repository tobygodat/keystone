# Open engineering issues

## KS-001. Snapshot replacement durability

Snapshot replacement now syncs the temporary file before rename and the existing parent directory afterward. A failed directory sync returns an applied result with a durability warning. Memory follows the installed file, and retry syncs only the directory. A deterministic fault test checks the installed file, memory, returned ID, and retry without duplicate creation. Pre-rename failures retain the previous state.

Durability of newly created ancestor directories remains open. These checks do not simulate system crashes or power loss.

## KS-002. Native desktop smoke automation

Status is partially resolved. `scripts/native-smoke.py` passes native Wayland keyboard creation, settlement, draft entry, and process-restart checks on the current Hyprland desktop. It captures empty, populated, and restored states. Long-input scrolling and failed-storage compositor-close protection also passed at 100 ms per character. Long input also passed at 10 ms per character (KS-005). The nested driver now proves native mouse creation through the Claude welcome button, its input focus, and short-draft restart. Other mouse entry points and input-method composition coverage remain open. GPUI simulated-input tests cover mouse creation and settlement separately.


## KS-003. Draft navigation after a storage failure

Status is resolved in the visual pass. Navigation, creation, settlement, and app-controlled close retry the visible draft before replacing it. A failed retry retains the text and selection. Forced process termination still cannot preserve text that storage rejected.

## KS-004. Full composer input

Status is partially resolved. The composer accepts multiline drafts and normalizes pasted line endings. Text wraps, grows to eight visible rows, and scrolls vertically. Search remains single-line. Attachments and provider submission remain accepted MVP work. Native IME coverage remains open.

## KS-005. Native long-input stall

Status is closed. It no longer reproduces on the current build. Native synthetic typing at 10 ms per character used to overwhelm the debug window and end with process exit -9. Debugger samples showed font shaping, painting, and layout in GPUI's synchronous key-event redraw path. The same long-input flow passed with 100 ms per character. This supported a rendering backlog, not a proven single-function deadlock. Explicit Inter and selective rendering-dependency optimization did not make the rapid-input check pass, and that speculative optimization profile was removed. Evidence is retained in `.audit/evidence/visual-debug/`.

On 2026-10-15 the 10 ms check passed once per build on Hyprland 0.56.2, with dependencies built at `opt-level = 2`. It passed both before and after the sidebar became a cached view. Later that day it was repeated 27 times at 10 ms per character, and every run passed:

| Desktop | Script | Draft typed at 10 ms | Runs passed |
| --- | --- | --- | --- |
| Headless Sway 1.12 | `scripts/native-stress.py` | 1,093 characters | 12 of 12 |
| Headless Sway 1.12 | `scripts/native-stress.py --repeat 60` | 2,677 characters, composer scrolled | 5 of 5 |
| Hyprland 0.56.2 | `scripts/native-smoke.py` | 264-character long tail, as before | 5 of 5 |
| Hyprland 0.56.2 | `scripts/native-smoke.py`, `KEYSTONE_LONG_REPEAT=24` | 1,056-character long tail | 5 of 5 |

Every run persisted the exact draft and showed it in the window. In the headless runs, the draft was saved 0.0 to 0.3 s after `wtype` exited. The app saves 250 ms after the last edit, so none of those runs had a key backlog. An earlier headless batch had 4 setup failures that happened before typing. Those were a click race in the driver, now fixed, not stalls. The evidence is in `.audit/native-stress-*` and `.audit/ks005-hyprland-20261015T211041/`, on this machine only.

The redraw path remains synchronous per key event, so this shows the current build keeps up at this rate, not that the old cause is gone. Rerun `python3 scripts/native-stress.py --runs 10` after changes to the composer, text layout, or the dependency profile. If the stall returns, reopen this issue.

## KS-006. Nested desktop capture depends on host visibility

Status is resolved. The pinned computer-use-mcp driver ran its private Sway as a host window, and Hyprland stops drawing hidden windows, so capture timed out and fresh menus missed clicks while it was offscreen. `scripts/patches/computer-use-mcp-headless.patch` adds a headless backend, now the verification default. A run from a shell with no host Wayland display rendered at exactly 1280 by 900 and accepted input. Failed-run evidence is in `.audit/verify-nested-002/`; headless evidence is in `.audit/verify-headless-001/` and `.audit/verify-20261015T172005-206124/`. `headless:false` still has the original limitation.

## KS-007. Verification controller startup cleanup

Status is resolved. Independent review found that driver execution failure could leave a temporary state directory before the controller was assigned. The constructor now records the failure and removes its scratch state. A real non-executable driver attempt returned exit 1, retained its cleanup record, and left no new state directory. Evidence is in `.audit/verify-driver-exec-failure/`.

## KS-008. Placeholder and stale editor geometry

Verification reproduced an empty-composer crash after End followed by typing. The placeholder supplied byte offsets that did not belong to the empty draft. Independent review also found that edits could leave navigation using geometry for the previous buffer. Status is resolved. Placeholder layout is now paint-only, replacements invalidate editable geometry, and navigation before repaint uses current logical line boundaries. The regression failed before the fix and passed afterward. Native End followed by typing also passed.

## KS-009. Native verification keyboard restart

Status is resolved for verification. The underlying GPUI bug remains. A nested Sway restart after temporary `wtype` keyboard use hit GPUI 0.2.2's `keymap_state.as_mut().unwrap()` while handling a Wayland keyboard modifiers event, before a window appeared.

The cause is in GPUI's Wayland keyboard handler (`gpui-0.2.2/src/platform/linux/wayland/client.rs`). It unwraps its keymap state on every modifiers event, but that state exists only after an XKB keymap event. When `wtype` exits, Sway destroys its virtual keyboard. That keyboard was the seat's active one, so the seat is left with none, though it still advertises a keyboard. A client that binds the keyboard then gets no keymap event at all. Gaining focus sends it `enter` and `modifiers`, and GPUI panics. A `WAYLAND_DEBUG=1` trace in `.audit/ks009-wayland-debug/20261015T211427/` shows exactly that sequence. After a driver key event, the same launch first receives `keymap(xkb_v1)` and runs normally. The driver's own virtual keyboard becomes the seat's again only once it sends a key. Upstream tracks the same panic as zed-industries/zed#64660.

On 2026-10-15, relaunching after a `wtype` Ctrl+Q without priming reproduced the panic at `client.rs:1267` in 4 of 4 runs (`scripts/native-stress.py --no-prime`). The fix stays out of GPUI. The verification server's `launch` now sends F24 through the driver's keyboard before every start. That reselects a keyboard with a real keymap. With that step, 31 of 31 runs relaunched after `wtype` and kept running. A guard inside the app is not possible, because the panic happens in GPUI's event dispatch before any app code runs. A Linux user on a Wayland seat with no keyboard can still hit this until GPUI is upgraded or patched.

## KS-010. GPUI 0.2.2 text ellipsis

Status is open with a workaround. Native screenshots showed session titles rendered as a bare `...`, and a gallery label clipped without an ellipsis. GPUI 0.2.2's text element caches the first measured size of non-wrapping text and reuses it whatever width later layout passes offer, so `text_ellipsis` truncates to the first width it saw. `Label::truncate` in keystone-ui therefore clips to one line without an ellipsis. Revisit when upgrading GPUI. Evidence is in `.audit/verify-design-system-20261015T153154/0004.png`.

## KS-011. Driver screenshots reject the nested output's pixel format

Status is open with a workaround. On 2026-10-15, with Sway 1.12 nested in Hyprland 0.56.2, every driver `screenshot` call returned `unsupported SHM pixel format`. The pinned driver accepts only 8-bit ARGB, XRGB, ABGR, and XBGR buffers. Setting `render_bit_depth 8` on the nested output through `swaymsg` did not change the result. `grim` captured the same nested output successfully when pointed at the run's Wayland socket, so run screenshots through `grim` and keep using the driver for input. See the setup notes in the verification skill. The failed calls are recorded in `.audit/verify-20261015T165545-settings/actions.jsonl`.

