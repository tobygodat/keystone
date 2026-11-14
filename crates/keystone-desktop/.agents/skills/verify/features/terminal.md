# Integrated terminal

Each session's shells in a drawer below the main area: split panes, scrollback kept across a window close, and every shell ended by Quit. See [the design](../../../../../../docs/design/terminal.md).

## Sub-features

- `terminal-drawer` holds the session's panes, `terminal-pane-0` to `terminal-pane-3`, under a header with `terminal-split`, `terminal-close` and, with more than one pane, `terminal-pane-N-close` on hover. `terminal-resize` is the drawer's top edge.
- The shell is the user's login shell, started in the session's folder or worktree. Every window showing the session shows the same panes, output and scrollback.
- Text goes through the pane's input handler, so dead keys and input methods compose in a pane; the composition shows underlined at the cursor. On macOS, Option types the layout's character unless Settings → Shortcuts → `settings-option-as-meta` is on; Linux keeps Alt as Meta and shows no switch.
- Keys go to the shell. On Linux every Ctrl+letter does, except the keys of the commands marked `in_terminal` in `COMMANDS`: Toggle terminal, Split terminal, Close terminal, Focus next and previous terminal pane, and Quit, each with whatever key the keymap gives it. Tab, Shift+Tab and F2 reach the shell on both platforms.
- Selection by drag, Copy with Ctrl+Shift+C (⌘C) and Paste with Ctrl+Shift+V (⌘V), through the system clipboard.
- Closing a window other than the last keeps the shells. Closing the last window with no turn running quits Keystone on Linux, and Quit ends every shell and everything it started, jobs that ignore SIGHUP included. The drawer starts closed after a relaunch.

## How to get to it (user POV)

- Press Ctrl+J (⌘J), or choose **Toggle terminal** in the command palette, or click the top bar's terminal button (`terminal-toggle`, absent in a window narrow enough to show its views as tabs).
- Ctrl+Shift+D (⇧⌘D) or `terminal-split` splits; Ctrl+Alt+← and → (⌥⌘← and →) move between panes; Ctrl+Alt+W (⌥⌘W), `terminal-close` or typing `exit` closes one.

## Driving it

On Linux, `python3 scripts/terminal-journey-linux.py` runs the whole journey on the verify skill's private Sway with real keyboard, pointer and Wayland clipboard: the three ways to open it, `pwd`, `seq`, the wheel, `vi` and `less`, splits and pane focus, copy and paste read back with `wl-paste`, a second window, both themes at 1100×720 and 860×560, the Browser beside the drawer, Ctrl+Q from inside a pane, and that no shell, job or Chrome process outlives Quit. Evidence goes to `.audit/terminal-journey-linux-*/` and the verify run's `.audit/verify-*/`. On a Mac, `python3 scripts/terminal-journey.py` runs it through the automation inbox.

The harness reads what the shell writes to files, `/proc` and the workspace database. It cannot read text off the screen, so look at the screenshots.

## Not covered

- Scrollback across an app restart, terminal tabs, dragging output into the composer.
- Option and input methods on a real Mac: the macOS journeys skip AppKit's input handling, so only GPUI tests cover them.
