# Keystone

A Rust and GPUI desktop workspace for Claude Code and Codex.

This is a native UI preview styled after the supplied MonoCode reference. Create and select local session records, search them, save multiline drafts, see the sessions that need you (an approval, a failed turn, or an unread reply) above the ones still working, settle and un-settle them into a collapsible Settled section, and restore them on restart. Codex sessions send messages to your installed `codex`, and Claude Code sessions to your installed `claude`. Both stream the reply, with approvals, steering, stop, and the conversation restored after a restart. The optional visual fixture is labeled as sample content.

## Run the preview

Run `cargo run -p keystone-desktop --locked` in a Linux graphical session. Press Ctrl+Q to quit. The gear at the bottom of the sidebar chooses the theme (System, Light, or Dark), the density, and the interface font. Escape closes an open menu. Ctrl+N creates a Claude session, Ctrl+Shift+N creates a Codex session, Ctrl+L focuses the composer, Ctrl+Enter sends, Ctrl+F focuses search, and Ctrl+Shift+S toggles settlement.

Replies render as Markdown, with tables, code blocks, and copy buttons. The shield button in the composer sets each session's approval mode: Provider settings (the default, which uses your own Codex or Claude Code configuration), Read only, Ask first, Auto, or Full access. Full access asks for confirmation first. A change applies to every message sent after it, steers included, and is saved with the session. Codex gets the matching approval policy and sandbox, and Claude Code the matching `--permission-mode`; see "Approval modes" in [the provider design](docs/design/providers.md).

Codex and Claude Code run with your own `codex` and `claude` logins and, under Provider settings, your own approval settings, in the directory Keystone was started from. Set `KEYSTONE_PROJECT_DIR` to choose another directory, or `KEYSTONE_CODEX_PATH` and `KEYSTONE_CLAUDE_PATH` to use specific binaries. Each Claude session runs its own `claude` process until Keystone quits. Each session's completed messages are kept in `sessions/<id>.jsonl` next to the workspace file. See [the provider design](docs/design/providers.md).

The preview saves to `$XDG_DATA_HOME/keystone/workspace.json`, or `$HOME/.local/share/keystone/workspace.json`. Set `KEYSTONE_STATE_PATH` to use an isolated state file. A file lock prevents concurrent desktop processes from writing the same workspace. Draft writes wait 250 ms after typing stops. Session changes and normal window close flush the visible draft immediately; failed saves keep the session open.

## Customize the sidebar

An early experiment in personalizing the interface in plain language. Click **Customize** in the top bar. It shows the sidebar even if you hid it, and hides it again when you close. You can move the sidebar left or right and set its width from 220 to 520 px. Use the buttons, drag the highlighted inner edge, or press Alt+Left, Alt+Right, Alt+[ and Alt+]. You can also type a request such as "put the sidebar on the right and make it a bit wider", choose Claude Code or Codex, and press **Preview** or Ctrl+Enter.

The request uses your own `claude` or `codex` login in a new session with the provider's Read only policy and an empty temporary folder. Keystone denies any approval requests. The provider returns a validated layout proposal that only changes the preview. Changing the layout yourself while a request runs cancels it. **Apply** (Ctrl+S) saves the preview, **Discard** goes back to the saved layout, **Reset** previews the default (left, 302 px), and **Undo** restores the preceding saved layout. Undo keeps one step during the current app run. Escape or the close button discards an unapplied preview. This experiment changes sidebar side and width. A narrow window draws the sidebar narrower without changing the saved width.

## View on your laptop

You can watch the app on another Wayland machine while it builds and runs on this one. Install waypipe on the laptop once with `sudo pacman -S waypipe`. Then run this from a Keystone checkout on the laptop:

```sh
scripts/laptop-preview
```

It connects to `tobygodat@desk` over SSH and runs `~/keystone/scripts/preview.py` there under waypipe. Set `KEYSTONE_PREVIEW_HOST` for another host. Plain `waypipe ssh` fails over Tailscale SSH, which creates forwarded sockets as root, so the script forwards a localhost TCP port and bridges it with socat on both ends.

The window opens on the laptop. `scripts/preview.py` follows the worktree that changed most recently, uncommitted edits included, so it shows whichever agent is working right now. It prints a line when it switches. After a change has been quiet for about 3 seconds, it rebuilds at low priority in `~/.cache/keystone-preview/target`, so it never waits on an agent's cargo lock, and restarts the app. A failed build leaves the last good app running and prints the errors. Pass a branch name or worktree path to pin one worktree, for example `scripts/laptop-preview my-branch`. `--once` builds and runs without following changes.

The preview keeps its own workspace in `~/.local/share/keystone-preview/workspace.json`, so it doesn't conflict with a copy running on the desktop. Its Claude Code and Codex sessions run in the checkout the script lives in, not in the worktree being previewed. Set `KEYSTONE_PROJECT_DIR` to choose another directory. Pressing Ctrl+Q in the app or Ctrl+C in the terminal, closing the terminal, or dropping the connection stops everything on both machines.

## Native verification driver

Use the [desktop verification skill](crates/keystone-desktop/.agents/skills/verify/SKILL.md) for a separate Sway desktop with screenshots, mouse clicks, keyboard input, and disposable state. It includes a four-feature verification map and exact launch, doctor, drive, and cleanup instructions. Run it from the desktop crate context to discover the package-local Codex skill, or read the linked file directly from the workspace root.

The driver was proved against the actual GPUI binary. It runs as the `keystone-verify` MCP server and renders headless by default, so the host window does not need to stay visible. See [the verification report](docs/verification-driver.md) for the tested journey and remaining coverage.

## Run checks

- Run `bash scripts/check.sh` for formatting, the theme color check, and core behavior tests.
- Run `bash scripts/check.sh ui` for GPUI simulated-input tests of the app and its components.
- Run `bash scripts/check.sh core-fast` for the same core checks with the nextest runner. Opt-in; see [docs/dev-iteration.md](docs/dev-iteration.md).
- Run `bash scripts/check.sh build` to compile the native application and the component gallery.
- Run `python scripts/perf.py --note "what changed"` to record build and render timings. See [performance](docs/performance.md).
- Install sccache and mold (`sudo pacman -S --needed sccache mold`) to share dependency builds across worktrees and link faster. Builds still work without them. See [performance](docs/performance.md#decisions).
- Run `python scripts/native-smoke.py` on Hyprland with the Lua dispatcher API to exercise native keyboard creation, settlement, draft entry, and restart with isolated state. It captures empty, populated, and restored screenshots. It restores the previous window focus and stops its own preview process.

Set `KEYSTONE_VISUAL_FIXTURE=1` to show a labeled sample transcript, written in Markdown, with a list, a table, and a code block. It uses the same window and layout as the normal preview. Set `KEYSTONE_CAPTURE_DIR` to choose a separate native evidence directory. On the current Hyprland Lua profile, `KEYSTONE_TEST_CLOSE=1` also checks that failed storage blocks a compositor close request. Native typing defaults to 100 ms per character. `KEYSTONE_KEY_DELAY_MS=10` runs the rapid-typing case, and `KEYSTONE_LONG_REPEAT` lengthens the long draft. `scripts/native-stress.py` repeats that case on headless desktops (KS-005, closed).

The composer supports multiline drafts. Enter and Shift+Enter insert a new line. Text wraps to the available width, grows to eight visible rows, and scrolls vertically for longer drafts. Home and End move within a visual row. Ctrl+Home and Ctrl+End move to the beginning and end of the draft. Search remains single-line. Live providers and attachments remain future work.

GPUI input tests do not establish native rendering correctness. Native window checks and live provider checks are separate.

GPUI comes from gpui-pre 0.3.9, the Zed snapshot that gpui-kit 0.7.3 pins. It no longer depends on xattr, so libc is no longer pinned.

The interface is built from the `keystone-ui` component library. Run `cargo run -p keystone-ui --example gallery` to see every component. See [the design system](docs/design-system.md).

See [the accepted product scope](docs/scope.md) and [the test strategy](docs/testing.md). Most accepted features are not implemented yet.
