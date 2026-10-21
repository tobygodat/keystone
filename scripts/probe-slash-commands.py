#!/usr/bin/env python3
"""No-inference probe of what Claude Code and Codex list as slash commands, prompts and skills.

Run it on a machine with the tested CLIs (Claude Code 2.1.301, Codex 0.163.0):

    python3 scripts/probe-slash-commands.py --out probe.json

Safety: every CLI runs under `unshare -rn` (a private user namespace with no network),
with a fresh HOME, a fixture CLAUDE_CONFIG_DIR or CODEX_HOME and a rebuilt environment
that carries no API keys or OAuth variables. Claude gets only the `initialize` control
request; Codex gets only `initialize`, `initialized` and list requests. No user message
is ever sent and no thread or turn is started, so nothing can reach a model.

The fixtures hold one user command, one project command, one user skill and one project
skill per provider. The output records every command each CLI lists (built-ins included,
since the homes are fresh), and which app-server methods mention skills or prompts.
Paths under the scratch root are replaced by `<root>`.
"""
import argparse
import json
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

CLAUDE_ARGS = ["-p", "--input-format", "stream-json", "--output-format", "stream-json",
               "--verbose", "--include-partial-messages", "--replay-user-messages",
               "--permission-prompts", "host", "--permission-prompt-tool", "stdio"]


def write(path: Path, text: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def fixtures(root: Path):
    home = root / "home"
    home.mkdir()
    claude = root / "claude-config"
    write(claude / "commands/fixture-review.md",
          "---\ndescription: Fixture user command\nargument-hint: [file]\n---\nReview $ARGUMENTS\n")
    write(claude / "commands/fixture-ns/deep.md",
          "---\ndescription: Fixture namespaced user command\n---\nDeep $ARGUMENTS\n")
    write(claude / "skills/fixture-skill/SKILL.md",
          "---\nname: fixture-skill\ndescription: Fixture user skill\n---\nDo the fixture thing.\n")
    project = root / "project"
    write(project / ".claude/commands/fixture-project.md",
          "---\ndescription: Fixture project command\nargument-hint: <ticket>\n---\nProject $ARGUMENTS\n")
    write(project / ".claude/skills/fixture-project-skill/SKILL.md",
          "---\nname: fixture-project-skill\ndescription: Fixture project skill\n---\nProject skill.\n")
    codex = root / "codex-home"
    write(codex / "prompts/fixture-prompt.md",
          "---\ndescription: Fixture Codex prompt\nargument-hint: FILE=<path>\n---\nLook at $FILE\n")
    write(codex / "skills/fixture-codex-skill/SKILL.md",
          "---\nname: fixture-codex-skill\ndescription: Fixture Codex user skill\n---\nCodex skill.\n")
    write(project / ".codex/skills/fixture-codex-project-skill/SKILL.md",
          "---\nname: fixture-codex-project-skill\ndescription: Fixture Codex repo skill\n---\nRepo skill.\n")
    write(project / ".agents/skills/fixture-agents-skill/SKILL.md",
          "---\nname: fixture-agents-skill\ndescription: Fixture .agents repo skill\n---\nAgents skill.\n")
    subprocess.run(["git", "init", "-q", str(project)], check=True)
    return home, claude, codex, project


def env(home: Path, **extra):
    base = {"HOME": str(home), "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "dumb"}
    base.update({k: str(v) for k, v in extra.items()})
    return base


def scrub(value, root: Path):
    return json.loads(json.dumps(value).replace(str(root), "<root>"))


class Process:
    def __init__(self, argv, environment, cwd):
        self.proc = subprocess.Popen(["unshare", "-rn", *argv], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     env=environment, cwd=cwd)
        self.buffer = b""

    def send(self, message: dict):
        assert message.get("method") not in {"thread/start", "turn/start", "thread/resume"}
        assert message.get("type") in {None, "control_request"}
        if message.get("type") == "control_request":
            assert message["request"]["subtype"] == "initialize"
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
        return self.proc.returncode, self.proc.stderr.read().decode(errors="replace")[-2000:]


def probe_claude(binary, home, config, project, root):
    p = Process([binary, *CLAUDE_ARGS], env(home, CLAUDE_CONFIG_DIR=config), project)
    p.send({"type": "control_request", "request_id": "probe",
            "request": {"subtype": "initialize"}})
    deadline = time.time() + 60
    frames, answer = [], None
    while answer is None:
        message = p.line(deadline)
        if message is None:
            break
        frames.append({"type": message.get("type"), "subtype": message.get("subtype")})
        if message.get("type") == "control_response":
            answer = message["response"]
    code, stderr = p.stop()
    response = (answer or {}).get("response") or {}
    return scrub({
        "frames_before_answer": frames,
        "subtype": (answer or {}).get("subtype"),
        "error": (answer or {}).get("error"),
        "response_keys": sorted(response),
        "commands": response.get("commands"),
        "agents": [a.get("name") if isinstance(a, dict) else a for a in response.get("agents") or []],
        "exit": code,
        "stderr_tail": stderr,
    }, root)


def codex_methods(binary, home, codex_home, root):
    out = root / "schema"
    result = subprocess.run(["unshare", "-rn", binary, "app-server", "generate-json-schema",
                             "--out", str(out)], env=env(home, CODEX_HOME=codex_home),
                            capture_output=True, text=True, timeout=120)
    methods = set()
    for path in out.rglob("*.json") if out.exists() else []:
        text = path.read_text()
        for word in ("skill", "prompt", "command"):
            if word in text.lower():
                try:
                    data = json.loads(text)
                except json.JSONDecodeError:
                    continue
                stack = [data]
                while stack:
                    item = stack.pop()
                    if isinstance(item, dict):
                        method = item.get("properties", {}).get("method", {})
                        for value in method.get("enum", []) if isinstance(method, dict) else []:
                            if any(w in value.lower() for w in ("skill", "prompt", "command")):
                                methods.add(value)
                        stack.extend(item.values())
                    elif isinstance(item, list):
                        stack.extend(item)
    schema_files = sorted(str(p.relative_to(out)) for p in out.rglob("*.json")) if out.exists() else []
    return {"exit": result.returncode, "stderr_tail": result.stderr[-1000:],
            "methods": sorted(methods),
            "schema_files_matching": [f for f in schema_files
                                      if any(w in f.lower() for w in ("skill", "prompt", "command"))]}


def probe_codex(binary, home, codex_home, project, root, methods):
    p = Process([binary, "app-server"], env(home, CODEX_HOME=codex_home), project)
    p.send({"id": 1, "method": "initialize", "params": {
        "clientInfo": {"name": "keystone", "title": "Keystone", "version": "probe"},
        "capabilities": {"optOutNotificationMethods": []}}})
    answers = {}

    def wait(request_id, seconds=30):
        deadline = time.time() + seconds
        while True:
            message = p.line(deadline)
            if message is None:
                return None
            if message.get("id") == request_id and "method" not in message:
                return message

    answers["initialize"] = wait(1)
    p.send({"method": "initialized"})
    candidates = [("skills/list", {"cwds": [str(project)]}),
                  ("skills/list", {"cwds": [str(project)], "forceReload": True}),
                  ("prompts/list", {}),
                  ("customPrompts/list", {})]
    candidates += [(m, {}) for m in methods if m.endswith("/list") and m not in
                   {"skills/list", "prompts/list", "customPrompts/list"}]
    for n, (method, params) in enumerate(candidates, start=2):
        p.send({"id": n, "method": method, "params": params})
        answers[f"{method} {json.dumps(params)}"] = wait(n)
    code, stderr = p.stop()
    return scrub({"answers": answers, "exit": code, "stderr_tail": stderr}, root)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--claude", default=shutil.which("claude"))
    parser.add_argument("--codex", default=shutil.which("codex"))
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="slash-probe-"))
    try:
        home, claude, codex, project = fixtures(root)
        result = {"time_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
        for name, binary in (("claude", args.claude), ("codex", args.codex)):
            version = subprocess.run([binary, "--version"], capture_output=True, text=True)
            result[f"{name}_version"] = version.stdout.strip()
        result["claude"] = probe_claude(args.claude, home, claude, project, root)
        result["codex_schema"] = codex_methods(args.codex, home, codex, root)
        result["codex"] = probe_codex(args.codex, home, codex, project, root,
                                      result["codex_schema"]["methods"])
        Path(args.out).write_text(json.dumps(result, indent=1))
        print(f"wrote {args.out}")
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
