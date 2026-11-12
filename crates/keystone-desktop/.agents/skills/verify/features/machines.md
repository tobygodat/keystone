# Machines

Linking a machine over SSH, and Claude sessions on it (TOB-441, `docs/design/remote.md`).

## A machine without real SSH

The app runs `ssh` from its `PATH`. Pass `start` a `PATH` whose first folder holds a stand-in `ssh` that skips the options up to `--`, takes the destination and the remote command, and runs that command with `env -i` and the host's own environment: `HOME`, `KEYSTONE_HOST_DIR` on a short path (the socket's path must fit `SUN_LEN`), `KEYSTONE_CLAUDE_PATH` naming `keystone-fake-claude`, and a `PATH` that holds a scratch copy of `target/debug/keystone-host`. Make destinations produce each failure: exit 255 with "Permission denied" or "Host key verification failed", a `PATH` without `keystone-host` for exit 127, and an unknown name for SSH's own error. A `down` file the stand-in checks first, exiting 255 with "Network is unreachable", keeps the link down after you kill the app's `keystone-host attach` child, which `ps --ppid <app pid>` finds.

Prove it over real SSH when the machine allows it. Don't change sshd or `authorized_keys` to get there.

## Entry points

- Palette: Link a machine opens Settings → Machines with the destination field focused. New Claude session on a machine lists the machines, then takes a typed folder.
- Mouse: the settings cog, then Machines, then the field and Link (`machine-link`). Each machine's row has New Claude session here (`machine-new-session-<id>`), Remove (`machine-remove-<id>`, disabled while sessions use it), Retry after a failure (`machine-retry-<id>`), and the Quit switch (`machine-end-on-quit-<id>`). The new-session menu has On a machine… (`new-remote`).

## Checks

- Each failure shows its reason under the field (`machine-link-state`), wraps inside the card, keeps the typed text, and saves nothing: read the `machines` table, since `workspace` doesn't list it.
- A linked machine's row shows Connected with the host's Claude version, and the field clears.
- A remote session's sidebar row (`session-machine-<id>`) and header (`session-header-machine`) show the machine and its state. Kill the transport with the `down` file in place: both say Reconnecting, Send and Allow/Deny are disabled and say why, Ctrl+Enter keeps the draft with "The machine isn't connected", and after the file goes the turn finishes with every word once.
- Quit during a turn, relaunch: Connected within a couple of seconds, the open turn's row says Keystone was closed during it, and the reply follows. With the Quit switch on, Quit ends the turn: after the relaunch the transcript says "Stopped" and the sidebar row leaves Working without saying "Turn failed". Closing the last window doesn't end it.
- The standard journey (Ctrl+N, draft, Ctrl+Q, relaunch) still shows the local session with no machine mark.
