# Machines

Linking a machine over SSH, and Claude and Codex sessions on it, on its default login or one added for it, with images (TOB-441, TOB-320, `docs/design/remote.md`).

## A machine without real SSH

The app runs `ssh` from its `PATH`. Pass `start` a `PATH` whose first folder holds a stand-in `ssh` that skips the options up to `--`, takes the destination and the remote command, and runs that command with `env -i` and the host's own environment: `HOME`, `KEYSTONE_HOST_DIR` on a short path (the socket's path must fit `SUN_LEN`), `KEYSTONE_CLAUDE_PATH` naming `keystone-fake-claude`, `KEYSTONE_CODEX_PATH` naming `keystone-fake-codex`, and a `PATH` that holds a scratch copy of `target/debug/keystone-host`. Make destinations produce each failure: exit 255 with "Permission denied" or "Host key verification failed", a `PATH` without `keystone-host` for exit 127, and an unknown name for SSH's own error. A `down` file the stand-in checks first, exiting 255 with "Network is unreachable", keeps the link down after you kill the app's `keystone-host attach` child, which `ps --ppid <app pid>` finds.

To see a message's answer come late or wrong, give the stand-in more switches. `holdin` and `holdout` files hold the lines going in or out while they exist. A filter on the host's output, armed by a `mangle` file, turns the next `ok` reply into a refusal and holds the `Taken` event until a `holdtaken` file goes, so the app is told "Not sent" for a message the host ran.

Prove it over real SSH when the machine allows it. Don't change sshd or `authorized_keys` to get there.

## Entry points

- Palette: Link a machine opens Settings → Machines with the destination field focused. New session on a machine lists the machines, then takes a typed folder.
- Mouse: the settings cog, then Machines, then the field and Link (`machine-link`). Each machine's row has New session here (`machine-new-session-<id>`), Remove (`machine-remove-<id>`, disabled while sessions use it), Retry after a failure (`machine-retry-<id>`), and the Quit switch (`machine-end-on-quit-<id>`). The new-session menu has On a machine… (`new-remote`).

## Checks

- Each failure shows its reason under the field (`machine-link-state`), wraps inside the card, keeps the typed text, and saves nothing: read the `machines` table, since `workspace` doesn't list it.
- A linked machine's row shows Connected with the host's Claude and Codex versions, and the field clears.
- Settings → Accounts asks which machine a new login is on once one is linked (`account-machine`), and lists it as "Work on desk" with the command to sign it in there. A remote session's model menu (`model-selector`) lists that machine's Default and its logins, each "on desk"; a local session's lists neither. Pick Codex there and send: the reply streams as a local Codex reply does. The stand-in's `KEYSTONE_FAKE_ENV_RECEIPT` shows the login's `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, made only on the host side.
- A remote session's model menu lists the machine's models for its provider and login, from the host; pick one and send: the reply names it. While the catalog loads the menu says "Loading choices…" under Provider default.
- A message to a machine reads "Sending" until the host answers. A refused one reads "Not sent", with its reason under the composer, and stays in its bubble with its images. The composer is never filled for it.
  - **Send again** (`send-again-<row>`) sends that same message under its same ID, through the login the session has now. The bubble moves to the end of the transcript with a muted "Sent again" note, so its reply follows it. The host runs a refused ID as new and answers a taken one with its recorded `ok`, running nothing: the transport log shows the one ID sent again, and the provider sees it once.
  - **Edit** (`edit-message-<row>`) takes its text and images into the composer, and the bubble reads "Edited". With anything in the composer, Edit is off and its tooltip says why.
  - On a message with images, the "Not sent · Send again · Edit" line sits under the image row.
  - While the machine can't be reached, Send again is off with the composer's reason ("Reconnecting to desk").
  - A message the host has no record of (`unrecorded`) reads "may already have run", and so does Send again's tooltip.
- Delete the image from the host's `attachments/` folder, or rewrite it, and send it again: the bubble reads "Not sent" with the reason, and Send again uploads the image again and runs the message once.
- With the filter armed, send: the composer stays empty while the bubble reads "Not sent". Remove `holdtaken`: the bubble reads sent, its actions go, and Ctrl+Enter sends nothing. Edit it first instead, and the bubble reads "This message ran after all." once the `Taken` arrives.
- A login whose folder overlaps the machine's default home is refused with the way out: "Pick another login in the model menu, then press Send again". Pick Default on desk and press Send again: the same ID goes out and runs once.
- With `holdout`, send, Ctrl+Q, then relaunch with `down`: the bubble reads "Couldn't reach desk" with Discard (`discard-message-<row>`), and nothing settles, however long it waits. Remove `down`: the host's record settles it once, with no second `send`. Discard gives it up, and if the host's record then shows it ran, the bubble reads sent. A turn running at quit reads "Keystone was closed during this turn on desk." and "Waiting for desk to say how this turn went" until it ends.
- Keyboard: Ctrl+Shift+T, then Up/Down to the message (or its image row), then Tab past Copy reaches Send again, Edit and Discard, and Enter or Space presses them. Send again that is off is skipped.
- A login whose home is `<symlink into the machine's ~/.codex>/..`, under a symlinked parent of it, or a symlink to it, is refused with the machine's name.
- Paste or attach an image to a remote session and send it twice: the host's `attachments/` folder holds it by its SHA-256, and a transport log (a stand-in `ssh` that `tee`s its stdin) shows one `put_attachment` and two `send`s naming the hash. A relative path or a file that isn't an image is refused under the composer.
- A remote session's sidebar row (`session-machine-<id>`) and header (`session-header-machine`) show the machine and its state. Kill the transport with the `down` file in place: both say Reconnecting, Send and Allow/Deny are disabled and say why, Ctrl+Enter keeps the draft with "The machine isn't connected", and after the file goes the turn finishes with every word once.
- Quit during a turn, relaunch: Connected within a couple of seconds, the open turn's row says Keystone was closed during it, and the reply follows. With the Quit switch on, Quit ends the turn: after the relaunch the transcript says "Stopped" and the sidebar row leaves Working without saying "Turn failed". Closing the last window doesn't end it.
- The standard journey (Ctrl+N, draft, Ctrl+Q, relaunch) still shows the local session with no machine mark.
