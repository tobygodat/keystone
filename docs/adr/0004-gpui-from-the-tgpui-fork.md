# 0004. Take GPUI from the pinned tGPUI fork

Status: Accepted, 2026-10-16 (TOB-10, #11). Recorded as an ADR 2026-10-27.

## Context

Keystone started on crates.io `gpui = 0.2.2`. That release had bugs Keystone had to work around (KS-009, KS-010, KS-013, now TOB-212). Zed and Zeron run newer GPUI, and Keystone needed fixes upstream doesn't carry. gpui-kit 0.7.3 pins `gpui-pre = 0.3.9` (Zed at `93d7456`).

## Decision

`keystone-ui` and `keystone-desktop` take `gpui-pre`, `gpui-pre-platform` and `gpui-pre-wgpu` from one pinned commit of `github.com/tobygodat/tGPUI`, Keystone's private fork of the gpui-pre 0.3.9 snapshot. Every crate pins the same `rev`. The fork carries Keystone's own patches, including the KS-009 keymap fix, the taffy layout change and UC-14's private drag MIME type. Each rebase of the fork must carry them.

This supersedes "GPUI stays pinned at crates.io `=0.2.2`" from the 2026-10-15 hand-off.

## Consequences

- A GPUI change is a tGPUI commit, then a `rev` bump in every `Cargo.toml` that names it.
- Workarounds written for GPUI 0.2.2, such as `Label::truncate`, need rechecking on the fork (TOB-212).
- The move brought API churn and slower nested flex layout under taffy 0.13: 2,000 nested list items took about 28 s against 82 ms on 0.2.2. Keep layouts flat and virtualized.
- gpui-pre's test scheduler rejects wake-ups from other OS threads unless a test calls `allow_parking`, which every desktop test does.
