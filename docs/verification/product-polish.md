# Native product polish verification — 2026-10-17

This pass covers streaming presentation, session controls and visual polish. It does not complete the product scope in `docs/scope.md`. The editable [Paper file]() distinguishes current screens from future concepts.

## Artifact and environment

The first integrated native run used commit `0e2dc40` (subsequent `edaec61` changes comments only), frozen at `.audit/integrated-keystone-desktop`, SHA-256 `978421c88d2ee9b9b4d74ef7451997d4776575e7e0a805b4fdaa60865dcae395`. `doctor` confirmed the launched process matched the frozen file and the file remained unchanged. Linux private headless Sway, initially 1280×900; floating-window checks used 1024×768. Paper boards use 1280×800, so no pixel-parity claim is made.

The original screenshots and streaming recording below predate the neutral palette correction and show the prior navy palette, with tinted surfaces and warm text. The behavior they record is unchanged.

A preceding controls run used a worker's build accidentally left in the shared Cargo output. It was discarded. Subsequent checks launch a separate frozen copy, not the shared build path.

Evidence directories (retained locally in the worktree):

- `.audit/verify-20261017T172501-243094`: named actions, screenshots, doctor results, workspace snapshots and graceful restarts.
- `.audit/verify-20261017T173012-243094`: burst-streaming journey.
- `.audit/burst-frames`: 308 native screencopy frames with monotonic capture timestamps over approximately five seconds. The [recording](product-polish/streaming.mp4) uses their average cadence for playback; it is not a frame-rate benchmark.

## Clickthrough results

| Journey / entry point | Observed result |
| --- | --- |
| Welcome Claude pill → type → Send | Reply appears; session derives `Inspect the session controls` from the first message. |
| Title click → Ctrl+A → type → Enter | `Polish review` appears in the sidebar/header and saves with `title_source: Renamed`. |
| Claude command → Allow | Approval disappears; command output is `scripted`, status is green `Exit 0`, and transcript records Allowed. |
| Command Copy click | Wayland clipboard reads exactly `scripted`; button shows feedback. |
| Claude long reply → Ctrl+Enter steer → Stop | Both user messages and partial replies remain; transcript shows Stopped. |
| Ctrl+Shift+N → Codex command → Deny | Transcript records Denied and command status Not run; reply completes. |
| Full access choice → Cancel | Confirmation dismisses; saved mode remains unchanged. |
| Session Settle click | Codex session saves `settled: true`; Settled count becomes one. |
| Search field → `Polish` → Escape → select | Matching session remains accessible; Escape clears the search. |
| Settings Light / Compact | Saved preferences read `Light` and `Compact` after the respective clicks. |
| Draft → Ctrl+Q → relaunch | Exit code zero; title, conversation, draft and Light theme restore. Reopened draft reads `Draft survives restart`. See [restored view](product-polish/restored-light.png). |
| Floating titlebar drag | Native window moves from (80,50) to (148,89). Controls continue responding. |
| Project folder click | A native Nautilus window titled `tmp` opens. |
| Close window icon | Tracked Keystone process exits with code zero. |

The [stopped conversation](product-polish/stopped-dark.png) shows retained partial replies and the stop marker. Fake Claude/Codex providers exercise real app transport and UI handling but make no model calls; this does not independently establish current live-provider compatibility.

## Streaming evidence

A temporary wrapper grouped the fake Claude's text deltas into 24-word bursts, spaced approximately 280 ms apart; canonical protocol content was unchanged. Native captures 0087, 0091 and 0094 show progressively longer prefixes of one burst, rather than the entire burst appearing in one paint. The first group of changed transcript captures spans approximately 1.385–1.516 s into capture, including capture/paint latency. This supports visual interpolation, not an independent measurement of the internal 120 ms policy.

Deterministic tests establish the fixed deadline, late catch-up, grapheme boundaries, canonical approval visibility before reveal, idle timer shutdown, and scroll anchoring. The native recording retains normal line-wrap movement; this pass does not add word-boundary buffering or general surface animations.

## Verification limits and review

Native driver wheel input did not move history (existing KS-012), so native Jump to latest remains unproved. Maximize did not change floating Sway geometry. TOB-31 tracks these environment/app distinctions. Simulated list and jump tests passed. Minimize, every font option and every approval mode were not exercised natively in this pass.

Both native runs ended with `errors: []` and `scratch_retained: null`; screenshots and logs remain. Integrated `scripts/check.sh --unmeasured core` and `ui` passed. Benchmarks remained ignored. Independent Opus 5.5 and GPT-6.1 Sol reviews found no critical canonical-streaming defect; their keyboard, contrast, rename and session-return findings receive one focused fix pass. The final-fix evidence follows.

Pointer-only transcript actions remain tracked in TOB-30. Larger incomplete product journeys remain in TOB-28.

## Final fix verification

Commit `9cb44e3` was built and frozen at `.audit/final-keystone-desktop`, SHA-256 `63ffe73c6f5070cbb1d5dcdda70ad1fb0153531e1cf12236be83d7935834d84e`. Run `.audit/verify-20261017T173532-290746` confirmed binary identity, unchanged executable, and graceful exit code zero. Cleanup again returned no errors and removed its scratch directory.

- Opening Rename selected the title. Tab three times returned to the field; typing replaced the selected title with `Keyboard cycle`.
- Submitting a blank field showed the validation error inside the modal.
- Making only the disposable state directory non-writable caused a [visible inline storage error](product-polish/rename-error.png), with `Saved after retry` retained in the field. After restoring directory permissions, Enter closed the dialog and the real workspace saved exactly that title. The driver's named wait missed `rename-error` for this wrapped error; the screenshot establishes its visible presence.
- The [final approval card](product-polish/approval-dark.png) used the corrected blue and its Allow click completed the scripted command.
- Final targeted rename tests (2) and session reselect/create test (1) passed. The fix worker's desktop suite passed 76 tests plus UI component suites; the parent did not repeat unrelated suites after those results.

Both independent reviewers (Opus 5.5 and GPT-6.1 Sol) reported no code blockers in cycle two. The remaining documentation notes were corrected here and in the palette brief. Contrast calculated from sRGB tokens: dark Primary 4.86:1 at rest, 5.22:1 hover, 5.62:1 pressed over the overlay; light Primary 6.27:1, 5.48:1 and 4.80:1 over white. These are computed contrast ratios, not screenshot pixel measurements.

The implementation follows **Model the Domain** by separating canonical reply data from presentation progress, **Laziness Protocol** by reusing the parser and list anchors, and **Prove It Works** by checking native artifacts and persisted outcomes. No general animation framework or upstream component fork was needed.

## Dispatch ledger

- Architecture audit: `claudeAgent/claude-opus-5-5`, medium.
- Alternative streaming design: `codex/gpt-6.1-sol`, medium.
- Paper design and final synchronization: `claudeAgent/claude-opus-5-5`, high.
- Streaming implementation: `claudeAgent/claude-opus-5-5`, high.
- Session controls implementation: `claudeAgent/claude-opus-5-5`, high.
- Focused review fixes: `claudeAgent/claude-opus-5-5`, high.
- Comment review: `claudeAgent/claude-opus-5-5`, medium; 22 redundant comments removed (26 lines), no runtime changes. Public API and behavior contracts retained; helper-only refactors declined.
- Independent review cycles 1 and 2: `claudeAgent/claude-opus-5-5` and `codex/gpt-6.1-sol`, both medium.
- Orchestration, integration, final small lifecycle/contrast fixes, native checks and evidence: parent in-thread.

## Neutral palette correction

Commit `f44bc6a` replaces tinted surfaces and text with neutral grays in both themes; blue remains on interactive components. Opus 5.5 (`claudeAgent/claude-opus-5-5`) implemented the theme, documentation and all 13 Paper board updates. Parent integration built `keystone-desktop` with `--locked --offline -j 2` and launched a frozen copy with SHA-256 `fdb48f2eaaba78fcffd5c7c19fe15a1b7f1f37e4b1df752801d97b02421a85a6`.

Native evidence: `.audit/verify-20261017T175142-312927`, private headless Sway at 1280×900. The sample transcript renders neutral body/code text and surfaces. Settings → Light and Settings → Dark visibly change the theme and persist the corresponding preference. Creating a Claude session and typing a draft shows the blue Send action and selected-row treatment; the saved draft matches `Neutral surfaces, blue actions`. The fixture makes no provider call. See [dark palette](product-polish/neutral-dark.png) and [light palette](product-polish/neutral-light.png).

Doctor confirmed the live process matched the unchanged frozen binary. Ctrl+Q exited with code zero; cleanup returned no errors and no retained scratch. The worker passed formatting and the UI build; this token-only correction adds no tests or further review round.
