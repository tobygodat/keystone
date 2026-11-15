# Product

## Users

Developers who run Claude Code and Codex on their own machine, often several sessions and accounts at once, and want one native window to start, steer, review and land that work. The first user is Toby, on Apple Silicon macOS. Linux is not a target for now ([ADR 0013](docs/adr/0013-macos-first-queue-plan-mode-multi-agent-previews-plugins.md)).

## Purpose

Keystone is a native desktop workspace for Claude Code and Codex. It drives the user's own installed `claude` and `codex`, with their own logins and configuration, and adds what the CLIs lack: many sessions side by side, an attention inbox, durable history, cross-provider handoffs and second opinions, and Git and pull request controls.

## Positioning

Keystone aims for t3code-grade orchestration and backend reliability, implemented natively in Rust and GPUI with its own interface. It takes some of t3code's workflow, such as queued messages, plan mode and child sessions, but not its interface or sidebar, and it doesn't promise full CLI parity: an external terminal covers interactive CLI features it doesn't support. The full scope, with what is deferred and removed, is [ADR 0001](docs/adr/0001-product-scope.md) as amended by [ADR 0010](docs/adr/0010-terminal-browser-scheduling-editor-diffs-remote-devices.md) and [ADR 0013](docs/adr/0013-macos-first-queue-plan-mode-multi-agent-previews-plugins.md).

ADR 0010 brings seven features into scope that aren't built yet: an integrated terminal, a browser preview panel that agents can drive, scheduled tasks, a built-in file editor, a diff viewer, remote environments over SSH or Tailscale, and a device panel for the iOS Simulator and Android Emulator. ADR 0013 brings in queued messages, plan mode, child sessions with fan-out and workflows, HTML previews and Claude artifacts beside a session, and plugins for Keystone itself. Side conversations (`/btw`), Monos, session snoozing, project command buttons and session export stay out.

## Principles

- **The provider stays native.** Keystone runs the unmodified CLI with the user's login and settings. It never handles credentials, and removing an app feature never disables the provider's own tools, memory or skills.
- **Never fabricate.** Keystone doesn't invent agent output or imply a provider is connected when it isn't. Sample content is labeled as sample.
- **Approval is the user's.** Settlement never approves a request, and automatic settlement respects active work and pending approvals. Keystone changes the provider's own approval mode and never auto-approves on its behalf.
- **Messages during work wait or steer.** A message sent during a turn can wait for the turn to end, or steer it. Recovering interrupted work is a separate, explicit action.
- **Closing a window isn't quitting.** Work keeps running until an explicit Quit.
- **Fast and measured.** Build time and render cost are tracked in the repo, and changes are proven in the real window, not only in simulated tests.

## Accessibility

- Every action has a keyboard path. Transcript actions, diagram controls and widgets take part in keyboard focus, with a visible focus ring and Enter or Space to activate.
- Interface text sits on whole pixels, and density changes control sizes without breaking the layout.
- Light, dark and system themes, and a choice of interface font.
- Pointer-only gaps, such as panel buttons without a Tab order, are bugs.
