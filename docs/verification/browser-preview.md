# Browser preview (TOB-302), 2026-11-02

`python3 scripts/macos-browser.py` on an Apple Silicon Mac with Chrome 156 and Claude Code 2.1.317, through the automation inbox. Screenshots are GPUI's own frames of the window.

1. ⌘N made a Claude session in a throwaway `orders-demo` project serving a page with `python3 -m http.server`. ⌘⌥B opened the Browser surface, which listed that server ([dev-server.png](browser-preview/dev-server.png)).
2. ⌘⌥B closed it, and the palette's Toggle browser, clicked by mouse, opened it again. Clicking the server loaded the page beside the session.
3. A click in the page's field, typing "Grace Hopper" and Enter added an order through the surface's input ([user-input.png](browser-preview/user-input.png)).
4. With the surface closed, the real Claude session was asked to use only the browser tools. Its `browser_open` brought the surface back, and it snapshotted, typed "Ada Lovelace", clicked Add order and read back "2 orders". The note line names what it did ([agent-drove-the-page.png](browser-preview/agent-drove-the-page.png)).
5. ⌘Q exited with code 0 and the headless Chrome stopped. The relaunch restored the surface.
