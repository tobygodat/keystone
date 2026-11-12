# Browser preview

The project's browser page beside the session, which the user browses and agents drive through the `browser_*` tools. The page runs in headless Chrome; the surface draws its frames. See [the design](../../../../../../docs/design/browser-preview.md).

## Sub-features

- `browser-surface` is a layout component like Notes: at most one, beside a conversation or as a tab with one. It shows the page of the selected session's folder. The toolbar has `browser-back`, `browser-forward`, `browser-reload`, the address field `browser-address` and the dev server menu `browser-servers`.
- `browser-servers` lists the folder's dev servers: loopback listeners whose process works inside the folder and that serve HTML or a redirect. With no page loaded, `browser-empty` lists them as `browser-open-server-N` buttons.
- `browser-page` is the frame. Clicks, drags, the wheel and keys go to the page; Tab stays in it; ⌘V pastes text, ⌘C and ⌘X copy the selection, ⌘A selects all (Ctrl on Linux).
- `browser-agent-note` names what an agent last did on the page, until the user clicks or types there.
- `browser-tools` are `browser_open`, `browser_status`, `browser_navigate`, `browser_snapshot`, `browser_act`, `browser_screenshot` and `browser_evaluate`. `browser_open`, `browser_navigate`, `browser_act` and `browser_evaluate` open the surface in each window showing the caller without moving focus. Claude asks before `browser_evaluate` and before loading an address that isn't this machine's. Read-only callers and second opinions can't act or evaluate.
- `browser-missing`: without Chrome, Chromium, Edge or Brave (or `KEYSTONE_BROWSER_PATH`), opening a page shows the error in `browser-error`.

## How to get to it (user POV)

- Press ⌘⌥B (Ctrl+Alt+B on Linux), or choose **Toggle browser** in the command palette. With the surface focused, the shortcut closes it.
- Click **+** on a tab strip and choose **Browser**.
- Click a dev server in the empty surface or the `browser-servers` menu, or type an address and press Enter.

## Driving it

On a Mac, `python3 scripts/macos-browser.py` runs the whole journey: the shortcut and the palette by mouse, a demo dev server, the user's own typing in the page, a real Claude session driving it (`--no-agent` skips that), quitting and relaunching. Under the Linux verify skill, set `KEYSTONE_BROWSER_PATH` to a Chrome or Chromium binary; it runs headless and needs no display. The fake Claude's `orchestrator test: drive browser URL` opens URL, clicks the page's first button and replies with what it read, so an agent journey needs no real provider.

## Not covered

- Recording, device emulation, cookie import, IME composition, native file pickers and pages in more than one tab per project.
