#!/usr/bin/env python3
"""Live probe of how Claude Code and Codex ask the user a question.

Records real frames for Claude's AskUserQuestion tool and MCP `elicitation` control
request, and Codex's `item/tool/requestUserInput` and `mcpServer/elicitation/request`,
each answered and declined. Every case runs a real turn on a cheap model in a
disposable git repository, using the user's own login, so it spends subscription
usage: run it by hand, never from the test suite.

`--mcp-server` runs the stdio MCP server the probe gives each CLI. Its one tool,
`choose_plan`, asks the client a form question through `elicitation/create` and
returns whatever comes back.
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

FORM = {
    "type": "object",
    "properties": {
        "size": {"type": "string", "title": "Size", "description": "Which size to use", "enum": ["small", "large"], "enumNames": ["Small", "Large"]},
        "toppings": {"type": "array", "title": "Toppings", "items": {"type": "string", "enum": ["cheese", "olives", "basil"]}},
        "note": {"type": "string", "title": "Note", "description": "Anything else"},
        "rush": {"type": "boolean", "title": "Rush order"},
    },
    "required": ["size"],
}
TOOL = {
    "name": "choose_plan",
    "description": "Asks the user to choose a plan and returns their choice. Call it when asked to choose a plan.",
    "inputSchema": {"type": "object", "properties": {}},
}


def mcp_server():
    def send(message):
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()

    pending_call = None
    for line in sys.stdin:
        message = json.loads(line)
        method = message.get("method")
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": message["id"], "result": {"protocolVersion": message["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "probe", "version": "1"}}})
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": message["id"], "result": {"tools": [TOOL]}})
        elif method == "tools/call":
            pending_call = message["id"]
            send({"jsonrpc": "2.0", "id": "elicit-1", "method": "elicitation/create", "params": {"mode": "form", "message": "Choose a plan for the probe", "requestedSchema": FORM}})
        elif "id" in message and method is None and pending_call is not None:
            text = json.dumps(message.get("result", message.get("error")))
            send({"jsonrpc": "2.0", "id": pending_call, "result": {"content": [{"type": "text", "text": "elicitation answer: " + text}]}})
            pending_call = None
        elif "id" in message and method is not None:
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "not supported"}})


class CLI:
    def __init__(self, name, args, cwd, events, env=None):
        self.name = name
        self.events = events
        self.queue = queue.Queue()
        self.started = time.monotonic()
        self.process = subprocess.Popen(args, cwd=cwd, env=env, text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=1, start_new_session=True)
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

    def until(self, predicate, timeout=180):
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
    (path / "README.md").write_text("Probe repository.\n")
    return path


def mcp_command():
    return [sys.executable, "-I", str(pathlib.Path(__file__).resolve()), "--mcp-server"]


ASK = (
    "Use the AskUserQuestion tool exactly once, with two questions. Question 1, header 'Color': "
    "'Which color do you prefer?' with options Red and Blue, single choice. Question 2, header "
    "'Fruit': 'Which fruits do you like?' with options Apple, Banana and Cherry, multiSelect true. "
    "After the tool returns, reply with one line that repeats exactly what it returned, and stop."
)
PLAN = "Call the choose_plan tool exactly once, then reply with one line that repeats exactly what it returned, and stop."


def claude_case(binary, root, case, prompt, on_request):
    cwd = repo(root, "claude-" + case)
    config = {"mcpServers": {"probe": {"type": "stdio", "command": mcp_command()[0], "args": mcp_command()[1:]}}}
    args = [binary, "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--replay-user-messages", "--permission-prompts", "host", "--permission-prompt-tool", "stdio", "--model", "haiku", "--strict-mcp-config", "--mcp-config", json.dumps(config), "--settings", '{"disableAllHooks":true}']
    events = []
    cli = CLI("claude", args, cwd, events)
    try:
        cli.send({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}})
        cli.until(lambda m: m.get("type") == "control_response")
        cli.send({"type": "user", "uuid": "6b657973-746f-46e6-8005-000000000001", "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}, "parent_tool_use_id": None})
        while True:
            message = cli.until(lambda m: m.get("type") in ("control_request", "result"))
            if message is None or message.get("type") == "result":
                break
            request = message["request"]
            if request.get("subtype") == "can_use_tool" and request.get("tool_name") != "AskUserQuestion":
                answer = {"behavior": "allow", "updatedInput": request.get("input", {})}
            else:
                answer = on_request(request)
            cli.send({"type": "control_response", "response": {"subtype": "success", "request_id": message["request_id"], "response": answer}})
        time.sleep(1)
    finally:
        cli.close()
    return events


def claude(binary, root):
    def answer_questions(request):
        questions = request["input"]["questions"]
        answers = {questions[0]["question"]: "Blue", questions[1]["question"]: "Apple, Cherry"}
        return {"behavior": "allow", "updatedInput": {"questions": questions, "answers": answers}}

    def decline_questions(request):
        return {"behavior": "deny", "message": "The user declined to answer."}

    def accept_form(request):
        return {"action": "accept", "content": {"size": "large", "toppings": ["cheese", "basil"], "note": "extra crisp", "rush": True}}

    def decline_form(request):
        return {"action": "decline"}

    return {
        "ask-answered": claude_case(binary, root, "ask-answered", ASK, answer_questions),
        "ask-declined": claude_case(binary, root, "ask-declined", ASK, decline_questions),
        "elicit-accepted": claude_case(binary, root, "elicit-accepted", PLAN, accept_form),
        "elicit-declined": claude_case(binary, root, "elicit-declined", PLAN, decline_form),
    }


CODEX_ASK = (
    "Use the request_user_input tool exactly once with two questions: id 'color', header 'Color', "
    "'Which color do you prefer?' with options Red and Blue; and id 'name', header 'Name', 'What should "
    "the file be called?' with options notes.txt and todo.txt, and isOther true so I can type my own. Give every option a one-sentence description. After it returns, reply with one line that repeats exactly "
    "what it returned, and stop. Do not run any commands."
)


def codex_case(binary, root, case, prompt, extra_config, collaboration, on_request, model):
    cwd = repo(root, "codex-" + case)
    command = mcp_command()
    args = [binary, "app-server", "--listen", "stdio://", "-c", f"mcp_servers.probe.command={json.dumps(command[0])}", "-c", f"mcp_servers.probe.args={json.dumps(command[1:])}"]
    for item in extra_config:
        args += ["-c", item]
    events = []
    cli = CLI("codex", args, cwd, events)
    try:
        cli.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "keystone-probe", "title": "Keystone Probe", "version": "0"}, "capabilities": {"experimentalApi": True}}})
        cli.until(lambda m: m.get("id") == 1)
        cli.send({"method": "initialized"})
        cli.send({"id": 2, "method": "thread/start", "params": {"cwd": str(cwd), "model": model, "ephemeral": True, "approvalPolicy": "on-request", "sandbox": "workspace-write"}})
        started = cli.until(lambda m: m.get("id") == 2)
        if not started or "result" not in started:
            return events
        thread = started["result"]["thread"]["id"]
        params = {"threadId": thread, "input": [{"type": "text", "text": prompt, "text_elements": []}], "effort": "low"}
        if collaboration:
            params["collaborationMode"] = {"mode": collaboration, "settings": {"model": model, "reasoning_effort": "low"}}
        cli.send({"id": 3, "method": "turn/start", "params": params})
        while True:
            message = cli.until(lambda m: m.get("method") == "turn/completed" or ("method" in m and "id" in m) or (m.get("id") == 3 and "error" in m))
            if message is None or message.get("method") == "turn/completed" or message.get("id") == 3:
                break
            reply = on_request(message)
            cli.send({"id": message["id"], **reply})
        time.sleep(1)
    finally:
        cli.close()
    return events


def codex(binary, root, model):
    def answer(message):
        method = message["method"]
        params = message["params"]
        if method == "item/tool/requestUserInput":
            answers = {}
            for question in params["questions"]:
                options = question.get("options") or []
                answers[question["id"]] = {"answers": ["probe-notes.txt"] if question.get("isOther") else [options[-1]["label"]]}
            return {"result": {"answers": answers}}
        if method == "mcpServer/elicitation/request":
            schema = params.get("requestedSchema") or {}
            if "size" in (schema.get("properties") or {}):
                return {"result": {"action": "accept", "content": {"size": "large", "toppings": ["cheese", "basil"], "note": "extra crisp", "rush": True}}}
            return {"result": {"action": "accept", "content": {}}}
        if method.endswith("requestApproval"):
            return {"result": {"decision": "accept"}}
        return {"error": {"code": -32601, "message": "probe does not answer " + method}}

    def decline(message):
        method = message["method"]
        if method == "item/tool/requestUserInput":
            return {"result": {"answers": {}}}
        if method == "mcpServer/elicitation/request":
            schema = message["params"].get("requestedSchema") or {}
            if "size" in (schema.get("properties") or {}):
                return {"result": {"action": "decline"}}
            return {"result": {"action": "accept", "content": {}}}
        return answer(message)

    feature = ["features.default_mode_request_user_input=true"]
    return {
        "ask-default-mode": codex_case(binary, root, "ask-default-mode", CODEX_ASK, [], None, answer, model),
        "ask-plan-mode": codex_case(binary, root, "ask-plan-mode", CODEX_ASK, [], "plan", answer, model),
        "ask-feature-answered": codex_case(binary, root, "ask-feature-answered", CODEX_ASK, feature, None, answer, model),
        "ask-feature-declined": codex_case(binary, root, "ask-feature-declined", CODEX_ASK, feature, None, decline, model),
        "elicit-accepted": codex_case(binary, root, "elicit-accepted", PLAN, [], None, answer, model),
        "elicit-declined": codex_case(binary, root, "elicit-declined", PLAN, [], None, decline, model),
    }


def sanitize(value, root):
    text = json.dumps(value)
    text = text.replace(str(root), "/tmp/probe")
    text = text.replace(str(pathlib.Path.home()), "/home/user")
    text = re.sub(r'"(email|account_uuid|organization_uuid|accountId|userId)": ?"[^"]*"', r'"\1": "<redacted>"', text)
    return json.loads(text)


INIT_KEEP = ("type", "subtype", "cwd", "session_id", "model", "permissionMode", "claude_code_version", "capabilities", "uuid")
FIXTURE_THREAD = "01a1418d-549d-7553-bd08-4183fac44aa7"
FIXTURE_CASES = {
    "claude": ["ask-answered", "ask-declined", "elicit-accepted", "elicit-declined"],
    "codex": ["ask-feature-answered", "ask-feature-declined", "elicit-accepted", "elicit-declined"],
}


def fixture_lines(provider, events):
    """What the provider printed, for a driver to replay. Claude's `initialize` answer
    and the inventory in `system/init` list the user's own skills, commands and tools,
    so they are left out, as are Codex's account reports and the user's own MCP
    servers. Codex's handshake answers are left out, since the driver tests make their
    own, and each recording's thread ID becomes the tests' one."""
    lines = []
    thread = None
    for event in events:
        frame = event["frame"]
        if event["dir"] != "out" or not isinstance(frame, dict):
            continue
        if provider == "claude":
            if frame.get("type") == "control_response" and "commands" in json.dumps(frame.get("response", {}).get("response", {}))[:20]:
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
            # The account's plan and usage, and the user's own MCP servers.
            if frame.get("method", "").startswith("account/"):
                continue
            if frame.get("method") == "mcpServer/startupStatus/updated" and frame["params"].get("name") != "probe":
                continue
        lines.append(json.dumps(frame, separators=(",", ":")))
    text = "\n".join(lines).replace("/private/tmp/probe", "/tmp/probe")
    if thread:
        text = text.replace(thread, FIXTURE_THREAD)
    return text + "\n"


def write_fixtures(recordings, target, versions):
    for provider, cases in FIXTURE_CASES.items():
        for case in cases:
            events = json.loads((recordings / f"{provider}-{case}.json").read_text())
            path = target / f"{provider}-{versions[provider]}-{case.replace('-feature', '')}.jsonl"
            path.write_text(fixture_lines(provider, events))
            print(path)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mcp-server", action="store_true")
    parser.add_argument("--fixtures", metavar="DIR", help="write driver fixtures from the recordings in --out to DIR, and exit")
    parser.add_argument("--claude", default="claude")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--codex-model", default="gpt-6-luna")
    parser.add_argument("--only", choices=("claude", "codex", "both"), default="both")
    parser.add_argument("--out", default=".audit/probe-questions")
    args = parser.parse_args()
    if args.mcp_server:
        return mcp_server()
    if args.fixtures:
        versions = {"claude": "2.1.316", "codex": "0.166.0"}
        return write_fixtures(pathlib.Path(args.out), pathlib.Path(args.fixtures), versions)
    root = pathlib.Path(tempfile.mkdtemp(prefix="keystone-questions-"))
    results = {}
    try:
        if args.only != "codex":
            results["claude"] = claude(args.claude, root)
        if args.only != "claude":
            results["codex"] = codex(args.codex, root, args.codex_model)
    finally:
        out = pathlib.Path(args.out)
        out.mkdir(parents=True, exist_ok=True)
        for provider, cases in results.items():
            for case, events in cases.items():
                path = out / f"{provider}-{case}.json"
                path.write_text(json.dumps(sanitize(events, root), indent=1) + "\n")
                print(path)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
