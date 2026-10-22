# Settings

Change the theme, fonts, density, and text size, and keep the choice across restarts.

## Sub-features

- `settings-open` opens the page with Ctrl+Comma, the sidebar's cog, or the palette's "Open settings", and closes it with Back, the page's cog, Escape, or Ctrl+Comma again.
- `settings-sections` switches between the Appearance, Fonts, Notifications and Accounts sections. Notifications is covered in [notifications](notifications.md).
- `settings-theme` switches between System, Light, and Dark.
- `settings-font` picks the interface font and the code font, each with "Default" first.
- `settings-density` picks Compact, Default, or Spacious, by click or by keyboard.
- `settings-persist` saves every choice in the workspace `preferences` and restores it after a restart.

## How to get to it (user POV)

- Press Ctrl+Comma, or click the cog at the bottom left of the sidebar. The settings page takes the place of the workspace below the top bar, whose breadcrumb reads **keystone / Settings**.
- The page's left column, as wide as the sidebar, lists Back, then the sections. The selected section is highlighted, and the cog stays at the bottom left, shown selected.
- Appearance holds Theme, Density, and Text size. Fonts holds Interface font and Code font.
- Tab and Shift+Tab move through Back, the sections, then the shown section's controls. Up and Down on a focused section move the selection. Space or Enter activates a focused row or button. Left and Right move a focused density choice, and change the text size on the slider. Tab in an open menu closes it and moves on.
- Escape closes an open menu first, then the page. Focus returns to the composer, or to the window on the welcome screen.

## Driving it with keystone-verify

- Call `input` with `{"actions":[{"action":"keypress","keys":["ctrl","comma"]}]}`. `wait_for` `{"id":"settings-page"}`. Close it with `click` `{"id":"settings-back"}`, reopen with `click` `{"id":"settings"}`, and require the cog to show selected and keep its place.
- `click` `{"id":"settings-theme"}`, then `{"id":"settings-theme-option-1"}` for Light. Capture and require the light palette. Option 0 is System, option 2 Dark.
- `click` `{"id":"density-compact"}`. Require the cog to shrink at once (28 px compact, 32 px default), and the sidebar rows after closing.
- `click` the right end of `{"id":"settings-text-size"}` for 16 px. Require larger text and no clipped rows.
- `click` `{"id":"settings-section-fonts"}`. `click` `{"id":"settings-ui-font"}` or `{"id":"settings-mono-font"}`, then `{"id":"settings-ui-font-option-N"}`. Option 0 is Default. Require page text, or inline code in a reply for the code font, to change.
- Read `workspace` and require `preferences` to hold `theme`, `density`, `ui_font`, `mono_font`, and `text_size`. Restart the app and require the same look.

## Gotchas

- The font menus list only installed candidates, plus a saved font that is no longer installed. A headless desktop may offer few choices.
- While the page is open, Ctrl+K still opens the palette over the page. A command aimed at the hidden workspace (new session, search, composer, rename, settle, sidebar, Files via Ctrl+Shift+E or Toggle files, Notes via Ctrl+Shift+J or Toggle notes, Customize) closes the page first, then runs. `settings_yields_to_workspace` in `settings_page.rs` lists them. Ctrl+Enter does nothing on the page.
- `text_size` and `mono_font` are absent from older workspaces and load as the defaults (13 px, the first installed code font).
