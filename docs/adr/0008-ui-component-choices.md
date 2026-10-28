# 0008. UI component choices

Status: Accepted, 2026-10-15 to 2026-10-19. Recorded as an ADR 2026-10-27 from `docs/handoff.md` and the UI components plan (UC-01 to UC-14, all Done).

## Context

Keystone's tabs, splits, windows, command palette, file tree and settings controls were built as a stack of component PRs. Several alternatives were tried or considered and rejected along the way.

## Decision

- **Shared pieces land before the features that use them.** Each component lands first in `keystone-ui` with gallery evidence, then as a desktop feature.
- **`PopoverMenu` opens on mouse down and closes on any press outside the card**, the trigger included, so a click-based toggle would reopen it. The caller owns the `open` state.
- **`Label::truncate` measures and cuts its own text** instead of using GPUI 0.2.2's `text_ellipsis`, which cached the first width it measured (KS-010). It rounds the measured width up to a whole pixel. Recheck on the fork (TOB-212).
- **The palette, select and tree build on a keyboard-driven `VirtualList`**, not on `PopoverMenu` and `MenuItem`. Those have no keyboard navigation, focus or scrolling, and build every child eagerly.
- **The file tree is a flat `visible` list on `uniform_list`**, not nested elements. Nested flex nodes made taffy re-measure subtrees.
- **Cross-window tab drags use a private MIME type**, not the fork's file-list payload. With that payload any app could accept the fake file, and the drag preview vanished.
- **Session ownership moved out of the window view before a second window opened.** Otherwise closing a window would kill running work, which ADR 0001 forbids.
- **Tabs, splits and windows shipped as separate PRs**, because each fails differently: tabs on drag, splits on resize, windows on process lifetime.

## Consequences

- New components follow the "Adding a component" steps in `DESIGN.md`.
- The tGPUI drag commit is private and must survive every fork rebase (ADR 0004).
