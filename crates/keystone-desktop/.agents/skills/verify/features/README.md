# Keystone verification map

Use this map with ../SKILL.md. The primary user interface is the native GPUI window. Core tests and GPUI simulated input are supporting checks.

## Baseline

Start the nested driver with an empty disposable workspace. Require Doctor to identify the newly launched binary and a focused native window. Inspect a screenshot before choosing mouse coordinates. The requested desktop size is advisory. Never click using stale coordinates.

Read state with `{"kind":"workspace"}`. Preserve it with `{"kind":"snapshot"}`. These commands only observe the real state file. After every mutation, wait for the visible result and saved state. A wait alone is not proof.

## Features

- [Sessions](sessions.md) covers welcome, sidebar menu, keyboard creation, and selection.
- [Drafts and restart](drafts.md) covers focus, typing, autosave, and restoration.
- [Settlement](settlement.md) covers settling, un-settling, and settled filtering.
- [Search and navigation](search.md) covers search entry points, matching, clearing, and sidebar visibility.

## Coverage rules

For each run, record the entry points actually exercised. Do not mark an entire feature complete when only a shortcut passed. Record precondition failures rather than removing the failing step. Provider execution, PR viewing, saved prompts, split panes, and model controls are accepted future scope, not implemented journeys.
