# Keystone

A Rust and GPUI desktop workspace for Claude Code and Codex.

This is a native UI preview styled after the supplied MonoCode reference. Create and select local session records, search them, save multiline drafts, see the sessions that need you (an approval, a failed turn, or an unread reply) above the ones still working, settle and un-settle them into a collapsible Settled section, and restore them on restart. Codex sessions send messages to your installed `codex`, and Claude Code sessions to your installed `claude`. Both stream the reply, with approvals, steering, stop, and the conversation restored after a restart. The optional visual fixture is labeled as sample content.

## Run the preview

Run `cargo run -p keystone-desktop --locked` in a Linux graphical session or on an Apple Silicon Mac. On macOS, use ⌘ wherever this README says Ctrl, except Ctrl+Tab; see [Keystone on macOS](docs/macos.md). Press Ctrl+Q to quit. The gear at the bottom of the sidebar chooses the theme (System, Light, or Dark), the density, and the interface font. Escape closes an open menu. Ctrl+N creates a Claude session, Ctrl+Shift+N creates a Codex session, Ctrl+L focuses the composer, Ctrl+Enter sends, Ctrl+F focuses search, and Ctrl+Shift+S toggles settlement.

Replies render as Markdown, with tables, code blocks, and copy buttons. The shield button in the composer sets each session's approval mode: Provider settings (the default, which uses your own Codex or Claude Code configuration), Read only, Ask first, Auto, or Full access. Full access asks for confirmation first. A change applies to every message sent after it, steers included, and is saved with the session. Codex gets the matching approval policy and sandbox, and Claude Code the matching `--permission-mode`; see "Approval modes" in [the provider design](docs/design/providers.md).

To attach an image, paste it with Ctrl+V, drop it on the composer, or choose Attach files… from the paperclip menu (Ctrl+Shift+U) to pick files; where no file picker is available, a field takes a typed path, relative to the session's folder or starting with `~/`. Capture screenshot in the same menu (Ctrl+Alt+S) attaches a region you pick: with `slurp` and `grim` on Wayland compositors with the wlroots layer-shell and screencopy protocols, such as Sway and Hyprland, or `screencapture -i` on macOS. PNG, JPEG, GIF and WebP images show as chips under the draft and are saved with it. Claude Code receives each image as an image content block and Codex as a local image, and the message's row in the transcript shows them. Any other file is mentioned by its path, so the agent reads it with its own tools. Keystone copies each image into an `attachments` folder beside the workspace database, so it still sends after the original moves. Images over 20 MiB are refused; set `KEYSTONE_ATTACHMENT_MAX_MB` for another limit. A copy is deleted at startup 30 days after it was last attached or sent, unless a draft still holds it; set `KEYSTONE_ATTACHMENT_KEEP_DAYS` for another number of days, or 0 to keep copies forever. See [attached images](docs/design/storage.md#attached-images).

Codex and Claude Code run with your own `codex` and `claude` logins and, under Provider settings, your own approval settings, in the directory Keystone was started from. Set `KEYSTONE_PROJECT_DIR` to choose another directory, or `KEYSTONE_CODEX_PATH` and `KEYSTONE_CLAUDE_PATH` to use specific binaries. Each Claude session runs its own `claude` process until Keystone quits. Each session's completed messages are kept in the workspace SQLite database. See [the provider design](docs/design/providers.md).

Ctrl+Shift+G, **Toggle changes** in the palette, or **Changes** in a tab strip's **+** menu opens Changes for the selected session's folder: its changed files grouped as conflicts, staged, changes and untracked, with no diff view. Stage or unstage a file or a whole group, open a file in your editor or copy its path, and commit with Ctrl+Enter. Keystone runs your own `git`, so your hooks and signing apply; a failed commit shows their output and keeps the message. An unfinished commit message is saved per folder and restored after a restart. **Push** pushes the branch, setting its upstream on the first push, and never forces; a rejected push shows git's message. The pull request button runs your own `gh pr create` with a title (the session's, to start), a description, a base branch and a draft switch, and the new pull request's number shows in the top bar beside the session. Keystone never holds a GitHub token: when `gh` is missing or signed out, or the repository has no remote, Changes shows the command to run in a terminal. GitHub only. Changes needs git 2.26 or newer.

The preview saves to `$XDG_DATA_HOME/keystone/workspace.sqlite3`, or `$HOME/.local/share/keystone/workspace.sqlite3`, and on macOS to `~/Library/Application Support/Keystone/workspace.sqlite3`. The legacy `workspace.json` and `sessions/*.jsonl` are imported together on first run and left untouched as backups. Set `KEYSTONE_STATE_PATH` to an isolated state or database path: legacy `*.json` maps to `*.sqlite3`, and other legacy names get `.sqlite3` appended. An existing SQLite file is recognized by its header. A file lock prevents concurrent desktop processes from writing the same workspace. Draft writes wait 250 ms after typing stops. Session changes and normal window close flush the visible draft immediately; failed saves keep the session open. See [storage guarantees](docs/design/storage.md) before copying or repairing a live database.

## Quick composer

`keystone-desktop --quick` opens a small window with the provider, account and model button, a project picker and a text field. Enter starts a session in that project and sends the text, Shift+Enter adds a line, and Escape closes the window and keeps the text for next time. Ctrl+Alt+Space, or **Quick composer** in the palette, opens it from inside Keystone. The new session shows in the sidebar of every window on that project.

The command hands the request to the running Keystone over a Unix socket, in `$XDG_RUNTIME_DIR`, or beside the workspace database where that is unset, as on macOS, and exits. When Keystone is not running, it starts Keystone and then opens the window. Wayland gives apps no global shortcuts, so bind the command in your compositor, and float the window by its title, `Quick composer`. In Sway:

```
bindsym $mod+Ctrl+space exec keystone-desktop --quick
for_window [app_id="keystone" title="^Quick composer$"] floating enable
```

On macOS, bind the command in your own hotkey tool.

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

The preview keeps its own workspace in `~/.local/share/keystone-preview/workspace.sqlite3` (the preview script still names the legacy import path), so it doesn't conflict with a copy running on the desktop. Its Claude Code and Codex sessions run in the checkout the script lives in, not in the worktree being previewed. Set `KEYSTONE_PROJECT_DIR` to choose another directory. Pressing Ctrl+Q in the app or Ctrl+C in the terminal, closing the terminal, or dropping the connection stops everything on both machines.

## Write in the composer

The composer supports multiline drafts. Enter and Shift+Enter insert a new line. Text wraps to the available width, grows to eight visible rows, and scrolls vertically for longer drafts. Home and End move within a visual row. Ctrl+Home and Ctrl+End move to the beginning and end of the draft. Search is single-line.

## Develop

Keystone is a Rust workspace on a pinned GPUI fork. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) maps the code, [docs/CONVENTIONS.md](docs/CONVENTIONS.md) covers building, checks and native verification, and [AGENTS.md](AGENTS.md) lists the checks a change must pass. [PRODUCT.md](PRODUCT.md) says what Keystone is for, and [DESIGN.md](DESIGN.md) holds the visual system. Planned work and known bugs are in [Linear](the issue tracker).
