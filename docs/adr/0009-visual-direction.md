# 0009. Visual direction: neutral surfaces, blue for components

Status: Accepted, 2026-10-17. Recorded as an ADR 2026-10-27 from `docs/design/polish-brief.md` ("night harbor").

## Context

The first styling pass used a navy palette with tinted surfaces and warm text. The polish brief set the contract for the next pass, against the Paper file [Keystone | Product design](). Screenshots of its boards are in `docs/design/product-design/`.

## Decision

- Surfaces, borders and text are neutral grays in both themes. Blue belongs to components: Send, the one primary action in an approval, toggles and checked menu items, focus, and the selected row.
- Layout keeps its proportions: a 302 px sidebar, a 640 px transcript and composer, 44 px bars.
- Inter for the interface and GeistMono for commands and code, with the `TextSize` scale 10, 11, 12, 13, 14 and 24. The transcript body is 14 px on a 22 px line in `text`. The user's own words use `text_strong`. Replies never do.
- No new spacing or radius tokens.
- The Paper file's concept pages (project files, Git and pull requests, prompts and integrations) are concepts, not a contract.

Commit `f44bc6a` applied the neutral correction to both themes. Native evidence is in `docs/verification/product-polish.md`.

## Consequences

- `crates/keystone-ui/src/theme.rs` holds the token values. `DESIGN.md` holds the rules that follow from this direction.
- A new accent color, or blue on a non-interactive surface, needs a new ADR.
