# 0006. sccache and mold through a rustc wrapper, separate targets per worktree

Status: Accepted, 2026-10-16. Recorded as an ADR 2026-10-27 from `docs/performance.md` and `docs/dev-workflow-research.md`.

## Context

Agents work in many parallel worktrees, and each one builds every dependency from scratch. A shared Cargo target directory caused artifact interference between worktrees.

## Decision

- `.cargo/config.toml` sets `scripts/rustc-wrapper.sh` as the rustc wrapper. It runs rustc through sccache when sccache is installed, and adds `-C link-arg=-fuse-ld=mold` when mold is installed. Without either it runs rustc unchanged, so the repo still builds anywhere.
- mold is passed as a constant flag. A linker script inside the checkout would put the checkout path into every cache key: a new worktree then hit 2 of 499 crates.
- Each worktree keeps its own `target/`. They share only sccache (`~/.cache/sccache`, 10 GB by default).
- Don't use `--remap-path-prefix` or `SCCACHE_BASEDIRS`. sccache 0.17 hashes the remap flag's value and the basedir setting doesn't rewrite rustc arguments, so they gave no hits. Retry only with a new reason, such as an upstream fix.

Measured on 2026-10-16, building `keystone-desktop` cold in a fresh worktree, interleaved, under a load average of 30 to 240 on 24 cores (CPU time is the comparable figure):

| Build | CPU s | Wall s (load) | Touch rebuild s (median of 3) | sccache hits |
| --- | --- | --- | --- | --- |
| Plain | 994 | 161 (238) | 0.68 | |
| sccache and mold, empty cache | 1,009 | 65 (92) | 0.54 | 0 of 499 |
| sccache and mold, warm cache, new worktree | 640 | 49 (46) | 0.53 | 406 of 499 |
| Plain, again | 977 | 68 (32) | 0.92 | |

## Consequences

- A cache miss costs about 2% more CPU. A new worktree with a warm cache spends 35% less CPU. mold cuts a touch rebuild by about 0.15 to 0.4 s.
- 93 crates still miss in each new worktree, because serde's build script writes into `target/` and serde's rlib embeds that path. That includes GPUI, the longest step on the critical path.
- Install both on Arch with `sudo pacman -S --needed sccache mold`.
