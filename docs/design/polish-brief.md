# Polish brief: night harbor

The visual contract for the next styling pass on the current desktop UI. It covers only what the app already renders: the welcome screen, a conversation while it streams, an approval, the session sidebar with search and Settled, the settings menu, the approval mode menu, and the Full access confirmation. Future pages in the Paper file are concepts and are not part of this contract.

Source of truth: the Paper file [Keystone | Product design](). Its tokens carry the same names and values as the tables below. Screenshots of each artboard live in [`product-design/`](product-design/).

## Boards

All boards are 1280 × 800, the design reference size; native verification also covers 1280 × 900 and 1024 × 768.

| Page | Board | Screenshot |
| --- | --- | --- |
| 00 · Foundations | 00 · Night harbor tokens | [00-foundations.jpg](product-design/00-foundations.jpg) |
| 01 · Current app | 01 · Welcome · first run | [01-welcome.jpg](product-design/01-welcome.jpg) |
| | 02 · Conversation · streaming | [02-conversation-streaming.jpg](product-design/02-conversation-streaming.jpg) |
| | 03 · Approval | [03-approval.jpg](product-design/03-approval.jpg) |
| | 04 · Search · Settled open | [04-search-settled.jpg](product-design/04-search-settled.jpg) |
| | 05 · Settings menu | [05-settings-menu.jpg](product-design/05-settings-menu.jpg) |
| | 06 · Approval mode menu | [06-approval-mode-menu.jpg](product-design/06-approval-mode-menu.jpg) |
| | 07 · Full access confirm | [07-full-access-confirm.jpg](product-design/07-full-access-confirm.jpg) |
| | 08 · Rename session | [08-rename-session.jpg](product-design/08-rename-session.jpg) |
| | 08b · Rename error | [08b-rename-session-error.jpg](product-design/08b-rename-session-error.jpg) |
| 02 · Future concepts | 10 · Concept · Project and files | [10-concept-project-files.jpg](product-design/10-concept-project-files.jpg) |
| | 11 · Concept · Git and pull request | [11-concept-git-pr.jpg](product-design/11-concept-git-pr.jpg) |
| | 12 · Concept · Prompts and integrations | [12-concept-prompts-integrations.jpg](product-design/12-concept-prompts-integrations.jpg) |

Copy in the mocks is sample content. Where a label or hint differs from the source, such as the shortened composer status in board 02, the source wins.

## Direction

Neutral gray surfaces and neutral text; blue belongs to components. Blue appears in four places only: the Send button, the one primary action in an approval, toggles and checked menu items, and the selected row. Everything else is neutral. Layout keeps today's proportions: the 302 px sidebar, the 640 px transcript and composer, the 44 px bars.

## Dark palette

Every value below replaces the matching field in `Colors::dark()` in `crates/keystone-ui/src/theme.rs`. Surfaces, borders, washes, and text are neutral grays; washes stay translucent white, as the design system requires. Blue is reserved for components: primary fills, the selected wash, focus, accent marks, and text selection.

| Field | Today | New | Note |
| --- | --- | --- | --- |
| `background` | `#131313` | `#141414` | Main column. |
| `sidebar` | `#171717` | `#0D0D0D` | Now a step darker than the main column. |
| `surface` | `#1E1E1E` | `#181818` | |
| `surface_raised` | `#232323` | `#1E1E1E` | Command cards, approval card. |
| `overlay` | `#252525` | `#232323` | Popovers, dialogs. |
| `composer` | `#232324` | `#1C1C1C` | |
| `composer_border` | white 7% | white 8% | Same as `border`. |
| `composer_shadow` | black 35% | black 22% | Softer lift; 40% read as a dark halo. |
| `element_hover` | white 6% | white 5% | |
| `element_active` | white 12% | white 10% | |
| `element_selected` | white 9% | `rgba(130,165,230,0.13)` | Blue-tinted. Selected row, selected tab, open menu button. |
| `element_filled` | white 12% | white 9% | User bubble, Stop, Settle, welcome pills. |
| `element_filled_hover` | white 18% | white 15% | |
| `element_disabled` | white 10% | white 10% | Send with an empty draft. |
| `element_inverse` | `#E6E6E6` | `#4470C0` | Send and primary fill. Now blue. |
| `border` | white 8% | white 8% | |
| `border_strong` | white 14% | white 14% | |
| `border_focused` | `#467BB0` | `#5B87D6` | Focus ring. |
| `shadow` | black 45% | black 45% | |
| `scrim` | black 50% | black 50% | |
| `text` | `#E6E6E6` | `#E6E6E6` | Neutral off-white. |
| `text_strong` | `#F2F2F2` | `#F2F2F2` | |
| `text_muted` | `#9C9C9C` | `#A3A3A3` | |
| `text_faint` | `#6E6E6E` | `#858585` | Times, section headers, Ctrl F hints. 5.3:1 on the sidebar. |
| `text_placeholder` | `#8F8F8F` | `#A3A3A3` | Same as muted, so empty fields stay readable. |
| `text_inverse` | `#131313` | `#FFFFFF` | Icon and label on Send and primary buttons. |
| `accent` | `#6B9BD2` | `#6F98DE` | Working wedge, keystone, checks, streaming dot. |
| `danger` | `#EA9990` | `#EC9A8F` | |
| `success` | `#8FC79A` | `#8FC79A` | Unchanged. |
| `warning` | `#E0B86A` | `#E2BA6E` | |
| `provider_claude` | `#D9805F` | `#D9805F` | Unchanged. |
| `provider_codex` | `#93A6E0` | `#93A6E0` | Unchanged. |
| `code_text` | `#82A9CE` | `#B8B8B8` | Neutral, so only components carry blue. |
| `selection` | `#467BB0` at 44% | `#5B87D670` (43.9%) | |
| `caret` | `#E6E6E6` | `#E6E6E6` | |

Light uses neutral grays too (`background` `#FAFAFA`, `sidebar` `#F2F2F2`, `surface_raised` `#F6F6F6`, `code_text` `#555555`) and a blue selected-row wash (`#3A6EA518`). Primary actions share the same meaning in both themes: `element_inverse` becomes `#345FA8` and `text_inverse` stays `#FFFFFF`. White on `#345FA8` measures 6.3:1. Dark uses `#4470C0` because primary buttons such as Allow carry 12 px white labels: white on it measures 4.9:1, and the hover and press fades darken it on the dark overlay. In light mode the 94% hover and 88% pressed fills retain at least 4.8:1 on white. The earlier `#4E7BCF` measured 4.2:1.

## Type

Inter for the interface, GeistMono for commands and code, as today. The scale in `TextSize` does not change: 10, 11, 12, 13, 14, 24.

- Transcript body stays 14 px on a 22 px line, in `text`. Replies never use `text_strong`.
- The user bubble uses `text_strong`, so the user's own words read a step brighter than the reply.
- The welcome heading stays `TextSize::Title` semibold in `text_strong`.
- The sidebar wordmark "keystone" uses `text_strong` at `TextSize::Body` semibold.
- Section headers in the sidebar stay 11 px medium `text_faint`.

## Spacing and radius

No new tokens. `spacing` stays 4, 8, 12, 16, 24. `radius` stays 6, 7, 8, 10, 12, 18. The mocks use these exact values.

## Component changes

These are the visual component changes beyond the palette; behavioral changes are recorded in the verification report.

1. **User bubble.** `transcript.rs` fills the bubble with `element_filled` instead of `element_selected`, so it stays neutral now that selection is blue. Text color `text_strong`.
2. **Welcome pills.** `welcome_pill` fills with `element_filled` and hovers to `element_filled_hover`, for the same reason.
3. **Primary button.** Add `ButtonStyle::Primary`: background `element_inverse`, hover at 94% opacity, pressed at 88% opacity, label and icon `text_inverse`. Use it for Allow in the approval card. Send keeps its own round 32 px element with the same colors.
4. **Approval card.** Keep `Elevation::Raised`. Replace the `border_focused` border with `border_strong`, so the blue Allow is the only blue in the card. Header is "{Provider} asks to run" in `TextSize::Ui` muted. Command uses mono `TextSize::Ui` (12 px) in the default text color. Detail in `TextSize::Sm` muted. Buttons Allow (Primary) then Deny (Subtle).
5. **Working status.** "Working…" gets a 6 px `accent` dot before it, with an 8 px gap.
6. **Command card.** Exit status label uses `success` for exit 0 and `danger` for a non-zero exit. "Running" stays muted.
7. **Checked menu items.** The check in `MenuItem::checked` uses `accent`.

Nothing else changes: row heights, density behavior, paddings, the 302 px sidebar, and the 640 px content width stay as they are.

## Not in this pass

The Paper file also shows concept pages for project files, Git and pull requests, and saved prompts with integrations. They are labelled "Concept" on the canvas. Do not build them from this brief.
