# Open engineering issues

## KS-001. Snapshot replacement is not power-loss durable

Status is open. `Store::commit` syncs the temporary file before rename but does not sync the parent directory afterward. The current tests cover ordinary process restarts, not system crashes or power loss. Resolve directory-sync error semantics before promising crash-durable saves or integrating live execution.

## KS-002. Native desktop smoke automation

Status is partially resolved. `scripts/native-smoke.py` passes native Wayland keyboard creation, settlement, draft entry, and process-restart checks on the current Hyprland desktop. It captures empty, populated, and restored states. Long-input scrolling and failed-storage compositor-close protection also passed at 100 ms per character. The nested driver now proves native mouse creation through the Claude welcome button, its input focus, and short-draft restart. Other mouse entry points and input-method composition coverage remain open. GPUI simulated-input tests cover mouse creation and settlement separately.


## KS-003. Draft navigation after a storage failure

Status is resolved in the visual pass. Navigation, creation, settlement, and app-controlled close retry the visible draft before replacing it. A failed retry retains the text and selection. Forced process termination still cannot preserve text that storage rejected.

## KS-004. Full composer input

Status is open. The preview explicitly supports single-line drafts and rejects multiline insertion. Full multiline composition, attachments, and provider submission remain accepted MVP work. Horizontal cursor-follow scrolling is implemented for the current field.

## KS-005. Native long-input stall

Status is open. Native synthetic typing at 10 ms per character can overwhelm the debug window and end with process exit -9. Debugger samples showed font shaping, painting, and layout in GPUI's synchronous key-event redraw path. The same long-input flow passed with 100 ms per character. This supports a rendering backlog, not a proven single-function deadlock. Explicit Inter and selective rendering-dependency optimization did not make the rapid-input check pass. The speculative optimization profile was removed. `KEYSTONE_KEY_DELAY_MS=10` reproduces the stress case; the default native check uses 100 ms. Evidence is retained in `.audit/evidence/visual-debug/`.

## KS-006. Nested desktop capture depends on host visibility

Status is open with a documented workaround. The pinned computer-use-mcp driver timed out capturing a nested Sway window while it was offscreen in Hyprland. The same running session captured successfully after its observed host window was focused. A subsequent complete visible-window run passed. Keep the nested desktop visible during verification. Fully background capture remains unverified. Failed-run evidence is in `.audit/verify-nested-002/`; passing evidence is in `.audit/verify-nested-003/`.

## KS-007. Verification controller startup cleanup

Status is resolved. Independent review found that driver execution failure could leave a temporary state directory before the controller was assigned. The constructor now records the failure and removes its scratch state. A real non-executable driver attempt returned exit 1, retained its cleanup record, and left no new state directory. Evidence is in `.audit/verify-driver-exec-failure/`.
