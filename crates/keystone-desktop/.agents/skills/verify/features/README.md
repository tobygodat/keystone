# Keystone verification map

Use this map with ../SKILL.md. The primary user interface is the native GPUI window. Core tests and GPUI simulated input are supporting checks.

## Baseline

Drive the app through the `keystone-verify` MCP server. Call `start` for a private headless Sway desktop with an empty disposable workspace. It renders offscreen at the requested size, so host focus and visibility do not matter. Require `doctor` to identify the newly launched binary and a focused native window.

Prefer named elements. Call `click` with `{"id":"..."}`, then `wait_for` the element that should appear or disappear. `elements` lists the named elements in the latest frame. Its `visible` bounds exclude clipping but not overlap by menus or popovers. A named click does not prove the visual result. Inspect a screenshot after each step. Unnamed controls, such as the window controls, still need coordinates from a fresh screenshot. Never click using stale coordinates.

Read state with `workspace`. Preserve it with `snapshot`. These tools only observe the real state file. After every mutation, wait for the visible result and saved state. A wait alone is not proof.

## Features

- [Sessions](sessions.md) covers welcome, sidebar menu, keyboard creation, selection, the Needs action and Working sections, and row times.
- [Drafts and restart](drafts.md) covers focus, typing, autosave, and restoration.
- [Settlement](settlement.md) covers row and shortcut settling, un-settling, and the collapsible Settled section.
- [Search and navigation](search.md) covers search entry points, matching, clearing, and sidebar visibility.
- [Customize](customize.md) covers native component moves, split sizing, duplicate conversations, tabs, restricted Claude Code helper previews, Codex helper refusal, Borrow, per-project Apply/Discard/Undo/Reset, fitting, focus, and restart.
- [Conversations](conversations.md) covers sending, streaming, approvals, steering, stopping, and restoring a conversation.

## Coverage rules

For each run, record the entry points actually exercised. Do not mark an entire feature complete when only a shortcut passed. Record precondition failures rather than removing the failing step. PR viewing, saved prompts, and model controls remain separate scope. Composition controls have their own native evidence requirements.
