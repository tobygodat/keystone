# TOB-30 keyboard transcript acceptance

Final application source `7fda22c7c69157d18db80b90ef876709c674fe01`, based on integrated main `b718620`, was built and verified on desk. App SHA256: `7474adeb99cd4425df6e03a1eb509c419f4df37a269344b03c7c81d5d96f36fd`. Private driver SHA256: `683277d3e2dfd2840250739cec720b1b2e8dc7f27e4c500bdf3c2ca922cb79c7`; built from the pinned source with both repository patches and its focused wheel test passing. Later receipt-only commits do not change application source.

No installed app, real session, host clipboard, global compositor configuration or live provider was used.

## Checks and adversarial corrections

- `bash scripts/check.sh ui`: 203 desktop, 57 UI, 49 component and one layout diagnostic passed. Benchmarks remain explicitly ignored in that lane.
- The black-box keyboard/clipboard regression fails on unchanged main `b718620`: no clipboard content after transcript entry/Home/Copy, instead of `Question 0`.
- Adversarial review identified a selected reasoning row disappearing after empty completion, an obsolete keyboard cursor surviving Send, and entry pinning a tall latest reply to its top. Each has an asserting failing-before regression and passes after its correction.
- Additional checks cover Copy after pointer focus, Escape exit, the first arrow after wheel input, hidden tabs, and a project switch while a copy action holds focus.
- The ignored `perf_transcript_draw` diagnostic with 2,000 turns measured 1.02 ms/frame without action focus, 0.94 ms with a focused copy action, and 0.99 ms streaming with action focus. Unchanged-main's original unfocused diagnostic measured 0.97/0.95 ms. These are local single samples, not a statistical result or native-automation performance claim.
- The original binding registration order is preserved. The row outline is painted without reducing content width. Copy byte policies remain the existing pointer policies, checked against literal clipboard bytes.

## Exact-head native acceptance

Run `verify-20261019T020632-1662116` used a private headless Sway instance and a fake-Codex wrapper. It focused a visible reasoning row, received an empty completion, and visibly moved focus to the surviving reply without a false Jump button. The focused fenced-code action copied the exact same 15 bytes before and after reply completion.

![Focus reconciled to a surviving reply]()

After a graceful exit 0, the run loaded a labeled synthetic 1,203-row log. This fixture proves history handling, not 400 user-created turns. Native Customize created a duplicate occurrence and later combined the occurrences into preferred tabs; no internal native layout/focus setter was used.

Keyboard-only navigation copied exact Wayland clipboard bytes for a user message (27 bytes including Unicode), command output (22), a command without output (16), trimmed command output (17), a complete tall reply (2,362), its first code fence (18), and a nested fence (11). Enter and Space activated existing buttons; Ctrl+Shift+C and reverse traversal matched their content. The focused code control inside the tall reply was actually revealed.

![Focused code copy in a tall reply]()

Entering the tall latest reply preserved its exact visible row geometry and following state. Moving among already visible rows no longer repinned them. Explicit Send from an earlier keyboard row targeted only the last-focused occurrence and left the other pane's geometry unchanged. Composer-focused completion stayed unread. Re-entry after Send stayed at the latest reply, with no obsolete row jump, and deliberate focused-latest observation acknowledged it.

Tab from row → Copy → Jump and Shift+Tab from row → Jump produced visible focus. Enter and Space each activated Jump in a separate occurrence without moving the untargeted pane. Jump paints after the transcript, so its label is readable.

![Readable keyboard-focused Jump]()

The run also proved Copy initializes an empty region cursor, Escape returns to the composer, preferred and 500×900 fitted tabs release hidden action focus, Settings yields to transcript entry, and switching to an existing second session while a copy action holds focus preserves usable input and independent drafts. Restart restored the preferred tab and the keyboard route still copied the first logical row.

All three launches at this source exited through Ctrl+Q with code 0. Doctor matched the live binary and reported `binary_unchanged:true`; cleanup returned `errors:[]` and `scratch_retained:null`. The implementer inspected the native screenshots.

## Retained earlier evidence and limits

Earlier runs `013126-1556163`, `013455-1560035`, `013814-1567220`, `014115-1573223` and `015914-1645418` preserve initial acceptance, streaming identity, keyboard Jump, and the intermediate hidden-row correction. Across all six native runs, fifteen launches exited 0 and all six stops returned empty/null cleanup. The initial harness's wrong reply-index wait, relaunch geometry race, and XKB comma spelling error remain preserved; these were corrected, not erased.

Raw action journals, expected/actual clipboard bytes, snapshots, exit records, screenshots and test logs are retained in the crew's KEY-23 evidence archive. No full accessibility, range-selection, native project-picker, real-provider, IME or maximize claim is made. Project focus switching is simulated coverage. Mermaid PR #43 is separate: its Diagram Copy must forward the optional `Markdown::copy_focus` override when integrated. Markdown parsing and diagram rendering are unchanged here.

The final replay also proves Escape first dismisses Customize and the filter menu while a transcript is focused; a subsequent plain Escape exits to the composer. Its asserting regression failed before the propagation fix. Directional navigation of a partially clipped row also has an asserting before/after regression.

Both Opus 5.5 and Sol 6.1 independently reviewed and cross-challenged the change in two bounded rounds, confirmed each other's findings, and approved the corrected dispositions. Known Low limitation: when a reply is taller than the viewport, GPUI's minimal reveal lands at its bottom when moving down and its top when moving up; transcript entry itself preserves the existing viewport. No unresolved Medium/High finding remains.
