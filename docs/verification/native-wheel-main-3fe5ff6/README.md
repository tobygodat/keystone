# Native wheel proof and maximize diagnosis

The reusable shared driver now scrolls real Keystone panes and the composer. This completes the tooling phase of TOB-31, not the whole issue: keyboard Jump to latest is an application focus gap, Sway does not implement maximize, and the combined Files #40 / Settings #41 main smoke is still required.

The later [Files #40 integration check](files40-integration.md) verifies the resolved merge, editor launch environment and corrected native wheel path together. It also records the authorized foreground attempt's guarded abort and supersedes the earlier precaution as a permission blocker. Settings #41 and the complete combined-main gate remain pending.

## Identity and reproduction

The native app was built from freshly fetched main `3fe5ff614c5c6587e3656ab421c3451a7be32296`, with no app, tGPUI pin, installed executable or existing session changes. Its SHA256 was `bdfa7f74fbf4efb2cba4a88eee7c54b98df9ede80c55cb4ee67deeeb164d3558`. The final driver SHA256 was `d76df85d502745bc5f48e65184e956cbcc1d53e8c5a501a568a5e43210c09668`: upstream `7acd4518c0028f034e33dfd20bb5b159adf15be5` plus the checked-in headless and wheel patches. Doctor matched both live app launches and reported the binary unchanged.

Follow the [driver build](../../../crates/keystone-desktop/.agents/skills/verify/setup.md) and [native journey](../../../crates/keystone-desktop/.agents/skills/verify/features/wheel-and-maximize.md). The correction is installed in this worktree only; the old shared executable was not replaced. All input went through ordinary computer-use-mcp `input`, including the named `wheel` convenience tool. There was no private injector.

```bash
python3 scripts/test_native_driver.py
cargo test --manifest-path "$checkout/Cargo.toml" --locked -j 2 wheel_ticks_preserve -- --nocapture
cargo build -p keystone-desktop -p keystone-agent --bins --locked --offline -j 2
python3 scripts/native-driver.py --driver .audit/tools/computer-use-mcp \
  --binary target/debug/keystone-desktop --trace-wayland
```

Both patched-driver builds and its focused Rust regression passed; the five wrapper tests passed. No unrelated UI/core suite or performance run was needed for this tooling-only change. One bounded independent read-only review found an opposite-direction fractional remainder could swallow a whole notch. That was fixed, covered by a regression, rebuilt, re-reviewed with no remaining actionable findings, and verified in the final native run.

## Emission and native behavior

[Protocol excerpts](protocol-excerpts.txt) show actual requests and received events, not only successful API calls. Three upward notches emitted `axis_discrete(..., 0, -45, -3)` and the app received `axis_value120(0, -360)` followed by a frame. The four pane probes received `-360,+120,-600,+120`. The composer probes received `-720,+360`. A preliminary raw horizontal `+7.5` / vertical `-7.5`, followed by a whole vertical `+15`, still emitted vertical `+1` discrete notch; the next fractional input produced independent horizontal `+1` and vertical `-1` notches. Raw distance units remain unchanged: 15 units per notch.

The native fixture had 80 labeled synthetic turns, loaded only after Ctrl+Q exited 0 and the window disappeared. This is not proof of user-created history. Duplicate conversation, Apply, close Customize, pane focus, wheel, Send and acknowledgement were subsequent real input. The provider was local scripted Codex, with no inference or tools.

- Both occurrences moved up and down while the untargeted pane retained exactly the same row IDs and absolute/clipped bounds. After the probes, pane 0's row 155 was at y=-26, clipped at y=71; pane 3's row 155 was at y=499, below its viewport origin y=476. Their earlier offsets were distinct. The draft survived native arrangement unchanged.
- Focusing either earlier occurrence retained `viewed_at=1792384984272`. Send after last focusing pane 3 jumped only pane 3 and displayed new rows 160/161. Pane 0's exact geometry survived Send and completion.
- Completion with composer focus remained unread: `updated_at=1792385003238`, `viewed_at=1792384984272`. Clicking the earlier pane still did not acknowledge it. Clicking latest pane 3 advanced `viewed_at` to `1792385003812` and changed the sidebar out of Needs action.
- The 3,009-character, 70-line composer draft moved from lines 62–69 to 44–51 after six upward notches, then to 53–60 after three downward notches. Both transcript panes retained their exact offsets, and the persisted draft remained byte-for-byte identical.

Screenshots of the completed-but-unread and focused-latest-acknowledged states preserve the visible states. Composer upward and downward preserve the separate long-draft path.

## Keyboard Jump: application gap, not a driver pass

With pane 0 earlier, native Tab followed by Enter and Space left Jump to latest visible and retained the earlier rows. Its screenshot shows transcript focus rather than a focused Jump button. Source corroborates the failed route: `main.rs:2052–2089` builds the button without `track_focus`; `keystone-ui/src/button.rs` defaults `focus` to `None` and adds keyboard/focus behavior only when explicitly tracked. No Jump command exists in the current shared command table. This does not claim exhaustive physical-keyboard accessibility coverage.

The bounded follow-up belongs with TOB-30: retain occurrence-scoped Jump focus, add it to a coherent keyboard route with visible focus and Enter/Space activation, and prove the intended pane jumps without acknowledging an earlier unfocused occurrence. No app source was changed in this driver PR.

## Maximize: unsupported on isolated Sway 1.12

The app's own container was made floating through only the run's unique Sway IPC socket. Before maximize, after maximize, and after the second restore-attempt click, its outer rect was `(250,100,1100,800)`, inner rect `(1,1,1098,798)`, `floating=user_on`, `fullscreen_mode=0`. The output remained `HEADLESS-1`, `(0,0,1600,1000)`, scale 1. These are all retained in the [receipt](receipt.json).

The app emitted `xdg_toplevel.set_maximized()` for both clicks. Sway replied at the unchanged 1098×798 size. Its [1.12 request handler](https://github.com/swaywm/sway/blob/1.12/sway/desktop/xdg_shell.c) only schedules a configure, without applying maximize; initial WM capabilities advertise only fullscreen. GPUI toggles based on the compositor-confirmed maximized state, so the second click correctly remains a maximize request when Sway never maximized. There is no basis to label this an app restore defect.

Sway's IPC `fullscreen_mode` is not an xdg maximized flag, and the client text trace abbreviates configure state arrays, so neither is misrepresented as a direct maximized-state decoder. Geometry, request/configure trace and the compositor handler together establish the platform limitation. Successful maximize/restore on a supporting compositor remains unverified. No fullscreen workaround or global compositor setting was used.

A later [read-only Hyprland follow-up](hyprland-follow-up.md) confirmed a supporting compositor exists on the device, but found no safe per-window route to the app's pointer-only titlebar control under the no-other-focus-change boundary. It stopped before launching or sending input. This is an automation limitation, not a claim that Hyprland lacks maximize support.

## Cleanup, retained failures and handoff

Both final-run app launches quit through native Ctrl+Q with exit code 0. Stop returned `errors:[]`, `scratch_retained:null`; the temporary state directory is absent. Screenshots, state snapshots and action logs remain. [The machine-readable receipt](receipt.json) includes doctor, exact timestamps, row/pixel observations, compositor trees/outputs and cleanup. Public protocol excerpts contain scripted data only.

The final automated script's five-second long-draft persistence wait expired before autosave completed. The failed log is retained. A fresh state read then found the exact full draft, and continuation on the same instance passed composer wheel, unchanged draft/pane, graceful quit and cleanup assertions without retyping. This is not an all-green single-script claim or a save-latency performance claim. The earlier exploratory run and initial build-placement failure are retained too.

Full raw runs, PNGs, action/stderr logs, manifests, build/test output, replay helper and the failed/continued script logs are kept in an unpublished evidence archive. Raw run names are `verify-20261019T003927-1392691` and final `verify-20261019T004311-1400861`.

TOB-31 should remain In Progress. After authorized integration of #40/#41, rebuild exact combined main and repeat the relevant two-pane/composer/unread smoke. Keyboard Jump needs its separate application fix; maximize retains this explicit Sway limitation unless a supporting isolated compositor is tested. No real provider integration, physical wheel device, IME, other compositor or performance acceptance is claimed.
