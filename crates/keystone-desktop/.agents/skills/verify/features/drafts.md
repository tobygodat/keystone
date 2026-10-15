# Drafts and restart

Write a single-line draft, switch sessions, and reopen Keystone without losing the saved text.

## Sub-features

- `draft-focus` focuses the composer by clicking or Ctrl+L.
- `draft-autosave` persists typed text after the debounce.
- `draft-switch` keeps each session's draft separately.
- `draft-restart` restores saved text and selection after a normal quit.
- `draft-multiline` rejects multiline paste with a visible explanation.

## How to get to it (user POV)

- Create or select a session, then click the draft field or press Ctrl+L.
- Type text. Select another session and return.
- Quit with Ctrl+Q or the window close control, then reopen Keystone.

## Driving it with native-driver

Preconditions:

- One owned, focused Keystone window with a selected session.
- Empty composer for the literal assertion below.

- Send `{"kind":"call","tool":"input","arguments":{"actions":[{"action":"keypress","keys":["ctrl","l"]}]}}`.
- Send `{"kind":"call","tool":"input","arguments":{"actions":[{"action":"type","text":"Verified draft."}]}}`.
- Send `{"kind":"call","tool":"input","arguments":{"actions":[{"action":"wait","duration_ms":1000}]}}`. Read `workspace` and require the selected session's draft to equal `Verified draft.`. Capture the visible composer and preserve a snapshot.
- Create Codex with Ctrl+Shift+N. Focus with Ctrl+L and type `Second draft.`. Click the original row after observing a screenshot. Require `Verified draft.` to return and both saved drafts to remain distinct.
- Press Ctrl+Q through `input`. Poll `windows_list` until empty. Run Doctor and require the app PID to have exited.
- Send `{"kind":"launch"}`. Run Doctor, capture the window, and require the original selection and visible draft to match the saved snapshot.
- Focus the restored composer and append ` Again.`. Require the file and screenshot to show the combined text. This distinguishes a restored usable editor from a static image.
- Test window close separately using `window_action` with `action:"close"` and the observed window ID. Do not claim that Ctrl+Q proves the close-control path.

## Gotchas

- Autosave waits 250 ms after changes. Observe the file instead of assuming a fixed wait always suffices.
- `stop` forcibly ends remaining apps. It does not prove graceful-save behavior. Quit through the UI first.
- Very rapid long text input has a known rendering issue. A short passing draft does not resolve it.
- `type` sends native key events. It is not clipboard paste and cannot prove multiline-paste rejection. That entry point remains unverified until a real paste route is available.
- Storage-failure close blocking requires fault injection in the disposable state path and is not covered by this normal-save journey.
