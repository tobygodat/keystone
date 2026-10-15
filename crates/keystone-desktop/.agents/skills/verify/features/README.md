# Keystone verification map

Use this map with ../SKILL.md. The primary user interface is the native GPUI window. Core tests and GPUI simulated input are supporting checks.

## Baseline

Drive the app through the `keystone-verify` MCP server. Call `start` for a private headless Sway desktop with an empty disposable workspace. It renders offscreen at the requested size, so host focus and visibility do not matter. Require `doctor` to identify the newly launched binary and a focused native window.

Prefer named elements. Call `click` with `{"id":"..."}`, then `wait_for` the element that should appear or disappear. `elements` lists the named elements in the latest frame. Its `visible` bounds exclude clipping but not overlap by menus or popovers. A named click does not prove the visual result. Inspect a screenshot after each step. Unnamed controls, such as the titlebar tab, still need coordinates from a fresh screenshot. Never click using stale coordinates.

Read state with `workspace`. Preserve it with `snapshot`. These tools only observe the real state file. After every mutation, wait for the visible result and saved state. A wait alone is not proof.

## Features

- [Sessions](sessions.md) covers welcome, sidebar menu, keyboard creation, and selection.
- [Drafts and restart](drafts.md) covers focus, typing, autosave, and restoration.
- [Settlement](settlement.md) covers settling, un-settling, and settled filtering.
- [Search and navigation](search.md) covers search entry points, matching, clearing, and sidebar visibility.
- [Codex conversations](conversations.md) covers sending, streaming, approvals, steering, stopping, and restoring a conversation.

## Coverage rules

For each run, record the entry points actually exercised. Do not mark an entire feature complete when only a shortcut passed. Record precondition failures rather than removing the failing step. Claude Code execution, PR viewing, saved prompts, split panes, and model controls are accepted future scope, not implemented journeys.
