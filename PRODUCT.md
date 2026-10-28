# Product

## Users

Developers who run Claude Code and Codex on their own machine, often several sessions and accounts at once, and want one native window to start, steer, review and land that work. The first user is Toby, on Linux (Hyprland) and Apple Silicon macOS.

## Purpose

Keystone is a native desktop workspace for Claude Code and Codex. It drives the user's own installed `claude` and `codex`, with their own logins and configuration, and adds what the CLIs lack: many sessions side by side, an attention inbox, durable history, cross-provider handoffs and second opinions, and Git and pull request controls.

## Positioning

Keystone aims for t3code-grade orchestration and backend reliability, implemented natively in Rust and GPUI with its own interface. It doesn't adopt t3code's interface, sidebar or workflow opinions, and it doesn't promise full CLI parity: an external terminal covers interactive CLI features it doesn't support. The full scope, with what is deferred and removed, is [ADR 0001](docs/adr/0001-product-scope.md).

## Principles

- **The provider stays native.** Keystone runs the unmodified CLI with the user's login and settings. It never handles credentials, and removing an app feature never disables the provider's own tools, memory or skills.
- **Never fabricate.** Keystone doesn't invent agent output or imply a provider is connected when it isn't. Sample content is labeled as sample.
- **Approval is the user's.** Settlement never approves a request, and automatic settlement respects active work and pending approvals. Keystone changes the provider's own approval mode and never auto-approves on its behalf.
- **Messages during work steer.** There is no follow-up queue. Recovering interrupted work is a separate, explicit action.
- **Closing a window isn't quitting.** Work keeps running until an explicit Quit.
- **Fast and measured.** Build time and render cost are tracked in the repo, and changes are proven in the real window, not only in simulated tests.

## Accessibility

- Every action has a keyboard path. Transcript actions, diagram controls and widgets take part in keyboard focus, with a visible focus ring and Enter or Space to activate.
- Interface text sits on whole pixels, and density changes control sizes without breaking the layout.
- Light, dark and system themes, and a choice of interface font.
- Pointer-only gaps, such as panel buttons without a Tab order, are bugs.
