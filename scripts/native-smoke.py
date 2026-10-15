#!/usr/bin/env python3
"""Requires the installed Hyprland Lua API, not legacy dispatch names."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
ARTIFACTS = Path(os.environ.get("KEYSTONE_CAPTURE_DIR", ROOT / ".audit" / "native-smoke"))
DRAFT = "Native smoke draft survives restart."
KEY_DELAY = os.environ.get("KEYSTONE_KEY_DELAY_MS", "100")


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, timeout=60 if args[0] == "wtype" else 10)


def clients():
    return json.loads(command("hyprctl", "clients", "-j"))


def focus(address):
    command("hyprctl", "dispatch", f'hl.dsp.focus({{ window = "address:{address}" }})')


def wait_for(description, check, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.1)
    raise RuntimeError(f"Timed out waiting for {description}")


def stop(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def run(result):
    for executable in ("hyprctl", "grim", "wtype"):
        if shutil.which(executable) is None:
            result["status"] = "unsupported"
            raise RuntimeError(f"Native smoke requires {executable}")
    result["compositor"] = command("hyprctl", "version")
    previous_window = json.loads(command("hyprctl", "activewindow", "-j")).get("address")
    process = None
    with tempfile.TemporaryDirectory(prefix="keystone-smoke-") as state_dir:
        state_path = Path(state_dir) / "workspace.json"
        env = dict(os.environ, KEYSTONE_STATE_PATH=str(state_path))
        with (ARTIFACTS / "process.log").open("w") as log:
            def launch():
                nonlocal process
                process = subprocess.Popen([str(ROOT / "target/debug/keystone-desktop")], env=env, stdout=log, stderr=log)
                result["pids"].append(process.pid)

                def own_window():
                    if process.poll() is not None:
                        raise RuntimeError(f"Keystone exited with {process.returncode}; see process.log")
                    return next((client for client in clients() if client.get("pid") == process.pid), None)

                window = wait_for("spawned native window", own_window, 20)
                if window.get("xwayland"):
                    raise RuntimeError("Expected a native Wayland window")
                focus(window["address"])
                wait_for("spawned window focus", lambda: json.loads(command("hyprctl", "activewindow", "-j")).get("pid") == process.pid)
                time.sleep(0.5)
                return window

            def input_keys(*args):
                if process.poll() is not None:
                    raise RuntimeError("Keystone exited before input")
                active = json.loads(command("hyprctl", "activewindow", "-j"))
                if active.get("pid") != process.pid:
                    raise RuntimeError("Focus moved away from Keystone; refusing input")
                command("wtype", *args)

            def shortcut(key, shift=False):
                args = ["-M", "ctrl"]
                if shift:
                    args += ["-M", "shift"]
                args += ["-k", key]
                if shift:
                    args += ["-m", "shift"]
                args += ["-m", "ctrl"]
                input_keys(*args)

            def workspace():
                if not state_path.exists():
                    return {"sessions": [], "selected": None}
                return json.loads(state_path.read_text())["workspace"]

            def expect(name, predicate):
                def matching_state():
                    if process.poll() is not None:
                        raise RuntimeError(f"Keystone exited with {process.returncode} while waiting for {name}")
                    state = workspace()
                    return state if predicate(state) else None

                value = wait_for(name, matching_state)
                result["checks"].append(name)
                return value

            def capture(name):
                time.sleep(0.3)
                window = next(client for client in clients() if client.get("pid") == process.pid)
                x, y = window["at"]
                width, height = window["size"]
                command("grim", "-g", f"{x},{y} {width}x{height}", str(ARTIFACTS / f"{name}.png"))
                (ARTIFACTS / f"{name}.window.json").write_text(json.dumps(window, indent=2))

            try:
                launch()
                input_keys("-k", "Escape")
                expect("empty workspace", lambda s: s["sessions"] == [])
                capture("empty")
                shortcut("n")
                expect("Claude created through native shortcut", lambda s: len(s["sessions"]) == 1 and s["sessions"][0]["provider"] == "Claude")
                shortcut("n", shift=True)
                expect("Codex created through native shortcut", lambda s: len(s["sessions"]) == 2 and s["sessions"][1]["provider"] == "Codex")
                shortcut("s", shift=True)
                expect("selected Codex settled", lambda s: s["sessions"][1]["settled"])
                shortcut("s", shift=True)
                expect("selected Codex unsettled", lambda s: not s["sessions"][1]["settled"])
                shortcut("l")
                time.sleep(0.3)
                input_keys("-d", KEY_DELAY, "--", DRAFT)
                populated = expect("composer draft persisted", lambda s: s["sessions"][1]["draft"] == DRAFT)
                capture("populated")
                input_keys("-d", KEY_DELAY, "--", " A long draft should keep its caret visible." * 6)
                expect("long draft persisted", lambda s: s["sessions"][1]["draft"] == DRAFT + " A long draft should keep its caret visible." * 6)
                capture("long-input")
                shortcut("a")
                input_keys("-d", KEY_DELAY, "--", DRAFT)
                expect("draft replacement persisted", lambda s: s["sessions"][1]["draft"] == DRAFT)
                stop(process)
                launch()
                expect("workspace file unchanged across restart", lambda s: s == populated)
                capture("restored")
                shortcut("s", shift=True)
                expect("restarted app loaded selected Codex", lambda s: len(s["sessions"]) == 2 and s["sessions"][1]["settled"] and s["sessions"][1]["draft"] == DRAFT)
                if os.environ.get("KEYSTONE_TEST_CLOSE") == "1":
                    saved_path = state_path.with_suffix(".saved")
                    state_path.rename(saved_path)
                    state_path.mkdir()
                    shortcut("l")
                    time.sleep(0.3)
                    shortcut("a")
                    input_keys("-d", KEY_DELAY, "--", "Unsaved draft must survive native close")
                    if json.loads(command("hyprctl", "activewindow", "-j")).get("pid") != process.pid:
                        raise RuntimeError("Focus moved before native close; refusing dispatch")
                    command("hyprctl", "dispatch", "hl.dsp.window.close()")
                    time.sleep(0.5)
                    if process.poll() is not None or not any(c.get("pid") == process.pid for c in clients()):
                        raise RuntimeError("Native close discarded an unsaved draft")
                    capture("blocked-close")
                    state_path.rmdir()
                    saved_path.rename(state_path)
                    shortcut("s", shift=True)
                    expect("native close preserved unsaved draft for retry", lambda s: s["sessions"][1]["draft"] == "Unsaved draft must survive native close")
                (ARTIFACTS / "persisted-state.json").write_text(state_path.read_text())
                result["status"] = "passed"
            except Exception:
                result["process_exit"] = process.poll() if process else None
                if state_path.is_file():
                    (ARTIFACTS / "failure-state.json").write_text(state_path.read_text())
                if process is not None and process.poll() is None:
                    capture("failure")
                    if os.environ.get("KEYSTONE_DEBUG_HANG") == "1":
                        trace = subprocess.run(["gdb", "-q", "-batch", "-ex", "thread apply all bt 12", "-p", str(process.pid)], capture_output=True, text=True, timeout=30)
                        (ARTIFACTS / "hang.txt").write_text(trace.stdout + trace.stderr)
                raise
            finally:
                stop(process)
                if previous_window and any(client.get("address") == previous_window for client in clients()):
                    focus(previous_window)


def main():
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    result = {"status": "failed", "input": "native Wayland keyboard via wtype", "key_delay_ms": int(KEY_DELAY), "checks": [], "pids": []}
    try:
        run(result)
    except subprocess.CalledProcessError as error:
        result["error"] = f"{error}: {error.output}"
        if error.cmd[0] in ("hyprctl", "wtype"):
            result["status"] = "unsupported"
    except Exception as error:
        result["error"] = str(error)
    (ARTIFACTS / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
