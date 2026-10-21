# Multiple windows

Keystone opens several windows on one workspace. Each window has its own tabs, panes, active view and selection, and its composer sends to the session that window shows. Each window also saves its own current project, so a window can show any project. At launch the first window opens on the project Keystone was launched in, as before; every other window, and a first window reopened while Keystone runs, opens on the project it last showed. A window's project saves when it changes, with its placement, and with Quit. Sessions, their turns, accounts, recovery, idle eviction and orchestrator children belong to the app, so every window's sidebar lists every session and a window that closes leaves its work running.

## Sub-features

- `windows-new` opens a window with Ctrl+Alt+N or the palette's "New window". It opens empty, on the opener's project, beside the opener where the compositor allows. Its title is "Keystone N".
- `windows-open-session` opens a session in a new window from the sidebar row's right-click menu, a session tab's right-click menu (pane strips and fitted tabs), or the palette's "Open session in new window" (Ctrl+Alt+Shift+N, the window's selected session). The menu item is `menu-open-in-new-window`.
- `windows-own-view` keeps tabs, panes, the active view and the selection per window. Selecting in one window never changes what another shows.
- `windows-shared-session` shows one session in two windows from the same live conversation; typing in one window's composer shows in the other at once while it shows that session, and a window that selects the session takes the text being typed elsewhere, so Quit saves the newest text.
- `windows-close` closes a window with its title-bar Close, Ctrl+Shift+W or the compositor. Its sessions and turns keep running and stay in every other window's sidebar. Closing the first window hands its place to the next one, whose layouts, tabs and selection then count as the first window's, its current project's included when it kept the default layout there; the closed window's layouts stay for every project the next one never showed.
- `windows-last` closing the last window quits through the Quit save at once when no turn runs. While a turn runs, Keystone keeps running with no window and quits through the same save when the last turn ends; the window is kept so the next launch opens it. A running turn waiting for an approval keeps the last window open (the error line says why), and so does a last window whose place cannot be saved while a turn runs; a turn that asks for one while no window is open opens the windows again so it can be answered, and so does a Quit that cannot save, to show its error.
- `windows-move-tab` moves a session tab to another window on the same project: drag it onto that window's strip or a pane edge, or release it over the desktop to open it in a new window. "Move tab to window…" (Ctrl+Alt+M) lists the other windows on this project and "New window" for the active tab. The window it leaves and the one it reaches save in one write; the session keeps running. While the tab is outside every window, the compositor carries it under a private type (`application/x-gpui-app-drag;id=…`) that no other application accepts, and shows the tab as the drag icon. On Wayland a drop on another application changes nothing, and so does Esc before the tab leaves its window. A window on another project refuses the tab with "Tab not moved. …".
- `windows-restore` reopens every window with its size, its project, its panes, tabs and selection after Quit and after SIGKILL. A window's own layout and tabs save in one write each time they change, a new window saves before it opens, and its placement saves half a second after it stops moving.

Ctrl+Q quits every window through one save of every window's draft, placement and project.

## Driving it with keystone-verify

`elements` lists elements from every window. Each element names its window's title, and `elements`, `click`, `wait_for` and `wheel` take `window` (for example `"Keystone 2"`) to pick one when a name repeats. Require `windows_list` to show each window under the one app PID. While Keystone runs with no window open, `elements` lists nothing, and a window the compositor no longer lists shows no elements.

- Press Ctrl+Alt+N. Require a second window titled "Keystone 2" and `workspace` showing `preferences.windows` with two entries.
- Right-click `session-2`, click `menu-open-in-new-window`. Require the new window to show session 2 while the first keeps its own.
- Start a fake reply with a delay in window 2, close window 2 with Ctrl+Shift+W, and require the reply to finish in window 1's sidebar and transcript.
- With two windows on the project, drag a tab from window 1 (`mouse_down`, several `move`s, `mouse_up`) onto window 2's strip, then onto a pane edge. Screenshot mid-drag and require the tab under the pointer. Require `workspace` to list the session under window 2 only. Release one over the desktop and require "Keystone 3" holding it; release one over a foreign Wayland window (`foot`) and require `workspace` unchanged.
- Press Ctrl+Alt+M, pick "Keystone 2", and require the active tab there; pick "New window" and require a new window.
- Quit with Ctrl+Q and relaunch; repeat with SIGKILL. Require the same windows, panes, tabs and selections.

## Limits

- Sway and most Wayland compositors place windows themselves, so saved positions may not be honored; sizes are. Placement records the origin the platform reports.
- While Keystone runs with no window, a second launch is refused by the workspace lock until the last turn ends. A provider that never ends its turn keeps that windowless process alive.
- Windows reopened for an approval are the saved ones; a session that last showed in a window that closed is reached from the sidebar, where it is marked as asking.
- A placement's fields from a newer version are not kept.
- Sway takes no keys while it owns a drag, so once the tab leaves its window Esc does nothing there; release it over another application to cancel. Compositors that cancel drags on Esc (GNOME, KDE) end it with nothing changed; not verified here.
- A desktop drop needs the compositor's layer shell and viewporter, so it opens a window on Sway and other wlroots compositors; elsewhere a release over the desktop changes nothing. On X11 and macOS the platform does not take the drag, so other windows cannot receive it and a release outside the window changes nothing, since it may be over another app; use Move tab to window…. Not verified here.
- Narrow windows show fitted tabs, which do not drag between windows; use Move tab to window….
