---
name: verify
description: Drive Keystone's native Rust GPUI desktop on Linux with a private headless Wayland desktop. Use after UI changes to prove user interactions, screenshots, draft persistence, and restart behavior.
---

# Verify Keystone

Read [the feature map](features/README.md) before choosing a journey. On a Mac, where no private Sway desktop exists, run `python3 scripts/macos-smoke.py` instead; see [Keystone on macOS](../../../../../docs/macos.md). This skill lives with the desktop crate. Run all shell commands from the workspace root, two directories above that crate. The core crate has no separate user interface.

## Launch

Build the actual preview:

```bash
cargo build -p keystone-desktop --locked --offline -j 2
```

The driver must exist at `.audit/tools/computer-use-mcp` and include the headless, wheel, atspi, packed-rgb and nvidia patches. See [setup](setup.md) if it is missing or predates any of them.

The server launches `target/debug/keystone-desktop` from its own checkout. Pass `--binary PATH`, or set `KEYSTONE_BINARY`, to launch another build. In a git worktree, `.audit/` is not shared. When the worktree has no driver, the server uses the main checkout's, so do not build it again. To choose one explicitly, pass `--driver <main checkout>/.audit/tools/computer-use-mcp`. If `start` says that driver predates a patch, build your own as [setup](setup.md) describes and leave the shared copy alone. Build the worktree's own app first, or point `--binary` at a build of the same commit. `doctor` reports the hash of the binary that actually ran.

Verification runs through the `keystone-verify` MCP server, registered in the workspace `.mcp.json`. It is `scripts/native-driver.py`. Approve it once if the harness asks. If MCP tools are unavailable, speak the same JSON-RPC to `python3 scripts/native-driver.py` over stdio.

Call `start`. Pass `env` to set app variables for the run and its relaunches: `KEYSTONE_CODEX_PATH`, `KEYSTONE_CLAUDE_PATH`, `KEYSTONE_PROJECT_DIR`, `KEYSTONE_FAKE_CODEX_DELAY_MS`, `KEYSTONE_FAKE_CLAUDE_DELAY_MS`, `KEYSTONE_VISUAL_FIXTURE`, `KEYSTONE_TEST_CLAUDE_IDLE_MS`, `KEYSTONE_TEST_LOG_BUDGET_BYTES`, `KEYSTONE_TEST_CHECKPOINT_KEEP` and `KEYSTONE_TEST_SOUND_COMMAND` (debug builds; the driver sets the latter to `true` unless `env` names it, and points `PULSE_SERVER` and `PIPEWIRE_REMOTE` nowhere, see [sounds](features/sounds.md)), `VISUAL`, `EDITOR`, `KEYSTONE_FAKE_MCP_CONFIG`, `KEYSTONE_FAKE_IMAGE_RECEIPT`, `KEYSTONE_ATTACHMENT_MAX_MB`, `KEYSTONE_ATTACHMENT_KEEP_DAYS`, `KEYSTONE_TEST_SCREENSHOT_COMMAND` (debug builds), `PATH` and `GH_CONFIG_DIR` (for a stand-in `gh`, see [changes](features/changes.md)), `KEYSTONE_BROWSER_PATH` (see [browser](features/browser.md)), and the fixture provider homes `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `HOME`. Unless `env` names it, `HOME` is an empty folder of the run, and so are `CLAUDE_CONFIG_DIR` and `CODEX_HOME` unless the server itself was started with them, so a run that names no fixture never reads the user's own provider config or `~/.agents/skills`. See [conversations](features/conversations.md), [attachments](features/attachments.md), [files](features/files.md) and [integrations](features/integrations.md). It creates a private Sway desktop, launches the binary, and creates a new evidence directory under `.audit/verify-*`. The app gets a fresh `KEYSTONE_STATE_PATH` under `/tmp/keystone-verify-*`, its own private D-Bus session bus (returned as `bus`, with no activatable services, so desktop notifications never reach the user's desktop; it also has no xdg-desktop-portal, so the System theme does not follow the host and portal file pickers and link opening fail there), and has `KEYSTONE_VISUAL_FIXTURE` removed unless `env` sets it. No authentication, migration command, seed data, or network service is needed. One server holds one run at a time; `stop` it before the next `start`.

`start` makes a 1920 by 1080 desktop at scale 1, the size and scale of the user's display, unless `width` and `height` say otherwise. Keep that size for screenshots a person will see, and add `"width":720` for the narrow window. `screenshot` saves the PNG at the desktop's exact size and returns its `width` and `height` from the saved file. Never resize or recompress a screenshot you present as the app's rendering; a scaled screenshot looks softer than the app. Show detail as a 1:1 crop, enlarged only by whole-number nearest-neighbour steps.

For anything that moves or happens over time, record it with `record_start` and `record_stop`. The 30 fps MP4 lands in the evidence directory as `recording-NN.mp4`, at the desktop's size or scaled to fit 1920 by 1080, ready for `tools.pr.uploadAsset`; `record_stop` fails if it isn't. `stop` finishes a running recording.

The desktop is headless by default. It renders offscreen on the GPU at exactly the requested size, so host focus, workspaces, and visibility do not affect capture or input. Several agents can verify at once. Pass `headless:false` only to watch the run as a host window. That mode reintroduces KS-006: the host compositor stops drawing a nested window it does not show (seen with Hyprland and with GNOME), and then input, frames and captures stall ("Wayland IO timed out") until the window is visible on the host again.

A successful `start` proves only process creation, and the window can map a moment later, so `wait_for` `{"id":"welcome-claude"}` before the first `doctor`, then `screenshot`. Readiness requires a native Keystone window showing the welcome composer, no error, and an empty sidebar.

## Doctor

Require `session_status` to report a running desktop, `windows_list` to contain the launched Keystone PID, and `processes` to show `matches_launch:true` for the live process. Require `binary_unchanged:true`. After restart, older tracked PIDs may be exited. At least the newest PID must be live. Doctor only reads state.

If the window is absent, inspect the evidence directory, capture the desktop, and report the concrete startup failure. Each launch keeps the app's stderr, where a panic goes, in `app-NN.stderr.log`. Its exit code, or the signal that ended it, goes in `exit-NN.json`, and `doctor` reports it for exited PIDs. `driver.stderr.log` holds computer-use-mcp's own output. If the binary changed after launch, quit the app and relaunch before claiming verification of the new build.

## Drive

`elements` lists every named element in the app's latest frame, in desktop pixels:

```json
{"id":"welcome-claude","bounds":{"x":893,"y":503,"w":172,"h":28},"visible":{"x":893,"y":503,"w":172,"h":28},"center":{"x":979,"y":517}}
```

The app writes this itself. Until a frame's viewport matches the window, which can take a moment after launch, `elements` reports not ready and `wait_for` keeps waiting. Names are the GPUI debug selectors used by simulated tests, so they match the test suite. `visible` is the part inside its clip. An open menu covering an element is not subtracted, so check the screenshot when menus are open. The window controls are named `minimize`, `maximize` and `close` (drawn off macOS only). Unnamed controls still need screenshot coordinates. Name a control with `debug_selector` on a component or `automation_id` on a div when a journey needs it.

Prefer names and waits over coordinates and sleeps:

- `click` with `{"id":"new-session"}` clicks a named element's visible center. It fails if the name is missing, duplicated, or clipped. `{"x":..,"y":..}` clicks pixels from the latest screenshot.
- `wait_for` with `{"id":"new-codex"}` waits until the element is visible. Add `"present":false` to wait for it to go.
- `input` takes driver actions: `{"actions":[{"action":"keypress","keys":["ctrl","l"]}]}`, `{"action":"type","text":"Verification draft"}`, and `{"action":"wait","duration_ms":500}`.
- `wheel` takes a fresh named target and signed whole notch counts: `{"id":"conversation","vertical":-3}` scrolls upward three notches. Positive vertical scrolls down; positive horizontal scrolls right. It uses the same upstream `input` path as raw `scroll`, whose units remain axis distances (15 per notch). Read actual row/pixel movement, not only a successful input response. See [wheel and maximize](features/wheel-and-maximize.md) for the native acceptance journey.
- With several windows, every element names its `window` title, and `elements`, `click`, `wait_for` and `wheel` take `window` to choose one when a name repeats. See [windows](features/windows.md).
- `screenshot` returns the PNG and saves it as evidence. A driver built before the packed-rgb patch rejects the headless output's 24-bit BGR888 buffers ("unsupported SHM pixel format", TOB-214); then the server captures the same output with `grim` instead. `capture` in the result says which did, so `grim` means the driver needs rebuilding.
- Outside the main conversation view, ids carry the view's suffix: `approve-view-14`, `transcript-row-0-view-10`, `handoff-from-view-13`. Read `elements` rather than guessing it.
- An `input` `wait` action takes at most 3000 ms. Use `wait_for` for anything longer.
- `workspace` reads saved SQLite workspace and session projections in one read-only transaction. `snapshot` uses SQLite's backup API, including committed WAL contents, and preserves both the database and a matching JSON projection in the evidence directory. Its `path` remains the JSON export; `database_path` names the self-contained backup.
- `call` reaches any upstream tool, such as `{"tool":"windows_list"}`.

A named click proves where the input went, not what it did. Confirm each step through the visible result and the saved state. Re-observe after state changes rather than batching across them. The feature map lists mouse and keyboard entry points; proving one does not prove the others.

To restart, press Ctrl+Q through `input`, wait until `windows_list` is empty and the tracked PID has exited, then call `launch`. A graceful quit has exit code 0 in `doctor`; a crash during quit also removes the window. Launch reuses the run's SQLite database. The configured `state` path is the legacy JSON import location; `start` also returns the live `database` path. Run `doctor` again and inspect the restored UI. Never launch twice against a running instance. The one exception is `launch` with `args` `["--quick"]`, which runs `keystone-desktop --quick` in the private session: it hands the request to the running app and exits, and `doctor` lists it as exited. See [quick composer](features/quick.md).

## Evidence

Every driver request and sanitized result is recorded in `actions.jsonl`, along with each `elements` read. Screenshots are numbered PNGs in the same directory. Read state after the user action and assert literal expected values. Never populate state with internal setters to prove a user journey.

For event diagnosis, launch `python3 scripts/native-driver.py --trace-wayland`. The driver's stderr records virtual-pointer requests; each app stderr records received Wayland events. This is opt-in because traces are verbose and can contain typed content. Use scripted data and inspect traces for private information before publishing them. Require Wheel `axis_discrete` emission and received `axis_value120` (or `axis_discrete` on an older pointer version), followed by a frame, before claiming corrected native wheel delivery.

Capture both the action and its resulting visible state. A screenshot alone does not prove persistence. A saved file alone does not prove rendering or restored input. For restart, inspect the reopened UI and use it again. Include the feature ID, entry point, expected outcome, observed outcome, and screenshot paths in a run report. Mark untested entry points explicitly.

Use screenshots for the reference comparison. Check the 264 px default sidebar column, the top bar breadcrumb, the centered welcome heading and composer, the 640 px conversation column, text legibility, clipping, and smaller windows. Do not claim pixel parity at different sizes or scales. Codex and Claude sessions both send messages. Replies from the scripted providers are test data and prove Keystone's handling, not the provider's. A run with the real `codex` or `claude` proves the integration but spends the user's usage. The fixture transcript is sample content and cannot prove provider behavior.

Under verification the sidebar renders uncached, because a reused cached view reports no geometry. Do not use verification runs for performance claims.

## Cleanup

For graceful-save proof, first quit through Ctrl+Q and observe the window disappear. `stop` terminates remaining apps and does not prove a graceful save. Call `stop` even after a failed interaction. If the server's stdin closes, it also attempts cleanup. The driver stops only its tracked applications and nested compositor. The server copies final persisted state and removes its own temporary state directory after successful shutdown. It preserves the evidence directory.

Require the `stop` result to have an empty `errors` list and `scratch_retained:null`. Confirm the PNGs, action log, and state snapshots still exist. If cleanup reports failure, retain the scratch directory and report it. Do not kill by process name or remove evidence to hide a failed run.

## Helpers

`python3 scripts/native-stress.py --runs 10` repeats the KS-005 typing stress on fresh headless desktops. Each run types a 1,093-character draft with `wtype -d 10` on the nested socket, quits through `wtype`, and relaunches, which also checks KS-009. Both quits must exit with code 0. The draft must be saved within `--max-persist-lag` seconds of the last key, 1 s by default. `--delay-ms` and `--repeat` change the rate and length. `--no-prime` skips the pre-launch key event and should crash on relaunch. Results go to `.audit/native-stress-*/summary.json`. Pass `--driver` as for the server.

`scripts/native-smoke.py` remains an alternate foreground Hyprland keyboard regression test. Run `python scripts/native-smoke.py` only when foreground desktop control is intended. It does not replace mouse-entry or nested-desktop proof. Run `bash scripts/check.sh ui` for simulated GPUI input tests, but never report those as native visual verification.
