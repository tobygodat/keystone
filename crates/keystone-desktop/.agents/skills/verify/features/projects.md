# Projects

Each window shows one project at a time and keeps its own. The project decides the folder new sessions start in, the sessions the sidebar lists, and the layout and tabs the window shows.

## Sub-features

- `project-switcher` at the top of the sidebar names the window's project and opens the project list. Ctrl+O and the palette's **Switch project** open the same list.
- The list (`projects`, rows `projects-row-N`, input `projects-input`) shows the window's project first (detail `Current · path`), then every other listed project, most recently opened first, then the folders of sessions no project holds, listed or removed (detail `Has sessions · path`), such as sessions from before projects, with a folder inside another of those offered as the outer one, then **Open folder…**. Opening one of those lists it.
- Typing a path that starts with `/` or `~` adds a row for that folder at the top. Enter opens it. A path with no folder there opens nothing and shows `Project not opened. …`; the list stays open.
- **Open folder…**, Ctrl+Shift+O and the palette's **Open folder as project** ask the platform's folder picker. Where there is none, as in a verify run, the error says so and the list opens with `~/` typed.
- Switching saves the draft, then shows the project's own layout, tabs and sessions, and the session the window showed when it last left that project, else its most recently viewed one. The sidebar lists only sessions whose folder is the project's or inside it. Sessions saved before folders were recorded are listed in every project.
- New sessions start in the window's project folder.
- Every window keeps its own project. A new window opens on the project of the window it came from.
- A relaunch shows each window's last project and session. At launch, `KEYSTONE_PROJECT_DIR` opens the first window on that folder instead. A window that never saved a project, as after an upgrade from before projects, opens on the folder of the session it shows, else on the folder Keystone was launched in. A window saves and lists the project it opens on at once, so a crash does not lose it. A saved selection of another project's session gives way to one of this project's; a window with no selection stays empty. Choosing a folder outside the project for a conversation from before folders moves the window to that folder's project, so the conversation stays shown.
- A project whose folder is missing stays listed with `Folder missing`, and the switcher says so. Its sessions stay readable; no session can start there.
- Each project wears a tile (`Monogram`): the first letter or digit of its folder name on one of seven colors, in `project-switcher` and on its rows in the list. Until the user picks one, the color comes from a hash of the project's path, so it never changes on relaunch. A project whose folder is missing keeps its tile.
- A right click on `project-switcher`, Ctrl+Alt+K and the palette's **Change project color…** list **Automatic** and the seven colors, each showing the tile in that color, with `Current` on the project's choice. Choosing one saves it with the project in `workspace.projects` as `color`, and every window showing the project repaints; **Automatic** removes it. A color a newer release saved reads as automatic and is kept until the user chooses another.
- Shift+Delete on a highlighted row, or its `projects-remove` button, takes the project off the list. The window's own project and projects other windows show cannot be removed. Nothing on disk changes, and its sessions and layouts stay saved. The removal is remembered, so its sessions' folder is not offered again as `Has sessions`; typing its path opens it again.

## Driving it with keystone-verify

Make two git repos A and B under the run's scratch folder. Run the driver with an unreachable session bus (`DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent/...`), so no portal on the host can answer, and confirm it in the app's `/proc/<pid>/environ`. Fake provider wrappers that log `$PWD` show the folder each session started in. `launch` keeps the run's `env`, so prove relaunch restore in a run without `KEYSTONE_PROJECT_DIR`, and prove the variable in a run of its own.

- Press Ctrl+O, type A's full path and press Enter. Create a session and send. The provider's folder must be A.
- Press Ctrl+O, type B's full path and press Enter. Require the switcher to name B, an empty sidebar and the welcome composer. Create a session and send: the folder must be B.
- Click `project-switcher` and choose A's row. Require A's session selected and the only row. `workspace` must list A first, then B.
- Press Ctrl+Shift+O. With no portal, the list opens with `~/` typed and the error says so.
- Press Ctrl+Alt+N: the new window opens on A. Switch it to B through its `project-switcher`. Each window lists only its own project's session.
- Quit and `launch`. Each window comes back on its project and session.
- Quit, rename B's folder, `launch`. The window on B still lists B's session, and the switcher says `Folder missing`.
- Close the window on B, open the list in the other, highlight B and press Shift+Delete. Require it gone from the list and marked `"removed": true` in `workspace.projects`, B's folder and files untouched, and its session still saved.
- Colors: with A and B listed, their tiles show distinct automatic colors (pick folder names whose colors differ; `ProjectColor::automatic` decides). Right-click `project-switcher` on B, type a color and press Enter: B's tile changes and `workspace.projects` saves `color` for B only. Quit and `launch`: B keeps it and A's is unchanged. Press Ctrl+Alt+K, choose **Automatic**: `color` is gone and the hash color is back.
- In a separate run with `KEYSTONE_PROJECT_DIR=A`, the first window opens on A, and Quit lists A.

## Gotchas

- The list uses the palette's component, so app shortcuts are inert while it is open, and Escape closes it.
- `workspace` reads the projects projection as `workspace.projects`.
