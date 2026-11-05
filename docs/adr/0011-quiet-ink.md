# 0011. Visual direction: quiet ink

Status: Accepted, 2026-11-02. Supersedes [0009](0009-visual-direction.md).

## Context

Toby found the 0009 look ugly (TOB-300). An audit of the real app in both themes found:

- The blue fill on Send and Allow was the only saturated color on screen.
- Code blocks, tables and tool rows were bordered boxes on fills.
- Body text at 15 px sat beside 13 px interface text with little else to carry hierarchy.
- Six corner radii were in use, from 6 to 20 px.
- The light theme's surfaces were too close together, and its selection washes were too heavy.

The direction takes its values from the MonoCode dark style in Toby's T3 Code fork (`apps/web/src/fork/theme.css`). It is a reference, not a copy. Toby approved it on 2026-11-02, with before and after screenshots of the real app.

## Decision

- **Surfaces sit close in value and hairlines separate them.** Dark is `#171717` for the main area, `#151515` for the sidebar and `#1b1b1b` or `#1e1e1e` for raised surfaces. Light is `#ffffff`, `#f8f8f8` and `#f5f5f5`.
- **Text is ink in opaque tiers.** Dark uses `#ebebeb`, `#ababab` and `#969696`. Light uses `#171717`, `#5c5c5c` and `#6e6e6e`. Hover, selection and strokes are washes of white or black, from 4 to 15%.
- **The primary action is filled with ink.** Send, Allow and `ButtonStyle::Primary` use `element_inverse`: `#ebebeb` with dark text in the dark theme, and `#171717` with white text in the light theme. No interface element is filled with blue.
- **Blue is for focus rings, links and text selection.** Status colors mark state (failed, waiting, success). Shared components put them on a mark, not on whole runs of text, as each component is reworked.
- **Raised blocks have a fill and no hairline.** `Elevation::Raised` draws the fill only. A block that needs emphasis, such as an approval, adds its own border. Surfaces and overlays keep their hairline.
- **Type.** `TextSize::Body` is 14 px. The transcript keeps its 22 px line. The rest of the scale is unchanged.
- **Radii.** 6 px for controls and menu rows, 8 px for rows and panels, and 12 px for popovers and the composer.

Layout widths, Inter and GeistMono stay as 0009 set them.

## Consequences

- `crates/keystone-ui/src/theme.rs` and `tokens.rs` hold the values. `DESIGN.md` holds the rules that follow from them.
- Later TOB-300 PRs make two values user choices in Settings → Appearance: the color of the primary action, defaulting to ink, and the transcript text size, defaulting to 14 px. They also rework the composer, the sidebar rows and the side panels on these tokens.
- A saturated fill outside those user choices needs a new ADR.
