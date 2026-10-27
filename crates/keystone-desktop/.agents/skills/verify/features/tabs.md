# Tabs and split panes

Sessions open as tabs, and each window splits into panes, its own tabs and selection apart from other windows ([windows](windows.md)), each with its own tab strip. One conversation view at a time is active: the one the user last clicked, typed in or moved to by keyboard. It shows the selected session, which the composer, the top bar and the session shortcuts act on. The layout, which session each tab holds and the selection are saved together in SQLite in one write, so a relaunch, or a crash, restores them as one.

## Sub-features

- `tabs-single` is a window with one conversation view. It has no tab strip and follows the sidebar's selection, as before tabs. A state saved before tabs loads this way.
- `tabs-split-right` splits the active pane to the right with Ctrl+\ or the split button (`split-right` in the top bar, `pane-split-{ViewId}` on a pane's strip). A pane with several tabs moves its active tab into the new pane. A pane of one opens the most recent unsettled session that is not on screen, else the most recent settled one; if every session is on screen the error line says so.
- `tabs-open` opens a session from the sidebar. A click shows it in the active tab, or brings forward the tab that already holds it, in whichever pane. Ctrl+click (⌘-click on macOS) or a middle click opens it in a new tab next to the active one. Ctrl+T starts a session with the active session's provider in a new tab.
- `tabs-activate` makes a view active with a click on its tab or anywhere in its transcript, or by moving focus into it with F6, Shift+F6, Tab or Ctrl+Shift+T. The active pane's strip has an accent underline.
- `tabs-cycle` moves through every conversation tab in layout order with Ctrl+Tab and Ctrl+Shift+Tab, and wraps.
- `tabs-close` closes the active tab with Ctrl+W, or any tab with its close button or a middle click. A pane closes with its last tab and its split collapses. The last conversation view never closes; Ctrl+W there releases its tab so it follows the selection again.
- `tabs-drag` drags a tab within its strip to reorder, onto another pane's strip to move it there, or onto a pane's edge (`pane-drop-{ViewId}-left/right/above/below`, drawn only during a drag) to split it out beside that pane. The same drops work from another window on the same project (see `windows-move-tab`).
- `tabs-restore` brings back the panes, tab order, active tabs, divider positions and selection after Quit and relaunch, and after a SIGKILL. Each session keeps its own draft.

Per-session state follows the session, not the pane: a running session's tab shows the working dot, a recovery card shows in the pane that holds its session and takes keyboard focus there, an idle-evicted Claude session reloads its history when its tab is drawn and resumes on Send, and orchestrator child sessions appear in the sidebar without changing any pane until opened.

## How to get to it (user POV)

- Open two sessions, then press Ctrl+\ or click the split button in the top bar.
- Click sessions in the sidebar; middle-click one to keep the current tab and open another.
- Press Ctrl+Tab, Ctrl+W and Ctrl+T, or use the tab strip with the mouse.

## Driving it with keystone-verify

Preconditions: scripted providers, a disposable project and at least three sessions.

- Press Ctrl+\ (`{"action":"keypress","keys":["ctrl","backslash"]}`). Require `surface-tabs-0` and a second strip, two `conversation*` elements side by side, and `workspace` showing `preferences.project_tabs[<project>]` with two views and two different sessions.
- Drag `surface-tabs-{a}-tab-0` onto `surface-tabs-{b}-tab-0` through `call` press, move and release. Require the tab listed in pane b's strip and gone from pane a.
- Start a drag of a tab and move over a pane's right edge. Require `pane-drop-{ViewId}-right` and a highlight; release and require a new pane.
- Click a sidebar row already open in another pane. Require that pane's strip underlined, no new tab, and the composer showing that session's draft.
- Press Ctrl+Tab three times. Require the active tab to advance and wrap.
- Press Ctrl+W until one pane is left. Require the split collapsed in a screenshot and in `workspace`.
- Quit with Ctrl+Q, relaunch, and require the same panes, tab order, active tab and drafts. Repeat with SIGKILL instead of Quit.

## Gotchas

- Tab and strip ids are `surface-tabs-{first ViewId in the pane}-tab-{n}` and `-close`, so they change when a pane's first tab changes. Read `elements` after each change.
- A narrow window (for example 720 px) fits every view into one row of tabs above a single conversation. The fitted tabs (`fitted-tab-{ViewId}`) name sessions too, are at most 200 px wide with long titles cut off, scroll sideways, and a click activates them.
- While Customize is open, tabs are layout occurrences: they read "Conversation N", Ctrl+Tab, Ctrl+W and drags between strips do nothing, and a tab's close button closes it in the draft.
