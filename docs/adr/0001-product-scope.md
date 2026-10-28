# 0001. Product scope

Status: Accepted, 2026-10-15. Recorded as an ADR 2026-10-27 from `docs/scope.md`.

## Context

The feature-selection conversation settled what Keystone is and what it leaves out. These decisions describe the intended product, not what is built. Linear tracks what is built.

## Decision

**Direction.** Keystone aims to match t3code-grade orchestration and backend reliability, implemented natively in Rust, with its own interface. It does not adopt t3code's interface, sidebar, or workflow opinions.

**Initial product.** A Rust and GPUI desktop application. Local execution with Claude Code and Codex. Preserve provider-native capabilities and configuration. Use app-owned orchestration for cross-provider work and durable session management. Provide an external-terminal fallback for unsupported interactive CLI features. Do not promise complete CLI parity through structured integrations.

Support multiple accounts per provider, such as two Claude Max subscriptions. Preserve each account's provider-native configuration and isolate accounts from each other.

Keep project navigation, Git worktrees, tabs, split panes, multiple windows, tab dragging, session organization, unread indicators, filters, search, and restoration. Closing a window can leave work running. Explicit Quit ends the application.

Keep conversation forks, context handoffs between providers, second opinions, checkpoints, rollback, and detailed agent activity. Messages during active work steer only. There is no follow-up queue. Keep native provider compaction. No separate plan-mode UI.

Keep file browsing with external-editor links, notes, usage displays, permission controls, saved prompts, persistent drafts, attachments, image and video previews, screenshot capture, rich Markdown, copy actions, and sending selections to agents. Prefer interactive charts, diagrams, forms, and tools within chat when useful.

Keep the pull request tab, checks and review monitoring, repair actions, Git staging, commit, push, and PR creation controls. Keep the attention inbox with settle and un-settle. The Needs action grouping, settle, and un-settle stay as they are and must not be extended. Settlement must not approve requests. Automatic settlement must respect active work and pending approvals.

Keep notifications, optional sounds, command palette, shortcuts, quick composer, themes, fonts, density, project icons and colors, recovery controls, and usage-limit recovery. Recovery of interrupted work is distinct from queueing messages.

Keep MCP, skill, and plugin management. Discover existing Claude and Codex configuration, preserve provider-specific settings, and selectively sync compatible items. Detect conflicting external edits. Keep slash-command discovery, file mentions, and importing existing provider sessions where supported.

**Deferred.**

- Remote control through Tailscale.
- Integrated terminal.
- Advanced provider settings UI.

**Removed.**

- Monos, app-owned persistent cross-project agents, and scheduled tasks.
- Built-in file editor and local Git diff viewer.
- Browser preview panel.
- Session snoozing and project command buttons.
- Session export.
- Side conversations through `/btw`.
- Follow-up message queues and dedicated plan-mode controls.

Removing app features does not disable the provider's native tools, memory, or skills.

**First increment.** Establish the Rust workspace, tested session state and persistence, and a native GPUI session window, with provider execution after it. The first increment must never fabricate agent output or imply that a provider is connected.

## Consequences

- A feature outside these lists needs a new ADR before it is built.
- The Needs action grouping and settlement are frozen. Work that wants to extend them needs a new ADR first (see TOB-103).
- Deferred items stay out of Linear's active work until a new ADR brings them back.
