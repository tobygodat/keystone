# 0005. Optimize dependencies in dev builds

Status: Accepted, 2026-10-15. Recorded as an ADR 2026-10-27 from `docs/performance.md`.

## Context

GPUI, taffy and text shaping dominate render cost, and they are very slow unoptimized. With an unoptimized dev profile, the 200-session sidebar draw benchmark took 52 to 101 ms per frame, which made debug runs and GPUI tests sluggish.

## Decision

The workspace `Cargo.toml` sets `[profile.dev.package."*"] opt-level = 2`. Dependencies build optimized, and workspace crates stay unoptimized so they rebuild fast. `[profile.dev] debug = 1` keeps line tables only.

## Consequences

- On 2026-10-15 `sidebar_draw_ms` fell from 52 to 101 ms to 8 to 11 ms. Incremental rebuilds fell from about 1.5 s to about 1.1 s, because the binary shrank.
- A fresh clone pays a one-time dependency rebuild: 205 s for the app and 70 s for the test build on a 22-core machine.
- This did not fix the KS-005 rapid-typing stall on its own. The stall stopped reproducing later on the same profile.
