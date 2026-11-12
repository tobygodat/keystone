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
| `terminal_draw_ms` | Mean window frame with the terminal drawer showing a full grid of at least 200×50 cells whose color changes at every cell (the eight ANSI colors in turn), after only the terminal is notified, as shell output does. From the ignored `perf_terminal_draw` test. Added with the terminal (TOB-301); earlier rows end before it. |
| `terminal_idle_ms` | Mean window frame with that drawer open after only the root view is notified, as a composer keystroke does. The cached drawer is not drawn again; its painted frame is replayed. |

The draw benchmark runs in the GPUI test platform, so it measures layout and scene building, not GPU presentation. Compare rows from the same machine. Treat differences under about 20% as noise on a loaded desktop.

## Decisions

The build and render decisions these numbers drove are ADRs: [0005](adr/0005-optimize-dependencies-in-dev-builds.md) optimizes dependencies in dev builds, [0006](adr/0006-sccache-and-mold-through-a-rustc-wrapper.md) adds sccache and mold through a rustc wrapper, and [0007](adr/0007-cached-sidebar-and-virtualized-transcript.md) caches the sidebar view and virtualizes the transcript.

## Next opportunities

- A sidebar rebuild still lays out every row. If session counts grow into the thousands, switch the list to `uniform_list` so only visible rows are built.
- `Label::truncate` shapes each label on every layout and once more when it cuts one (KS-010, TOB-212). With 200 rows the refresh benchmark shows no measurable cost, even when every excerpt is cut, because GPUI caches shaped lines across frames. A first frame with new text pays the shaping. If that shows up, cache the cut per row width.
- Each row now derives its attention and formats its time, which calls `localtime_r` once per row. With 200 rows that added about 0.1 ms to a refresh (1.08 ms to 1.20 ms, medians of 15 alternating runs).
