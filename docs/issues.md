# Open engineering issues

## KS-001. Snapshot replacement durability

Snapshot replacement now syncs the temporary file before rename and the existing parent directory afterward. A failed directory sync returns an applied result with a durability warning. Memory follows the installed file, and retry syncs only the directory. A deterministic fault test checks the installed file, memory, returned ID, and retry without duplicate creation. Pre-rename failures retain the previous state.

Durability of newly created ancestor directories remains open. These checks do not simulate system crashes or power loss.

## KS-002. Native desktop smoke automation

Status is partially resolved. `scripts/native-smoke.py` passes native Wayland keyboard creation, settlement, draft entry, and process-restart checks on the current Hyprland desktop. It captures empty, populated, and restored states. Long-input scrolling and failed-storage compositor-close protection also passed at 100 ms per character. The nested driver now proves native mouse creation through the Claude welcome button, its input focus, and short-draft restart. Other mouse entry points and input-method composition coverage remain open. GPUI simulated-input tests cover mouse creation and settlement separately.


## KS-003. Draft navigation after a storage failure

Status is resolved in the visual pass. Navigation, creation, settlement, and app-controlled close retry the visible draft before replacing it. A failed retry retains the text and selection. Forced process termination still cannot preserve text that storage rejected.

## KS-004. Full composer input

Status is partially resolved. The composer accepts multiline drafts and normalizes pasted line endings. Text wraps, grows to eight visible rows, and scrolls vertically. Search remains single-line. Attachments and provider submission remain accepted MVP work. Native IME coverage remains open.

## KS-005. Native long-input stall

Status is open. Native synthetic typing at 10 ms per character can overwhelm the debug window and end with process exit -9. Debugger samples showed font shaping, painting, and layout in GPUI's synchronous key-event redraw path. The same long-input flow passed with 100 ms per character. This supports a rendering backlog, not a proven single-function deadlock. Explicit Inter and selective rendering-dependency optimization did not make the rapid-input check pass. The speculative optimization profile was removed. `KEYSTONE_KEY_DELAY_MS=10` reproduces the stress case; the default native check uses 100 ms. Evidence is retained in `.audit/evidence/visual-debug/`.

## KS-006. Nested desktop capture depends on host visibility

Status is open with a documented workaround. The pinned computer-use-mcp driver timed out capturing a nested Sway window while it was offscreen in Hyprland. The same running session captured successfully after its observed host window was focused. A subsequent complete visible-window run passed. Keep the nested desktop visible during verification. Fully background capture remains unverified. Failed-run evidence is in `.audit/verify-nested-002/`; passing evidence is in `.audit/verify-nested-003/`.

## KS-007. Verification controller startup cleanup

Status is resolved. Independent review found that driver execution failure could leave a temporary state directory before the controller was assigned. The constructor now records the failure and removes its scratch state. A real non-executable driver attempt returned exit 1, retained its cleanup record, and left no new state directory. Evidence is in `.audit/verify-driver-exec-failure/`.

## KS-008. Placeholder and stale editor geometry

Verification reproduced an empty-composer crash after End followed by typing. The placeholder supplied byte offsets that did not belong to the empty draft. Independent review also found that edits could leave navigation using geometry for the previous buffer. Status is resolved. Placeholder layout is now paint-only, replacements invalidate editable geometry, and navigation before repaint uses current logical line boundaries. The regression failed before the fix and passed afterward. Native End followed by typing also passed.

## KS-009. Native verification keyboard restart

Status is open. A nested Sway restart after temporary `wtype` keyboard use hit GPUI 0.2.2's `keymap_state.as_mut().unwrap()` while handling a Wayland keyboard modifiers event, before a window appeared. Sending a fresh keyboard event through the driver before launch recovered that session. Virtual keyboard event ordering is the suspected cause. The exact dependency panic was observed by launching the app against the same disposable state and compositor. This is separate from snapshot restoration and the fixed composer geometry crash.

## KS-010. GPUI 0.2.2 text ellipsis

Status is open with a workaround. Native screenshots showed session titles rendered as a bare `...`, and a gallery label clipped without an ellipsis. GPUI 0.2.2's text element caches the first measured size of non-wrapping text and reuses it whatever width later layout passes offer, so `text_ellipsis` truncates to the first width it saw. `Label::truncate` in keystone-ui therefore clips to one line without an ellipsis. Revisit when upgrading GPUI. Evidence is in `.audit/verify-design-system-20261015T153154/0004.png`.
