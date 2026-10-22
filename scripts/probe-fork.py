#!/usr/bin/env python3
"""No-inference probe of native conversation forks in Claude Code and Codex.

Run it on a machine with the tested CLIs (Claude Code 2.1.301, Codex 0.163.0):

    python3 scripts/probe-fork.py --out crates/keystone-agent/tests/fixtures/fork-probe.json

Safety: every CLI runs under `unshare -rn` (a private user namespace with no network),
with a fresh HOME, a scratch CLAUDE_CONFIG_DIR or CODEX_HOME and a rebuilt environment
that carries no API keys or OAuth variables. Claude gets only the `initialize` and
`get_context_usage` control requests, which it answers locally. Codex gets
`initialize`, `thread/start`, `thread/shellCommand` (a user shell command recorded as
a turn without a model), `thread/turns/list` and `thread/fork`. No user message is ever
sent and no model turn is started, so nothing can reach a model.

Claude: a synthetic three-turn session (written in the store layout of
`tests/fixtures/recovery/README.md`) is resumed with `--fork-session` and
`--resume-session-at`. Claude refuses an ID it cannot find before reading stdin, and
`get_context_usage` counts the messages the fork kept, so the counts show where it cut.
Codex: three shell-command turns, then `thread/fork` with `lastTurnId`.

IDs Codex generates are replaced by stable names; paths under the scratch root by
`<root>`.
"""
import argparse
import hashlib
import json
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
SESSION = "0b1c2d3e-0000-4000-8000-0000000000f1"


def env(home: Path, **extra):
    base = {"HOME": str(home), "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "dumb"}
    base.update({k: str(v) for k, v in extra.items()})
    return base


class Process:
    def __init__(self, argv, environment, cwd):
        self.proc = subprocess.Popen(["unshare", "-rn", *argv], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     env=environment, cwd=cwd)
        self.buffer = b""

    def send(self, message: dict):
        assert message.get("method") not in {"turn/start", "turn/steer", "thread/resume"}
        assert message.get("type") in {None, "control_request"}
        if message.get("type") == "control_request":
            assert message["request"]["subtype"] in {"initialize", "get_context_usage"}
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
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        return self.proc.returncode, self.proc.stderr.read().decode(errors="replace")[-600:]


def claude_store(config: Path, project: Path):
    """Three turns; the second runs a tool. Returns the store file and each turn's last
    entry, the ID Keystone passes to `--resume-session-at` to keep through that turn."""
    folder = "".join(c if c.isalnum() else "-" for c in str(project))
    path = config / "projects" / folder / f"{SESSION}.jsonl"
    path.parent.mkdir(parents=True)
    rows, last = [], [None]

    def add(kind, content, uuid, **extra):
        message = {"role": kind, "content": content}
        if kind == "assistant":
            message.update({"id": extra.pop("message"), "type": "message",
                            "model": "claude-test", "stop_reason": extra.pop("stop")})
        rows.append({"parentUuid": last[0], "isSidechain": False, "userType": "external",
                     "cwd": str(project), "sessionId": SESSION, "version": "2.1.301",
                     "type": kind, "message": message, "uuid": uuid,
                     "timestamp": f"2026-10-21T10:00:{len(rows):02d}Z", **extra})
        last[0] = uuid

    ends = []
    add("user", "first question", "6b657973-746f-46e6-8005-000000000001")
    add("assistant", [{"type": "text", "text": "first answer"}],
        "a0000000-0000-4000-8000-000000000001", message="msg_1", stop="end_turn")
    ends.append(last[0])
    add("user", "second question, run a tool", "6b657973-746f-46e6-8005-000000000002")
    add("assistant", [{"type": "tool_use", "id": "toolu_1", "name": "Bash",
                       "input": {"command": "ls"}}],
        "a0000000-0000-4000-8000-000000000002", message="msg_2", stop="tool_use")
    add("user", [{"tool_use_id": "toolu_1", "type": "tool_result", "content": "a.txt"}],
        "r0000000-0000-4000-8000-000000000002")
    add("assistant", [{"type": "text", "text": "second answer"}],
        "a0000000-0000-4000-8000-000000000003", message="msg_3", stop="end_turn")
    ends.append(last[0])
    add("user", "third question", "6b657973-746f-46e6-8005-000000000003")
    add("assistant", [{"type": "text", "text": "third answer"}],
        "a0000000-0000-4000-8000-000000000004", message="msg_4", stop="end_turn")
    ends.append(last[0])
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    return path, ends, [row["uuid"] for row in rows if row["type"] == "user"
                        and isinstance(row["message"]["content"], str)]


def claude_run(binary, home, config, project, extra):
    p = Process([binary, *CLAUDE_ARGS, *extra], env(home, CLAUDE_CONFIG_DIR=config), project)
    p.send({"type": "control_request", "request_id": "init",
            "request": {"subtype": "initialize"}})
    p.send({"type": "control_request", "request_id": "usage",
            "request": {"subtype": "get_context_usage"}})
    answers, frames = {}, []
    deadline = time.time() + 60
    while len(answers) < 2:
        message = p.line(deadline)
        if message is None:
            break
        if message.get("type") == "control_response":
            answers[message["response"].get("request_id")] = message["response"]
        else:
            frames.append({k: message.get(k) for k in ("type", "subtype", "is_error")})
    code, stderr = p.stop()
    usage = (answers.get("usage") or {}).get("response") or {}
    breakdown = usage.get("messageBreakdown") or {}
    return {
        "argv_tail": extra,
        "initialize": (answers.get("init") or {}).get("subtype"),
        "kept_message_tokens": {k: breakdown.get(k) for k in
                                ("userMessageTokens", "assistantMessageTokens",
                                 "toolCallTokens", "toolResultTokens")} if breakdown else None,
        "frames": frames,
        "exit": code,
        "stderr_tail": stderr.strip(),
    }


def probe_claude(binary, root):
    home, config, project = root / "claude-home", root / "claude-config", root / "claude-project"
    home.mkdir()
    project.mkdir()
    store, ends, prompts = claude_store(config, project)
    before = hashlib.sha256(store.read_bytes()).hexdigest()
    version = subprocess.run(["unshare", "-rn", binary, "--version"], env=env(home),
                             capture_output=True, text=True).stdout.strip()
    fork = ["--resume", SESSION, "--fork-session"]
    runs = {
        "through_turn_1": claude_run(binary, home, config, project,
                                     [*fork, "--resume-session-at", ends[0]]),
        "through_turn_2": claude_run(binary, home, config, project,
                                     [*fork, "--resume-session-at", ends[1]]),
        "whole": claude_run(binary, home, config, project, fork),
        "at_turn_3_prompt": claude_run(binary, home, config, project,
                                       [*fork, "--resume-session-at", prompts[2]]),
        "unknown_entry": claude_run(binary, home, config, project,
                                    [*fork, "--resume-session-at", "not-an-entry"]),
        "unknown_session": claude_run(binary, home, config, project,
                                      ["--resume", "0b1c2d3e-0000-4000-8000-0000000000ff",
                                       "--fork-session"]),
    }
    after = hashlib.sha256(store.read_bytes()).hexdigest()
    files = sorted(str(p.relative_to(config)) for p in config.rglob("*.jsonl"))
    return {"version": version, "turn_ends": ends, "turn_prompts": prompts, "runs": runs,
            "source_unchanged": before == after, "session_files_after": files}


def probe_codex(binary, root):
    home, codex_home, project = root / "codex-home-dir", root / "codex-home", root / "codex-project"
    for path in (home, codex_home, project):
        path.mkdir()
    version = subprocess.run(["unshare", "-rn", binary, "--version"], env=env(home),
                             capture_output=True, text=True).stdout.strip()
    schema = root / "schema"
    subprocess.run(["unshare", "-rn", binary, "app-server", "generate-json-schema", "--out",
                    str(schema)], env=env(home, CODEX_HOME=codex_home), capture_output=True,
                   timeout=120)
    fork_params = json.loads((schema / "v2" / "ThreadForkParams.json").read_text())
    methods = []
    for entry in json.loads((schema / "ClientRequest.json").read_text()).get("oneOf", []):
        methods += entry["properties"]["method"].get("enum", [])
    p = Process([binary, "app-server"], env(home, CODEX_HOME=codex_home), project)
    ids = [0]
    notes = []

    def call(method, params):
        ids[0] += 1
        p.send({"id": ids[0], "method": method, "params": params})
        deadline = time.time() + 30
        while True:
            message = p.line(deadline)
            if message is None:
                return None
            if message.get("id") == ids[0] and "method" not in message:
                return message
            notes.append(message.get("method"))

    def wait_for(method):
        deadline = time.time() + 30
        while True:
            message = p.line(deadline)
            if message is None:
                return None
            notes.append(message.get("method"))
            if message.get("method") == method:
                return message

    call("initialize", {"clientInfo": {"name": "keystone", "title": "Keystone",
                                       "version": "probe"},
                        "capabilities": {"optOutNotificationMethods": []}})
    p.send({"method": "initialized"})
    source = call("thread/start", {"cwd": str(project)})["result"]["thread"]["id"]
    for n in (1, 2, 3):
        call("thread/shellCommand", {"threadId": source, "command": f"echo turn {n}"})
        wait_for("turn/completed")

    def turns(thread):
        answer = call("thread/turns/list", {"threadId": thread, "sortDirection": "asc",
                                            "itemsView": "full"})
        return [(t["id"], [i.get("aggregatedOutput") for i in t["items"]])
                for t in answer["result"]["data"]]

    source_turns = turns(source)
    names = {source: "source"}
    names.update({turn: f"turn-{n + 1}" for n, (turn, _) in enumerate(source_turns)})
    through_2 = call("thread/fork", {"threadId": source, "lastTurnId": source_turns[1][0],
                                     "cwd": str(project), "excludeTurns": True})
    fork = through_2["result"]["thread"]
    names[fork["id"]] = "fork-through-turn-2"
    whole = call("thread/fork", {"threadId": source, "cwd": str(project), "excludeTurns": True})
    names[whole["result"]["thread"]["id"]] = "fork-whole"
    unknown = call("thread/fork", {"threadId": source, "lastTurnId": "not-a-turn"})
    result = {
        "version": version,
        "fork_method_listed": "thread/fork" in methods,
        "rollback_methods_listed": sorted(m for m in methods if "revert" in m or "rollback" in m),
        "fork_params": sorted(fork_params["properties"]),
        "last_turn_id_doc": fork_params["properties"]["lastTurnId"]["description"],
        "source_turns": source_turns,
        "fork_through_turn_2": {"forkedFromId": fork.get("forkedFromId"),
                                "turns": turns(fork["id"])},
        "fork_whole": {"turns": turns(whole["result"]["thread"]["id"])},
        "source_turns_after": turns(source),
        "unknown_turn": unknown.get("error"),
        "notifications": sorted({n for n in notes if n}),
    }
    code, stderr = p.stop()
    result["exit"] = code
    text = json.dumps(result)
    for raw, name in sorted(names.items(), key=lambda kv: -len(kv[0])):
        text = text.replace(raw, name)
    return json.loads(text)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--claude", default="claude")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="keystone-fork-probe-"))
    try:
        claude = os.path.realpath(subprocess.run(["which", args.claude], capture_output=True,
                                                 text=True).stdout.strip() or args.claude)
        codex = os.path.realpath(subprocess.run(["which", args.codex], capture_output=True,
                                                text=True).stdout.strip() or args.codex)
        result = {"claude": probe_claude(claude, root), "codex": probe_codex(codex, root)}
        slug = "".join(c if c.isalnum() else "-" for c in str(root))
        text = json.dumps(result, indent=2).replace(str(root), "<root>")
        text = text.replace(slug, "<root as a Claude folder name>") + "\n"
        Path(args.out).write_text(text)
        print(text)
    finally:
        subprocess.run(["rm", "-rf", str(root)])


if __name__ == "__main__":
    sys.exit(main())
