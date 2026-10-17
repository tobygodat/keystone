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
| `IconName`, `Icon`, `Icons` | Typed icon names and the asset source that serves them. Icons are drawn on a 24-unit grid and tinted with the text color. `Wedge` and `WedgeFilled` mark working and settled sessions, and a `WedgeFilled` in `Color::Warning` marks a session waiting for approval or with a failed turn, `Diamond` is the provider glyph, `Pencil` starts a session, and `ArchStones` with `ArchKeystone` stack into the arch mark so the keystone can take the accent color. Filled shapes set `fill='white'` on their own paths. |
| `Label`, `Kbd` | Text with a size, a color, and optional truncation or monospace. `truncate` keeps one line and ends text that does not fit with "…". `Kbd` shows a shortcut hint. |
| `Button`, `IconButton` | `ButtonStyle::{Ghost, Subtle, Filled}`, `ButtonSize::{Compact, Default, Large}`, selected and disabled states, and tooltips. `Button::color` marks a state that needs attention, such as full access. `track_focus` makes a button focusable, with a `border_focused` ring while it has focus; Enter or Space then presses it. |
| `ConfirmDialog`, `DialogFocus` | A modal question with Cancel and Confirm. See below. |
| `ListItem` | A selectable row for lists such as the session sidebar. `on_hover` reports the pointer entering and leaving, so a list can build a row action only for the row under the pointer. |
| `PopoverMenu`, `MenuItem`, `MenuHeader`, `MenuSeparator` | An anchored overlay menu. The caller owns the open state. `MenuItem::checked` marks one choice in a group, and `MenuItem::detail` adds a muted line that explains it. |
| `VirtualList`, `VirtualListState` | A keyboard-driven list that renders only its visible rows, for pickers, palettes, and trees. See below. |
| `Markdown`, `markdown::Document` | Agent replies. `Document::parse` turns CommonMark with tables and strikethrough into plain data; `Markdown` renders it. See below. |
| `TextInput`, `InputMode` | An editable text field. `InputMode::SingleLine` refuses newlines and emits `text_input::RejectedMultiline`, so search typing and paste stay on one line. `InputMode::Multiline` wraps, and arrows, Home, and End move by visual row. Every edit emits `text_input::Changed`. Call `TextInput::bind_keys` once at startup. Bound keys reach the input as actions before any key listener runs, so a host that needs Up, Down, or Enter first, such as a list or palette, wraps it in a div with `capture_action` for `text_input::Up`, `Down`, or `Newline` and stops propagation. |
| `Tooltip` | A hover card with an optional shortcut. |

Run `cargo run -p keystone-ui --example gallery` to see every component in every state. Its header switches between themes and densities.

## Markdown

`Document::parse` uses `pulldown-cmark` (MIT) with tables and strikethrough on. It never fails, so a host can render a reply at every streamed delta. An open code fence runs to the end of the text, so a streaming code block does not flicker back into paragraphs. Raw HTML shows as text.

`Markdown::new(id, Arc<Document>)` is one self-contained element, so a transcript row can host it. Body text inherits the surrounding text size and line height. Headings, emphasis, inline code, links, lists, quotes, tables with column alignment, and rules use theme tokens only. Fenced code is monospace in `code_text`, scrolls sideways instead of wrapping, and has a copy button that shows a check for a moment. Its debug selector is `{id}-copy-{n}`, counting code blocks from 1. Links open with `App::open_url` only for `http`, `https`, and `mailto`; agent text cannot launch other handlers. Hosts that redraw often should cache the parsed document. The desktop keeps one per reply row in the virtualized transcript and parses a row again only when its text changes, such as at each streamed delta.

## Confirm dialog

`ConfirmDialog::new(id, &focus, title, message)` asks before a risky change. `tone(Color)` colors its icon and Confirm, `Danger` by default. `on_cancel` and `on_confirm` take `Fn(&mut Window, &mut App)`. The caller owns whether it is open: create a `DialogFocus` when it opens, keep it while it shows, and drop it in both handlers.

- It draws over the whole window, menus included, on a `scrim` wash that takes every press outside the card.
- `DialogFocus::new` focuses Cancel, so Enter alone never confirms. Tab and Shift+Tab move between the two buttons, and Enter or Space presses the focused one.
- Cancel, Escape, and a press on the scrim call `on_cancel`. Only Confirm calls `on_confirm`. Each handler should return focus where it was, such as the composer.
- The card's key context is `ConfirmDialog::KEY_CONTEXT` (`Dialog`). Bind app shortcuts with `Some("!Dialog")` so they stay off while it is open.
- Debug selectors are `{id}` for the card, and `{id}-cancel` and `{id}-confirm` for the buttons.

The gallery's "Open confirm dialog" button shows it.

## Virtual list

`VirtualList::new(id, &state, render_row)` draws a list of any length over GPUI's `uniform_list`. It renders only the rows on screen, so frame cost follows the viewport, not the row count. `render_row(index, highlighted, window, cx)` builds one row. `MenuItem::highlighted` paints the same wash as hover, so a `MenuItem` is the usual row.

- The caller holds an `Entity<VirtualListState>` made with `VirtualListState::new(count, cx)`. Its highlight is `None` only when the list is empty, and otherwise always names a row in range. A new list highlights row 0.
- Up and Down move one row, PageUp and PageDown move one viewport, and Home and End jump to the ends. Moves clamp at the ends and scroll the highlight into view. The keys work while the list has focus, in key context `VirtualList`.
- Enter and a click call `on_confirm` with the row. Hover moves the highlight without scrolling.
- `set_count` keeps the highlight in range when the rows change, such as when a filter narrows them.
- Every row must have the same fixed height. The list measures row 0 and places the rest from it.
- Row debug selectors are `{id}-row-{index}`.
- `move_by`, `move_to`, `page`, and `set_count` take no `cx`. A palette or select that owns its own input can forward its keys to them, then call `cx.notify()`. The actions live in `keystone_ui::virtual_list` for hosts that bind their own keys.
- A list takes focus when clicked. `VirtualList::focusable(false)` leaves focus with the host, so a palette's input keeps it.
- The list redraws the view that drew it on every move, so it works inside a cached view.

The gallery's List page shows 50,000 rows.

## Adding a component

Write a `RenderOnce` struct with builder methods, read colors from `cx.theme()`, and add it to the prelude and the gallery. Add a behavior test in `crates/keystone-ui/tests/components.rs` when the component handles input.

## Sources

The component shapes follow Zed's `ui` crate, and the visual values follow Zeron. Zed's UI crates are licensed GPL-3.0-or-later, so no Zed code was copied. Zeron is MIT-licensed, and only design values were adapted from it.

## Known limits

GPUI 0.2.2's `text_ellipsis` truncates to the first width it measured, so `Label::truncate` uses its own element. It measures the text on every layout and cuts it to its final width with a trailing "…". See KS-010 in [the issue list](issues.md).

Text sizes do not change with density or font choice yet. The default fonts, Inter and GeistMono Nerd Font Mono, are not bundled. GPUI falls back to a system font when they are missing. The desktop picks the first installed monospace font from a short list, so code stays monospace. The interface font has no such list: on a machine without Inter, GPUI's fallback showed no bold or italic, so Markdown emphasis was invisible until an installed font such as Liberation Sans was chosen in settings.

A copy click before any key press in the window copies nothing on Wayland. See KS-013.
