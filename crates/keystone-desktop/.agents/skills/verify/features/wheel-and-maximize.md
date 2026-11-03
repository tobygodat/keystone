# Native wheel and maximize acceptance

Build the pinned driver with the headless, wheel, atspi, packed-rgb and nvidia patches in [setup](../setup.md). Run `python3 scripts/test_native_driver.py` for wrapper regressions and the driver's own test suite (setup's `cargo test`, whose wheel test comes from the wheel patch) for signed, fractional and accumulated axis distances. Launch the exact app build through `native-driver.py --driver PATH --binary PATH --trace-wayland`; retain doctor, SHA/hash, requests, traces, state and native screenshots. Never replace an executable another owner is using.

## Two occurrences

Use a disposable scripted-provider session and long history. A labeled synthetic history may be loaded only while the tracked app has quit with code 0 and no window remains. Record that fixture boundary; it does not prove user-created history. Relaunch and duplicate the conversation through native Customize controls, Apply, then close Customize. Keep a draft through this arrangement.

Read each occurrence's fresh named center. Run `wheel` upward and downward over each, recording visible row IDs, absolute bounds and clipped bounds before and after. Require actual movement in both directions and exact unchanged geometry in the untargeted occurrence. End with distinct earlier offsets in the two panes. Preserve screenshots as well as row observations. Confirm driver `axis_discrete` and app `axis_value120`/`axis_discrete` for these actual actions, not merely a unit-test mock.

Click each earlier occurrence and require persisted `viewed_at` to remain below `updated_at`. Focus one occurrence last, click the composer, and explicitly Send through the scripted provider. Require only that last-focused visible occurrence to jump; the other's rows and bounds must remain unchanged through completion. A completed reply with composer focus stays unread. Earlier focus still does not acknowledge it; focused latest observation does. Record the actual timestamps and resulting sidebar state.

Separately exercise keyboard Tab/Shift+Tab and Enter/Space activation of **Jump to latest**, observing the visible focus and target pane. The button takes keyboard focus: Tab reaches it after the rows' actions, and Alt+End in the transcript focuses it, so Enter returns to the latest row. Never call a mouse click a keyboard pass. Do not expand driver work into application focus changes without a separate owner.

For the composer, type a multiline draft long enough to overflow its viewport. Move the pointer inside the observed text viewport and wheel in both directions. Compare native screenshots for text movement while both transcript panes retain their offsets. Confirm the saved draft is unchanged. This is separate from transcript scrolling.

## Floating maximize and restore

Only query or alter the run's private Sway socket from `start.session.sway_socket`; never use the host default. Read `get_tree` and `get_outputs` through `swaymsg -s SOCKET -r`. Select the window by the launched PID, set that private container floating, and choose a smaller initial size. Do not change global configuration.

Capture the rendered titlebar and window, output geometry, container `rect`, `window_rect`, floating membership and `fullscreen_mode` before clicking maximize, after clicking it, and after restore. Use coordinates from each fresh screenshot. `fullscreen_mode` is not an xdg maximized flag; Sway's tree does not expose that flag. With `--trace-wayland`, inspect xdg-toplevel `set_maximized`/`unset_maximized` and the compositor's configure state and dimensions too.

A request rejected or unsupported by the compositor is a recorded platform limitation, not an application pass or failure. If the app emits a wrong toggle or never requests restore despite a maximized configure, record that exact application defect separately. Do not manufacture success with compositor fullscreen commands, tiling, a screenshot alone, or global settings.

Quit through native Ctrl+Q and require exit code 0. Restart where applicable and recheck doctor/rendered state. Always stop the run and require `errors:[]` and `scratch_retained:null`. Retain failed assertions and cleanup records. Before TOB-31 closes, repeat the relevant journey on integrated main containing Files #40 and Settings #41.
