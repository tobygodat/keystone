# 0003. No GPL code; borrow patterns and permissive sources with credit

Status: Accepted, 2026-10-15. Recorded as an ADR 2026-10-27 from `docs/handoff.md` and the UI components plan.

## Context

The best GPUI component code is in Zed's `ui`, `theme`, `component`, `picker`, `fuzzy` and `workspace` crates, which are GPL-3.0-or-later. Zeron (`github.com/zeronsh/zeron`) is MIT. gpui-component (gpui-kit) is Apache-2.0.

## Decision

- Copy no Zed code. Take patterns only.
- Adapt design values and design ideas from Zeron, with credit. The provider design names the commit studied.
- Borrow gpui-component source file by file, with attribution in the file and in `THIRD_PARTY_NOTICES.md`, keeping Keystone's own tokens. Keystone does not adopt gpui-kit as a dependency: the trial (draft PR #10) rejected it.
- Write small things ourselves when no permissive crate fits the offline registry, such as the command palette's subsequence scoring instead of a fuzzy-matching crate.

## Consequences

- Keystone stays free to choose its own licence.
- Each borrowed file carries its source and licence, and `THIRD_PARTY_NOTICES.md` stays current.
- Accepting GPL code would need a new ADR that supersedes this one.
