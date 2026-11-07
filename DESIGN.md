# Design system: Keystone

`crates/keystone-ui` holds Keystone's theme, tokens, icons, and shared controls. The desktop app builds its controls from these components. Layout containers in the app still use GPUI styling, with colors taken from theme tokens.

## Rules

- Colors live only in `crates/keystone-ui/src/theme.rs`. `scripts/check.sh` fails on GPUI color constructors such as `rgb(`, `hsla(`, or `white(` anywhere else.
- Components read the theme through `cx.theme()` when they render. Call sites pass semantic `Color` values, not raw colors.
- Surfaces, borders, and text are neutral in both themes. Surfaces sit close in value, and hairlines separate them. Text is ink in three tiers: `text`, `text_muted` and `text_faint`.
- The primary action (Send, Allow, `ButtonStyle::Primary`) is filled with ink, `element_inverse` with `text_inverse`. No interface element is filled with blue. Blue is for focus rings, links and text selection.
- Status colors mark state. Put them on a mark, such as an icon or a dot, rather than on whole runs of text.
- Sidebar rows: an unseen reply shows a dot and a bright title (`Color::Strong`) with no line under it. A session waiting on the user keeps a `text` title and an amber reason under it. A working session's wedge is ink, and so is the keystone in the wordmark's arch. Everything else has a `text_muted` title. Settle shows as an icon button on the hovered or selected row.
- Hover and press use translucent neutral washes. One state color works on the sidebar, the main column, the composer, and popovers.
- `Elevation::Raised` is a fill with no hairline, for blocks inside content such as code and tables. A raised block that needs emphasis, such as an approval, adds its own border.
- Components size controls and rows through `cx.theme().density`, not fixed heights. Use `Density::control` for heights and `Density::pad` for row padding. Pass the Spacious size as the base: `control` keeps it at Spacious, takes seven eighths at Default and three quarters at Compact, rounds to an even number of pixels and never goes below 20 px. `pad` rounds to whole pixels. Space between and around blocks of content, such as transcript rows and the composer's padding, goes through `Density::space`. Pass the Default size, on the 4 px grid, as the base: it takes three quarters at Compact and five quarters at Spacious, rounded to the 4 px grid.
- Interface text sits on whole pixels. `TextSize::line_height` is the size times 1.4 (1.25 for titles), rounded to an even number of pixels. `Label` sets it, and the window root sets Base's, so rows of interface text do not drift by fractions of a pixel. The transcript, the composer, `TextInput` and Markdown set their own whole-pixel line heights.
- Interactive components accept `debug_selector`, so GPUI simulated-input tests can find them.
- Disabled controls ignore clicks and show a not-allowed cursor.

## Contents

| Item | Use |
| --- | --- |
| `Theme`, `Colors`, `Fonts`, `ActiveTheme` | The installed palette, density, and fonts. `keystone_ui::init` installs the dark theme. `keystone_ui::set_theme` replaces it and redraws every window. `Theme::transcript_text` gives the transcript's text size and line height: `transcript_size` when set, else `TextSize::Body`, on a whole-pixel line about 11:7 of it (22 px at 14). |
| `ActionColor` | The primary action's fill: `Ink`, the default, or one of five hues with white text. `Theme::with_action` applies it to `element_inverse` and `text_inverse`. Each keeps its label at 4.5:1 or more in both themes, at rest and at the primary button's hover and press opacity over every surface. In the transcript, Markdown code and tables, a command line and its output follow `Theme::transcript_code_text`. |
| `Appearance`, `Density` | `Light` or `Dark`, and `Compact`, `Default`, or `Spacious`. `Theme::new` combines them with `Fonts`. |
| `Color`, `TextSize`, `Elevation`, `spacing`, `radius` | Semantic tokens. Prefer these to raw pixels and colors. `Color::Strong` is the brightest text, for headings, selected titles, and draft text. Radii are 6 px for controls and menu rows, 8 px for rows and panels, and 12 px for popovers and the composer (`radius::COMPOSER`). `TextSize::px` scales with `Theme::text_scale`. |
| Shell colors | `sidebar` fills the left column. `composer`, `composer_border`, and `composer_shadow` style the floating composer. `element_inverse` with `text_inverse` is the ink-filled primary action (Send, Allow), kept at 4.5:1 or more for 12 px labels in both themes; `element_disabled` is its disabled wash. `diff_added` and `diff_removed` wash a diff's added and removed lines, whose `+` and `-` marks carry `success` and `danger`. `provider_claude` and `provider_codex` tint provider glyphs; the desktop app maps `Provider` to them, so keystone-ui stays provider-agnostic. `tiles` holds seven `Tint`s, a fill and the text on it, in the order of `ProjectColor::ALL`; each keeps its text at 4.5:1 or more and its fill at 3:1 or more against `sidebar`, `overlay` and `background`. |
| `StyledExt` | `surface(elevation, cx)` for backgrounds, borders, and shadows, and `hairline_*` for separators. |
| `IconName`, `Icon`, `Icons` | Typed icon names and the asset source that serves them. Icons are drawn on a 24-unit grid for 16 px and tinted with the text color. At 16 px a unit is two thirds of a pixel, so the 1.5-unit stroke is 1 px, and a straight stroke on 0.75 + 1.5k units (0.75, 2.25, 3.75 … 23.25) covers one whole pixel column or row. Draw icons with straight strokes at 16 px; at 14 or 15 px every straight stroke straddles two pixels and looks soft. A 1 px stroke cannot sit on the centre of an even box, so a symmetric icon such as Plus sits half a pixel left or up. `Wedge` and `WedgeFilled` mark working and settled sessions, and a `WedgeFilled` in `Color::Warning` marks a session waiting for approval or with a failed turn, `Diamond` is the provider glyph, `Pencil` starts a session, and `ArchStones` with `ArchKeystone` stack into the arch mark so the keystone can take its own color (bright ink, `Color::Strong`). Filled shapes set `fill='white'` on their own paths. |
| `Monogram` | A name's first letter or digit on one of the `tiles`, for telling projects apart. `PaletteItem::monogram` puts one before a palette row's label. |
| `Label`, `Kbd` | Text with a size, a color, and optional truncation or monospace. `truncate` keeps one line and ends text that does not fit with "…". `Kbd` shows a shortcut hint. |
| `Button`, `IconButton`, `Swatch` | `Swatch` is a square button holding a color dot, ringed while selected, for picking a color; give it a tooltip and an `aria_label` with the color's name. `ButtonStyle::{Ghost, Subtle, Filled, Primary}`, `ButtonSize::{Compact, Default, Large}`, selected and disabled states, and tooltips. `Primary` uses the ink action fill, `element_inverse`, and its `text_inverse` foreground. `Button::color` marks a state that needs attention, such as full access. `track_focus` makes a button focusable, with a `border_focused` ring while it has focus; Enter or Space then presses it. |
| `ConfirmDialog`, `DialogFocus` | A modal question with Cancel and Confirm. See below. |
| `ListItem` | A selectable row for lists such as the session sidebar. `on_hover` reports the pointer entering and leaving, so a list can build a row action only for the row under the pointer. `track_focus` makes it a focusable row with a ring that Enter or Space clicks. |
| `PopoverMenu`, `MenuItem`, `MenuHeader`, `MenuSeparator` | An anchored overlay menu. The caller owns the open state. `MenuItem::checked` marks one choice in a group, and `MenuItem::detail` adds a muted line that explains it. |
| `Switch`, `Slider`, `Select` | Settings controls: an on/off toggle, a value on a stepped range, and one choice from a list. See below. |
| `VirtualList`, `VirtualListState` | A keyboard-driven list that renders only its visible rows, for pickers, palettes, and trees. See below. |
| `CommandPalette`, `CommandPaletteState`, `PaletteItem`, `fuzzy` | A centered overlay that filters commands as the user types, and the fuzzy scorer behind it. See below. |
| `Tree`, `TreeState`, `TreeModel` | A keyboard-driven tree of any size that loads children lazily and draws only its visible rows. See below. |
| `Markdown`, `markdown::Document` | Agent replies. `Document::parse` turns CommonMark with tables and strikethrough into plain data; `Markdown` renders it. See below. |
| `TextInput`, `InputMode` | An editable text field. `InputMode::SingleLine` propagates the Enter action to its container (for submit), refuses inserted newlines and emits `text_input::RejectedMultiline`, so search typing and paste stay on one line. `InputMode::Multiline` wraps, and arrows, Home, and End move by visual row. Every edit emits `text_input::Changed`. Call `TextInput::bind_keys` once at startup. Bound keys reach the input as actions before any key listener runs, so a host that needs Up, Down, or Enter first, such as a list or palette, wraps it in a div with `capture_action` for `text_input::Up`, `Down`, or `Newline` and stops propagation. |
| `Tooltip` | A hover card with an optional shortcut. |
| `Split`, `SplitState`, `PaneEdge`, `Edge` | Resizable panes with draggable dividers and pane-edge drop zones. See below. |
| `TabBar`, `Tab`, `TabDot`, `DraggedTab` | A row of tabs that scrolls sideways, with close, a status dot, and drag reorder. See below. |

Run `cargo run -p keystone-ui --example gallery` to see every component in every state. Its header switches between themes and densities.

## Markdown

`Document::parse` uses `pulldown-cmark` (MIT) with tables and strikethrough on. It never fails, so a host can render a reply at every streamed delta. An open code fence runs to the end of the text, so a streaming code block does not flicker back into paragraphs. Raw HTML shows as text.

`Markdown::new(id, Arc<Document>)` is one self-contained element, so a transcript row can host it. Body text inherits the surrounding text size and line height. Headings, emphasis, inline code, links, lists, quotes, tables with column alignment, and rules use theme tokens only. Fenced code is monospace in `code_text`, scrolls sideways instead of wrapping, and has a copy button that shows a check for a moment. Its debug selector is `{id}-copy-{n}`, counting code blocks from 1. Links open with `App::open_url` only for `http`, `https`, and `mailto`; agent text cannot launch other handlers. Hosts that redraw often should cache the parsed document. The desktop keeps one per reply row in the virtualized transcript and parses a row again only when its shown text changes. A streaming reply shows each burst over a few frames, catching up within 120 ms, and the parser takes each longer prefix as an append.

## Confirm dialog

`ConfirmDialog::new(id, &focus, title, message)` asks before a risky change. `tone(Color)` colors its icon and Confirm, `Danger` by default. `on_cancel` and `on_confirm` take `Fn(&mut Window, &mut App)`. The caller owns whether it is open: create a `DialogFocus` when it opens, keep it while it shows, and drop it in both handlers.

- It draws over the whole window, menus included, on a `scrim` wash that takes every press outside the card.
- `DialogFocus::new` focuses Cancel, so Enter alone never confirms. Tab and Shift+Tab move between the two buttons, and Enter or Space presses the focused one. `DialogFocus::with_body(handle, window, cx)` initially focuses a body input and cycles through it, Cancel and Confirm in both directions. `body`, `icon` and `confirm_label` customize the content; display operation errors inside the body so they remain visible above the scrim.
- Cancel, Escape, and a press on the scrim call `on_cancel`. Only Confirm calls `on_confirm`. Each handler should return focus where it was, such as the composer.
- The card's key context is `ConfirmDialog::KEY_CONTEXT` (`Dialog`). Bind app shortcuts with `Some("!Dialog")` so they stay off while it is open.
- Debug selectors are `{id}` for the card, and `{id}-cancel` and `{id}-confirm` for the buttons.

The gallery's "Open confirm dialog" button shows it.

## Tab bar

`TabBar::new(id)` takes `Tab::new(id, label)` children and `selected(index)`. The caller owns the order and the selection and changes them in the handlers. A tab's id keeps its hover state with it when the bar reorders.

- `on_select` gets the pressed tab's index. `on_close` gets the index from the close button or a middle click, and a bar without it shows no close buttons. The selected tab shows its close button at rest. Other tabs, and a selected tab with a dot, show it on hover.
- `Tab::dot(TabDot::Working)` draws an accent dot in the close button's slot, and `TabDot::Dirty` a muted one. When the bar has `on_close`, hovering the tab swaps the dot for the close button.
- The bar scrolls sideways when the tabs overflow, and scrolls the selected tab into view whenever the selection changes.
- `on_move` makes tabs draggable. A drop on another tab of the same bar calls it with `(from, to)`: removing the tab at `from` and inserting it at `to` gives the new order. A drop on the empty end moves the tab last. While a tab hovers over a slot, an accent marker shows on the side it will land. A release anywhere else changes nothing.
- `on_insert` takes tabs dragged from another bar, as `(DraggedTab, to)`. A drop on a tab inserts before it, and a drop on the empty end appends. A bar with `on_move` or `on_insert` ends every `DraggedTab` drag released over it, because GPUI hands a drop to the first target under the pointer, so a target around the bar never sees it.
- The drag payload is `DraggedTab { bar_id, index }`. Other drop targets, such as split pane edges, accept it with `on_drop::<DraggedTab>`.
- Ctrl+Tab and Ctrl+Shift+Tab belong to the host, which binds them to change `selected`.
- Debug selectors are `{id}` for the bar, and `{id}-tab-{n}` and `{id}-tab-{n}-close`, counting from 0.

The gallery's tab bar section has eight tabs and an "Add tab" button that makes the bar overflow.

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

## Switch, slider, and select

The caller owns each control's value and stores what `on_change` asks for. Each control takes focus in Tab order and shows a `border_focused` ring while it has it. `disabled(true)` drops the handler and focus, fades the control, and shows a not-allowed cursor. On and chosen states use the neutral selection tokens, not the accent.

- `Switch::new(id, on)` asks for `!on` on a click, or on Space or Enter while focused. `label` puts text to its right. On is a `selection_edge` track with a `background` thumb.
- `Slider::new(id, value, range, step)` reports values snapped to `step` within `range`. A press on the track or a drag picks the value under the pointer. Left and Down step down, Right and Up step up, and Home and End jump to the ends. The fill and thumb are `selection_edge` on a `selection_fill` track.
- `Select::new(id, options, selected)` shows the chosen label, or `placeholder` while `selected` is `None`. A press, or Enter, Space, Up, or Down on the focused trigger, opens a `PopoverMenu` whose rows are a `VirtualList` of `MenuItem`s, so a long list stays cheap. The menu opens with the choice highlighted and checked, and shows eight rows before it scrolls. Enter or a click chooses and refocuses the trigger. A letter jumps to the next option that starts with it, wrapping. Escape closes the menu and refocuses the trigger. Tab and Shift+Tab close it, refocus the trigger, and go on to the window's own Tab handling, so focus moves on. A press outside closes it.
- Debug selectors are `{id}` for each control, `{id}-thumb` for the slider thumb, and `{id}-option-{n}` for select options, counting from 0.

The gallery's Controls page shows every state, including a 500-option select.

## Tree

`TreeModel` holds every node in one flat arena and the rows on screen in one flat `visible` vector. `TreeModel::new(roots)` takes `(name, NodeKind)` pairs, and `set_children(folder, children)` fills a folder and returns the new children's ids, which stay contiguous. The host keeps `Entity<TreeState>` made with `TreeState::new(model, cx)` and renders `Tree::new(id, &state, render_row)`, which draws the visible rows through `VirtualList`.

- Expanding a folder splices its visible descendants into `visible` in one step, and collapsing drains them. Neither walks more of the tree than the rows it adds or removes, and nothing walks the whole tree per frame. Nested folders keep their expanded state while a parent is collapsed.
- A folder whose children were never set shows a "Loading…" row when it expands and calls `on_expand(node_id)` once. The host loads the children and calls `TreeState::set_children`, which replaces the Loading row.
- Up, Down, PageUp, PageDown, Home, and End work as in `VirtualList`. Left collapses an expanded folder, and otherwise moves to the parent. Right expands a collapsed folder, and otherwise moves to its first child. Enter and a row click open a file through `on_open(node_id)` and toggle a folder. A chevron click toggles without bubbling to the row. Left and Right are bound in key context `Tree`.
- Each row is one flat element. The tree indents it by depth with padding, draws the chevron, and appends `render_row(&TreeEntry, highlighted, window, cx)`, which usually draws an icon and the name. Rows are one height, set by density, and the highlighted row uses `selection_fill`.
- The highlight follows its row through every expand and collapse. A row that a collapse hides hands the highlight to the collapsed folder.
- Every change, including a `set_children` that lands after a load, redraws the view that last drew the tree, so it works inside a cached view. Draw one `TreeState` in one view only, since its list's scroll position is shared.
- Debug selectors are `{id}-row-{index}` and `{id}-chevron-{index}`.

The gallery's Tree page shows a 50,000-node tree with a 10,000-file folder, nested folders, and a lazy folder that loads after a moment.

## Split

`SplitState::new(axis, panes)` holds each pane's share of the container as fractions that sum to 1, and `min_px` sets the shortest a pane may get, 120 px by default. The host keeps it in an `Entity` and renders `Split::new(id, &state)` with one child per pane. `set_fractions` restores saved shares and rejects any that are not finite and positive.

- Dragging a divider moves only the two panes beside it, and both stop at `min_px`. When two neighbours together are shorter than twice `min_px`, their divider stays at the midpoint. A double-click on a divider returns every pane to an equal share.
- `on_drop_edge::<T>(Fn(&T, PaneEdge, &mut Window, &mut App))` accepts a dragged `T` on a pane. `PaneEdge` names the pane and the `Edge` nearest the pointer in pixels. While a `T` hovers, the half of that pane on that edge shows a `selection` wash with a `border_focused` outline.
- Panes are absolute children of one container, positioned by fraction, so a split adds no nested flex layout.
- Panes keep their fractions when the window shrinks, so a window narrower than every pane's `min_px` together shows shorter panes.
- Debug selectors are `{id}-pane-{n}` and `{id}-divider-{n}`, counting from 0, and `{id}-drop-target` for the highlight.

The gallery's header button "Split" opens a three-pane page with an axis toggle and a draggable tab.

## Command palette

`CommandPalette::new(id, &state)` draws a centered overlay over the whole window that filters commands as the user types. The caller holds an `Entity<CommandPaletteState>` made with `CommandPaletteState::new(items, cx)`, creates it when the palette opens, focuses it with `window.focus(&state.focus_handle(cx), cx)`, and drops it in both handlers. `PaletteItem::new(id, label)` takes an optional `detail`, `kbd` and `monogram`.

- The query is a single-line `TextInput`. Every edit rescores the labels with `fuzzy::score`. An empty query shows every item in its original order. Otherwise the best score comes first, then the shorter label, then the original order. A new query highlights the first row.
- Matched characters are bold and drawn in `Color::Accent`. `Color::Strong` is too close to body text to read, and the fallback font may lack bold.
- The input keeps focus throughout. A div around it catches `text_input::Up`, `Down`, and `Newline` with `capture_action`, so Up and Down move the highlight and Enter calls `on_confirm` with the highlighted item. PageUp and PageDown move a page. A click on a row also confirms it.
- Escape and a press outside the card call `on_dismiss`. The card's key context is `CommandPalette`, and `keystone_ui::init` binds its keys.
- Rows come from a `VirtualList`, so 10,000 items cost no more to draw than ten. At most eight rows show at once. With no match, the card says "No matching commands".
- `set_query` replaces the query and filters as if it had been typed. `matches` and `highlighted_item` read the result.
- It moves the highlight by notifying the view that drew it, so it works inside a cached view.
- Debug selectors are `{id}` for the card, `{id}-input` for the input, and `{id}-row-{n}` for the rows.

`fuzzy::score(query, candidate)` returns `None` unless the query's characters appear in the candidate in order, ignoring case. Otherwise it returns the score and the byte offset of each matched character. It rewards word starts, including camelCase humps and characters after `-`, `_`, `/`, or a space. It also rewards runs of adjacent matches and penalises gaps and a late first match. It picks the best of all alignments, not the first one found.

The gallery's Palette page opens a palette of 2,000 sample commands.

## Adding a component

Write a `RenderOnce` struct with builder methods, read colors from `cx.theme()`, and add it to the prelude and the gallery. Add a behavior test in `crates/keystone-ui/tests/components.rs` when the component handles input.

## Sources

The component shapes follow Zed's `ui` crate, and the visual values follow Zeron. Zed's UI crates are licensed GPL-3.0-or-later, so no Zed code was copied. Zeron is MIT-licensed, and only design values were adapted from it.

`split.rs` adapts the structure of gpui-component 0.7.3's `resizable.rs` and gpui-base 0.7.3's `resizable/` (Copyright 2024 - 2026 Longbridge, Apache-2.0), with modifications. No code was copied verbatim.

The tab bar's structure, a tracked horizontal scroll strip with a trailing empty slot, follows `tab/tab_bar.rs` and `tab/tab.rs` in [gpui-component](https://github.com/longbridge/gpui-kit) 0.7.3, Copyright 2024 - 2026 Longbridge, licensed Apache-2.0. `crates/keystone-ui/src/tabs.rs` is a modified, rewritten adaptation and says so in its header.

`switch.rs`, `slider.rs`, and `select.rs` follow the structure of `switch.rs`, `slider.rs`, and `select.rs` in gpui-component 0.7.3, Copyright 2024 - 2026 Longbridge, licensed Apache-2.0. Each is a modified, rewritten adaptation over Keystone's theme, `PopoverMenu`, and `VirtualList`, and says so in its header. No code was copied verbatim.

The tree's structure, a tree state that owns the rows beside a styled `Tree` element with Left, Right, and Enter in a `Tree` key context, follows `tree.rs` in gpui-base 0.7.3 and gpui-component 0.7.3 (Copyright 2024 - 2026 Longbridge, Apache-2.0). `crates/keystone-ui/src/tree.rs` is a modified, rewritten adaptation and says so in its header. Upstream rebuilds its entry list on every toggle, and this one splices.

## Known limits

Known limits are Linear issues: the `Label::truncate` ellipsis workaround and click-only copy on Wayland are in TOB-212, and text size, font and System theme gaps are in TOB-216. The visual direction behind these rules is [ADR 0011](docs/adr/0011-quiet-ink.md), which supersedes [ADR 0009](docs/adr/0009-visual-direction.md), and the component choices are [ADR 0008](docs/adr/0008-ui-component-choices.md).
