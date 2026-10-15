# Performance

Keystone tracks build speed and render cost over time. Each run of the perf script appends one row to [perf-log.tsv](perf-log.tsv). Commit that row with the change it measures.

## Run the measurements

Run `python scripts/perf.py --note "what changed"` after a change that could affect build or render speed. Use `--no-log` for a trial run. The script builds first, then records the median of three samples for each timed metric.

| Column | Meaning |
| --- | --- |
| `touch_desktop_s` | Seconds to rebuild `keystone-desktop` after touching `main.rs`. |
| `touch_ui_s` | Seconds to rebuild after touching a `keystone-ui` component. |
| `desktop_debug_mb` | Size of the debug desktop binary. Most of it is debug info, and it drives link time. |
| `sidebar_draw_ms` | Mean time for one full window draw with 200 sessions, from the ignored `perf_sidebar_draw` GPUI test. |

The draw benchmark runs in the GPUI test platform, so it measures layout and scene building, not GPU presentation. Compare rows from the same machine. Treat differences under about 20% as noise on a loaded desktop.

## Decisions

Dependencies build at `opt-level = 2` in the dev profile, and workspace crates stay unoptimized. GPUI, taffy, and text shaping dominate render cost. On 2026-10-15 this change cut `sidebar_draw_ms` from a 52–101 ms range to 8–11 ms. Incremental rebuilds fell from about 1.5 s to about 1.1 s because the binary shrank. The cost is a one-time dependency rebuild, which took 205 s for the app and 70 s for the test build on a 22-core machine.

This is separate from KS-005. That rapid-typing stall was not fixed by an earlier optimization profile, and this change has not been checked against it.

## Next opportunities

- Every composer keystroke redraws the whole window, including every sidebar row. Render the sidebar as its own cached view, or use a virtualized list, so draw cost stops growing with session count.
- The ellipsis workaround in KS-010 costs nothing. Recheck it when upgrading GPUI.
