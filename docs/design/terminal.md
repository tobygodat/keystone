# Integrated terminal

Each session can open shells in a drawer below the window's main area
(TOB-301). It follows how T3 Code's
terminal behaves (`apps/server/src/terminal/Manager.ts` and
`apps/web/src/terminalUiStateStore.ts` in T3 Code), not how it looks. The code is in
`crates/keystone-desktop/src/terminal.rs` and `terminal/`. `pane.rs` owns one shell,
`element.rs` paints it, and `input.rs` turns keys into bytes. Tests are in
`terminal_tests/`, one file per concern. `scripts/terminal-journey.py` runs it in the
real app on macOS. `scripts/terminal-journey-linux.py` runs it on Linux through the
verify skill's private Sway, with real keyboard and pointer input and the Wayland
clipboard; pass `--driver` as the verify skill says.

## Emulator

Keystone uses `alacritty_terminal` 0.26 (Apache-2.0), Alacritty's emulator library,
for the screen grid, VT parsing (through `vte`), scrollback, selection and the PTY.
Its `tty` module opens the PTY and starts the shell as a session leader. Its
`EventLoop` thread reads output into the shared `Term` and writes input and resizes.
No other PTY crate is needed. Only `keystone-desktop` depends on it.

Alternatives considered:

- **wezterm's `termwiz` and `portable-pty`** (MIT). `termwiz` parses escapes and
  models a surface, but wezterm's full emulator, `wezterm-term`, is not published on
  crates.io by wezterm. Only third-party forks such as `tattoy-wezterm-term` are.
  Taking a fork pins Keystone to someone else's release schedule.
- **`vt100`** (MIT). It parses and keeps a screen with scrollback, but it has no
  PTY, no selection model and fewer modes. Keystone would add `portable-pty` and
  write selection itself.
- **libghostty-vt**, which T3 Code compiles to WebAssembly for its web terminal. It
  is a Zig library behind a C API, so Keystone would need Zig in its build and FFI
  bindings to an API Ghostty does not yet call stable.

`alacritty_terminal` is a maintained library with a stable Rust API and the full
emulator Alacritty ships, so it was the smallest dependency that covers the job.

### License audit (ADR 0003)

`cargo add` locked eight new crates. Every crate in `alacritty_terminal`'s tree,
across all targets, is permissive. None is GPL, LGPL or AGPL. Checked with
`cargo tree -p alacritty_terminal -e normal --target all` and each crate's license
field in `cargo metadata`, 2026-11-02.

| Crate (new in Cargo.lock) | Version | License |
| --- | --- | --- |
| `alacritty_terminal` | 0.26.0 | Apache-2.0 |
| `vte` | 0.15.0 | Apache-2.0 OR MIT |
| `rustix-openpty` | 0.2.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `signal-hook` | 0.4.6 | MIT OR Apache-2.0 |
| `home` | 0.5.12 | MIT OR Apache-2.0 |
| `cursor-icon` | 1.2.0 | MIT OR Apache-2.0 OR Zlib |
| `miow` | 0.6.1 | MIT OR Apache-2.0 (Windows only) |
| `windows-sys` | 0.59.0 | MIT OR Apache-2.0 (Windows only) |

The crates it shares with the rest of the tree are MIT, Apache-2.0, Unlicense OR MIT,
or Unicode-3.0 (`unicode-ident`). Keystone copies no Alacritty source. It calls the
library's public API, so `THIRD_PARTY_NOTICES.md`, which lists adapted and bundled
code, gains no entry. The key encodings follow xterm's documented control sequences.

## Ownership

- `Terminals`, a GPUI global installed with the app state, maps a session id to an
  `Entity<SessionTerminal>`. It lives at app level, not in a window. Closing a window
  keeps the shells, their output and their scrollback, and any window showing the
  session shows the same entity. Closing the last window with no turn running quits
  Keystone on Linux, as it does without a terminal, and Quit ends the shells, so only
  closing a window other than the last keeps them.
- Whether the drawer is open belongs to the session, not the window, so a drawer
  opened or hidden in one window opens or hides in every window showing that session.
  The shells are the session's, and a second window on it is a second view of the same
  work. GPUI sends an entity's notify only to windows whose last frame read that
  entity. A window that showed the session before it had a terminal never read one,
  so each window also observes `Terminals`, which changes when a session's terminal
  starts or goes.
- `SessionTerminal` holds one to four `Pane`s side by side (T3's
  `MAX_TERMINALS_PER_GROUP`), the pane focused last, whether the drawer is open, and
  its height. It is the drawer's view. Close, Focus next pane and the toggle act on
  the pane focused in the window they run in, and use the pane focused last only
  when that window has none focused. Splitting gives the new pane an equal share and
  keeps the others' proportions; closing one shares its width among the rest the
  same way.
- A `Pane` owns its `Term`, the sender to its IO thread, the shell's pid and a focus
  handle. Its shell is `$SHELL -l` (`/bin/zsh` on macOS, `/bin/bash` elsewhere when
  unset) with `TERM=xterm-256color` and `COLORTERM=truecolor`. It starts through
  `/usr/bin/env -u …`, because the PTY can only add variables. That leaves out
  Keystone's own `KEYSTONE_*` settings, `TMUX`, `TMUX_PANE`, `STY`, `WINDOW` and
  `ZELLIJ*`, so programs do not think they run inside the multiplexer that started
  Keystone (T3 Code's list), and the `ALACRITTY_WINDOW_ID` and `WINDOWID` the PTY
  sets. It starts in the
  session's recorded folder, which for a worktree session is its worktree. With no
  recorded folder, or one that is gone, it starts in the user's home. A session on
  another machine gets no shell here: the drawer says the terminal isn't available
  for sessions on that machine yet. Scrollback is
  10,000 lines.
- The IO thread's events reach the UI over a channel. The UI drains everything queued
  in one go and notifies only the `SessionTerminal`. It answers color and size queries
  and stores OSC 52 copies on the clipboard. It never reads the clipboard for a program.
- A shell that exits closes its pane. When the last pane closes, the drawer hides and
  the session's entry goes, so the next toggle starts a fresh shell. Focus that was in
  a closed pane moves to the next pane, or to the composer. Focus left in a pane the
  window no longer draws, because the window now shows another session or the drawer
  hid in another window, moves to the composer, so typing reaches something on screen.
- Closing a pane ends what runs in it. Before the PTY closes, Keystone finds the
  shell, the members of its process group and of the terminal's foreground group
  (`tcgetpgrp` on the PTY), and everything they started, each pinned through
  `keystone_agent::host::Root` and `Tree`, the process-tree code provider teardown
  uses (see "Provider processes" in [macos.md](../macos.md)). It sends them SIGHUP.
  Interactive shells ignore SIGTERM, which is why it is not T3's SIGTERM. After 1 s,
  whatever is left, a background job that ignores SIGHUP included, and anything they
  started since, gets SIGKILL. A pinned process is signalled only while its PID still
  holds it. When pinning fails, as on a Linux kernel older than 5.3 or under a seccomp
  filter without `pidfd_open`, Keystone says so on stderr and falls back to signalling
  the shell's PID and its process group, only while the shell is unreaped (`waitid`
  with `WNOWAIT`). The PTY's drop can still reap the shell between that check and
  the signal, so in that window of microseconds the PID could be reused. That fallback
  cannot reach the foreground job's group or jobs that outlive the shell. The
  foreground group's ID is not reserved the way the shell's own group is: once that
  group's leader exits, the ID could in theory be reused before the scan reads it.
- A closed pane's shell that is still ending stays in `Terminals`. Quit ends every
  shell after the workspace is saved (`save_and_teardown`) and waits up to 1 s in all
  for those and the open panes' shells before it kills the rest, so quitting right
  after closing a pane still kills its jobs.
- A shell that exited by itself gets no signal from Keystone: its PID and group may
  already be someone else's. `alacritty_terminal`'s PTY still sends that PID a SIGHUP
  as it drops, after it reaped the shell. Keystone drops it at once, so the PID has
  had milliseconds to be reused. Avoiding it would mean forking the crate.
- Keystone has no way to delete a session, so there is no delete hook.

## Shortcuts

| Command | Keys | Context |
| --- | --- | --- |
| Toggle terminal | ⌘J / Ctrl+J (T3's Mod+J) | App |
| Split terminal | ⌘⇧D / Ctrl+Shift+D | App |
| Close terminal | ⌘⌥W / Ctrl+Alt+W | App |
| Focus next terminal pane | ⌘⌥→ / Ctrl+Alt+→ | Terminal |
| Focus previous terminal pane | ⌘⌥← / Ctrl+Alt+← | Terminal |
| Copy selection | ⌘C on macOS, Ctrl+Shift+C on Linux | Terminal |
| Paste | ⌘V on macOS, Ctrl+Shift+V on Linux | Terminal |

Toggle opens the selected session's drawer, starting a shell if it has none, and
focuses it. Open and focused, it hides the drawer and returns to the composer. Open
but not focused, it focuses it. With no session selected it shows "No session open."
With Settings open, it closes Settings first, so the drawer opens where it shows.
The top bar's terminal button does the same with the mouse, and the palette lists
these commands and the two that move between panes. Those wrap around, and from the
palette they start at the pane focused last. A window narrow enough to show its views as tabs has no room in
its top bar for the button, so there only the shortcut and the palette open it.

GNOME, and some other Linux desktops, switch workspaces on Ctrl+Alt+← and Ctrl+Alt+→
and never pass those keys to an app, so there the pane shortcuts do nothing. Hyprland
and Sway bind nothing to them by default. No free Linux pair was clearly better:
Ctrl+Shift+arrows select text in shells and editors, and Alt+arrows reach the shell.
The palette's Focus next terminal pane and Focus previous terminal pane always work,
and so does a click.

Close is ⌘⌥W because ⌘W (Close tab) and ⌘⇧W (Close window) are taken, and on Linux
Ctrl+W must reach the shell (it deletes a word). The header's Close button and
typing `exit` close a pane too. Each pane also has its own close button on hover when
there is more than one.

On Linux, `secondary` is Ctrl, so the app's shortcuts (Ctrl+N, Ctrl+W, Ctrl+L, Ctrl+K…)
would take the shell's control keys. So inside the terminal every Ctrl+letter,
`secondary-[`, `secondary-]`, `secondary-\` and `secondary-space` goes to the shell,
except the keys of the commands that work from inside the terminal. `COMMANDS` marks
those rows with `in_terminal`: Toggle terminal, Split terminal, Close terminal, Focus
next terminal pane, Focus previous terminal pane, and Quit. Each keeps whatever key the
keymap gives it, its default or the user's from Settings → Shortcuts, so Ctrl+J
toggles, Ctrl+Q quits, and a Split rebound to Ctrl+G splits, all from inside a pane.
Every other Ctrl+letter, Ctrl+C, Ctrl+D, Ctrl+K, Ctrl+N and Ctrl+R included, reaches
the shell. Quit taking Ctrl+Q means a shell never gets Ctrl+Q, which is XON.
The shell's keys are no command, so Settings → Shortcuts names no clash with them:
`keymap::conflicts` skips every binding that runs nothing (`NoAction` and `Unbind`).

`bind_keys` does this by binding those keys to `NoAction` in the `Terminal` key
context, so the keystroke falls through to the pane's key handler. tGPUI ranks a
binding by the depth of the context that matched it, then by order, and the app's
`!Dialog && !CommandPalette` predicate matches at full depth. So the terminal's
bindings are added last, in `bind_keys`, to win that tie, and Settings → Shortcuts
binds them again after a change. `control_keys_reach_the_shell_and_app_shortcuts_resolve_per_platform`
checks the resolution and Ctrl+Q, and `a_rebound_toggle_works_from_inside_the_terminal`
and `a_rebound_split_works_from_inside_the_terminal` check rebound keys. Tab, Shift+Tab
and ⌘↩ are also turned off inside the terminal on both platforms, so Tab completes in
the shell and ⌘↩ cannot send the composer's draft. Shortcuts that use Ctrl+Shift or
Ctrl+Alt on Linux, such as Ctrl+Shift+P for the palette, still work there. On macOS
the app's shortcuts are ⌘ and keep working inside the terminal, and Ctrl+letter goes to
the shell. F2 (Rename session) is off inside the terminal on both platforms, so
programs such as `mc` get it.

In the transcript, Ctrl+Shift+D (⌘⇧D) shows the focused turn's changes, or on an
edit's row that edit (see `features/diff.md` in the verify skill), so there Split terminal runs from the palette, the drawer's
Split button, or a pane.

F6 (Focus next transcript) still moves focus out of the terminal on both platforms,
without hiding the drawer. From the transcript, ⌘L or Ctrl+L focuses the composer, so
F6 then Ctrl+L is the keyboard way from a pane to the composer on Linux. Ctrl+Tab and
Ctrl+Shift+Tab switch tabs inside the terminal too, on purpose. Shells do not use them,
and xterm sends nothing distinct for them.

## Input

`input::bytes` maps a keystroke to what xterm sends. Text is the key's character.
Enter is `\r`, Backspace `\x7f`. Arrows, Home and End use `ESC O` in application
cursor mode. PageUp, PageDown, Insert, Delete and F1 to F12 use xterm's codes, with
xterm's modifier parameter for Shift, Alt and Ctrl. Ctrl+letter is a control byte, and
Alt sends ESC before the key. ⌘ combinations return nothing, so they stay app
shortcuts. Typing scrolls back to the bottom and clears the selection. A paste turns
line breaks into Enter and is wrapped in bracketed-paste markers when the program asks.

The mouse wheel scrolls the scrollback. In the alternate screen with alternate scroll
on, as in `less`, it sends arrow keys instead. A drag selects (double-click words,
triple-click lines).

## Rendering and its cost

The drawer is a cached view (`AnyView::cached`). A composer keystroke notifies only
the window root, so GPUI replays the drawer's last frame without rendering or
painting it again. Shell output notifies only the `SessionTerminal`. GPUI then marks
its ancestor views dirty too, so the window root renders again, but the sidebar
(cached) and the transcript (virtualized) do the work they do for any keystroke
(ADR 0007).

`element::Grid` is a custom GPUI element. In prepaint it measures the monospace cell
from the theme's mono font and works out the grid that fits its bounds. A PTY has one
size, and a session's drawer can show in several windows of different sizes. So each
pane keeps the grid that fits each window showing its session, and the PTY takes the
fewest columns and the fewest lines among them, as tmux does for clients of different
sizes. One window's paint never changes another's fit, so the size settles instead of
alternating between windows, which would send the shell SIGWINCH every frame. A larger
window draws that grid with room to spare. Only a window whose frames are seen
counts: one that is on screen, or any window of a background automation launch, whose
covered windows the inbox draws. A window that switches session, opens Settings,
closes, or is minimized or covered (macOS reports both as hidden) gives up its claim
at once. `terminal::refit` fits every pane again and repaints the drawers whose size
changed, since a cached drawer would otherwise wait for the shell's next output. A
window shown again repaints its drawer to claim its size back. Each pane also keeps
where each window painted it, so a click or a drag maps to a cell through that
window's own layout.

The drawer's height is clamped as it renders and as it is dragged, to at least 120 px
and at most what leaves the top bar and the main area's own minimum. That minimum is
the one the arrangement uses to decide when a window is too small for its layout, so
for the default layout it is the composer at full height, its toolbar and Send button
included, and 120 px of transcript. In a window too short for both, the drawer keeps
its 120 px and the main area gives: a terminal of fewer lines is no use, and ⌘J or
Ctrl+J hides it.

Prepaint then resizes the `Term` and the PTY when the shared size changes. It then walks only the visible rows (`renderable_content`),
never the scrollback. Background cells merge into one quad per run of one color, and
the selection into one wash per row. Text is shaped one line per row, with a `TextRun`
for each change of color, weight, slant, underline or strikethrough. Wide characters
and characters outside ASCII, Latin-1, Latin Extended and box drawing get a segment
of their own, painted at their column, so a fallback font's glyph cannot shift the
cells after it. The cursor is a filled block with the character over it when the pane
has focus, and an outline when it does not. A program's bar and underline cursors are
kept. Colors come from `TerminalColors` in the theme: 16 ANSI colors, foreground,
background, cursor and selection for light and dark, xterm's 256-color cube and grays,
and 24-bit colors. A program's OSC 4, 10 and 11 colors win.

Two things set the cost, both found by profiling `perf_terminal_draw` with `sample`.
GPUI inserts every painted primitive into a bounds tree to order it. Inside
`Window::paint_layer` primitives take the layer's order instead, so the grid paints
in one layer. Quads still draw before text inside it. And every painted text line
pushes a layer of its own, which is one bounds-tree insert. Shaping one line per cell
style made a row whose color changes at every one of its 200 cells cost 200 inserts. Shaping a row as one line with many runs
costs one. On a busy laptop (load average 34 to 79 on 18 cores during the
measurements) these two changes took a root-only frame with a full 200×50 grid from
about 19 ms to 2 to 6 ms, and an output frame to 7 to 12 ms. That grid's color changes
at every cell, in a debug build. Committed numbers are in
[the perf log](../perf-log.tsv) (`terminal_draw_ms`, `terminal_idle_ms`).

## Limits and follow-ups

- Scrollback lives only in memory. T3 Code keeps it across a restart. Keystone does
  not yet.
- Mouse reporting to programs (clicks in `htop` or `vim` with `mouse=a`) is not sent.
  The wheel in the alternate screen sends arrows instead.
- There is no IME. Text arrives through key events, so dead keys and input methods
  that compose text are not supported.
- Alt sends ESC before the key on every platform. On macOS keyboard layouts that type
  characters with Option, such as `@` on a German layout, that character cannot be
  typed. An "Option as Meta" setting would fix it.
- A process that left the shell's tree before the pane closed, such as a daemon that
  called `setsid` or a job whose parent exited, is out of reach, as it is for
  provider teardown.
- Option as Meta and IME on macOS, releasing the `Term` lock before text shaping,
  bounding the IO thread's event channel, a cap on idle shells, and a policy for OSC 52
  clipboard writes are follow-ups in Linear.
- Bold text is drawn bold, not in the bright color.
- Panes split side by side only. There are no tabs or groups, and no dragging output
  into the composer.
- The drawer's height and open state are not saved across a restart.
