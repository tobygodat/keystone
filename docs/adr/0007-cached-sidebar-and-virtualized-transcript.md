# 0007. Cache the sidebar view and virtualize the transcript

Status: Accepted, 2026-10-16. Recorded as an ADR 2026-10-27 from `docs/performance.md`.

## Context

Every composer keystroke redrew the whole window: every sidebar row, and every transcript row. With 200 sessions, or a 1,000-turn transcript, typing got slow.

## Decision

- The session sidebar is a cached GPUI view (`SessionSidebar`). GPUI reuses its previous layout and paint until that entity is notified. `Keystone::refresh` notifies it. Call `refresh` instead of `cx.notify()` for any change the sidebar shows.
- The transcript is a virtualized GPUI `list`, so only the rows in view are laid out and painted.

## Consequences

- On 2026-10-15 `sidebar_draw_ms` fell from 4.78 ms to 0.42 ms, and a full sidebar rebuild cost 0.50 ms. The same entity uncached measured 5.9 ms for both. The likely reason a cached rebuild is still cheap is that GPUI lays out a cached view as a separate root with fixed bounds. That explanation was not profiled.
- On 2026-10-16 a 3,000-row transcript draw fell from 115 ms to under 1 ms, and streaming at the end costs the same as a still frame.
- A sidebar change that skips `refresh` won't show until something else notifies the sidebar.
- The list caches row heights, so an appearance change leaves off-screen rows stale until they scroll into view (TOB-215).
- Verification runs render the sidebar uncached. Take perf numbers from the `perf_` tests and `scripts/perf.py`, never from live verification runs.
