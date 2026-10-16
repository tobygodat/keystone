# Design system

`crates/keystone-ui` holds Keystone's theme, tokens, icons, and shared controls. The desktop app builds its controls from these components. Layout containers in the app still use GPUI styling, with colors taken from theme tokens.

## Rules

- Colors live only in `crates/keystone-ui/src/theme.rs`. `scripts/check.sh` fails on GPUI color constructors such as `rgb(`, `hsla(`, or `white(` anywhere else.
- Components read the theme through `cx.theme()` when they render. Call sites pass semantic `Color` values, not raw colors.
- Hover, press, and selection are translucent washes over the parent surface, white in the dark theme and black in the light theme. One state color works on the sidebar, the main column, the composer, and popovers.
- Components size controls and rows through `cx.theme().density`, not fixed heights. Use `Density::control` for heights and `Density::pad` for row padding.
- Interactive components accept `debug_selector`, so GPUI simulated-input tests can find them.
- Disabled controls ignore clicks and show a not-allowed cursor.

## Contents

| Item | Use |
| --- | --- |
| `Theme`, `Colors`, `Fonts`, `ActiveTheme` | The installed palette, density, and fonts. `keystone_ui::init` installs the dark theme. `keystone_ui::set_theme` replaces it and redraws every window. |
| `Appearance`, `Density` | `Light` or `Dark`, and `Compact`, `Default`, or `Spacious`. `Theme::new` combines them with `Fonts`. |
| `Color`, `TextSize`, `Elevation`, `spacing`, `radius` | Semantic tokens. Prefer these to raw pixels and colors. `Color::Strong` is the brightest text, for headings, selected titles, and draft text. `radius::COMPOSER` rounds the floating composer. |
| Shell colors | `sidebar` fills the left column. `composer`, `composer_border`, and `composer_shadow` style the floating composer. `element_inverse` with `text_inverse` is the one filled primary action (Send); `element_disabled` is its disabled wash. `provider_claude` and `provider_codex` tint provider glyphs; the desktop app maps `Provider` to them, so keystone-ui stays provider-agnostic. |
| `StyledExt` | `surface(elevation, cx)` for backgrounds, borders, and shadows, and `hairline_*` for separators. |
| `IconName`, `Icon`, `Icons` | Typed icon names and the asset source that serves them. Icons are drawn on a 24-unit grid and tinted with the text color. `Wedge` and `WedgeFilled` mark active and settled sessions, `Diamond` is the provider glyph, `Pencil` starts a session, and `ArchStones` with `ArchKeystone` stack into the arch mark so the keystone can take the accent color. Filled shapes set `fill='white'` on their own paths. |
| `Label`, `Kbd` | Text with a size, a color, and optional truncation or monospace. `Kbd` shows a shortcut hint. |
| `Button`, `IconButton` | `ButtonStyle::{Ghost, Subtle, Filled}`, `ButtonSize::{Compact, Default, Large}`, selected and disabled states, and tooltips. |
| `ListItem` | A selectable row for lists such as the session sidebar. `on_hover` reports the pointer entering and leaving, so a list can build a row action only for the row under the pointer. |
| `PopoverMenu`, `MenuItem`, `MenuHeader`, `MenuSeparator` | An anchored overlay menu. The caller owns the open state. `MenuItem::checked` marks one choice in a group. |
| `Tooltip` | A hover card with an optional shortcut. |

Run `cargo run -p keystone-ui --example gallery` to see every component in every state. Its header switches between themes and densities.

## Adding a component

Write a `RenderOnce` struct with builder methods, read colors from `cx.theme()`, and add it to the prelude and the gallery. Add a behavior test in `crates/keystone-ui/tests/components.rs` when the component handles input.

## Sources

The component shapes follow Zed's `ui` crate, and the visual values follow Zeron. Zed's UI crates are licensed GPL-3.0-or-later, so no Zed code was copied. Zeron is MIT-licensed, and only design values were adapted from it.

## Known limits

GPUI 0.2.2 cannot render a reliable text ellipsis, so `Label::truncate` clips. See KS-010 in [the issue list](issues.md).

Text sizes and the monospace font do not change with density or font choice yet. The default fonts, Inter and GeistMono Nerd Font Mono, are not bundled. GPUI falls back to a system font when they are missing.
