# Design system

`crates/keystone-ui` holds Keystone's theme, tokens, icons, and shared controls. The desktop app builds its controls from these components. Layout containers in the app still use GPUI styling, with colors taken from theme tokens.

## Rules

- Colors live only in `crates/keystone-ui/src/theme.rs`. `scripts/check.sh` fails on GPUI color constructors such as `rgb(`, `hsla(`, or `white(` anywhere else.
- Components read the theme through `cx.theme()` when they render. Call sites pass semantic `Color` values, not raw colors.
- Hover, press, and selection are translucent white washes over the parent surface, so one state color works on the rail, the sidebar, and popovers.
- Interactive components accept `debug_selector`, so GPUI simulated-input tests can find them.
- Disabled controls ignore clicks and show a not-allowed cursor.

## Contents

| Item | Use |
| --- | --- |
| `Theme`, `Colors`, `Fonts`, `ActiveTheme` | The installed palette and fonts. `keystone_ui::init` installs the dark theme. |
| `Color`, `TextSize`, `Elevation`, `spacing`, `radius` | Semantic tokens. Prefer these to raw pixels and colors. |
| `StyledExt` | `surface(elevation, cx)` for backgrounds, borders, and shadows, and `hairline_*` for separators. |
| `IconName`, `Icon`, `Icons` | Typed icon names and the asset source that serves them. |
| `Label`, `Kbd` | Text with a size, a color, and optional truncation or monospace. `Kbd` shows a shortcut hint. |
| `Button`, `IconButton` | `ButtonStyle::{Ghost, Subtle, Filled}`, `ButtonSize::{Compact, Default, Large}`, selected and disabled states, and tooltips. |
| `ListItem` | A selectable row for lists such as the session sidebar. |
| `PopoverMenu`, `MenuItem`, `MenuSeparator` | An anchored overlay menu. The caller owns the open state. |
| `Tooltip` | A hover card with an optional shortcut. |

Run `cargo run -p keystone-ui --example gallery` to see every component in every state.

## Adding a component

Write a `RenderOnce` struct with builder methods, read colors from `cx.theme()`, and add it to the prelude and the gallery. Add a behavior test in `crates/keystone-ui/tests/components.rs` when the component handles input.

## Sources

The component shapes follow Zed's `ui` crate, and the visual values follow Zeron. Zed's UI crates are licensed GPL-3.0-or-later, so no Zed code was copied. Zeron is MIT-licensed, and only design values were adapted from it.

## Known limits

GPUI 0.2.2 cannot render a reliable text ellipsis, so `Label::truncate` clips. See KS-010 in [the issue list](issues.md). Light theme, density, and font choice are in scope but not built. The token names are semantic, so each can be added without changing call sites.
