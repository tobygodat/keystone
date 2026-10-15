# Keystone

A Rust and GPUI desktop workspace for Claude Code and Codex.

This is a native UI preview styled after the supplied MonoCode reference. Create and select local session records, search them, filter settled sessions, save single-line drafts, settle and un-settle them, and restore them on restart. Provider execution is not connected. The application does not send prompts. The optional visual fixture is labeled as sample content.

## Run the preview

Run `cargo run -p keystone-desktop --locked` in a Linux graphical session. Press Ctrl+Q to quit. Ctrl+N creates a Claude session, Ctrl+Shift+N creates a Codex session, Ctrl+L focuses the composer, Ctrl+F focuses search, and Ctrl+Shift+S toggles settlement.

The preview saves to `$XDG_DATA_HOME/keystone/workspace.json`, or `$HOME/.local/share/keystone/workspace.json`. Set `KEYSTONE_STATE_PATH` to use an isolated state file. A file lock prevents concurrent desktop processes from writing the same workspace. Draft writes wait 250 ms after typing stops. Session changes and normal window close flush the visible draft immediately; failed saves keep the session open.

## Native verification driver

Use the [desktop verification skill](crates/keystone-desktop/.agents/skills/verify/SKILL.md) for a separate Sway desktop with screenshots, mouse clicks, keyboard input, and disposable state. It includes a four-feature verification map and exact launch, doctor, drive, and cleanup instructions. Run it from the desktop crate context to discover the package-local Codex skill, or read the linked file directly from the workspace root.

The driver was proved against the actual GPUI binary. Keep its nested window visible during capture on Hyprland. See [the verification report](docs/verification-driver.md) for the tested journey and remaining coverage.

## Run checks

- Run `bash scripts/check.sh` for formatting and core behavior tests.
- Run `bash scripts/check.sh ui` for GPUI simulated-input tests.
- Run `bash scripts/check.sh build` to compile the native application.
- Run `python scripts/native-smoke.py` on Hyprland with the Lua dispatcher API to exercise native keyboard creation, settlement, draft entry, and restart with isolated state. It captures empty, populated, and restored screenshots. It restores the previous window focus and stops its own preview process.

Set `KEYSTONE_VISUAL_FIXTURE=1` to show a labeled sample transcript with a table and code block. It uses the same window and layout as the normal preview. Set `KEYSTONE_CAPTURE_DIR` to choose a separate native evidence directory. On the current Hyprland Lua profile, `KEYSTONE_TEST_CLOSE=1` also checks that failed storage blocks a compositor close request. Native typing defaults to 100 ms per character. The 10 ms stress case remains an open rendering issue.

The composer is single-line in this increment. Multiline paste is rejected with a visible explanation rather than flattened. Full multiline composition and live providers remain work for the next increment.

GPUI input tests do not establish native rendering correctness. Native window checks and live provider checks are separate.

Cargo.lock pins libc 0.2.186 because GPUI 0.2.2 depends on xattr 0.2.3, which references the Linux ENOATTR constant removed in newer libc releases. Recheck this compatibility when upgrading GPUI.

See [the accepted product scope](docs/scope.md) and [the test strategy](docs/testing.md). Most accepted features are not implemented yet.
