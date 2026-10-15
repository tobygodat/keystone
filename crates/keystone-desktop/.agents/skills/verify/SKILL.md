---
name: verify
description: Drive Keystone's native Rust GPUI desktop on Linux with a nested Wayland desktop. Use after UI changes to prove user interactions, screenshots, draft persistence, and restart behavior.
---

# Verify Keystone

Read [the feature map](features/README.md) before choosing a journey. This skill lives with the desktop crate. Run all shell commands from the workspace root, two directories above that crate. The core crate has no separate user interface.

## Launch

Build the actual preview:

```bash
cargo build -p keystone-desktop --locked --offline -j 2
```

The driver must exist at `.audit/tools/computer-use-mcp`. See [setup](setup.md) if it is missing. It uses a nested Sway desktop on the current Wayland session. Native CUA is unavailable on this Linux host.

Start an interactive PTY with the following command. Retain the execution session ID so later commands reach this controller's stdin. Use a new evidence directory for every attempt:

```bash
python scripts/native-driver.py --driver .audit/tools/computer-use-mcp --root "$PWD" --evidence "$PWD/.audit/verify-$(date +%Y%m%dT%H%M%S)-$$" --width 1280 --height 900
```

The controller initializes MCP, creates a nested desktop, and launches `target/debug/keystone-desktop`. It supplies a fresh `KEYSTONE_STATE_PATH` under `/tmp/keystone-verify-*` and removes `KEYSTONE_VISUAL_FIXTURE`. No authentication, migration command, seed data, or network service is needed. Each run owns one MCP connection and one disposable workspace. Different runs must use different evidence directories.

`ok:true` from launch proves only process creation. Run Doctor, then capture a screenshot. Readiness requires a focused native Keystone window showing the welcome composer, no error, and an empty sidebar. Do not drive an old host window or infer readiness from process creation.

## Doctor

Send this JSON line to the controller's stdin:

```json
{"kind":"doctor"}
```

Require `session_status` to report a running desktop, `windows_list` to contain the launched Keystone PID, and `processes` to show `matches_launch:true` for the live process. Require `binary_unchanged:true`. After restart, older tracked PIDs may be exited. At least the newest PID must be live. Doctor only reads state. It does not repair or launch anything.

On this Hyprland host, capture timed out while the nested Sway window was offscreen and succeeded when it was brought into view. Keep the nested desktop visible during capture. Input still targets its own desktop. If capture times out, run Doctor and inspect host window metadata before retrying. Use `hyprctl clients -j` to match the compositor PID from `session_status`. Focus only its observed address with the installed Lua dispatcher, `hyprctl dispatch 'hl.dsp.focus({ window = "address:OBSERVED_ADDRESS" })'`. Substitute the observed address, never a guessed or stale one. Record the failed capture and the focus action. This changes host focus, so do not silently loop against a user moving to another workspace. A successful recovery does not erase the failed command or its nonzero run status.

If the window is absent, inspect `.audit/verify-*/driver.stderr.log`, capture the desktop, and report the concrete startup failure. Do not send input to another window. If the binary changed after launch, close this app and restart before claiming verification of the new build.

## Drive

Each example below is one JSON line sent through the retained PTY. The controller wraps real upstream MCP tools. `ok:false` means failure, even if the process continues to allow cleanup. A failed command also makes the final controller exit nonzero.

Capture before choosing a click target:

```json
{"kind":"call","tool":"screenshot","arguments":{}}
```

Inspect the returned PNG with the image-viewing tool. Coordinates are pixels within the nested desktop. Use the latest capture dimensions and observed button center. The host may resize Sway. Never reuse coordinates from a different screenshot size. GPUI debug selectors used by simulated tests are not native accessibility selectors.

Prefer the application's existing keyboard shortcuts for deterministic journeys:

```json
{"kind":"call","tool":"input","arguments":{"actions":[{"action":"keypress","keys":["ctrl","n"]}]}}
{"kind":"call","tool":"input","arguments":{"actions":[{"action":"keypress","keys":["ctrl","l"]}]}}
{"kind":"call","tool":"input","arguments":{"actions":[{"action":"type","text":"Verification draft"}]}}
{"kind":"call","tool":"input","arguments":{"actions":[{"action":"wait","duration_ms":1000}]}}
{"kind":"call","tool":"screenshot","arguments":{}}
{"kind":"snapshot"}
```

Check the current upstream input schema if a tool rejects a field. Do not blindly replay a partially completed batch. Re-observe first. Use separate calls around state changes. The feature map lists mouse and keyboard entry points; proving one does not prove the others.

To restart, press Ctrl+Q through `input`, wait until `windows_list` is empty and the tracked PID has exited, then send `{"kind":"launch"}`. Launch reuses the run's state file. Run Doctor again and inspect the restored UI. Never launch twice against an already running instance.

## Evidence

Every request and sanitized result is recorded in `actions.jsonl`. Screenshots are numbered PNGs in that same evidence directory. `snapshot` copies the real workspace file into timestamped JSON evidence. Read the snapshot and assert literal expected values after the user action. Never populate state with internal setters to prove a user journey.

Capture both the action and its resulting visible state. A screenshot alone does not prove persistence. A saved file alone does not prove rendering or restored input. For restart, inspect the reopened UI and use it again. Include the feature ID, entry point, expected outcome, observed outcome, and screenshot paths in a run report. Mark untested entry points explicitly.

Use screenshots for the reference comparison. Check rail and sidebar proportions, tabs, centered welcome heading, composer alignment, text legibility, clipping, and smaller windows. Do not claim pixel parity at different sizes or scales. The current preview has no provider execution, model menu, permission menu, or prompt submission. The fixture transcript is sample content and cannot prove provider behavior.

## Cleanup

For graceful-save proof, first quit through Ctrl+Q and observe the window disappear. `stop` terminates remaining apps and does not prove a graceful save. Send `{"kind":"stop"}` even after a failed interaction. On EOF, the controller also attempts cleanup. Wait for process completion. The driver stops only its tracked applications and nested compositor. The controller copies final persisted state and removes its own temporary state directory after successful shutdown. It preserves the evidence directory.

Require the cleanup record to have an empty `errors` list and `scratch_retained:null`. Confirm the recorded app, driver, and compositor PIDs exited. Confirm the PNGs, action log, and state snapshots still exist. If cleanup reports failure, retain the scratch directory and report it. Do not kill by process name or remove evidence to hide a failed run.

## Helpers

`scripts/native-driver.py` is the executable interactive stdio controller used above. Its command kinds are `doctor`, `call`, `workspace`, `snapshot`, `launch`, and `stop`. Generic calls use the upstream tool name and arguments. The pinned upstream driver is documented in setup.md.

`scripts/native-smoke.py` remains an alternate foreground Hyprland keyboard regression test. Run `python scripts/native-smoke.py` only when foreground desktop control is intended. It does not replace mouse-entry or nested-desktop proof. Run `bash scripts/check.sh ui` for simulated GPUI input tests, but never report those as native visual verification.
