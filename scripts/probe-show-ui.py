#!/usr/bin/env python3
"""No-inference probe of how each CLI hosts Keystone's show_ui tool.

Claude Code: an SDK-hosted MCP server added with mcp_set_servers, whose MCP messages
arrive as mcp_message control requests. Codex: dynamicTools on thread/start, with and
without the experimentalApi capability. Nothing here sends a user message or starts a
turn, and every CLI runs in a disposable home.
"""
import argparse
import json
import os
import pathlib
import queue
import secrets
import signal
import subprocess
import tempfile
import threading
import time

BASE = pathlib.Path(os.environ.get("KEYSTONE_PROBE_DIR", pathlib.Path.home() / ".cache/keystone/keystone-show-ui-probe"))
BASE.mkdir(mode=0o700, parents=True, exist_ok=True)
ROOT = pathlib.Path(tempfile.mkdtemp(prefix="private-", dir=BASE))
os.chmod(ROOT, 0o700)
TOKEN = secrets.token_hex(24)
EVENTS = []
PHASE = "setup"
TOOL = {"name": "show_ui", "description": "Probe fixture; never call", "inputSchema": {"type": "object", "properties": {"messages": {"type": "array"}}, "required": ["messages"]}}


def clean(value):
    text = json.dumps(value).replace(TOKEN, "<probe-bearer>").replace(str(ROOT), "<private-root>")
    return json.loads(text)


def record(kind, **data):
    EVENTS.append(clean({"phase": PHASE, "kind": kind, **data}))


def env_for(name):
    home = ROOT / name
    home.mkdir(mode=0o700)
    for directory in ("claude", "codex", "cwd", "xdg", "cache", "tmp"):
        (home / directory).mkdir(mode=0o700)
    env = {k: os.environ[k] for k in ("PATH", "LANG", "LC_ALL", "TERM") if k in os.environ}
    env.update(HOME=str(home), CLAUDE_CONFIG_DIR=str(home / "claude"), CODEX_HOME=str(home / "codex"), XDG_CONFIG_HOME=str(home / "xdg"), XDG_CACHE_HOME=str(home / "cache"), TMPDIR=str(home / "tmp"), CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1", DISABLE_TELEMETRY="1")
    return env, home / "cwd"


class CLI:
    def __init__(self, args, env, cwd, name, on_message=None):
        self.name = name
        self.queue = queue.Queue()
        self.on_message = on_message
        record("spawn", cli=name, args=args)
        self.process = subprocess.Popen(args, cwd=cwd, env=env, text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=1, start_new_session=True)
        self.lock = threading.Lock()
        for stream_name in ("stdout", "stderr"):
            def read(stream_name=stream_name):
                for line in getattr(self.process, stream_name):
                    try:
                        payload = json.loads(line)
                    except ValueError:
                        payload = line.rstrip()
                    record("cli_output", cli=name, stream=stream_name, message=payload)
                    if isinstance(payload, dict) and self.on_message and self.on_message(self, payload):
                        continue
                    self.queue.put(payload)
            threading.Thread(target=read, daemon=True).start()

    def send(self, request):
        assert request.get("type") != "user"
        assert request.get("method") not in ("turn/start", "turn/steer")
        record("cli_input", cli=self.name, message=request)
        with self.lock:
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


def sdk_server(client, message):
    """Answers mcp_message control requests as Keystone's in-process server does."""
    if message.get("type") != "control_request" or message["request"].get("subtype") != "mcp_message":
        return False
    rpc = message["request"]["message"]
    method = rpc.get("method")
    if "id" not in rpc:
        reply = {"jsonrpc": "2.0", "id": 0, "result": {}}
    elif method == "initialize":
        reply = {"jsonrpc": "2.0", "id": rpc["id"], "result": {"protocolVersion": rpc["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "keystone_ui", "version": "probe"}}}
    elif method == "tools/list":
        reply = {"jsonrpc": "2.0", "id": rpc["id"], "result": {"tools": [TOOL]}}
    else:
        reply = {"jsonrpc": "2.0", "id": rpc["id"], "error": {"code": -32601, "message": "No probe tool calls permitted"}}
    client.send({"type": "control_response", "response": {"subtype": "success", "request_id": message["request_id"], "response": {"mcp_response": reply}}})
    return True


def claude(binary):
    global PHASE
    results = {}
    args = [binary, "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--replay-user-messages", "--permission-prompts", "host", "--permission-prompt-tool", "stdio", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--settings", '{"disableAllHooks":true}']
    sdk = {"type": "sdk", "name": "keystone_ui", "timeout": 86400000, "disableAutoBackground": True}
    http = {"type": "http", "url": "http://127.0.0.1:9/mcp", "headers": {"Authorization": "Bearer " + TOKEN}}
    for case, servers in (("sdk-only", {"keystone_ui": sdk}), ("sdk-and-http", {"keystone": http, "keystone_ui": sdk})):
        PHASE = "claude-" + case
        env, cwd = env_for("claude-" + case)
        client = CLI(args, env, cwd, "claude", sdk_server)
        try:
            client.send({"type": "control_request", "request_id": "initialize", "request": {"subtype": "initialize"}})
            initialized = client.until(lambda m: m.get("response", {}).get("request_id") == "initialize")
            client.send({"type": "control_request", "request_id": "set-servers", "request": {"subtype": "mcp_set_servers", "servers": servers}})
            configured = client.until(lambda m: m.get("response", {}).get("request_id") == "set-servers")
            time.sleep(1)
            client.send({"type": "control_request", "request_id": "status", "request": {"subtype": "mcp_status"}})
            status = client.until(lambda m: m.get("response", {}).get("request_id") == "status")
            results[case] = {"initializeOk": initialized.get("response", {}).get("subtype"), "setServers": configured, "status": status}
        finally:
            client.close()
    return results


def codex(binary):
    global PHASE
    results = {}
    env, cwd = env_for("codex")
    provider = 'model_providers.keystone-probe={name="Offline probe",base_url="http://127.0.0.1:9/disabled-model-api",wire_api="responses",requires_openai_auth=false,supports_websockets=false}'
    args = [binary, "app-server", "--listen", "stdio://", "-c", 'model_provider="keystone-probe"', "-c", provider]
    env["OPENAI_BASE_URL"] = "http://127.0.0.1:9/disabled-model-api"
    spec = dict(TOOL, type="function")
    for case, experimental in (("experimental", True), ("stable", False)):
        PHASE = "codex-" + case
        client = CLI(args, env, cwd, "codex-" + case)
        try:
            capabilities = {"experimentalApi": True} if experimental else {}
            client.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "keystone-probe", "title": "Keystone Probe", "version": "0"}, "capabilities": capabilities}})
            initialized = client.until(lambda m: m.get("id") == 1)
            client.send({"method": "initialized"})
            client.send({"id": 2, "method": "thread/start", "params": {"cwd": str(cwd), "dynamicTools": [spec]}})
            started = client.until(lambda m: m.get("id") == 2)
            results[case] = {"initialize": "error" not in initialized, "threadStart": {"error": started.get("error"), "thread": bool(started.get("result", {}).get("thread"))}}
        finally:
            client.close()
    return results


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--claude", default="claude")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--only", choices=("claude", "codex", "both"), default="both")
    args = parser.parse_args()
    results = {}
    try:
        if args.only != "codex":
            results["claude"] = claude(args.claude)
        if args.only != "claude":
            results["codex"] = codex(args.codex)
    finally:
        output = ROOT / "wire.sanitized.json"
        output.write_text(json.dumps(clean({"results": results, "events": EVENTS}), indent=2) + "\n")
        output.chmod(0o600)
        print(str(output))
        print(json.dumps(clean(results), indent=2))


if __name__ == "__main__":
    main()
