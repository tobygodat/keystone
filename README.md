# Keystone

A Rust and GPUI desktop workspace for Claude Code and Codex.

This is a native UI preview styled after the supplied MonoCode reference. Create and select local session records, search them, filter settled sessions, save multiline drafts, settle and un-settle them, and restore them on restart. Codex sessions send messages to your installed `codex`, and Claude Code sessions to your installed `claude`. Both stream the reply, with approvals, steering, stop, and the conversation restored after a restart. The optional visual fixture is labeled as sample content.

## Run the preview

Run `cargo run -p keystone-desktop --locked` in a Linux graphical session. Press Ctrl+Q to quit. The gear at the bottom of the left rail chooses the theme (System, Light, or Dark), the density, and the interface font. Escape closes an open menu. Ctrl+N creates a Claude session, Ctrl+Shift+N creates a Codex session, Ctrl+L focuses the composer, Ctrl+Enter sends, Ctrl+F focuses search, and Ctrl+Shift+S toggles settlement.

Replies render as Markdown, with tables, code blocks, and copy buttons. The shield button in the composer sets each session's approval mode: Provider settings (the default, which uses your own Codex or Claude Code configuration), Read only, Ask first, Auto, or Full access. Full access asks for confirmation first. A change applies to every message sent after it, steers included, and is saved with the session. Codex gets the matching approval policy and sandbox, and Claude Code the matching `--permission-mode`; see "Approval modes" in [the provider design](docs/design/providers.md).

Codex and Claude Code run with your own `codex` and `claude` logins and, under Provider settings, your own approval settings, in the directory Keystone was started from. Set `KEYSTONE_PROJECT_DIR` to choose another directory, or `KEYSTONE_CODEX_PATH` and `KEYSTONE_CLAUDE_PATH` to use specific binaries. Each Claude session runs its own `claude` process until Keystone quits. Each session's completed messages are kept in `sessions/<id>.jsonl` next to the workspace file. See [the provider design](docs/design/providers.md).

The preview saves to `$XDG_DATA_HOME/keystone/workspace.json`, or `$HOME/.local/share/keystone/workspace.json`. Set `KEYSTONE_STATE_PATH` to use an isolated state file. A file lock prevents concurrent desktop processes from writing the same workspace. Draft writes wait 250 ms after typing stops. Session changes and normal window close flush the visible draft immediately; failed saves keep the session open.

## Native verification driver

Use the [desktop verification skill](crates/keystone-desktop/.agents/skills/verify/SKILL.md) for a separate Sway desktop with screenshots, mouse clicks, keyboard input, and disposable state. It includes a four-feature verification map and exact launch, doctor, drive, and cleanup instructions. Run it from the desktop crate context to discover the package-local Codex skill, or read the linked file directly from the workspace root.

The driver was proved against the actual GPUI binary. It runs as the `keystone-verify` MCP server and renders headless by default, so the host window does not need to stay visible. See [the verification report](docs/verification-driver.md) for the tested journey and remaining coverage.

## Run checks

- Run `bash scripts/check.sh` for formatting, the theme color check, and core behavior tests.
- Run `bash scripts/check.sh ui` for GPUI simulated-input tests of the app and its components.
- Run `bash scripts/check.sh build` to compile the native application and the component gallery.
- Run `python scripts/perf.py --note "what changed"` to record build and render timings. See [performance](docs/performance.md).
- Run `python scripts/native-smoke.py` on Hyprland with the Lua dispatcher API to exercise native keyboard creation, settlement, draft entry, and restart with isolated state. It captures empty, populated, and restored screenshots. It restores the previous window focus and stops its own preview process.

Set `KEYSTONE_VISUAL_FIXTURE=1` to show a labeled sample transcript, written in Markdown, with a list, a table, and a code block. It uses the same window and layout as the normal preview. Set `KEYSTONE_CAPTURE_DIR` to choose a separate native evidence directory. On the current Hyprland Lua profile, `KEYSTONE_TEST_CLOSE=1` also checks that failed storage blocks a compositor close request. Native typing defaults to 100 ms per character. The 10 ms stress case remains an open rendering issue.

The composer supports multiline drafts. Enter and Shift+Enter insert a new line. Text wraps to the available width, grows to eight visible rows, and scrolls vertically for longer drafts. Home and End move within a visual row. Ctrl+Home and Ctrl+End move to the beginning and end of the draft. Search remains single-line. Live providers and attachments remain future work.

GPUI input tests do not establish native rendering correctness. Native window checks and live provider checks are separate.

Cargo.lock pins libc 0.2.186 because GPUI 0.2.2 depends on xattr 0.2.3, which references the Linux ENOATTR constant removed in newer libc releases. Recheck this compatibility when upgrading GPUI.

The interface is built from the `keystone-ui` component library. Run `cargo run -p keystone-ui --example gallery` to see every component. See [the design system](docs/design-system.md).

See [the accepted product scope](docs/scope.md) and [the test strategy](docs/testing.md). Most accepted features are not implemented yet.
