#!/usr/bin/env python3
import argparse
import json
import os
import pathlib
import queue
import signal
import secrets
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BASE = pathlib.Path(os.environ.get("KEYSTONE_PROBE_DIR", pathlib.Path.home() / ".cache/keystone/keystone-mcp-probe"))
BASE.mkdir(mode=0o700, parents=True, exist_ok=True)
TOKEN = secrets.token_hex(24)
ROOT = pathlib.Path(tempfile.mkdtemp(prefix="private-", dir=BASE))
os.chmod(ROOT, 0o700)
EVENTS = []
PHASE = "setup"


def clean(value):
    text = json.dumps(value).replace(TOKEN, "<probe-bearer>").replace(str(ROOT), "<private-root>")
    return json.loads(text)


def record(kind, **data):
    EVENTS.append(clean({"phase": PHASE, "kind": kind, **data}))


class MCP(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, code, body=None):
        payload = json.dumps(body).encode() if body is not None else b""
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        record("mcp_get", path=self.path)
        self.reply(405)

    def do_DELETE(self):
        record("mcp_delete", path=self.path)
        self.reply(200)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        authorized = self.headers.get("Authorization") == "Bearer " + TOKEN
        record("mcp_request", path=self.path, authorized=authorized, request=request)
        if not authorized:
            self.reply(401, {"error": "probe bearer required"})
            return
        if "id" not in request:
            self.reply(202)
            return
        method = request.get("method")
        if method == "initialize":
            result = {"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "keystone-probe", "version": "1"}}
        elif method == "tools/list":
            result = {"tools": [{"name": "probe_noop", "description": "Probe fixture; never call", "inputSchema": {"type": "object", "properties": {}}}]}
        elif method in ("resources/list", "resources/templates/list", "prompts/list"):
            result = {{"resources/list": "resources", "resources/templates/list": "resourceTemplates", "prompts/list": "prompts"}[method]: []}
        elif method == "ping":
            result = {}
        else:
            self.reply(200, {"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32601, "message": "No probe tool calls permitted"}})
            return
        response = {"jsonrpc": "2.0", "id": request["id"], "result": result}
        record("mcp_response", path=self.path, response=response)
        self.reply(200, response)


def env_for(name):
    home = ROOT / name
    home.mkdir(mode=0o700)
    for directory in ("claude", "codex", "cwd", "xdg", "cache", "tmp"):
        (home / directory).mkdir(mode=0o700)
    env = {k: os.environ[k] for k in ("PATH", "LANG", "LC_ALL", "TERM") if k in os.environ}
    env.update(HOME=str(home), CLAUDE_CONFIG_DIR=str(home / "claude"), CODEX_HOME=str(home / "codex"), XDG_CONFIG_HOME=str(home / "xdg"), XDG_CACHE_HOME=str(home / "cache"), TMPDIR=str(home / "tmp"), CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1", DISABLE_TELEMETRY="1")
    return env, home / "cwd"


class CLI:
    def __init__(self, args, env, cwd, name):
        self.name = name
        self.queue = queue.Queue()
        record("spawn", cli=name, args=args, cwd=str(cwd))
        self.process = subprocess.Popen(args, cwd=cwd, env=env, text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=1, start_new_session=True)
        for stream_name in ("stdout", "stderr"):
            def read(stream_name=stream_name):
                for line in getattr(self.process, stream_name):
                    try:
                        payload = json.loads(line)
                    except ValueError:
                        payload = line.rstrip()
                    record("cli_output", cli=name, stream=stream_name, message=payload)
                    self.queue.put(payload)
            threading.Thread(target=read, daemon=True).start()

    def send(self, request):
        assert request.get("type") != "user"
        assert request.get("method") not in ("turn/start", "turn/steer", "mcpServer/tool/call")
        record("cli_input", cli=self.name, message=request)
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()

    def until(self, predicate, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                message = self.queue.get(timeout=min(0.2, deadline - time.monotonic()))
            except queue.Empty:
                if self.process.poll() is not None:
                    return {"probeError": "process exited", "returncode": self.process.returncode}
                continue
            if isinstance(message, dict) and predicate(message):
                return message
        return {"probeError": "timeout"}

    def close(self):
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait()
        finally:
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def claude(binary, url):
    global PHASE
    PHASE = "claude-initialize"
    env, cwd = env_for("claude")
    args = [binary, "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--replay-user-messages", "--permission-prompts", "host", "--permission-prompt-tool", "stdio", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--settings", '{"disableAllHooks":true}']
    client = CLI(args, env, cwd, "claude")
    try:
        client.send({"type": "control_request", "request_id": "initialize", "request": {"subtype": "initialize"}})
        initialized = client.until(lambda m: m.get("response", {}).get("request_id") == "initialize")
        PHASE = "claude-mcp-set-servers"
        client.send({"type": "control_request", "request_id": "set-servers", "request": {"subtype": "mcp_set_servers", "servers": {"keystone": {"type": "http", "url": url + "/claude", "headers": {"Authorization": "Bearer " + TOKEN}}}}})
        configured = client.until(lambda m: m.get("response", {}).get("request_id") == "set-servers")
        client.send({"type": "control_request", "request_id": "status", "request": {"subtype": "mcp_status"}})
        status = client.until(lambda m: m.get("response", {}).get("request_id") == "status")
        return {"initialize": initialized, "setServers": configured, "status": status}
    finally:
        client.close()


def codex(binary, url):
    global PHASE
    env, cwd = env_for("codex")
    provider = 'model_providers.keystone-probe={name="Offline probe",base_url="' + url + '/disabled-model-api",wire_api="responses",requires_openai_auth=false,supports_websockets=false}'
    args = [binary, "app-server", "--listen", "stdio://", "-c", 'model_provider="keystone-probe"', "-c", provider]
    env["OPENAI_BASE_URL"] = url + "/disabled-model-api"
    def start(name):
        client = CLI(args, env, cwd, name)
        client.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "keystone-probe", "title": "Keystone Probe", "version": "0"}}})
        response = client.until(lambda m: m.get("id") == 1)
        client.send({"method": "initialized"})
        return client, response
    def config(path):
        return {"mcp_servers": {"keystone": {"url": url + path, "http_headers": {"Authorization": "Bearer " + TOKEN}}}}
    PHASE = "codex-start"
    client, initialized = start("codex-start")
    try:
        client.send({"id": 2, "method": "thread/start", "params": {"cwd": str(cwd), "config": config("/codex-start")}})
        started = client.until(lambda m: m.get("id") == 2)
        time.sleep(2)
        thread = started.get("result", {}).get("thread", {}).get("id")
        client.send({"id": 3, "method": "mcpServerStatus/list", "params": {"threadId": thread}})
        status = client.until(lambda m: m.get("id") == 3)
        PHASE = "codex-resume-live"
        client.send({"id": 4, "method": "thread/resume", "params": {"threadId": thread, "cwd": str(cwd), "excludeTurns": True, "config": config("/codex-resume-live")}})
        live_resumed = client.until(lambda m: m.get("id") == 4)
        time.sleep(1)
        client.send({"id": 5, "method": "mcpServerStatus/list", "params": {"threadId": thread}})
        live_status = client.until(lambda m: m.get("id") == 5)
    finally:
        client.close()
    if not thread:
        return {"initialize": initialized, "start": started, "startStatus": status, "resume": "not attempted: start failed"}
    PHASE = "codex-resume"
    client, resume_initialized = start("codex-resume")
    try:
        client.send({"id": 2, "method": "thread/resume", "params": {"threadId": thread, "cwd": str(cwd), "excludeTurns": True, "config": config("/codex-resume")}})
        resumed = client.until(lambda m: m.get("id") == 2)
        time.sleep(2)
        client.send({"id": 3, "method": "mcpServerStatus/list", "params": {"threadId": thread}})
        resume_status = client.until(lambda m: m.get("id") == 3)
    finally:
        client.close()
    PHASE = "codex-resume-synthetic-metadata"
    rollout = pathlib.Path(started["result"]["thread"]["path"])
    rollout.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    timestamp = "2026-10-19T00:00:00.000Z"
    metadata = {"timestamp": timestamp, "type": "session_meta", "payload": {"id": thread, "timestamp": timestamp, "cwd": str(cwd), "originator": "keystone-probe", "cli_version": "0.163.0", "source": "cli", "model_provider": started["result"]["modelProvider"]}}
    rollout.write_text(json.dumps(metadata) + "\n")
    rollout.chmod(0o600)
    record("synthetic_rollout_fixture", path=str(rollout), content=metadata, reason="thread/start without a user turn does not persist a rollout")
    client, fixture_initialized = start("codex-resume-fixture")
    try:
        client.send({"id": 2, "method": "thread/resume", "params": {"threadId": thread, "cwd": str(cwd), "excludeTurns": True, "config": config("/codex-resume-fixture")}})
        fixture_resumed = client.until(lambda m: m.get("id") == 2)
        time.sleep(2)
        client.send({"id": 3, "method": "mcpServerStatus/list", "params": {"threadId": thread}})
        fixture_status = client.until(lambda m: m.get("id") == 3)
    finally:
        client.close()
    return {"initialize": initialized, "start": started, "startStatus": status, "liveResume": live_resumed, "liveResumeStatus": live_status, "resumeInitialize": resume_initialized, "coldResume": resumed, "coldResumeStatus": resume_status, "fixtureInitialize": fixture_initialized, "fixtureResume": fixture_resumed, "fixtureResumeStatus": fixture_status}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--claude", default="claude")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--only", choices=("claude", "codex", "both"), default="both")
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", 0), MCP)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    results = {}
    try:
        url = "http://127.0.0.1:" + str(server.server_port)
        if args.only != "codex":
            results["claude"] = claude(args.claude, url)
        if args.only != "claude":
            results["codex"] = codex(args.codex, url)
    finally:
        server.shutdown()
        output = ROOT / "wire.sanitized.json"
        output.write_text(json.dumps(clean({"results": results, "events": EVENTS}), indent=2) + "\n")
        output.chmod(0o600)
        print(str(output))
        print(json.dumps(clean(results), indent=2))


if __name__ == "__main__":
    main()
