# Streaming and interaction research

Inspected on 2026-10-17 for the native Keystone polish pass.

## Incoming text and visible text have different jobs

Keystone applies provider events immediately, persists conversation changes, and feeds each reply into its incremental Markdown parser. The parser already reuses completed blocks and mends incomplete inline syntax. The missing behavior is pacing between an incoming burst and the displayed prefix.

[Codex's streaming controller](https://github.com/openai/codex/blob/main/codex-rs/tui/src/streaming/controller.rs) separates stable rendered content from a mutable tail and queues stable lines for display. It holds incomplete tables in the mutable region because later rows can change column widths. Its terminal scrollback constraints differ from GPUI's retained layout. Keystone can reuse the separation of canonical content and presentation without importing the terminal controller.

[MonoCode's changelog](https://github.com/hardbeat920/monocode/blob/main/CHANGELOG.md) describes bounded Markdown highlighting caches and shared concurrent highlighting requests. This supports retaining stable work during streaming. It does not establish MonoCode's exact text reveal algorithm.

The chosen Keystone design keeps provider events, approvals, logs, and stop commands immediate. Only the visible assistant reply prefix is paced. A burst has a fixed catch-up deadline, so later appends cannot create an indefinitely growing typing backlog. Completed or replaced content becomes authoritative immediately. Historical replies do not replay an animation.

## Preserve reading position

[T3 issue 5903](https://github.com/pingdotgg/t3code/issues/5903) reports threads opening or finishing above the latest message. [T3 issue 17056](https://github.com/pingdotgg/t3code/issues/17056) reports live-follow snapping back after scrolling over embedded HTML. These are reports about T3, not defects reproduced in Keystone.

They define useful acceptance checks here. A reader at the bottom follows new output. A reader who scrolls upward keeps that position. A visible action returns to the latest content. Reveal ticks must reuse Keystone's existing logical row and offset anchoring rather than force a scroll to the end.

## Visual direction

The [Zeron product page](https://zeron.sh/) shows compact controls, quiet separators, and clear type hierarchy. Keystone retains its own layout and uses neutral gray surfaces and text, with blue reserved for components such as primary actions, selection, and focus. Provider identity and warning colors retain their meanings. Paper Desktop holds editable mockups for current states and clearly labeled future concepts.

No Zed, Zeron, Codex, or MonoCode source was copied for this pass. No claim is made about Claude Code's proprietary rendering implementation.

## Remaining product work

The accepted scope in [scope.md](../scope.md) is larger than this polish pass. TOB-28 tracks remaining product journeys and links them to existing component work. TOB-29 tracks partial transcript selection and collapsible activity.

Native screenshots, action logs, and exact-binary checks are the acceptance evidence. Unit tests cover pacing and persistence boundaries. A fake provider proves UI handling of its protocol events, not a live provider integration.

## Implementation decisions

The display parser advances at 16 ms timer intervals while a selected reply is behind. Each burst has a fixed 120 ms deadline, which subsequent text cannot extend. The first eight graphemes appear immediately; small deltas therefore incur no artificial delay. Extended grapheme boundaries protect combining marks and emoji sequences. Completion, replacement, late timers and turn termination show authoritative text immediately. Background sessions and restored history do not run reveal timers. Reveal invalidation uses the same list anchoring as provider updates. The deadline is an interaction policy, not a measured provider-latency improvement or a frame-rate guarantee.

Titles derive from the first message, using up to 48 graphemes and preferring a nearby word boundary. Explicit renames are durable. Older workspace files have no title-source field and load as placeholders: their next sent message supplies a title, even when earlier history exists. Saving with an older app version can drop the new title-source field; downgrade/upgrade round trips are not guaranteed to preserve that metadata.

We reused the existing incremental Markdown parser, list invalidation, button styles and confirmation dialog. A general animation toolkit or tGPUI source import was unnecessary for this behavior. Copy actions show confirmation, and reply-copy space stays reserved during streaming to avoid a completion layout shift. Pointer-first transcript actions remain a known accessibility gap under TOB-30.

## Review disposition

Independent Opus 5.5 and GPT-6.1 Sol reviews agreed on fixing rename keyboard navigation and primary-label contrast. The focused fix pass also addresses modal save feedback, unchanged renames disabling automatic titles, and stale presentation state when returning to a live session. Palette/brief discrepancies are reconciled to implemented values. Broad extraction of window chrome and rename into new modules is deferred: it does not correct an observed behavior and would expand this pass. Additional helper abstractions for straightforward tuple keys and visibility conditions were declined for the same reason.

**Attention:** native wheel scrolling and maximize remain unproved in the private Sway driver (TOB-31). Simulated anchoring/jump tests are not native evidence. Native burst recording uses scripted, grouped protocol events; it demonstrates visible reveal behavior but makes no claim about live model cadence. The wider accepted product scope is still incomplete.
