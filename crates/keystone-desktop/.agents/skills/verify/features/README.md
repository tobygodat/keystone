# Keystone verification map

Use this map with ../SKILL.md. The primary user interface is the native GPUI window. Core tests and GPUI simulated input are supporting checks.

## Baseline

Drive the app through the `keystone-verify` MCP server. Call `start` for a private headless Sway desktop with an empty disposable workspace. It renders offscreen at the requested size, so host focus and visibility do not matter. Require `doctor` to identify the newly launched binary and a focused native window.

Prefer named elements. Call `click` with `{"id":"..."}`, then `wait_for` the element that should appear or disappear. `elements` lists the named elements in the latest frame. Its `visible` bounds exclude clipping but not overlap by menus or popovers. A named click does not prove the visual result. Inspect a screenshot after each step. Unnamed controls, such as the window controls, still need coordinates from a fresh screenshot. Never click using stale coordinates.

Read state with `workspace`. Preserve it with `snapshot`. These tools observe the real SQLite projections read-only, including committed WAL contents. Snapshots retain a self-contained database backup and its matching JSON workspace export. After every mutation, wait for the visible result and saved state. A wait alone is not proof.

## Features

- [Sessions](sessions.md) covers welcome, sidebar menu, keyboard creation, selection, the Needs action and Working sections, and row times.
- [Drafts and restart](drafts.md) covers focus, typing, autosave, and restoration.
- [Save and Quit](quit.md) covers the shared close/Quit gate, sticky failed-save recovery, provider teardown, and interrupted restoration.
- [Settlement](settlement.md) covers row and shortcut settling, un-settling, and the collapsible Settled section.
- [Search and navigation](search.md) covers search entry points, matching, clearing, and sidebar visibility.
- [Customize](customize.md) covers native component moves, split sizing, duplicate conversations, tabs, restricted Claude Code helper previews, Codex helper refusal, Borrow, per-project Apply/Discard/Undo/Reset, fitting, focus, and restart.
- [Wheel and maximize](wheel-and-maximize.md) covers emitted native wheel events, independent pane movement, deliberate unread observation, long-draft scrolling and an honest compositor maximize/restore verdict.
- [Settings](settings.md) covers the settings page: opening, sections, theme, fonts, density, text size, and restart.
- [Files](files.md) covers the sidebar's Sessions and Files toggle, lazy folder listing that respects `.gitignore`, opening files in an external editor, Copy path, and the spawn error row.
- [Conversations](conversations.md) covers sending, streaming, approvals, steering, stopping, choosing the provider and model before the first Send, and restoring a conversation.
- [File mentions](mentions.md) covers the composer's `@` picker over the session folder's files, choosing a path as one token that Backspace removes whole, Escape, sending as typed, Files' Mention in composer, and tokens after a restart.
- [Attachments](attachments.md) covers attaching images by paste, drop, the attach button or a typed path, their chips, refusals, restoring them with the draft, sending them to each provider in its own image form, and mentioning other files by path.
- [Handoff](handoff.md) covers handing a session off to a new linked session on another provider, account or model: the handoff text and budget, the selector, Send on the chosen account, link rows both ways, and restart.
- [Claude idle shutdown](provider-idle.md) covers proven-idle eviction of a scripted Claude, lazy resume with the same thread, folder, flags, account and requested model, and held work that keeps its process.
- [Keyboard transcript actions](transcript-keyboard.md) covers virtual row/action navigation, exact copy bytes, occurrence-scoped Jump and hidden-focus transitions.
- [Tabs and split panes](tabs.md) covers session tabs, Split right, opening from the sidebar, cycling, closing and collapsing, dragging tabs between strips and to pane edges, pane dividers, and restoring all of it after Quit or a crash.
- [Usage](usage.md) covers each account's limit windows in Settings → Accounts, the composer footer meter at 80% and above, and the saved reading with its age after a restart.
- [Projects](projects.md) covers the project switcher in the sidebar header and Ctrl+O, opening a folder by typed path or the platform picker, recent projects, each window's own project, sessions, tabs and layout on switch and relaunch, new sessions in the project folder, missing folders, and removing a project from the list.
- [Worktrees](worktrees.md) covers starting a session in its own git worktree from the welcome composer, the new-session menu and the palette, the branch on the row and the top bar, Remove worktree refusing uncommitted or unpushed work, the missing-folder state, orchestrator launches into a worktree, and relaunch and crash restore.
- [Integrations](integrations.md) covers Settings → Integrations: each account's MCP servers, skills and plugins with scope and source file, names without secret values, and server status from running sessions.
- [Multiple windows](windows.md) covers new windows by keyboard, sidebar and tab menus, each window's own tabs, panes, selection and project, one session in two windows, closing a window while its work runs, the last window, and restoring every window after Quit or a crash.
- [Notifications](notifications.md) covers desktop notifications for a finished turn, an approval request or a failed turn in a session nobody sees, their coalescing, opening a session from one, and their switches in Settings.
- [Saved prompts](prompts.md) covers the prompt library in Settings → Prompts, global and project scope, inserting from the / menu and the palette's Insert prompt…, shortcuts that a provider command shares, and restart.
- [Command palette](palette.md) covers opening with Ctrl+K or Ctrl+Shift+P, filtering, running a command, dismissing with focus restored, and dialog blocking.

## Coverage rules

For each run, record the entry points actually exercised. Do not mark an entire feature complete when only a shortcut passed. Record precondition failures rather than removing the failing step. PR viewing remains separate scope, as do model controls beyond the draft's provider and model choice (effort, fast mode, service tier, switching after the first Send). Composition controls have their own native evidence requirements.
