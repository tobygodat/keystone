# Performance

Keystone tracks build speed and render cost over time. Each run of the perf script appends one row to [perf-log.tsv](perf-log.tsv). Commit that row with the change it measures.

## Run the measurements

Run `python scripts/perf.py --note "what changed"` after a change that could affect build or render speed. Use `--no-log` for a trial run. The script builds first, then records the median of three samples for each timed metric.

| Column | Meaning |
| --- | --- |
| `touch_desktop_s` | Seconds to rebuild `keystone-desktop` after touching `main.rs`. |
| `touch_ui_s` | Seconds to rebuild after touching a `keystone-ui` component. |
| `desktop_debug_mb` | Size of the debug desktop binary. Most of it is debug info, and it drives link time. |
| `sidebar_draw_ms` | Mean window draw with 200 sessions after only the root view is notified, as a composer keystroke does. From the ignored `perf_sidebar_draw` GPUI test. |
| `sidebar_refresh_ms` | Mean window draw with 200 sessions after the sidebar is also notified, as a session, section, or search change does. Rows before this column existed leave it empty. |
| `transcript_draw_ms` | Mean window draw with a 1,000-turn (3,000-row) transcript open after the root view is notified, as a composer keystroke does. From the ignored `perf_transcript_draw` test. Added after `note`, so older rows end before it. |
| `transcript_stream_ms` | Mean window draw for the same transcript while a reply streams at its end, one delta per frame. Added with the virtualized list; the baseline row leaves it empty. |

The draw benchmark runs in the GPUI test platform, so it measures layout and scene building, not GPU presentation. Compare rows from the same machine. Treat differences under about 20% as noise on a loaded desktop.

## Decisions

Dependencies build at `opt-level = 2` in the dev profile, and workspace crates stay unoptimized. GPUI, taffy, and text shaping dominate render cost. On 2026-10-15 this change cut `sidebar_draw_ms` from a 52–101 ms range to 8–11 ms. Incremental rebuilds fell from about 1.5 s to about 1.1 s because the binary shrank. The cost is a one-time dependency rebuild, which took 205 s for the app and 70 s for the test build on a 22-core machine.

This is separate from KS-005. That rapid-typing stall was not fixed by an earlier optimization profile. With this change in place, the 10 ms native check passed in 27 of 27 repeated runs on 2026-10-15, on headless Sway and Hyprland, and KS-005 is closed. See [issues.md](issues.md).

The session sidebar is a cached GPUI view (`SessionSidebar`). GPUI reuses its previous layout and paint until that entity is notified, so composer keystrokes skip every session row. `Keystone::refresh` notifies it; call `refresh` instead of `cx.notify()` for any change the sidebar shows. On 2026-10-15 this cut `sidebar_draw_ms` from 4.78 ms to 0.42 ms. A full sidebar rebuild costs 0.50 ms. The same entity without caching measured 5.9 ms for both cases. The likely reason a cached rebuild is still cheap is that GPUI lays out a cached view as a separate root with fixed bounds. That explanation was not profiled.

The transcript is a virtualized GPUI `list`. With 3,000 rows, a draw fell from 115 ms to under 1 ms on 2026-10-16, because only the rows in view are laid out and painted. Streaming a reply at the end of that transcript costs the same as a still frame.

Builds go through `scripts/rustc-wrapper.sh`, set as the rustc wrapper in `.cargo/config.toml`. When mold is installed, the wrapper adds `-C link-arg=-fuse-ld=mold`. When sccache is installed, it runs rustc through sccache. Without either, it runs rustc unchanged, so the repo still builds on a machine that has neither. On Arch, install both with `sudo pacman -S --needed sccache mold`. The cache lives in `~/.cache/sccache` and defaults to 10 GB.

On 2026-10-16 each configuration built `keystone-desktop` cold in a fresh worktree, interleaved so that plain builds ran before and after the others. Other projects kept the load average between 30 and 240 on 24 cores, so CPU time is the comparable figure. Wall time is shown with the load when the build started.

| Build | CPU s | Wall s (load) | Touch rebuild s (median of 3) | sccache hits |
| --- | --- | --- | --- | --- |
| Plain | 994 | 161 (238) | 0.68 | |
| sccache and mold, empty cache | 1,009 | 65 (92) | 0.54 | 0 of 499 |
| sccache and mold, warm cache, new worktree | 640 | 49 (46) | 0.53 | 406 of 499 |
| Plain, again | 977 | 68 (32) | 0.92 | |

A cache miss costs about 2% more CPU. A new worktree with a warm cache spends 35% less CPU. mold cuts a touch rebuild by about 0.15 to 0.4 s.

The other 93 crates miss because serde's build script writes into `target/`, and serde's rlib embeds that path. Every crate built on serde then has a different input hash in each worktree, including GPUI, the longest step on the critical path. `--remap-path-prefix` would remove the path from the rlib. sccache 0.17 hashes that flag's value, though, and `SCCACHE_BASEDIRS` does not rewrite rustc arguments, so the remap yielded no hits at all. mold is passed as a constant flag for the same reason. A linker script inside the checkout puts the checkout's path into every key, and a new worktree then hit 2 of 499 crates.

## Next opportunities

- A sidebar rebuild still lays out every row. If session counts grow into the thousands, switch the list to `uniform_list` so only visible rows are built.
- `Label::truncate` shapes each label on every layout and once more when it cuts one (KS-010). With 200 rows the refresh benchmark shows no measurable cost, even when every excerpt is cut, because GPUI caches shaped lines across frames. A first frame with new text pays the shaping. If that shows up, cache the cut per row width.
- Each row now derives its attention and formats its time, which calls `localtime_r` once per row. With 200 rows that added about 0.1 ms to a refresh (1.08 ms to 1.20 ms, medians of 15 alternating runs).
