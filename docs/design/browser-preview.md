# Browser preview

A browser surface beside a session, so the user can watch a local dev server while an
agent works on it, and the agent can drive the same page through the `keystone` MCP
server. It ports the behavior of T3 Code's preview panel
(TOB-302), not its interface.
[ADR 0010](../adr/0010-terminal-browser-scheduling-editor-diffs-remote-devices.md)
brought it into scope and left the embedding open; this document settles it.

## Embedding decision

Keystone runs headless Chrome (or another Chromium) as a child process and talks to it
over the DevTools protocol (CDP) on a pipe. The page's pixels come back as screencast
frames, which a GPUI surface draws as an image. The surface forwards the user's mouse,
wheel and keys back as CDP input events. Nothing native is embedded in the window.

The first target is the Mac. Linux, under Wayland and X11, isn't a goal yet, but the
choice keeps it open: nothing in it is macOS-only. Three options were weighed.

**A native webview in the GPUI window** (wry, WKWebView on macOS, WebKitGTK on Linux).

- Linux rules it out. wry's child webviews need a GTK widget tree. GPUI's Linux
  backends are their own Wayland and X11 clients, not GTK. X11 could reparent a
  WebKitGTK window, but Wayland has no way to embed another toolkit's surface in ours.
- A native view sits above GPUI's Metal or Vulkan layer, so Keystone's menus, dialogs,
  tooltips and drag previews would draw under it. T3 Code has the same problem with
  Electron's guest views and works around it by moving the view offscreen.
- Agents get no automation API. WKWebView offers JavaScript evaluation and snapshots,
  but no accessibility tree and no trusted input events. WebKitGTK offers WebDriver only
  in automation mode. The two engines also differ from each other and from the Chrome
  most dev servers are tested in.

**CEF** in off-screen rendering mode.

- It paints into a buffer, so it would draw inside GPUI on every platform, and it has
  full CDP.
- It costs about 250 MB of Chromium binaries per platform, a pinned download at build
  time, and on macOS a set of helper app bundles inside an `.app`. Keystone has no
  macOS bundle yet (TOB-220). The Rust bindings (`cef-rs`) are young.
- It would add a second browser engine to every build for a feature most sessions never
  open.

**What others ship.** Every agent-drivable in-app browser found in November 2026
runs Chromium with CDP for the agent: T3 Code (an Electron `<webview>` driven by
Playwright over CDP), Claude desktop's Browser pane (Electron), the Codex app (its own
Chromium, with CDP behind a Developer mode) and Cursor (reported as Chromium over CDP).
They can, because they are Electron. Native Mac browsers that agents drive
(`brainfuel/mcp-browser`, WebKitUI MCP) use WKWebView and write their own JavaScript
tools; trusted clicks there need synthesized `NSEvent`s, which reportedly miss
off-screen elements and lean on private API. Longbridge's `gpui-wry` embeds WKWebView
in GPUI on macOS and warns that it covers any GPUI element beneath it. For a native
app, Chromium with CDP means off-screen rendering: headless Chrome or CEF.

**Headless Chrome over CDP** (chosen, with Toby, 2026-11-02).

- It draws inside GPUI on every platform, because only images cross into the window.
  Wayland, X11 and macOS behave the same, and menus and dialogs draw over the page.
- Agents get the whole of CDP: the accessibility tree for snapshots, trusted input,
  screenshots, evaluation. This is the surface T3 Code's agents use through Playwright.
- `--remote-debugging-pipe` carries CDP over two file descriptors as NUL-separated
  JSON. No port is opened, so no other local process can attach to the browser, and no
  WebSocket client is needed. The feature adds no crates: `serde_json`, `base64` and
  `image` are already in the tree. A spike on Chrome 156 (macOS) read the accessibility
  tree, received screencast frames, clicked a button and read the result in 2.6 s,
  browser start included.
- The browser exits when Keystone closes the pipe, so a crash leaves no orphan.
- Chrome starts with its background traffic off, as Playwright starts it: no variation
  seeds, component updates, pings or crash reports. On Linux that traffic held a page's
  first load for more than ten seconds.

What it costs:

- It needs a Chromium-family browser installed: Google Chrome, Chromium, Microsoft Edge
  or Brave, found in the usual places, or `KEYSTONE_BROWSER_PATH`. Without one, the
  surface says so and the tools return that error. Keystone never downloads a browser.
- Frames are JPEG and arrive at most at display rate, a frame or two behind the page.
  That suits watching a dev server. It doesn't suit video or games.
- Input is synthesized. Typing, clicks, wheel, drag and the common editing keys work.
  IME composition, native context menus, the native file chooser and DevTools windows
  don't. Paste goes through Keystone's clipboard as inserted text.

## Shape

- `keystone-agent/src/browser/` stays GPUI-free. `cdp.rs` owns the child process and
  the pipe: a writer thread, a reader thread that splits messages, replies matched to
  calls by ID, and events fanned out to subscribers. `page.rs` holds the agent
  operations on one page: navigate, snapshot, act, screenshot, evaluate and wait.
  `servers.rs` finds dev servers.
  `outline.rs` turns the accessibility tree into the agent's outline.
- `Browsers` holds one browser process, started on first use, with one page per project
  folder. The desktop's `AppState` owns it, so closing a window doesn't close a page.
  Quitting does.
- `browser.rs` in the desktop is the surface, a `Component::Browser` beside the others,
  so it docks, tabs and persists like Notes. It shows the page of the selected
  session's folder: a toolbar (Back, Forward, Reload, the address, dev servers) above
  the frame, and a small card over the frame's bottom-left corner naming what an agent
  last did there. The card takes no input and never moves the page, so a click lands
  where the user aimed; the next click or key in the page clears it. Its minimum
  width is the side panels' 220 px.
- An agent that drives the page while the Browser sits behind another tab, or behind
  the narrow window's fitted tabs, puts the Pull request tab's dot on its tab until it
  is shown.
- Each folder's address is saved as the page moves (`browser_page/` rows beside the
  commit drafts). After a relaunch the first surface to show that folder loads it
  again, then reloads it past Chrome's cache, so a server that is down shows as down
  rather than as the copy Chrome kept. A page that no longer loads shows Chrome's error above the project's dev
  servers, and the address keeps the one that failed. ⌘⌥B (Ctrl+Alt+B on
  Linux), the palette's Toggle browser and the surface "+" menu open it.
- A frame's arrival wakes `AppState`, which saves any page's new address and emits one
  `BrowserChanged` per burst. Only
  the `BrowserView` redraws, and it decodes the newest JPEG off the UI thread, one at a
  time, so a busy page skips frames rather than queueing them.
- The user's clicks, drags, wheel and keys go to the page as CDP input. ⌘V types the
  clipboard's text, ⌘C and ⌘X copy the page's selection, ⌘A selects all, and Tab stays
  in the page (Ctrl on Linux). Other ⌘ shortcuts stay Keystone's, and any key bound to
  a command runs the command first. Commands can't take a key that types or edits text
  (`keymap::refusal`), so rebinding never takes typing away from the page.

## Agent tools

The tools join the orchestrator's list (see [orchestration](orchestrator.md)) and act
on the page of the caller's project. Every call needs a live caller with a running turn,
like the session tools.

| Tool | Behavior |
| --- | --- |
| `browser_open {url?}` | Starts the browser if needed, opens the Browser surface beside the caller's session in each window that shows it (focus stays where it is), and loads `url` when given. Returns status and dev servers. `browser_navigate`, `browser_act` and `browser_evaluate` open the surface the same way, so an agent never drives the page out of sight. |
| `browser_status {}` | URL, title, loading state, viewport size and the project's dev servers. It doesn't start the browser. |
| `browser_navigate {url?, history?}` | Loads `url`, or goes `back`, `forward` or `reload`, and waits for the load event up to 15 s. |
| `browser_snapshot {}` | URL, title, visible text (capped) and an indented accessibility outline where each interactive or named node carries a ref like `e12`. Refs last until the next snapshot or navigation. |
| `browser_act {action, ref?, ...}` | `click`, `type`, `press`, `hover`, `scroll` or `select` on a ref, or at `x`,`y` for clicks. Uses trusted CDP input at the element's centre after scrolling it into view. A stale ref is an error that says to snapshot again. |
| `browser_screenshot {}` | The viewport as a JPEG image block. |
| `browser_evaluate {expression}` | Runs JavaScript in the page and returns the value as JSON, capped at 64 KiB. |

URLs are `http` and `https` only. A bare `localhost:5173` gets `http://`. Navigation isn't
limited to loopback, as in T3 Code. Read-only callers and second opinions may open,
navigate, read and screenshot, but `browser_act` and `browser_evaluate` are refused,
since a click can change the app under test.

Browser tools have a 40-second dispatch budget, where the session tools have five,
because the first call starts Chrome and a navigation waits for its load. One page runs
one operation at a time, so two agents' actions never interleave, and a call whose
caller has already been told it timed out does nothing. A screenshot's `image` becomes
an MCP image block beside the JSON text.

Claude runs the browser tools without asking, as it does the session tools, with two
exceptions that go through the provider's own approval, so Ask mode asks:
`browser_evaluate`, which runs any script and can reach the network, and a
`browser_open` or `browser_navigate` whose `url` isn't this machine's (`localhost`,
`*.localhost`, `127.0.0.0/8`, `[::1]` or `0.0.0.0`). That matches Claude's own web
fetches, and keeps an agent from taking the page to a site, or sending data to one in
an address, unasked. Codex decides
its MCP calls under its own approval policy, as it does for the session tools.

The same rule covers where an agent's actions lead. Whoever acted on the page last,
the user in the surface or an agent through `browser_open`, `browser_navigate`,
`browser_act` or `browser_evaluate`, owns the navigations it makes until the other
acts. Keystone pauses each main-frame document request (`Fetch.enable` for
`Document`). One an agent caused, by a link, a form, a script, a service worker's
`client.navigate()`, a redirect or a pop-up, goes ahead only to this machine, to the address the agent asked for by name, to the
page it is on, or within the redirects of an address already let through. Anywhere
else gets an empty 204, which leaves the page where it was, the note says so,
and the agent's next reply carries `refused_navigation` with the address and the
instruction to ask for it with `browser_navigate`, which goes through the approval
above. The user's own clicks and typing go where they lead.

A page in Chrome's back/forward cache comes back with no request, so history moves
are checked apart from it. `browser_navigate` with `history` reads the entry it
would move to (`Page.getNavigationHistory`) and applies the same rule before it
moves. A script's `history.back()` that restores another machine's page from the
cache while an agent owns the page is caught when the page is shown
(`Page.frameNavigated` of type `BackForwardCacheRestore`) and sent back to where it
was, with the note. The restored page is shown, and its scripts run, until that
navigation lands, a moment later.

The check covers the page itself, the main frame. Its iframes, `fetch` and beacons
reach any address, as in any browser.

Refs are stable: an element
keeps its ref across snapshots and a ref is never reused for another element, so an
old ref names the same element or fails.

Headless Chrome leaves a JavaScript dialog open until it is answered, and the page
does nothing else meanwhile. Keystone dismisses `alert`, `confirm` and `prompt`, as
Playwright does, accepts `beforeunload` so navigations finish, and shows what it
dismissed in the surface's note.

## What a page can reach

The pages are agents' choice, so the engine keeps them in their tab. A live test
(`a_page_keeps_to_its_tab`) checks each point on real Chrome.

- **Local files.** Typed and agent addresses are `http` and `https` only. Chrome
  itself refuses a web page's navigation to `file:`, `chrome:` and `data:` addresses,
  and `fetch` of `file:`.
- **Downloads** are refused (`Browser.setDownloadBehavior`), and the note says
  so. Before, a page or an agent's click saved files into the user's Downloads folder
  without a word.
- **Pop-ups.** A `window.open` or `target=_blank` page used to keep running where
  nobody could see it. Every new tab now starts paused (`Target.setAutoAttach` with
  `waitForDebuggerOnStart`, filtered to pages, so service and shared workers start as
  usual), so Keystone answers a pop-up's first document request
  itself, closes it, and loads its address in the tab, where the checks above apply.
  The pop-up never fetches its page. One that stays blank is closed after a second.
- **Other origins and localhost.** Pages reach any site and any loopback port, as in
  any browser. The profile is Keystone's own, shared by every project's page, with no
  sign-ins of the user's.
- **The app.** Page script has no way to Keystone: the pipe is Chrome's own, no
  binding is exposed, and only frames, the address, the title and dismissed dialog
  text (cut to 120 characters) come back. Snapshot text reaches the agent as page
  content.
- **Permissions.** Chrome runs with `--deny-permission-prompts`, so every prompt,
  location, notifications, camera and microphone, screen capture, MIDI, clipboard
  reads and the rest, is refused at once. A script run before each page's own
  (`Page.addScriptToEvaluateOnNewDocument`) reports each refusal through the console,
  and the note says "The page asked for its location, and Keystone refused".
  Only a name from Keystone's own list is read from that message, so a page can't
  write its own note. A page's `navigator.clipboard.writeText` needs no prompt and
  resolves, but the text stays in headless Chrome's own clipboard: it doesn't reach
  the system clipboard. The surface's own copy (⌘C, Ctrl+C on Linux) does.
- **Ports.** Chrome opens no debugging port.

## Dev servers

The surface and `browser_status` list the project's dev servers: loopback TCP listeners
whose process works inside the project directory, that answer `GET /` with HTML or a
redirect. Listeners and each process's working directory come from `lsof` (`lsof
-Fpcn` and `lsof -a -d cwd -Fpn`) on macOS and Linux alike, so discovery needs `lsof`
installed; without it the list is empty. T3 Code instead tags agent processes with an
environment variable; Keystone doesn't change the provider's environment, so it
matches on the folder.

## Proof

`cargo test -p keystone-agent --test browser_live -- --ignored` drives a real Chrome
against a page the test serves: snapshot refs, clicks, typing, clearing, a select,
Enter submitting a form, stale refs after a reload, screenshots, evaluation, frames,
the containment above, refused permission prompts and their note, where an
agent's clicks, scripts, redirects, pop-ups, service workers and history moves may
lead the page, and service and shared workers starting. It passes on Linux
with Chrome 157; the permission and navigation checks haven't run on macOS yet.
On a Mac, `python3 scripts/macos-browser.py` runs the whole journey in the
real app: it opens the surface by shortcut and by mouse, loads a demo dev server,
types into it, has a real Claude session drive it, quits and relaunches.

## Profile

The browser profile lives in `browser/` beside the workspace database, so a
`KEYSTONE_STATE_PATH` run gets its own. It is never the user's own Chrome profile.
Cookie import, recording and device emulation are follow-ups.
