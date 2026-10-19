# Supporting-compositor follow-up: safe app input unavailable

This records the earlier read-only phase under a Captain precaution, not a user requirement. The Captain subsequently authorized temporary foreground focus for one disposable test. See the [integration follow-up](files40-integration.md) for that attempt's guarded abort and final focus/pointer restoration; the historical restriction below is not a permanent product blocker.

Read-only discovery on 2026-10-19 confirmed the device's existing Hyprland 0.56.2 compositor, commit `efb50993780079460b0cbed1363e2166a2de1d9f`. Its active output was 1920×1080 at scale 1. The exact main app binary still had SHA256 `bdfa7f74fbf4efb2cba4a88eee7c54b98df9ede80c55cb4ee67deeeb164d3558`. No disposable app was launched in this follow-up, and no compositor state or configuration was changed.

Hyprland's [Lua dispatcher API](https://wiki.hypr.land/Configuring/Basics/Dispatchers/) distinguishes `mode="maximized"` from fullscreen and permits an explicit `window` selector. The [exact installed commit's dispatcher source](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/config/lua/bindings/LuaBindingsDispatchers.cpp) confirms that selection. This is a possible compositor-driven test, not proof that Keystone's titlebar request and restore toggle work. No such dispatcher was invoked here.

The app entry point cannot currently be driven with the required guarantee that another app's focus and state remain untouched:

- `keystone-desktop/src/main.rs:644–647` exposes maximize as an IconButton click calling `window.zoom_window()`. It has no tracked keyboard focus, accessible invoke name, or command binding for this action. The automation file reports geometry; it is not an input endpoint.
- The reusable native driver owns a private Sway connection. Its named click/input path cannot attach to an existing Hyprland window by PID or address.
- The available Linux desktop interface supports only whole-screen input, not app/window handles. It sends real input to the desktop and moves the real pointer. The repository's foreground native smoke explicitly focuses its test window, so using it would violate this phase's no-other-focus-change boundary.
- A window-targeted keyboard shortcut is not a substitute for the missing app maximize binding. Repositioning the pointer, focusing a new app and restoring the previous focus, injecting a titlebar click globally, or using debugger/internal setters would not satisfy the requested isolated native user-action proof.

The phase therefore stopped before launch or input. There is no new maximize/restore pass, application defect, screenshot, doctor or graceful-exit claim. The prior Sway result remains an isolated compositor limitation, not the final product verdict. Successful titlebar maximize/restore on a supporting compositor remains unverified because safe per-window app input is unavailable under the current boundary.

A future test needs either a window-scoped native pointer/semantic invoke that leaves other clients untouched, or an explicitly authorized foreground test window with temporary focus. The latter is outside this phase's permission. Compositor-forced maximization can be reported separately if commissioned, but must not be presented as proof of the app's titlebar path.
