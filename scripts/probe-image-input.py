#!/usr/bin/env python3
"""No-inference probe of how Claude Code and Codex take image input.

Run it on a machine with the tested CLIs (Claude Code 2.1.301, Codex 0.163.0):

    python3 scripts/probe-image-input.py --out probe.json

For each attachment type Keystone accepts (png, jpeg, gif, webp) each CLI gets one
user message carrying a small fixture image, sent the way Keystone sends it: a
stream-json `user` line whose content is a text block and a base64 `image` block for
Claude, and a `text` plus a `localImage` input item on `turn/start` for Codex. The probe
records every request the CLI then makes to its model API and the images in it: their
media type and the SHA-256 of their bytes, compared with the fixture's.

Safety: the whole probe runs under `unshare -rn`, a private user namespace whose only
network is its own loopback. The model API each CLI is pointed at is a fake server on
that loopback, which answers every request with a 400 error, so no model is ever
reached. Every CLI runs with a fresh HOME, a fresh CLAUDE_CONFIG_DIR or CODEX_HOME, and
an environment rebuilt from scratch whose only key is the dummy one the fake server
accepts. Paths under the scratch root are replaced by `<root>`.
"""
import argparse
import base64
import hashlib
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
MESSAGE = "What is in this image?"
MENTION = "Summarise @notes.txt and the image"
SENTINEL = "keystone-mention-sentinel-5d1c"
TYPES = {"png": "image/png", "jpeg": "image/jpeg", "gif": "image/gif", "webp": "image/webp"}
# A noisy 2400 x 1800 PNG of about 13 MB, to see whether either CLI resizes or re-encodes
# a large image before it reaches the model.
LARGE = ("large.png", "image/png", ["-size", "2400x1800", "xc:", "+noise", "Random", "-depth", "8"])
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
            chunk = os.read(self.proc.stdout.fileno(), 1 << 20)
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


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def images_in(value):
    """Every image in a model request: Anthropic `image` blocks with a base64 source,
    and OpenAI `input_image` parts with a data URL, as media type, byte hash and size."""
    found = []
    if isinstance(value, dict):
        source = value.get("source")
        if value.get("type") == "image" and isinstance(source, dict):
            data = base64.b64decode(source.get("data") or "")
            found.append({"block": "image", "source_type": source.get("type"),
                          "media_type": source.get("media_type"),
                          "sha256": sha(data), "bytes": len(data)})
        elif value.get("type") == "input_image":
            url = value.get("image_url") or ""
            head, _, data = url.partition(",")
            raw = base64.b64decode(data) if head.endswith(";base64") else b""
            found.append({"block": "input_image", "media_type": head[5:].split(";")[0],
                          "sha256": sha(raw), "bytes": len(raw)})
        for child in value.values():
            found += images_in(child)
    elif isinstance(value, list):
        for child in value:
            found += images_in(child)
    return found


def model_requests(since):
    shown = []
    for request in Recorder.requests[since:]:
        if "/messages" not in request["path"] and "/responses" not in request["path"]:
            continue
        try:
            body = json.loads(request["body"])
        except json.JSONDecodeError:
            shown.append({"path": request["path"], "unparsed": True})
            continue
        shown.append({"path": request["path"], "images": images_in(body),
                      "typed_text_present": MESSAGE in request["body"],
                      "mentioned_file_present": SENTINEL in request["body"]})
    return shown


def verdict(fixture, requests):
    """Whether the first model request carries exactly the fixture's bytes and type."""
    first = requests[0]["images"] if requests and "images" in requests[0] else []
    return {"model_requests": len(requests),
            "image_count_in_first_request": len(first),
            "same_bytes": any(i["sha256"] == fixture["sha256"] for i in first),
            "same_media_type": any(i["media_type"] == fixture["media_type"] for i in first)}


def probe_claude(binary, root, port, fixture, text=None):
    """Sends `text` (else MESSAGE) and `fixture` as blocks, or `text` alone as a string
    when `fixture` is None."""
    home, config, project = root / "claude-home", root / "claude-config", root / "project"
    shutil.rmtree(home, ignore_errors=True)
    shutil.rmtree(config, ignore_errors=True)
    home.mkdir()
    config.mkdir()
    since = len(Recorder.requests)
    p = Process([binary, *CLAUDE_ARGS], env(home, CLAUDE_CONFIG_DIR=config,
                                           ANTHROPIC_API_KEY=DUMMY_KEY,
                                           ANTHROPIC_BASE_URL=f"http://127.0.0.1:{port}",
                                           DISABLE_TELEMETRY=1, DISABLE_AUTOUPDATER=1),
                project)
    p.send({"type": "control_request", "request_id": "probe", "request": {"subtype": "initialize"}})
    content = text
    if fixture is not None:
        data = base64.b64encode(Path(fixture["path"]).read_bytes()).decode()
        content = [{"type": "text", "text": text or MESSAGE},
                   {"type": "image", "source": {"type": "base64",
                                                "media_type": fixture["media_type"],
                                                "data": data}}]
    p.send({"type": "user", "message": {"role": "user", "content": content},
            "parent_tool_use_id": None, "uuid": "00000000-0000-4000-8000-000000000001"})
    deadline = time.time() + 90
    replayed, result = None, None
    while result is None:
        message = p.line(deadline)
        if message is None:
            break
        if message.get("type") == "user" and replayed is None:
            content = message.get("message", {}).get("content")
            replayed = [{"type": b.get("type"), **({"text": b["text"]} if "text" in b else {}),
                         **({"media_type": b["source"].get("media_type")}
                            if isinstance(b.get("source"), dict) else {})}
                        for b in content if isinstance(b, dict)] if isinstance(content, list) \
                else content
        if message.get("type") == "result":
            result = {k: message.get(k) for k in ("subtype", "is_error", "result")}
    code, stderr = p.stop()
    requests = model_requests(since)
    return {"replayed_user_content": replayed, "result": result, "exit": code,
            "stderr_tail": stderr, "requests": requests,
            **(verdict(fixture, requests) if fixture else {})}


def probe_codex(binary, root, port, fixture):
    home = codex_home = root / "codex-home"
    project = root / "project"
    shutil.rmtree(codex_home, ignore_errors=True)
    codex_home.mkdir()
    (codex_home / "config.toml").write_text(
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
    user_item = None
    if thread:
        p.send({"id": 3, "method": "turn/start", "params": {"threadId": thread, "input": [
            {"type": "text", "text": MESSAGE},
            {"type": "localImage", "path": fixture["path"]}]}})
        answers["turn_start_error"] = (wait(3) or {}).get("error")
        deadline = time.time() + 60
        while True:
            message = p.line(deadline)
            if message is None or message.get("method") == "turn/completed":
                answers["turn_completed"] = message is not None
                break
            item = (message.get("params") or {}).get("item") or {}
            if message.get("method") == "item/completed" and item.get("type") == "userMessage":
                user_item = item.get("content")
    code, stderr = p.stop()
    requests = model_requests(since)
    return {"answers": answers, "user_message_item_content": user_item, "exit": code,
            "stderr_tail": stderr, "requests": requests, **verdict(fixture, requests)}


def scrub(value, root: Path):
    return json.loads(json.dumps(value).replace(str(root), "<root>"))


def inner(args):
    subprocess.run(["ip", "link", "set", "lo", "up"], check=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Recorder)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]
    root = Path(tempfile.mkdtemp(prefix="image-probe-"))
    global ROOT
    ROOT = str(root)
    try:
        project = root / "project"
        project.mkdir()
        subprocess.run(["git", "init", "-q", str(project)], check=True)
        (project / "notes.txt").write_text(SENTINEL + "\n")
        fixtures = {}
        for kind, media_type in TYPES.items():
            path = root / "fixtures" / f"probe.{kind}"
            path.parent.mkdir(exist_ok=True)
            subprocess.run([args.magick, "-size", "64x48", "gradient:#2a6f97-#f4a261",
                            str(path)], check=True)
            data = path.read_bytes()
            fixtures[kind] = {"path": str(path), "media_type": media_type,
                              "sha256": sha(data), "bytes": len(data)}
        name, media_type, recipe = LARGE
        path = root / "fixtures" / name
        subprocess.run([args.magick, *recipe, str(path)], check=True)
        data = path.read_bytes()
        fixtures["large-png"] = {"path": str(path), "media_type": media_type,
                                 "sha256": sha(data), "bytes": len(data)}
        result = {"time_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                  "message": MESSAGE, "fixtures": fixtures}
        for name, binary in (("claude", args.claude), ("codex", args.codex)):
            home = root / f"{name}-version-home"
            home.mkdir()
            version = subprocess.run([binary, "--version"], capture_output=True, text=True,
                                     cwd=home, env=env(home, CLAUDE_CONFIG_DIR=home / "claude",
                                                       CODEX_HOME=home / "codex"))
            result[f"{name}_version"] = version.stdout.strip()
        result["claude"] = {kind: probe_claude(args.claude, root, port, fixture)
                            for kind, fixture in fixtures.items()}
        # A file mention beside an image: does Claude Code still read the file when the
        # message is a list of blocks rather than a string?
        result["claude_mention_beside_image"] = probe_claude(
            args.claude, root, port, fixtures["png"], text=MENTION)
        result["claude_mention_alone"] = probe_claude(args.claude, root, port, None,
                                                      text=MENTION)
        result["codex"] = {kind: probe_codex(args.codex, root, port, fixture)
                           for kind, fixture in fixtures.items()}
        Path(args.out).write_text(json.dumps(scrub(result, root), indent=1) + "\n")
        print(f"wrote {args.out}")
    finally:
        server.shutdown()
        shutil.rmtree(root, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--claude", default=shutil.which("claude"))
    parser.add_argument("--codex", default=shutil.which("codex"))
    parser.add_argument("--magick", default=shutil.which("magick"))
    parser.add_argument("--out", required=True)
    parser.add_argument("--inner", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.inner:
        return inner(args)
    argv = [sys.executable, os.path.abspath(__file__), "--inner", "--claude",
            os.path.realpath(args.claude), "--codex", os.path.realpath(args.codex),
            "--magick", os.path.realpath(args.magick), "--out", os.path.abspath(args.out)]
    return subprocess.run(["unshare", "-rn", *argv]).returncode


if __name__ == "__main__":
    sys.exit(main())
