# Files #40 integration check

Current main `0950083723835c1285edc86195e24f856e64c609` was merged normally into the existing wheel PR branch, without rebasing or force-pushing. The sole textual conflict was the verification feature index; both **Wheel and maximize** and **Files** links are retained. Files' `VISUAL`/`EDITOR` launch support auto-merged with the wheel and trace path. No app/root source differs from this main base.

Six focused Python tests pass, including a new regression that accepts both editor overrides while rejecting a caller-owned state path. The unchanged pinned-driver wheel regression also passes. Bounded independent read-only review of the resolved merge found no actionable issue: both feature links, editor propagation, run-owned paths and wheel/trace behavior are preserved. No unrelated suite was repeated.

## Native post-Files smoke

To avoid a redundant application build, the run used a private copy of KEY-16's verified Files binary, SHA256 `92bfcd834cd6159417cda3b4a1470a0d49afb183b0af47a589b1010826272da9`, built at `5b1abbf708fa3313210b46b635aaae3af3f509f9`. That commit and merged main `0950083` have the identical complete Git tree `613a4e34ae218ceb3432faf022aa9bf09e20f15c`. This is an exact source-equivalence claim, not a claim that the binary was rebuilt under the squash-merge commit name. Doctor matched the copied executable on both live launches.

The private headless desktop ran the merged wrapper with the corrected driver. Reading only the two selected variables from the app's own process environment confirmed `VISUAL=/nonexistent/isolated-visual` and `EDITOR=/nonexistent/isolated-editor`. No editor was launched. Both provider paths were isolated, and the conversation used only the local scripted Codex binary.

The native run passed both pane directions with exact untargeted row/pixel preservation, distinct earlier offsets, last-focused second-pane Send, completion remaining unread with composer focus, earlier focus not acknowledging, and focused latest acknowledging. A separate 20-line overflowing composer draft scrolled both ways while retaining its exact saved text and both transcript offsets. Actual discrete/value120 events are retained in the [machine receipt](files40-integration.json). Both launches quit through Ctrl+Q with exit 0; teardown returned `errors:[]`, `scratch_retained:null`. This replay completed without the earlier long-draft wait timeout.

![Post-Files independent pane offsets]()

[Post-Files composer wheel]() and the saved observations establish the separate text-input path. This is not a rerun of Files' complete browser/editor acceptance matrix, a live provider test or a performance check. Combined smoke after Settings #41 remains required.

## Supporting-compositor attempt and scope correction

The Captain clarified that temporary foreground focus of the disposable test app was authorized; the earlier no-other-focus-change boundary was a precaution, not a user or permanent product requirement. One host Hyprland attempt launched the exact older-main binary with isolated state and providers disabled. Its pointer guard changed during own-window setup/focus, so the run stopped before any click/key input, without racing or retrying. The app trace contains no maximize/restore request; this attempt therefore has no maximize verdict or screenshot.

Only that disposable window was closed by exact address, and its tracked process exited 0. Initial cleanup recorded a pointer-restore mismatch; a guarded cursor-only correction restored the original pointer and focus, with every other window's layout/state map unchanged. Final cleanup had no errors or retained scratch; the initial failure is preserved, not relabeled as a clean Ctrl+Q run.

The raw host receipt and exact scratch controller are retained in the Captain's `handoffs/archive-host-attempt.tar.gz`. Raw post-Files native evidence is in `handoffs/archive-files40-integration-evidence.tar.gz`, run `verify-20261019T010338-1436665`, with action/stderr logs, screenshots, state, manifests and replay source. Supporting-compositor titlebar maximize/restore and keyboard Jump remain unverified; no additional host attempt occurred while integration was pending.
