# Keystone on macOS

Keystone builds and runs on Apple Silicon Macs. GPUI draws through Metal there. Intel Macs are not tested.

## Run it

Install the Command Line Tools (`xcode-select --install`) and Rust, then run `cargo run -p keystone-desktop --locked`. Press ⌘Q to quit.

Xcode is not needed. On macOS Keystone builds GPUI with two features. `runtime_shaders` has Metal compile the shaders when the app starts, since the Command Line Tools have no `metal` compiler. `font-kit` is GPUI's macOS text system: without it GPUI draws no text at all and says so only in a log Keystone does not show, so `bash scripts/check.sh apple` fails if it is ever dropped.

The workspace lives in `~/Library/Application Support/Keystone/workspace.sqlite3`. `KEYSTONE_STATE_PATH` still chooses another one.

## What differs from Linux

**Shortcuts.** Every shortcut that uses Ctrl on Linux uses ⌘ on macOS. The bindings say `secondary-…`, which GPUI reads as Cmd on macOS and Ctrl elsewhere, and hints show `⌘N` where Linux shows `Ctrl+N`. Ctrl+Tab and Ctrl+Shift+Tab, which switch tabs, keep Ctrl, because ⌘Tab switches apps. Text fields also take ⌘← and ⌘→ for the start and end of a line and ⌘↑ and ⌘↓ for the start and end of the text, since Mac keyboards have no Home or End key. `bash scripts/check.sh` fails on a new `ctrl-` binding or a `Ctrl+` hint written out by hand.

**Window controls.** macOS draws the close, minimize and zoom buttons, the traffic lights, at the left end of Keystone's top bar, centred on it at every density. The bar starts its own controls to their right, and keeps that gap in full screen. Linux keeps Keystone's minimize, maximize and close buttons at the right end of the bar. The macOS smoke journey's screenshots come from GPUI's renderer, so they do not show the lights; `screencapture -l <window id>` does.

**Menu bar.** The Keystone menu has Settings… (⌘,), Services, Hide Keystone (⌘H), Hide Others (⌥⌘H), Show All and Quit Keystone (⌘Q). Window has Minimize (⌘M) and Zoom. There is no Edit menu: text fields and the transcript bind ⌘C, ⌘X, ⌘V and ⌘A to their own actions, and a menu item can name only one of them. Linux has no menu bar.

**Launching from Finder.** An app started from Finder, the Dock or `open` gets launchd's short `PATH` and `/` as its folder. When `TERM` is unset, Keystone takes the environment your login shell sets up (`$SHELL -l -i`, at most 5 seconds) and starts in your home folder. So `claude`, `codex`, `git`, `gh` and what they run, such as `node`, are found, and variables you export, such as `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, apply as they do in a terminal. The shell runs in a session of its own with no terminal, as under Finder; an interactive shell in the background of a terminal's session would stop when it touched the terminal. A launch from a terminal keeps the terminal's environment.

**Opening files.** With no GUI editor in `$VISUAL` or `$EDITOR`, files open with `open` instead of `xdg-open`.

**Notifications.** The macOS notification center accepts only apps in a bundle. Keystone has no `.app` bundle yet, so a `cargo run` build posts no notifications and carries on without them. Notification clicks are untested on macOS for the same reason.

**Provider processes.** Stopping a provider also stops the processes it started, including ones that left its process group. Linux finds them through `/proc` and pins each with a pidfd. macOS finds them through `proc_listallpids` and `proc_pidinfo`, and checks each process's start time again before it signals, because macOS has no pidfd. The helper that keeps the process group's ID reserved during a stop polls each second for Keystone's exit on macOS, where Linux tells it at once. Keystone learns that a provider exited through kqueue on macOS, because `waitid` there also returns, again and again, for a process that is only stopped.

**Keystone's MCP server.** A response is sent in full and the connection half-closed, then whatever the client still sends is read for up to 100 ms before the socket closes. Closing with input unread resets the connection, and macOS then discards the part of the response the client has not read yet.

**Recovery.** Reading a crashed Claude conversation checks that the file it opened is inside Claude's folder: with `/proc/self/fd` on Linux and `F_GETPATH` on macOS.

## Verify on a Mac

The verify skill's private Sway desktop exists only on Linux. On a Mac, run:

```sh
python3 scripts/macos-smoke.py
```

It builds the debug app with the `capture` feature and the scripted providers, then launches it with a throwaway home, workspace and project. It creates a Claude session with ⌘N, sends a message with ⌘↩, waits for the fake Claude's reply, opens the palette with ⌘K to show its hints, types a draft, quits with ⌘Q, relaunches, and checks that the session, reply and draft came back. It also checks that the launch, which has no `TERM`, adopted the login shell's `PATH`. Screenshots and `report.json` go to `.audit/macos-smoke-*/`.

macOS lets a process inject input or read the screen only with Accessibility and Screen Recording permission, so the journey uses GPUI's own hooks instead. A debug build started with `KEYSTONE_AUTOMATION_INBOX` runs commands a driver writes to that folder (see `crates/keystone-ui/src/automation.rs`). Keys and clicks enter through `Window::dispatch_keystroke` and `Window::dispatch_event`, the calls GPUI makes for real input, so bindings, focus and hit-testing run as they do for a person. macOS's own event translation, input methods and window activation are skipped. Launched with `KEYSTONE_AUTOMATION_BACKGROUND` set as well, Keystone stays behind your other apps and never takes focus, unless the launch opens the quick composer. macOS draws no new frames while a window is fully covered, so the smoke journey, which waits for frames, does not set it. Screenshots are the window's frames from GPUI's Metal renderer (`Window::render_to_image`), so they show the window's content without its title bar or the menu bar, at the window's size in pixels (2200×1440 for the default 1100×720 window on a Retina display). The harness checks state through the workspace database and cannot read text off the screen, so look at the screenshots: a build without `font-kit` passed the journey with every label blank.

Drive Keystone on a Mac through these hooks, never with screenshot-and-click computer use such as Codex Computer Use, a computer-use MCP server, `cliclick` or `osascript` keystrokes. Those move the pointer and type into whatever window is in front, so they take over the Mac from the person using it. Accessibility-tree tools such as [agent-desktop](https://github.com/vercel-labs/agent-desktop) cannot drive Keystone yet, even in their headless mode, which neither moves the pointer nor takes focus. GPUI puts only elements with an explicit `.role()` into the macOS accessibility tree, and Keystone sets a role on almost none. agent-desktop sees the title bar's three buttons and an empty view. TOB-325 tracks exposing the controls.

Some tests hold timing budgets that a busy Mac can miss, such as the workspace's 100 ms wait for its writer and the 500 ms budget of `many_large_additions_are_trimmed_quickly`. When the lanes fail only there, rerun them with `RUST_TEST_THREADS=4`.

## The Linux guard

Linux has no macOS SDK, so nothing native runs here. `bash scripts/check.sh apple` type-checks every crate, tests included, for `aarch64-apple-darwin` with warnings as errors. It needs `rustup target add aarch64-apple-darwin`, clang and llvm-ar.

It catches code that does not compile on macOS: a `cfg(target_os = "linux")` item with no macOS arm, a Linux-only `libc` call or constant, and a variable or `mut` left unused by a cfg. It compiles C sources (SQLite) to empty objects, uses empty placeholders for the Metal shaders that only macOS can compile, and links nothing. So it cannot catch a link error, a shader error, a crash, or anything that differs at run time, such as paths, menus, rendering or a test's result. Only a Mac shows those.

## Not done yet

An `.app` bundle, which notifications need, then signing, notarization, a DMG and auto-update are TOB-220. Intel Macs are not tested.

## Quick composer shortcut

There is no global shortcut for the quick composer. macOS has no compositor binding, so bind `keystone-desktop --quick` in a hotkey tool such as Shortcuts or Raycast. Inside Keystone it is ⌥⌘Space, which macOS gives to Finder's search window unless that shortcut is turned off in System Settings → Keyboard → Keyboard Shortcuts → Spotlight; the palette's **Quick composer** always works.
