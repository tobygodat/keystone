# Drafts and restart

Write a multiline draft, switch sessions, and reopen Keystone without losing the saved text.

## Sub-features

- `draft-focus` focuses the composer by clicking or Ctrl+L.
- `draft-autosave` persists typed text after the debounce.
- `draft-switch` keeps each session's draft separately.
- `draft-restart` restores saved text and selection after a normal quit.
- `draft-multiline` accepts Enter and multiline paste, wraps text, and scrolls after eight visible rows.

## How to get to it (user POV)

- Create or select a session, then click the draft field or press Ctrl+L.
- Type text. Select another session and return.
- Quit with Ctrl+Q or the window close control, then reopen Keystone.

## Driving it with keystone-verify

Preconditions:

- One owned, focused Keystone window with a selected session.
- Empty composer for the literal assertion below.

- Call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","l"]}]}`.
- Call `input` with `{"actions":[{"action":"type","text":"Verified draft."}]}`.
- Call `input` with `{"actions":[{"action":"wait","duration_ms":1000}]}`. Read `workspace` and require the selected session's draft to equal `Verified draft.`. Capture the visible composer and preserve a snapshot.
- Create Codex with Ctrl+Shift+N. Focus with Ctrl+L and type `Second draft.`. `click` the original row by its `session-<id>`. Require `Verified draft.` to return and both saved drafts to remain distinct.
- Press Ctrl+Q through `input`. Poll `call` with `{"tool":"windows_list","arguments":{}}` until empty. Run `doctor` and require the app PID to have exited.
- Call `launch`, then `wait_for` `{"id":"composer-field"}`. Run `doctor`, capture `screenshot`, and require the original selection and visible draft to match the saved snapshot.
- `click` `{"id":"composer-field"}` and append ` Again.`. Require the file and screenshot to show the combined text. This distinguishes a restored usable editor from a static image.
- Insert a new line with Enter and type a second paragraph. Require a literal newline in the saved draft. Click a later row and insert a marker. Check its exact position in the saved text.
- Paste a draft longer than eight visual rows through a native clipboard route. Inspect wrapping, scrolling, selection, and caret visibility at a narrow window width.
- Test window close separately using `call` with the `window_action` tool, with `action:"close"` and the observed window ID. Do not claim that Ctrl+Q proves the close-control path.

## Gotchas

- Autosave waits 250 ms after changes. Observe the file instead of assuming a fixed wait always suffices.
- `stop` forcibly ends remaining apps. It does not prove graceful-save behavior. Quit through the UI first.
- Very rapid long text input has a known rendering issue. A short passing draft does not resolve it.
- `type` sends native key events. It is not clipboard paste. Use a native clipboard owner such as `wl-copy` on the observed nested Wayland socket before Ctrl+V.
- A temporary virtual keyboard, such as `wtype` on the nested socket, leaves Sway's seat without a keyboard when it exits. GPUI 0.2.2 then panics on startup. `launch` sends F24 through the driver's own keyboard first to restore it. Starting the app any other way after `wtype` needs the same step. See KS-009 in `docs/issues.md`.
- Storage-failure close blocking requires fault injection in the disposable state path and is not covered by this normal-save journey.
