# Testing Keystone

Use behavior tests for the edit loop. Keep native desktop tests small. E2E-only testing makes session lifecycle failures harder to isolate and couples every iteration to graphics and provider availability.

## Fast loop

Run formatting, then the tests for the changed crate. Keep the session core independent of GPUI so its tests do not compile the graphics stack. Test observable lifecycle rules, persistence, interruption, stale events, and denied actions. Do not test getters or mirror private implementation details.

Use temporary project directories and isolated application data. Never run tests against real provider configuration or user repositories. Scripted providers should exercise the same protocol boundary as live adapters. Label their output as test data.

## GPUI behavior

GPUI's `#[gpui::test]` supports simulated platform input and deterministic asynchronous execution. Use it to test the composer, session selection, permission responses, and restoration. These tests do not prove native rendering, window-manager behavior, clipboard integration, or GPU compatibility.

## Native checks

Run a small native smoke suite for launch, input focus, keyboard shortcuts, window close and reopen, and saved state. Capture screenshots and process logs on failure. Do not call a headless model test desktop E2E. Native automation must be demonstrated on the target Linux session before declaring it supported.

Use the [project verification skill](../crates/keystone-desktop/.agents/skills/verify/SKILL.md) for nested Wayland control. The `keystone-verify` MCP server starts a headless private desktop, clicks and waits on named elements from the app's frame, and stores action logs, screenshots, and copies of the actual workspace file. A named click does not prove the visual result. Inspect the screenshot. Review [the feature map](../crates/keystone-desktop/.agents/skills/verify/features/README.md) so a shortcut check does not silently substitute for an untested mouse entry point.

## Provider checks

Run separate opt-in live checks for Claude Code and Codex after adapter changes and before release. Verify streaming, cancellation, steering, approval round trips, and resume in a disposable repository. Live checks can consume subscription limits and require credentials. Do not make them the default edit loop.

## Build feedback

Keep Cargo's incremental artifacts between runs. Use `cargo check` for compile feedback and build the executable when validating actual behavior. Pin dependencies with Cargo.lock. Start with `cargo test`; add nextest or compiler caching only after measuring a bottleneck. Do not clean target between ordinary edits.

## Evidence

- https://docs.rs/gpui/0.2.2/gpui/attr.test.html
- https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md
