# Screen walk

`scripts/screen-walk.py` screenshots the surfaces of the native app so interface text and elements can be reviewed in place, and compared before and after a change.

```bash
cargo build -p keystone-desktop -p keystone-agent --bins --locked
python3 scripts/screen-walk.py walk --out .audit/walk-before
# change the interface, rebuild
python3 scripts/screen-walk.py walk --out .audit/walk-after
python3 scripts/screen-walk.py sheet .audit/walk-before .audit/walk-after --out .audit/sheets
```

`walk` runs at 1920×1080 and, as the narrow check, 720×1080 unless `--width` or `--height` is given. Each width gets a fresh private headless Sway through `scripts/native-driver.py`, a fresh state database, a scratch git project in `/tmp/keystone-walk-<width>` (the same path every run, so before and after shots match; a lock refuses a second walk at the same width), empty provider homes and the scripted `keystone-fake-claude` and `keystone-fake-codex`. No real provider runs, and nothing reads the user's own `~/.claude`, `~/.claude.json` or `~/.codex`. It writes `<out>/<width>/<surface>.png` and `<out>/manifest.json`, which lists every surface as `shot`, `skipped` (a feature this build does not have) or `failed`, with the reason. A surface is `shot` only after an element that proves it opened is drawn, so a broken shortcut fails its surface instead of saving the wrong screen. It exits non-zero when a surface failed. The pointer is parked in the bottom-right corner before each shot so no tooltip or hover state shows.

`--theme` and `--density` set that theme and density in Settings → Appearance before the first shot.

`sheet` tiles one walk four to a row, or pairs two walks before and after, two to a row, into `sheet-<width>-<n>.png` files, in walk order and from the surfaces each manifest marks `shot`. It needs ImageMagick's `montage`.

## Surfaces

In order: welcome; the sidebar's new-session and filter menus; an empty session; the model and approval menus; the Full access dialog; the slash menu; a reply while it streams and when it ends; the approval card and the command row after it, with its output; a widget card; the usage meter; the recovery card and its report after Continue; a Codex session; the sidebar's sections and its no-match search; the palette and its empty state; the project switcher; Files; the rename dialog; a handoff; tabs; a split (at 1920; at 720 the tabs shot is the narrow layout); the move-tab list; a second window; Customize and its borrow dialog; the open-folder fallback; and every Settings page, found from the `settings-section-*` elements so a new page is walked without a change here.

Palette commands from features that are not on every branch run in a new session with one reply, and only when `crates/keystone-desktop/src/commands.rs` has their id: Changes (`toggle-changes`), Notes (`toggle-notes`), Insert prompt, Attach files, New session in worktree and Ask for a second opinion. Add a row to `OPTIONAL` in the script for another such surface.

Not walked yet: reasoning and file-change rows (the scripted providers emit neither), the borrow dialog with saved layouts to choose from, composer error states, Git and the pull request form inside Changes, a split at 720, and cards that need their own scripted trigger, such as the usage-limit and delegated-result cards. Each needs a step once its feature and a fake trigger are on main.
