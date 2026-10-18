# Customize the sidebar

Preview, apply, and undo the session sidebar's side and width, by direct edits or by a Claude Code request.

## Sub-features

- `customize-entry` opens the panel from the top bar, including while the sidebar is hidden, and closing restores that visibility.
- `customize-direct` previews side and width from buttons, Alt+Left/Right/[/], and the inner-edge drag.
- `customize-ask` sends the prompt to Claude Code's layout helper and previews only a valid answer. Codex is refused as not supported before anything starts.
- `customize-commit` covers Apply, Discard, Reset, and Undo, and their saved state.
- `customize-fit` draws a narrower sidebar in a small window without changing the saved width.

## How to get to it (user POV)

- Click **Customize** at the right of the main column's top bar. Escape or the panel's close button closes it and discards an unapplied preview.
- **Left** and **Right** move the sidebar. **−** and **+** change the width by 10 px. The highlighted band on the sidebar's inner edge drags it wider or narrower.
- Choose **Claude Code** or **Codex**, type in the prompt field, and press **Preview** or Ctrl+Enter.
- **Apply** (Ctrl+S) saves. **Discard** returns to the saved layout. **Reset** previews left, 302 px. **Undo** is enabled only when nothing is unapplied.

## Driving it with keystone-verify

Names: `customize-toggle`, `customize-panel`, `customize-close`, `customize-side-left`, `customize-side-right`, `customize-width-decrease`, `customize-width`, `customize-width-increase`, `customize-provider-claude`, `customize-provider-codex`, `customize-prompt`, `customize-ask`, `customize-cancel`, `customize-retry`, `customize-status`, `customize-reset`, `customize-undo`, `customize-discard`, `customize-apply`, `sidebar-resize-handle`.

- `click` `{"id":"sidebar-toggle"}`, then `{"id":"customize-toggle"}`. Require `search-field` to return and `customize-panel` to appear.
- `click` `customize-side-right`. Require `search-field` at the right of the window. `workspace` must still show no `sidebar_layout` change.
- Drag the handle with `input` `{"actions":[{"action":"drag","path":[{"x":X,"y":Y},{"x":X-60,"y":Y}]}]}` from the handle's center. On the right, moving left widens it. Read `customize-width` in a fresh screenshot.
- `click` `customize-apply`. Require `workspace` to show `sidebar_layout` with the new side and `width_px`. `click` `customize-undo` and require the earlier layout saved.
- Quit with Ctrl+Q, `launch`, and require the saved layout drawn before opening the panel.
- For a scripted answer, start with `env` setting `KEYSTONE_CLAUDE_PATH` to a script that runs `target/debug/keystone-fake-claude` with `KEYSTONE_FAKE_CLAUDE_REPLY='{"kind":"change","version":1,"side":"right","width_px":360}'`. Prose, fences, or unknown fields in the reply must show an error in `customize-status` and leave the preview unchanged.
- Choose Codex and press Preview. Require `customize-status` to say it is not supported, with no Codex process started.
- For an unavailable provider, set `KEYSTONE_CLAUDE_PATH` to a missing file and press Preview. Require an error and **Retry** in the panel, with the prompt kept.
- The helper launches `claude` with `--tools ""`, empty MCP, no slash commands or hooks, and plan mode, and fails the request when Claude reports any of them on. A wrapper script on `KEYSTONE_CLAUDE_PATH` can log its arguments.

## Gotchas

- The panel takes Ctrl+Enter while it has focus, so the conversation composer's draft is neither sent nor changed. Check the composer text after asking.
- A real `claude` or `codex` request spends the user's usage and takes several seconds. Scripted replies prove Keystone's handling, not the provider's.
- A screenshot taken right after typing or a drag can show the previous frame. Capture again before concluding that input was lost.
- The handle exists only while the panel is open. The width readout notes when the window is narrowing the sidebar.
