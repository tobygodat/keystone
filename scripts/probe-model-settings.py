#!/usr/bin/env python3
"""Live probe of how Claude Code and Codex take a model, an effort level and fast mode,
at launch and between the turns of one conversation (TOB-297).

This spends real tokens on the user's own login, so it never runs from the test suite:

    python3 scripts/probe-model-settings.py --out docs/research/model-settings-probe.json

Every turn asks for one short word in a disposable git repository under a temporary
directory, on the cheapest models the catalogs list, except one fast-mode turn per
provider. Nothing from the user's own configuration is recorded besides the fields named
here; the initialize answer, which lists the user's skills, is reduced to its model
entries and fast-mode fields.
"""
import argparse
import http.client
import http.server
import json
import socketserver
import ssl
import threading
import os
import select
import subprocess
import sys
import tempfile
import time
from pathlib import Path

CLAUDE_ARGS = ["-p", "--input-format", "stream-json", "--output-format", "stream-json",
               "--verbose", "--include-partial-messages", "--replay-user-messages",
               "--permission-prompts", "host", "--permission-prompt-tool", "stdio"]
PROMPT = "Reply with the single word {word} and nothing else. Do not use tools."


class ApiLog(http.server.BaseHTTPRequestHandler):
    """Forwards Claude's API calls to api.anthropic.com and records, per `/v1/messages`
    call, only the model and the effort, thinking and speed fields."""
    protocol_version = "HTTP/1.1"
    calls = []

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length") or 0))
        if self.path.startswith("/v1/messages") and body:
            try:
                sent = json.loads(body)
                config = sent.get("output_config") or {}
                ApiLog.calls.append({"model": sent.get("model"), "effort": config.get("effort"),
                                     "structured": "format" in config,
                                     "thinking": sent.get("thinking"), "speed": sent.get("speed")})
            except (json.JSONDecodeError, AttributeError):
                ApiLog.calls.append({"unreadable": True})
        upstream = http.client.HTTPSConnection("api.anthropic.com",
                                               context=ssl.create_default_context())
        headers = {k: v for k, v in self.headers.items()
                   if k.lower() not in ("host", "content-length", "accept-encoding", "connection")}
        headers.update({"host": "api.anthropic.com", "accept-encoding": "identity"})
        upstream.request("POST", self.path, body=body, headers=headers)
        reply = upstream.getresponse()
        self.send_response(reply.status)
        for k, v in reply.getheaders():
            if k.lower() not in ("transfer-encoding", "content-length", "connection",
                                 "content-encoding"):
                self.send_header(k, v)
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        while chunk := reply.read1(65536):
            self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
            self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")

    def log_message(self, *args):
        pass


class Proxy(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


def api_calls_since(mark):
    return [c for c in ApiLog.calls[mark:] if not c.get("structured")]


class Lines:
    def __init__(self, argv, cwd, env=None):
        self.proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, cwd=cwd, env=env)
        self.buffer = b""

    def send(self, message):
        self.proc.stdin.write((json.dumps(message) + "\n").encode())
        self.proc.stdin.flush()

    def read(self, seconds):
        deadline = time.time() + seconds
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
            return {"unparsed": line.decode(errors="replace")[:300]}

    def until(self, done, seconds=180):
        seen = []
        while True:
            message = self.read(seconds)
            if message is None:
                return seen, None
            seen.append(message)
            if done(message):
                return seen, message

    def stop(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        return self.proc.returncode, self.proc.stderr.read().decode(errors="replace")[-1500:]


FAST_KEYS = ("fast_mode_state", "fast_mode_disabled_reason", "fastMode", "fast_mode")


def claude_summary(messages):
    """The fields of a Claude turn that say which model, effort and speed it ran with."""
    out = {"assistant_models": [], "init": None, "result": None, "status": [], "other": []}
    for m in messages:
        kind, sub = m.get("type"), m.get("subtype")
        if kind == "system" and sub == "init":
            out["init"] = {k: m.get(k) for k in ("model", "permissionMode", "effort",
                                                  "effortLevel", *FAST_KEYS) if k in m}
            out["init"]["keys"] = sorted(m)
        elif kind == "system" and sub == "status":
            out["status"].append({k: v for k, v in m.items() if k not in ("session_id", "uuid")})
        elif kind == "assistant":
            model = (m.get("message") or {}).get("model")
            if model and model not in out["assistant_models"]:
                out["assistant_models"].append(model)
        elif kind == "result":
            out["result"] = {k: m.get(k) for k in ("subtype", "is_error", "result",
                                                   "modelUsage", *FAST_KEYS) if k in m}
            if isinstance(out["result"].get("modelUsage"), dict):
                out["result"]["modelUsage"] = sorted(out["result"]["modelUsage"])
            out["result"]["keys"] = sorted(m)
            out["session_id"] = m.get("session_id")
        elif kind == "system" and sub not in ("init",):
            out["other"].append({"subtype": sub, **{k: m.get(k) for k in FAST_KEYS if k in m}})
    return out


def claude_control(p, request_id, request):
    p.send({"type": "control_request", "request_id": request_id, "request": request})
    seen, answer = p.until(lambda m: m.get("type") == "control_response"
                           and m.get("response", {}).get("request_id") == request_id, 60)
    response = (answer or {}).get("response") or {}
    body = response.get("response")
    if request["subtype"] == "get_settings" and isinstance(body, dict):
        effective = body.get("effective") or {}
        body = {"effective": {k: effective.get(k) for k in ("model", "effortLevel", "fastMode")}}
    return {"sent": request, "subtype": response.get("subtype"), "error": response.get("error"),
            "response": body,
            "other_frames": [{"type": m.get("type"), "subtype": m.get("subtype"),
                              **{k: m.get(k) for k in FAST_KEYS if k in m}}
                             for m in seen if m is not answer]}


def claude_turn(p, word):
    mark = len(ApiLog.calls)
    p.send({"type": "user", "session_id": "", "parent_tool_use_id": None,
            "message": {"role": "user", "content": PROMPT.format(word=word)}})
    seen, _ = p.until(lambda m: m.get("type") == "result")
    summary = claude_summary(seen)
    summary["api_calls"] = api_calls_since(mark)
    return summary


def claude_launch(binary, cwd, extra, base_url):
    env = {**os.environ, "ANTHROPIC_BASE_URL": base_url}
    p = Lines([binary, *CLAUDE_ARGS, *extra], cwd, env)
    init = claude_control(p, "init", {"subtype": "initialize"})
    response = init.get("response") or {}
    reduced = {k: response.get(k) for k in ("fast_mode_state", "fast_mode_disabled_reason",
                                            "current_permission_mode", "claude_code_version")}
    reduced["models"] = [{k: v for k, v in m.items() if k != "description"}
                         for m in response.get("models") or []]
    return p, {"argv_extra": extra, "initialize": reduced}


def probe_claude(binary, repo):
    proxy = Proxy(("127.0.0.1", 0), ApiLog)
    threading.Thread(target=proxy.serve_forever, daemon=True).start()
    base_url = f"http://127.0.0.1:{proxy.server_address[1]}"
    steps = []
    p, launch = claude_launch(binary, repo, ["--model", "haiku", "--effort", "low"], base_url)
    steps.append({"launch": launch})
    steps.append({"turn_1": claude_turn(p, "one")})
    steps.append({"get_settings_1": claude_control(p, "s1", {"subtype": "get_settings"})})
    steps.append({"set_model_sonnet": claude_control(p, "m1", {"subtype": "set_model",
                                                               "model": "sonnet"})})
    steps.append({"turn_2": claude_turn(p, "two")})
    steps.append({"apply_effort_high": claude_control(p, "e1", {
        "subtype": "apply_flag_settings", "settings": {"effortLevel": "high"}})})
    steps.append({"get_settings_2": claude_control(p, "s2", {"subtype": "get_settings"})})
    steps.append({"apply_effort_max": claude_control(p, "e2", {
        "subtype": "apply_flag_settings", "settings": {"effortLevel": "max"}})})
    steps.append({"get_settings_3": claude_control(p, "s3", {"subtype": "get_settings"})})
    steps.append({"turn_3_after_max": claude_turn(p, "three")})
    steps.append({"apply_effort_null": claude_control(p, "e3", {
        "subtype": "apply_flag_settings", "settings": {"effortLevel": None}})})
    steps.append({"turn_3b_after_null": claude_turn(p, "three")})
    steps.append({"apply_fast_on_sonnet": claude_control(p, "f1", {
        "subtype": "apply_flag_settings", "settings": {"fastMode": True}})})
    steps.append({"set_model_opus": claude_control(p, "m2", {"subtype": "set_model",
                                                             "model": "opus"})})
    steps.append({"turn_4_opus_fast": claude_turn(p, "four")})
    steps.append({"apply_fast_off": claude_control(p, "f2", {
        "subtype": "apply_flag_settings", "settings": {"fastMode": False}})})
    steps.append({"set_model_default": claude_control(p, "m3", {"subtype": "set_model",
                                                                "model": "default"})})
    steps.append({"set_model_haiku": claude_control(p, "m4", {"subtype": "set_model",
                                                              "model": "haiku"})})
    last = claude_turn(p, "five")
    session = last.pop("session_id", None)
    steps.append({"turn_5": last})
    code, stderr = p.stop()
    steps.append({"exit": code, "stderr_tail": stderr})
    if session:
        p, launch = claude_launch(binary, repo, ["--resume", session, "--model", "sonnet",
                                                 "--effort", "medium",
                                                 "--settings", json.dumps({"fastMode": False})],
                                  base_url)
        steps.append({"resume_launch": launch})
        steps.append({"resume_turn": claude_turn(p, "six")})
        code, stderr = p.stop()
        steps.append({"resume_exit": code, "stderr_tail": stderr})
        p, launch = claude_launch(binary, repo, ["--model", "haiku", "--effort", "max"],
                                  base_url)
        steps.append({"max_flag_launch": launch})
        steps.append({"max_flag_turn": claude_turn(p, "seven")})
        p.stop()
        p, launch = claude_launch(binary, repo, ["--model", "opus", "--settings",
                                                 json.dumps({"fastMode": True})], base_url)
        steps.append({"fast_flag_launch": launch})
        code, stderr = p.stop()
        steps.append({"fast_flag_exit": code})
    proxy.shutdown()
    return steps


CODEX_SETTINGS = ("model", "effort", "serviceTier", "reasoningEffort")


def codex_probe(binary, repo):
    p = Lines([binary, "app-server"], repo)
    counter = iter(range(1, 1000))
    log = []

    def request(method, params, seconds=180):
        rid = next(counter)
        p.send({"id": rid, "method": method, "params": params})
        seen, answer = p.until(lambda m: m.get("id") == rid and "method" not in m, seconds)
        return seen, answer

    def settings(frames):
        out = []
        for m in frames:
            method = m.get("method")
            params = m.get("params") or {}
            if method == "thread/settings/updated":
                s = params.get("threadSettings") or {}
                out.append({"settings": {k: s.get(k) for k in ("model", "effort", "serviceTier")}})
            elif method == "model/rerouted":
                out.append({"rerouted": params})
            elif method == "turn/completed":
                turn = params.get("turn") or {}
                out.append({"turn_completed": turn.get("status"),
                            "error": (turn.get("error") or {}).get("message")})
            elif method == "error":
                out.append({"error": params})
        return out

    def turn(thread, word, **overrides):
        params = {"threadId": thread, "input": [{"type": "text", "text": PROMPT.format(word=word)}],
                  **overrides}
        frames, answer = request("turn/start", params)
        record = {"sent": overrides, "answer_error": (answer or {}).get("error")}
        if answer and "result" in answer:
            more, _ = p.until(lambda m: m.get("method") == "turn/completed", 180)
            frames += more
        record["notifications"] = settings(frames)
        _, read = request("thread/read", {"threadId": thread})
        stored = ((read or {}).get("result") or {}).get("thread") or {}
        record["thread_read"] = {k: stored.get(k) for k in ("model", "reasoningEffort")}
        return record

    request("initialize", {"clientInfo": {"name": "keystone", "title": "Keystone",
                                          "version": "probe"},
                           "capabilities": {"experimentalApi": True}})
    p.send({"method": "initialized"})
    _, models = request("model/list", {})
    data = (models or {}).get("result", {}).get("data", [])
    log.append({"model_list": [{k: m.get(k) for k in (
        "model", "hidden", "isDefault", "defaultReasoningEffort", "supportedReasoningEfforts",
        "serviceTiers", "defaultServiceTier", "additionalSpeedTiers")} for m in data]})
    visible = [m["model"] for m in data if not m.get("hidden")]
    cheap = next((m for m in visible if m.endswith("luna")), visible[-1])
    other = next((m for m in visible if m.endswith("luna") and m != cheap), visible[0])
    frames, started = request("thread/start", {"cwd": repo, "model": cheap})
    result = (started or {}).get("result") or {}
    thread = result.get("thread", {}).get("id")
    log.append({"thread_start": {k: result.get(k) for k in ("model", "reasoningEffort",
                                                           "serviceTier")},
                "notifications": settings(frames)})
    log.append({"turn_1_low_default_tier": turn(thread, "one", effort="low",
                                                serviceTier="default")})
    log.append({"turn_2_nothing_sent": turn(thread, "two")})
    log.append({"turn_3_other_model_medium": turn(thread, "three", model=other,
                                                  effort="medium")})
    log.append({"turn_4_priority": turn(thread, "four", serviceTier="priority")})
    log.append({"turn_5_tier_null": turn(thread, "five", serviceTier=None)})
    log.append({"turn_6_tier_default": turn(thread, "six", serviceTier="default")})
    log.append({"turn_7_unsupported_effort": turn(thread, "seven", effort="ultra")})
    log.append({"turn_8_unknown_tier": turn(thread, "eight", serviceTier="flex")})
    p.stop()
    p = Lines([binary, "app-server"], repo)
    counter = iter(range(1, 1000))
    request("initialize", {"clientInfo": {"name": "keystone", "title": "Keystone",
                                          "version": "probe"}, "capabilities": {}})
    p.send({"method": "initialized"})
    frames, resumed = request("thread/resume", {"threadId": thread})
    result = (resumed or {}).get("result") or {}
    log.append({"resume_without_settings": {k: result.get(k) for k in (
        "model", "reasoningEffort", "serviceTier")}, "error": (resumed or {}).get("error")})
    p.stop()
    return log


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--claude", default=os.environ.get("KEYSTONE_CLAUDE_PATH", "claude"))
    parser.add_argument("--codex", default=os.environ.get("KEYSTONE_CODEX_PATH", "codex"))
    parser.add_argument("--only", choices=["claude", "codex"])
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="keystone-model-probe-") as root:
        repo = os.path.join(root, "repo")
        os.mkdir(repo)
        subprocess.run(["git", "init", "-q", repo], check=True)
        report = {
            "claude_version": subprocess.run([args.claude, "--version"], capture_output=True,
                                             text=True).stdout.strip(),
            "codex_version": subprocess.run([args.codex, "--version"], capture_output=True,
                                            text=True).stdout.strip(),
        }
        if args.only != "codex":
            report["claude"] = probe_claude(args.claude, repo)
        if args.only != "claude":
            report["codex"] = codex_probe(args.codex, repo)
    text = json.dumps(report, indent=1).replace(str(Path.home()), "~")
    Path(args.out).write_text(text + "\n")
    print(f"wrote {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
