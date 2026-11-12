#!/usr/bin/env python3
"""Live probe of what Claude Code and Codex send when an agent edits a file.

Records real frames for Claude's Edit, MultiEdit and Write tools (their `tool_result`
and `tool_use_result`) and Codex's `fileChange` items (`changes[].diff`). Each case runs
one real turn on a cheap model in a disposable git repository, using the user's own
login, so it spends subscription usage: run it by hand, never from the test suite.
"""
import argparse
import json
import os
import pathlib
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

FILES = {
    "a.txt": "one\ntwo\nthree\nfour\nfive\n",
    "b.txt": "red\ngreen\nblue\n",
    "d.txt": "old\n",
}
CLAUDE_PROMPT = (
    "Do exactly these file edits with your file tools, one tool call each, and run no commands. "
    "1. Use the Edit tool on a.txt to replace the line 'two' with 'TWO'. "
    "2. Use the MultiEdit tool once on b.txt to replace 'red' with 'RED' and 'blue' with 'BLUE'; "
    "if you have no MultiEdit tool, use Edit twice instead. "
    "3. Use the Write tool to create c.txt containing the two lines 'hello' and 'world'. "
    "4. Use the Write tool to replace all of d.txt with the one line 'new'. "
    "Then reply 'done' and stop."
)
CODEX_PROMPT = (
    "Make these changes with apply_patch and run no other commands: in a.txt replace the line "
    "'two' with 'TWO'; create c.txt containing the two lines 'hello' and 'world'; delete d.txt. "
    "Then reply 'done' and stop."
)


class CLI:
    def __init__(self, args, cwd, events):
        self.events = events
        self.queue = queue.Queue()
        self.started = time.monotonic()
        self.process = subprocess.Popen(args, cwd=cwd, text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=1, start_new_session=True)
        self.lock = threading.Lock()
        for stream in ("stdout", "stderr"):
            threading.Thread(target=self.read, args=(stream,), daemon=True).start()

    def record(self, direction, payload):
        self.events.append({"t": round(time.monotonic() - self.started, 3), "dir": direction, "frame": payload})

    def read(self, stream):
        for line in getattr(self.process, stream):
            try:
                payload = json.loads(line)
            except ValueError:
                payload = line.rstrip()
            self.record("out" if stream == "stdout" else "err", payload)
            if stream == "stdout" and isinstance(payload, dict):
                self.queue.put(payload)

    def send(self, message):
        self.record("in", message)
        with self.lock:
            self.process.stdin.write(json.dumps(message) + "\n")
            self.process.stdin.flush()

    def until(self, predicate, timeout=240):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                message = self.queue.get(timeout=0.2)
            except queue.Empty:
                if self.process.poll() is not None:
                    return None
                continue
            if predicate(message):
                return message
        return None

    def close(self):
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
            self.process.wait(timeout=5)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def repo(root, name):
    path = root / name
    path.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=path, check=True)
    for file, text in FILES.items():
        (path / file).write_text(text)
    return path


def claude(binary, root):
    cwd = repo(root, "claude-edits")
    args = [binary, "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--replay-user-messages", "--permission-prompts", "host", "--permission-prompt-tool", "stdio", "--model", "haiku", "--strict-mcp-config", "--settings", '{"disableAllHooks":true}']
    events = []
    cli = CLI(args, cwd, events)
    try:
        cli.send({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}})
        cli.until(lambda m: m.get("type") == "control_response")
        cli.send({"type": "user", "uuid": "6b657973-746f-46e6-8005-000000000001", "message": {"role": "user", "content": [{"type": "text", "text": CLAUDE_PROMPT}]}, "parent_tool_use_id": None})
        while True:
            message = cli.until(lambda m: m.get("type") in ("control_request", "result"))
            if message is None or message.get("type") == "result":
                break
            request = message["request"]
            answer = {"behavior": "allow", "updatedInput": request.get("input", {})} if request.get("subtype") == "can_use_tool" else {}
            cli.send({"type": "control_response", "response": {"subtype": "success", "request_id": message["request_id"], "response": answer}})
        time.sleep(1)
    finally:
        cli.close()
    return events


def codex(binary, root, model):
    cwd = repo(root, "codex-edits")
    events = []
    cli = CLI([binary, "app-server", "--listen", "stdio://"], cwd, events)
    try:
        cli.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "keystone-probe", "title": "Keystone Probe", "version": "0"}, "capabilities": {"experimentalApi": True}}})
        cli.until(lambda m: m.get("id") == 1)
        cli.send({"method": "initialized"})
        cli.send({"id": 2, "method": "thread/start", "params": {"cwd": str(cwd), "model": model, "ephemeral": True, "approvalPolicy": "on-request", "sandbox": "workspace-write"}})
        started = cli.until(lambda m: m.get("id") == 2)
        if not started or "result" not in started:
            return events
        thread = started["result"]["thread"]["id"]
        cli.send({"id": 3, "method": "turn/start", "params": {"threadId": thread, "input": [{"type": "text", "text": CODEX_PROMPT, "text_elements": []}], "effort": "low"}})
        while True:
            message = cli.until(lambda m: m.get("method") == "turn/completed" or ("method" in m and "id" in m) or (m.get("id") == 3 and "error" in m))
            if message is None or message.get("method") == "turn/completed" or message.get("id") == 3:
                break
            if message["method"].endswith("requestApproval"):
                cli.send({"id": message["id"], "result": {"decision": "accept"}})
            else:
                cli.send({"id": message["id"], "error": {"code": -32601, "message": "probe does not answer " + message["method"]}})
        time.sleep(1)
    finally:
        cli.close()
    return events


def sanitize(value, root):
    text = json.dumps(value)
    text = text.replace(str(root.resolve()), "/tmp/probe").replace(str(root), "/tmp/probe")
    text = text.replace(str(pathlib.Path.home()), "/home/user")
    text = re.sub(r'"(email|account_uuid|organization_uuid|accountId|userId)": ?"[^"]*"', r'"\1": "<redacted>"', text)
    return json.loads(text)


INIT_KEEP = ("type", "subtype", "cwd", "session_id", "model", "permissionMode", "claude_code_version", "uuid")
FIXTURE_THREAD = "01a1418d-549d-7553-bd08-4183fac44aa7"


def fixture_lines(provider, events):
    """What the provider printed, for a driver to replay. Claude's `initialize` answer and
    the inventory in `system/init` list the user's own skills, commands and tools, so
    they are left out, as are Claude's usage reports and Codex's handshake answers and
    account reports. Each Codex
    recording's thread ID becomes the tests' one."""
    lines = []
    thread = None
    for event in events:
        frame = event["frame"]
        if event["dir"] != "out" or not isinstance(frame, dict):
            continue
        if provider == "claude":
            if frame.get("type") in ("control_response", "rate_limit_event"):
                continue
            if frame.get("type") == "system" and frame.get("subtype") == "init":
                frame = {key: frame[key] for key in INIT_KEEP if key in frame}
        else:
            if frame.get("id") in (1, 2) and "method" not in frame:
                thread = thread or frame.get("result", {}).get("thread", {}).get("id")
                continue
            if frame.get("method") in ("thread/started", "warning", "configWarning", "remoteControl/status/changed"):
                thread = thread or frame.get("params", {}).get("thread", {}).get("id")
                continue
            if frame.get("method", "").startswith("account/") or frame.get("method") == "mcpServer/startupStatus/updated":
                continue
        lines.append(json.dumps(frame, separators=(",", ":")))
    text = "\n".join(lines).replace("/private/tmp/probe", "/tmp/probe")
    if thread:
        text = text.replace(thread, FIXTURE_THREAD)
    return text + "\n"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixtures", metavar="DIR", help="write driver fixtures from the recordings in --out to DIR, and exit")
    parser.add_argument("--claude", default="claude")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--codex-model", default="gpt-6-luna")
    parser.add_argument("--only", choices=("claude", "codex", "both"), default="both")
    parser.add_argument("--out", default=".audit/probe-edits")
    parser.add_argument("--versions", default="claude=2.1.322,codex=0.167.1")
    args = parser.parse_args()
    out = pathlib.Path(args.out)
    if args.fixtures:
        versions = dict(pair.split("=") for pair in args.versions.split(","))
        for provider in ("claude", "codex"):
            source = out / f"{provider}-edits.json"
            if source.exists():
                path = pathlib.Path(args.fixtures) / f"{provider}-{versions[provider]}-edits.jsonl"
                path.write_text(fixture_lines(provider, json.loads(source.read_text())))
                print(path)
        return
    root = pathlib.Path(tempfile.mkdtemp(prefix="keystone-edits-"))
    results = {}
    try:
        if args.only != "codex":
            results["claude"] = claude(args.claude, root)
        if args.only != "claude":
            results["codex"] = codex(args.codex, root, args.codex_model)
    finally:
        out.mkdir(parents=True, exist_ok=True)
        for provider, events in results.items():
            path = out / f"{provider}-edits.json"
            path.write_text(json.dumps(sanitize(events, root), indent=1) + "\n")
            print(path)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
