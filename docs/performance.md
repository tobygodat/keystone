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

The draw benchmark runs in the GPUI test platform, so it measures layout and scene building, not GPU presentation. Compare rows from the same machine. Treat differences under about 20% as noise on a loaded desktop.

## Decisions

Dependencies build at `opt-level = 2` in the dev profile, and workspace crates stay unoptimized. GPUI, taffy, and text shaping dominate render cost. On 2026-10-15 this change cut `sidebar_draw_ms` from a 52–101 ms range to 8–11 ms. Incremental rebuilds fell from about 1.5 s to about 1.1 s because the binary shrank. The cost is a one-time dependency rebuild, which took 205 s for the app and 70 s for the test build on a 22-core machine.

This is separate from KS-005. That rapid-typing stall was not fixed by an earlier optimization profile. With this change in place, the 10 ms native check passed once on 2026-10-15. See KS-005 in [issues.md](issues.md).

The session sidebar is a cached GPUI view (`SessionSidebar`). GPUI reuses its previous layout and paint until that entity is notified, so composer keystrokes skip every session row. `Keystone::refresh` notifies it; call `refresh` instead of `cx.notify()` for any change the sidebar shows. On 2026-10-15 this cut `sidebar_draw_ms` from 4.78 ms to 0.42 ms. A full sidebar rebuild costs 0.50 ms. The same entity without caching measured 5.9 ms for both cases. The likely reason a cached rebuild is still cheap is that GPUI lays out a cached view as a separate root with fixed bounds. That explanation was not profiled.

## Next opportunities

- A sidebar rebuild still lays out every row. If session counts grow into the thousands, switch the list to `uniform_list` so only visible rows are built.
- `Label::truncate` shapes each label on every layout and once more when it cuts one (KS-010). With 200 rows that adds about 0.04 ms to a sidebar refresh, or 0.13 ms when every excerpt is cut. GPUI caches shaped lines across frames, so the cost stays small. If it grows, cache the cut per row width.
