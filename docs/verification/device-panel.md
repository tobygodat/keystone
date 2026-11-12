# Device panel (TOB-307)

Native evidence from this Mac (Xcode 27.2, an iPhone 18 Pro simulator on iOS 27.2, AXe 1.8.0), driven only through Keystone's automation inbox. The design is [devices](../design/devices.md).

1. [The picker beside a new Claude session](device-panel/01-picker-beside-session.png), listing every available simulator from the real `simctl`, with the reason Android isn't available here. Branch head `3a2ff07`.
2. [A real Claude Code session drove the simulator](device-panel/02-agent-tapped-general.png). It called `device_open`, `device_launch` (Settings), `device_screenshot` twice, `device_tap` on General and `device_screenshot` again, then described both screens. The panel shows the General page live. Commit `75572c7`.
3. [The same journey on the branch head](device-panel/03-agent-reports-shifted-tap.png). The agent tapped General where its screenshot showed it, but Settings inserts an "Optimizing Search and Siri" row a few seconds after launch, so the tap opened that row. `simctl` alone shows the row arriving between 2 s and 13 s after launch, so this is the app moving, not a mapped tap landing wrong. The agent reported it as such.
4. [A click on the panel's frame taps the simulator](device-panel/04-panel-click-taps.png): the click on General opened it. Commit `37d0ffe`, whose device code the branch head carries unchanged.
5. [Close of a simulator Keystone booted shuts it down](device-panel/05-close-shuts-down.png); `simctl` reported it Shutdown 3.8 s later. Commit `37d0ffe`.

Not shown: Android Emulators, which this Mac can't run (no Android SDK); they are covered by scripted `adb` and `emulator` in the agent tests.
