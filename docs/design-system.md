# Design system

`crates/keystone-ui` holds Keystone's theme, tokens, icons, and shared controls. The desktop app builds its controls from these components. Layout containers in the app still use GPUI styling, with colors taken from theme tokens.

## Rules

- Colors live only in `crates/keystone-ui/src/theme.rs`. `scripts/check.sh` fails on GPUI color constructors such as `rgb(`, `hsla(`, or `white(` anywhere else.
- Components read the theme through `cx.theme()` when they render. Call sites pass semantic `Color` values, not raw colors.
- Hover, press, and selection are translucent washes over the parent surface, white in the dark theme and black in the light theme. One state color works on the rail, the sidebar, and popovers.
- Components size controls and rows through `cx.theme().density`, not fixed heights. Use `Density::control` for heights and `Density::pad` for row padding.
- Interactive components accept `debug_selector`, so GPUI simulated-input tests can find them.
- Disabled controls ignore clicks and show a not-allowed cursor.

## Contents

| Item | Use |
| --- | --- |
| `Theme`, `Colors`, `Fonts`, `ActiveTheme` | The installed palette, density, and fonts. `keystone_ui::init` installs the dark theme. `keystone_ui::set_theme` replaces it and redraws every window. |
| `Appearance`, `Density` | `Light` or `Dark`, and `Compact`, `Default`, or `Spacious`. `Theme::new` combines them with `Fonts`. |
| `Color`, `TextSize`, `Elevation`, `spacing`, `radius` | Semantic tokens. Prefer these to raw pixels and colors. |
| `StyledExt` | `surface(elevation, cx)` for backgrounds, borders, and shadows, and `hairline_*` for separators. |
| `IconName`, `Icon`, `Icons` | Typed icon names and the asset source that serves them. |
| `Label`, `Kbd` | Text with a size, a color, and optional truncation or monospace. `Kbd` shows a shortcut hint. |
| `Button`, `IconButton` | `ButtonStyle::{Ghost, Subtle, Filled}`, `ButtonSize::{Compact, Default, Large}`, selected and disabled states, and tooltips. `Button::color` marks a state that needs attention, such as full access. |
| `ListItem` | A selectable row for lists such as the session sidebar. |
| `PopoverMenu`, `MenuItem`, `MenuHeader`, `MenuSeparator` | An anchored overlay menu. The caller owns the open state. `MenuItem::checked` marks one choice in a group, and `MenuItem::detail` adds a muted line that explains it. |
| `Markdown`, `markdown::Document` | Agent replies. `Document::parse` turns CommonMark with tables and strikethrough into plain data; `Markdown` renders it. See below. |
| `Tooltip` | A hover card with an optional shortcut. |

Run `cargo run -p keystone-ui --example gallery` to see every component in every state. Its header switches between themes and densities.

## Markdown

`Document::parse` uses `pulldown-cmark` (MIT) with tables and strikethrough on. It never fails, so a host can render a reply at every streamed delta. An open code fence runs to the end of the text, so a streaming code block does not flicker back into paragraphs. Raw HTML shows as text.

`Markdown::new(id, Arc<Document>)` is one self-contained element, so a transcript row can host it. Body text inherits the surrounding text size and line height. Headings, emphasis, inline code, links, lists, quotes, tables with column alignment, and rules use theme tokens only. Fenced code is monospace in `code_text`, scrolls sideways instead of wrapping, and has a copy button that shows a check for a moment. Its debug selector is `{id}-copy-{n}`, counting code blocks from 1. Links open with `App::open_url` only for `http`, `https`, and `mailto`; agent text cannot launch other handlers. Hosts that redraw often should cache the parsed document. The desktop keeps one per reply row in the virtualized transcript and parses a row again only when its text changes, such as at each streamed delta.

## Adding a component

Write a `RenderOnce` struct with builder methods, read colors from `cx.theme()`, and add it to the prelude and the gallery. Add a behavior test in `crates/keystone-ui/tests/components.rs` when the component handles input.

## Sources

The component shapes follow Zed's `ui` crate, and the visual values follow Zeron. Zed's UI crates are licensed GPL-3.0-or-later, so no Zed code was copied. Zeron is MIT-licensed, and only design values were adapted from it.

## Known limits

GPUI 0.2.2 cannot render a reliable text ellipsis, so `Label::truncate` clips. See KS-010 in [the issue list](issues.md).

Text sizes do not change with density or font choice yet. The default fonts, Inter and GeistMono Nerd Font Mono, are not bundled. GPUI falls back to a system font when they are missing. The desktop picks the first installed monospace font from a short list, so code stays monospace. The interface font has no such list: on a machine without Inter, GPUI's fallback showed no bold or italic, so Markdown emphasis was invisible until an installed font such as Liberation Sans was chosen in settings.

A copy click before any key press in the window copies nothing on Wayland. See KS-013.
