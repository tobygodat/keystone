#!/usr/bin/env python3
"""No-inference probe of whether Claude Code and Codex expand `@path` file mentions.

Run it on a machine with the tested CLIs (Claude Code 2.1.301, Codex 0.163.0):

    python3 scripts/probe-file-mentions.py --out probe.json

Each CLI gets one user message, `Summarise @notes.txt, @src/deep/file.rs and
@"my notes/a b.txt"`, sent the way Keystone sends it: a stream-json `user` line for
Claude, and a `text` input item on `turn/start` for Codex. Each mentioned file holds a
sentinel string; the last one is quoted because its path has spaces. The probe records
every request the CLI then makes to its model API and whether a sentinel appears in it,
which is what expanding a mention would put there.

Safety: the whole probe runs under `unshare -rn`, a private user namespace whose only
network is its own loopback. The model API each CLI is pointed at is a fake server on
that loopback, which answers every request with a 400 error, so no model is ever
reached. Every CLI runs with a fresh HOME, a fresh CLAUDE_CONFIG_DIR or CODEX_HOME, and
an environment rebuilt from scratch whose only key is the dummy one the fake server
accepts. Paths under the scratch root are replaced by `<root>`.
"""
import argparse
import http.server
import json
import os
import select
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

CLAUDE_ARGS = ["-p", "--input-format", "stream-json", "--output-format", "stream-json",
               "--verbose", "--include-partial-messages", "--replay-user-messages",
               "--permission-prompts", "host", "--permission-prompt-tool", "stdio"]
MESSAGE = 'Summarise @notes.txt, @src/deep/file.rs and @"my notes/a b.txt"'
SENTINELS = {"notes.txt": "KEYSTONE-SENTINEL-NOTES-7f3a",
             "src/deep/file.rs": "KEYSTONE-SENTINEL-DEEP-91c2",
             "my notes/a b.txt": "KEYSTONE-SENTINEL-SPACED-c04e"}
DUMMY_KEY = "probe-not-a-real-key"
ROOT = ""


class Recorder(http.server.BaseHTTPRequestHandler):
    requests = []

    def handle_any(self):
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length).decode(errors="replace") if length else ""
        Recorder.requests.append({"method": self.command, "path": self.path, "body": body})
        error = json.dumps({"type": "error", "error": {"type": "invalid_request_error",
                                                       "message": "keystone probe: no model here"}})
        self.send_response(400)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(error)))
        self.end_headers()
        self.wfile.write(error.encode())

    do_GET = do_POST = do_PUT = handle_any

    def log_message(self, *_):
        pass


def write(path: Path, text: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def env(home: Path, **extra):
    base = {"HOME": str(home), "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "dumb"}
    base.update({k: str(v) for k, v in extra.items()})
    return base


class Process:
    def __init__(self, argv, environment, cwd):
        self.proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, env=environment, cwd=cwd)
        self.buffer = b""

    def send(self, message: dict):
        self.proc.stdin.write((json.dumps(message) + "\n").encode())
        self.proc.stdin.flush()

    def line(self, deadline: float):
        while b"\n" not in self.buffer:
            left = deadline - time.time()
            if left <= 0:
                return None
            ready, _, _ = select.select([self.proc.stdout], [], [], left)
            if not ready:
                return None
            chunk = os.read(self.proc.stdout.fileno(), 65536)
            if not chunk:
                return None
            self.buffer += chunk
        line, self.buffer = self.buffer.split(b"\n", 1)
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            return {"unparsed": line.decode(errors="replace")[:200]}

    def stop(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        return self.proc.returncode, self.proc.stderr.read().decode(errors="replace")[-1500:]


def blocks(content):
    """A message's content as (type, text) pairs, a string content being one text."""
    if isinstance(content, str):
        return [("text", content)]
    return [(b.get("type"), json.dumps(b) if b.get("type") not in {"text", "input_text"}
             else b.get("text", "")) for b in content or [] if isinstance(b, dict)]


def locate(value, path="$"):
    """The JSON paths of the strings in `value` that hold a sentinel, with 300
    characters around its first one."""
    if isinstance(value, dict):
        return [hit for k, v in value.items() for hit in locate(v, f"{path}.{k}")]
    if isinstance(value, list):
        return [hit for i, v in enumerate(value) for hit in locate(v, f"{path}[{i}]")]
    if isinstance(value, str):
        value = value.replace(ROOT, "<root>") if ROOT else value
        found = [(value.find(s), p) for p, s in SENTINELS.items() if s in value]
        if found:
            at = min(found)[0]
            return [{"at": path, "sentinels": [p for _, p in found],
                     "excerpt": value[max(0, at - 200):at + 40]}]
    return []


def findings(requests, since):
    """What the model requests after `since` carry: whether each sentinel and the typed
    `@path` text appear, and, for each request, its message content blocks that hold the
    typed text or a sentinel (each cut to its first 600 characters, and before any
    environment description Claude appends)."""
    model = [r for r in requests[since:] if "/messages" in r["path"] or "/responses" in r["path"]]
    bodies = "\n".join(r["body"] for r in model)
    shown = []
    for request in model:
        try:
            body = json.loads(request["body"])
        except json.JSONDecodeError:
            shown.append({"path": request["path"], "unparsed": True})
            continue
        hits = []
        for message in body.get("messages") or body.get("input") or []:
            if not isinstance(message, dict):
                continue
            for kind, text in blocks(message.get("content")):
                found = [p for p, s in SENTINELS.items() if s in text]
                if found or MESSAGE in text:
                    text = text.replace(ROOT, "<root>").split("\n\n# Environment")[0]
                    hits.append({"role": message.get("role"), "type": kind, "sentinels": found,
                                 "typed_text": MESSAGE in text, "text": text[:600]})
        shown.append({"path": request["path"], "model": body.get("model"),
                      "blocks_with_hits": hits,
                      "sentinel_locations": locate(body)})
    return {
        "all_requests": [{"method": r["method"], "path": r["path"]} for r in requests[since:]],
        "typed_text_present": json.dumps(MESSAGE)[1:-1] in bodies,
        "sentinels_present": {path: sentinel in bodies for path, sentinel in SENTINELS.items()},
        "model_requests": shown,
    }


def probe_claude(binary, root, port):
    home, config, project = root / "claude-home", root / "claude-config", root / "project"
    home.mkdir()
    config.mkdir()
    since = len(Recorder.requests)
    p = Process([binary, *CLAUDE_ARGS], env(home, CLAUDE_CONFIG_DIR=config,
                                           ANTHROPIC_API_KEY=DUMMY_KEY,
                                           ANTHROPIC_BASE_URL=f"http://127.0.0.1:{port}",
                                           DISABLE_TELEMETRY=1, DISABLE_AUTOUPDATER=1),
                project)
    p.send({"type": "control_request", "request_id": "probe", "request": {"subtype": "initialize"}})
    p.send({"type": "user", "message": {"role": "user", "content": MESSAGE},
            "parent_tool_use_id": None, "uuid": "00000000-0000-4000-8000-000000000001"})
    deadline = time.time() + 90
    frames, replayed, result = [], None, None
    while result is None:
        message = p.line(deadline)
        if message is None:
            break
        frames.append({"type": message.get("type"), "subtype": message.get("subtype")})
        if message.get("type") == "user" and replayed is None:
            replayed = message.get("message", {}).get("content")
        if message.get("type") == "result":
            result = {k: message.get(k) for k in ("subtype", "is_error", "result")}
    code, stderr = p.stop()
    return {"frames": frames, "replayed_user_content": replayed, "result": result,
            "exit": code, "stderr_tail": stderr, **findings(Recorder.requests, since)}


def codex_input_types(binary, root):
    """The input item types app-server's turn/start accepts, from its own schema."""
    out = root / "schema"
    subprocess.run([binary, "app-server", "generate-json-schema", "--out", str(out)],
                   env=env(root / "codex-home", CODEX_HOME=root / "codex-home"),
                   capture_output=True, timeout=120)
    types = set()
    for path in out.rglob("*.json") if out.exists() else []:
        stack = [json.loads(path.read_text())]
        while stack:
            item = stack.pop()
            if isinstance(item, dict):
                defs = item.get("definitions") or item.get("$defs") or {}
                for name, schema in defs.items():
                    if name in {"UserInput", "v2UserInput"} or name.endswith("UserInput"):
                        for variant in schema.get("oneOf") or schema.get("anyOf") or []:
                            kind = variant.get("properties", {}).get("type", {})
                            types.update(kind.get("enum") or [])
                stack.extend(item.values())
            elif isinstance(item, list):
                stack.extend(item)
    return sorted(types)


def probe_codex(binary, root, port, mention_items):
    """One Codex turn. With `mention_items`, the turn also carries a `mention` input
    item for each mentioned file, the item type app-server's schema lists beside
    `text`, to see whether Codex reads a file named that way."""
    home, codex_home, project = root / "codex-home", root / "codex-home", root / "project"
    write(codex_home / "config.toml",
          'model = "probe-model"\nmodel_provider = "probe"\n\n'
          '[model_providers.probe]\nname = "probe"\n'
          f'base_url = "http://127.0.0.1:{port}/v1"\nenv_key = "PROBE_API_KEY"\n'
          'wire_api = "responses"\nrequest_max_retries = 0\nstream_max_retries = 0\n')
    since = len(Recorder.requests)
    p = Process([binary, "app-server"], env(home, CODEX_HOME=codex_home, PROBE_API_KEY=DUMMY_KEY),
                project)
    answers = {}

    def wait(request_id, seconds=30):
        deadline = time.time() + seconds
        while True:
            message = p.line(deadline)
            if message is None:
                return None
            if message.get("id") == request_id and "method" not in message:
                return message

    p.send({"id": 1, "method": "initialize", "params": {
        "clientInfo": {"name": "keystone", "title": "Keystone", "version": "probe"},
        "capabilities": {"optOutNotificationMethods": []}}})
    answers["initialize_error"] = (wait(1) or {}).get("error")
    p.send({"method": "initialized"})
    p.send({"id": 2, "method": "thread/start", "params": {"cwd": str(project)}})
    started = wait(2) or {}
    thread = ((started.get("result") or {}).get("thread") or {}).get("id")
    answers["thread_start_error"] = started.get("error")
    if thread:
        items = [{"type": "text", "text": MESSAGE}]
        if mention_items:
            items += [{"type": "mention", "name": path, "path": str(project / path)}
                      for path in SENTINELS]
        p.send({"id": 3, "method": "turn/start", "params": {"threadId": thread, "input": items}})
        answers["turn_start_error"] = (wait(3) or {}).get("error")
        deadline = time.time() + 60
        while True:
            message = p.line(deadline)
            if message is None or message.get("method") == "turn/completed":
                answers["turn_completed"] = message is not None
                break
    code, stderr = p.stop()
    return {"answers": answers, "exit": code, "stderr_tail": stderr,
            **findings(Recorder.requests, since)}


def scrub(value, root: Path):
    return json.loads(json.dumps(value).replace(str(root), "<root>"))


def inner(args):
    subprocess.run(["ip", "link", "set", "lo", "up"], check=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Recorder)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]
    root = Path(tempfile.mkdtemp(prefix="mention-probe-"))
    global ROOT
    ROOT = str(root)
    try:
        project = root / "project"
        for path, sentinel in SENTINELS.items():
            write(project / path, f"{sentinel}\n")
        subprocess.run(["git", "init", "-q", str(project)], check=True)
        result = {"time_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                  "message": MESSAGE}
        for name, binary in (("claude", args.claude), ("codex", args.codex)):
            home = root / f"{name}-version-home"
            home.mkdir()
            version = subprocess.run([binary, "--version"], capture_output=True, text=True,
                                     cwd=home, env=env(home, CLAUDE_CONFIG_DIR=home / "claude",
                                                       CODEX_HOME=home / "codex"))
            result[f"{name}_version"] = version.stdout.strip()
        result["claude"] = probe_claude(args.claude, root, port)
        result["codex"] = probe_codex(args.codex, root, port, mention_items=False)
        result["codex"]["turn_start_input_types"] = codex_input_types(args.codex, root)
        result["codex_mention_items"] = probe_codex(args.codex, root, port, mention_items=True)
        Path(args.out).write_text(json.dumps(scrub(result, root), indent=1) + "\n")
        print(f"wrote {args.out}")
    finally:
        server.shutdown()
        shutil.rmtree(root, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--claude", default=shutil.which("claude"))
    parser.add_argument("--codex", default=shutil.which("codex"))
    parser.add_argument("--out", required=True)
    parser.add_argument("--inner", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.inner:
        return inner(args)
    argv = [sys.executable, os.path.abspath(__file__), "--inner", "--claude",
            os.path.realpath(args.claude), "--codex", os.path.realpath(args.codex),
            "--out", os.path.abspath(args.out)]
    return subprocess.run(["unshare", "-rn", *argv]).returncode


if __name__ == "__main__":
    sys.exit(main())
