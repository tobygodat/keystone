# Device

An iOS Simulator or Android Emulator open beside the selected session, which the user taps in the Device surface and the session's agent drives through the `device_*` tools. Keystone runs the user's own `xcrun simctl`, AXe, `emulator` and `adb`. See [devices](../../../../../../docs/design/devices.md).

## Sub-features

- `device-picker`: with nothing open beside the session, the surface lists devices (`device-row-N`, name and `OS · Off/Running/Booting`) with **Open** (`device-open-N`), each platform's reason when it can't run here (no Xcode, no SDK), "No simulators or emulators found.", and on the row of a device Keystone booted but couldn't shut down, why (`device-refused-N`). `device-refresh` lists again, which also tries that shutdown again.
- `device-screen`: the open device's name (`device-name`), OS and phase (`device-phase`: Opening…, Booting…, Running, Still booting, Not running, Shutting down…, Failed), the latest frame (`device-frame`) fitted to the pane, notes (`device-note`: why it failed, the frame's age when it is over 5 s old, after a failed capture or while the device isn't running, counting up on its own, the last error, the input hint when AXe is missing), Refresh (`device-refresh`) and Close (`device-close`). The screen shows from the moment Open is clicked, so Close works during a boot.
- `device-frames`: captured only while a window draws the surface for that session, every 250 ms after the last capture while that window is active, every 2 s otherwise, never when hidden or covered by settings.
- `device-input`: a click taps the device at that point and a drag swipes, unless the platform has no input tool.
- `device-close`: Close shuts the device down only when Keystone booted it and no other session has it open, also while it still boots. A failed shutdown keeps the device open with the error. Quit shuts down the devices Keystone booted, a device still starting included. Opening a running device again keeps it Running, with no Opening… between.
- `device-tools`: `device_list`, `device_open`, `device_screenshot` (image content), `device_launch`, `device_tap`, `device_swipe`, `device_type`, `device_close` on the `keystone` MCP server. The first agent open brings the surface out in windows showing the session.

## How to get to it (user POV)

- Press Ctrl+Alt+V (Cmd+Opt+V on macOS), or choose **Toggle device** in the command palette.
- Click **+** on a tab strip and choose **Device**.
- Click **Open** on a device, click or drag on the frame, click Close.

## Driving it with keystone-verify

On Linux, `python3 scripts/device-journey-linux.py` runs the whole journey: the panel with no device tools by shortcut, palette and + menu, then the Android flow against stand-in `adb` and `emulator` programs it writes and puts first on the app's `PATH` (open, frames, click to tap, drag to swipe, Close, the scripted Claude's tools, the panel beside Browser, Files and the terminal at three sizes in both themes, Quit). The stand-ins are not an emulator.

Preconditions by hand: on Linux there are no simulators, so point `KEYSTONE_XCRUN_PATH`, `KEYSTONE_ADB_PATH` and `KEYSTONE_EMULATOR_PATH` at scripted stand-ins in `start`'s `env`. On a Mac, create a dedicated simulator (`xcrun simctl create "Keystone verify" "iPhone 18 Pro"`) and delete it afterwards; never boot or shut down one another session uses.

Create a Claude session, press Ctrl+Alt+V: the picker lists the devices. Click **Open** on the dedicated simulator: the phase reads Opening…, Booting…, then Running, and frames follow. Clicking Close while it boots shuts it down and returns to the picker. Click the frame: with AXe installed the simulator reacts; without it the note names AXe. Click Close: the picker returns and the simulator is shut down. With the scripted Claude, send `orchestrator test: device <id> com.apple.Preferences`: the reply reads `Scripted device success: … screenshot=WxH mime=image/png tap=done` and the surface opens beside the session.

## Not covered

- Keyboard access to Open, Close and the frame.
- Android on macOS without an SDK; it is covered by scripted stand-ins only.
