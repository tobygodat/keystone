# Quick composer

A small window, titled "Quick composer", that starts a session in one step. It has the provider, account and model button (`model-selector`), a project picker (`quick-project`, items `quick-project-N`), a field (`quick-field`) and Send (`quick-send`).

## Sub-features

- `quick-cli` `keystone-desktop --quick` connects to the running app's Unix socket (`keystone-<hash>.sock` in `$XDG_RUNTIME_DIR`, or beside the workspace database where that is unset), which opens the window and answers once it shows; the command exits 0. With no app listening it takes the workspace lock and starts Keystone, then opens the window. While another process holds the lock it keeps asking, and takes over once the lock frees, for up to ten seconds, then exits 1.
- `quick-shortcut` Ctrl+Alt+Space, or **Quick composer** in the palette, opens it from a Keystone window. Asking while it is open brings it forward.
- `quick-choices` The model button works as the composer's before the first Send: provider, account, and that account's catalog in the chosen project. The project picker lists the open projects, most recent first. Both start on what the last quick session used.
- `quick-send` Enter (or Send) creates a session in the project with the chosen provider, account and model, selected in no window, sends the text and closes the window. Shift+Enter adds a line. The session shows in the sidebar of every window on that project. A send the new session refuses closes the window, leaves the text as that session's draft and shows the session, with the reason, in a Keystone window.
- `quick-draft` Escape (or the compositor's close) saves the text and choices and closes. Escape with a menu open closes only the menu. The draft is saved 250 ms after typing stops and with Quit, and the next opening shows it. A failed save keeps the window open with the error, and fails Quit.
- `quick-windowless` When the last window closed while a turn runs, opening the quick composer opens the windows again, so Keystone stays open.

## Driving it with keystone-verify

- With the app running, call `launch` with `args` `["--quick"]`. Require `doctor` to list the new PID as exited with code 0, `call` `{"tool":"windows_list"}` to show "Keystone" and "Quick composer" under the app's PID, and no other live process.
- Click `quick-field` in window "Quick composer", type, press Return. Require the window gone, `workspace` to list a new session with the chosen provider, the fake Claude's `.fake-claude-messages` in the project to hold the text, and `session-N` in the main window's sidebar.
- Launch `--quick` again, type, press Escape. Require the window gone and `workspace`'s `workspace.preferences.quick.text` to hold the text; launch `--quick` again and require it shown.
- Quit with Ctrl+Q, then `launch` with `["--quick"]`: require both windows under the new PID.
- Sway tiles the window unless told otherwise; the README's rule, `for_window [app_id="keystone" title="^Quick composer$"] floating enable`, can be sent to the private compositor with `swaymsg -s <sway_socket>` from `start`'s session.

## Limits

- Wayland gives apps no global shortcuts, and macOS needs none: the user binds `keystone-desktop --quick` in the compositor or a hotkey tool. The XDG GlobalShortcuts portal is not used.
- On Wayland, bringing an open quick composer forward is up to the compositor, which may only mark it urgent.
- Closing the last Keystone window while the quick composer is open quits as before, saving its draft.
- Not verified on macOS or X11.
