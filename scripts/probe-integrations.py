#!/usr/bin/env python3
"""No-inference probe of where Claude Code and Codex keep MCP servers, skills and plugins,
and how each reports a server's status (TOB-68).

Run it on a machine with the tested CLIs (Claude Code 2.1.301, Codex 0.163.0):

    python3 scripts/probe-integrations.py --out probe.json

Safety: every CLI runs under `unshare -rn` (a private user namespace with no network),
with a fresh HOME, a fixture CLAUDE_CONFIG_DIR or CODEX_HOME and a rebuilt environment
that carries no API keys or OAuth variables. The CLIs' own `mcp` and `plugin` commands
write only into those fixture folders. Claude gets only the `initialize` and
`mcp_status` control requests; Codex gets `initialize`, list requests and one ephemeral
`thread/start`, which starts the thread's MCP servers. No user message is sent and no
turn starts, so nothing reaches a model.

The fixtures hold one working stdio MCP server (a tiny Python script), one that exits
at once (`false`), one HTTP server that cannot connect, skills at every root and a
plugin from a local marketplace. Paths under the scratch root are replaced by `<root>`.
"""
import argparse
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

SERVER = r'''
import sys, json
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        result = {"protocolVersion": message["params"].get("protocolVersion", "2025-06-18"),
                  "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture", "version": "1.0"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "ping", "description": "Ping", "inputSchema": {"type": "object"}}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
'''


def write(path: Path, text: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def skill(root: Path, name: str):
    write(root / name / "SKILL.md", f"---\nname: {name}\ndescription: Fixture {name}\n---\nBody.\n")


def env(home: Path, **extra):
    base = {"HOME": str(home), "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "dumb"}
    base.update({k: str(v) for k, v in extra.items()})
    return base


def run(argv, environment, cwd):
    done = subprocess.run(["unshare", "-rn", *argv], env=environment, cwd=cwd,
                          capture_output=True, text=True, timeout=60)
    return (done.stdout + done.stderr).strip().splitlines()[-1:]


def files(root: Path, base: Path):
    """Every file under `root` but caches, telemetry and backups, as JSON or text."""
    out = {}
    for path in sorted(root.rglob("*")):
        relative = str(path.relative_to(base))
        if (not path.is_file() or path.suffix not in (".json", ".toml", ".md")
                or any(p in relative for p in ("telemetry", "backups", "cache/", ".git/", ".system"))):
            continue
        text = path.read_text(errors="replace")
        try:
            value = json.loads(text)
        except ValueError:
            out[relative] = text
            continue
        for key in ("machineID", "userID"):
            if isinstance(value, dict) and key in value:
                value[key] = "<scrubbed>"
        out[relative] = value
    return out


class Lines:
    """A process speaking JSON lines, read on a thread so no line waits in a buffer."""

    def __init__(self, argv, environment, cwd):
        self.proc = subprocess.Popen(["unshare", "-rn", *argv], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                     env=environment, cwd=cwd, text=True)
        self.lines = queue.Queue()
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.proc.stdout:
            self.lines.put(json.loads(line))

    def send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def until(self, match, seconds=20):
        """Messages until one that `match` accepts, which is returned last, or until
        `seconds` pass."""
        seen, end = [], time.time() + seconds
        while (left := end - time.time()) > 0:
            try:
                message = self.lines.get(timeout=left)
            except queue.Empty:
                break
            seen.append(message)
            if match(message):
                break
        return seen

    def close(self):
        self.proc.kill()
        self.proc.wait()


def probe_claude(claude: str, root: Path):
    home, config, project = root / "claude-home", root / "claude-config", root / "project"
    server = root / "server.py"
    e = env(home, CLAUDE_CONFIG_DIR=config)
    result = {"cli": {}}
    for scope, args in [
        ("user", ["fixture-ok", "-e", "FIXTURE_TOKEN=fixture-secret", "--", "python3", str(server)]),
        ("local", ["fixture-http", "-t", "http", "http://127.0.0.1:9/mcp", "-H", "Authorization: Bearer fixture-secret"]),
        ("project", ["fixture-bad", "--", "false"]),
    ]:
        result["cli"][f"mcp add -s {scope}"] = run([claude, "mcp", "add", "-s", scope, *args], e, project)
    market = root / "claude-market"
    write(market / ".claude-plugin/marketplace.json", json.dumps({
        "name": "fixture-market", "owner": {"name": "Fixture"},
        "plugins": [{"name": "fixture-plugin", "source": "./plugins/fixture-plugin"}]}))
    write(market / "plugins/fixture-plugin/.claude-plugin/plugin.json", json.dumps({
        "name": "fixture-plugin", "version": "1.0.0", "mcpServers": {"fixture-plugin-server": {"command": "true"}}}))
    skill(market / "plugins/fixture-plugin/skills", "fixture-plugin-skill")
    result["cli"]["plugin marketplace add"] = run([claude, "plugin", "marketplace", "add", str(market)], e, project)
    result["cli"]["plugin install -s user"] = run([claude, "plugin", "install", "fixture-plugin@fixture-market", "-s", "user"], e, project)
    result["cli"]["plugin install -s local"] = run([claude, "plugin", "install", "fixture-plugin@fixture-market", "-s", "local"], e, project)
    result["cli"]["plugin disable -s user"] = run([claude, "plugin", "disable", "fixture-plugin@fixture-market", "-s", "user"], e, project)
    skill(config / "skills", "fixture-skill")
    skill(project / ".claude/skills", "fixture-project-skill")
    skill(home / ".agents/skills", "fixture-home-agents-skill")
    result["files"] = files(root, root)

    process = Lines([claude, "-p", "--input-format", "stream-json", "--output-format", "stream-json",
                     "--verbose"], e, project)
    process.send({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}})
    answer = process.until(lambda m: m.get("type") == "control_response")[-1]
    result["initialize_commands"] = [c["name"] for c in answer["response"]["response"].get("commands", [])
                                     if not c.get("builtin")]
    for when, wait in (("mcp_status_at_once", 0), ("mcp_status_after_5s", 5)):
        time.sleep(wait)
        process.send({"type": "control_request", "request_id": when, "request": {"subtype": "mcp_status"}})
        answer = process.until(lambda m: m.get("type") == "control_response", 40)[-1]
        result[when] = answer["response"]
    process.close()
    return result


def probe_codex(codex: str, root: Path):
    home, codex_home, project = root / "codex-user", root / "codex-home", root / "project"
    server = root / "server.py"
    codex_home.mkdir()
    e = env(home, CODEX_HOME=codex_home)
    result = {"cli": {}}
    result["cli"]["mcp add stdio"] = run([codex, "mcp", "add", "fixture-ok", "--env", "FIXTURE_TOKEN=fixture-secret", "--", "python3", str(server)], e, project)
    result["cli"]["mcp add failing"] = run([codex, "mcp", "add", "fixture-bad", "--", "false"], e, project)
    result["cli"]["mcp add http"] = run([codex, "mcp", "add", "fixture-http", "--url", "http://127.0.0.1:9/mcp", "--bearer-token-env-var", "FIXTURE_BEARER"], e, project)
    market = root / "codex-market"
    write(market / ".agents/plugins/marketplace.json", json.dumps({
        "name": "fixture-market", "plugins": [{"name": "fixture-plugin",
        "source": {"source": "local", "path": "./plugins/fixture-plugin"}, "policy": {"installation": "AVAILABLE"}}]}))
    write(market / "plugins/fixture-plugin/.codex-plugin/plugin.json", json.dumps({
        "name": "fixture-plugin", "version": "1.0.0", "skills": "./skills/"}))
    skill(market / "plugins/fixture-plugin/skills", "fixture-plugin-skill")
    result["cli"]["plugin marketplace add"] = run([codex, "plugin", "marketplace", "add", str(market)], e, project)
    result["cli"]["plugin add"] = run([codex, "plugin", "add", "fixture-plugin@fixture-market"], e, project)
    write(project / ".codex/config.toml", '[mcp_servers.fixture-repo]\ncommand = "true"\n')
    skill(codex_home / "skills", "fixture-codex-skill")
    skill(codex_home / "skills", "fixture-off-skill")
    skill(home / ".agents/skills", "fixture-home-agents-skill")
    skill(home / ".codex/skills", "fixture-home-codex-skill")
    skill(project / ".agents/skills", "fixture-repo-agents-skill")
    skill(project / ".codex/skills", "fixture-repo-codex-skill")
    with open(codex_home / "config.toml", "a") as config:
        config.write(f'\n[[skills.config]]\npath = "{codex_home}/skills/fixture-off-skill/SKILL.md"\nenabled = false\n')

    def app_server():
        process = Lines([codex, "app-server"], e, project)
        ids = iter(range(1, 100))

        def request(method, params):
            n = next(ids)
            process.send({"jsonrpc": "2.0", "id": n, "method": method, "params": params})
            seen = process.until(lambda m: m.get("id") == n, 30)
            return seen[-1], [m for m in seen[:-1] if "method" in m]

        request("initialize", {"clientInfo": {"name": "probe", "version": "0"}})
        process.send({"jsonrpc": "2.0", "method": "initialized"})
        return process, request

    untrusted = {}
    process, request = app_server()
    untrusted["mcpServerStatus/list"] = request("mcpServerStatus/list", {"detail": "toolsAndAuthOnly"})[0]
    started, notes = request("thread/start", {"cwd": str(project), "ephemeral": True})
    notes += process.until(lambda m: False, 4)
    untrusted["startup_notifications"] = [n for n in notes if n["method"] == "mcpServer/startupStatus/updated"]
    untrusted["skills/list"] = request("skills/list", {"cwds": [str(project)]})[0]
    process.close()

    with open(codex_home / "config.toml", "a") as config:
        config.write(f'\n[projects."{project}"]\ntrust_level = "trusted"\n')
    trusted = {}
    process, request = app_server()
    started, notes = request("thread/start", {"cwd": str(project), "ephemeral": True})
    thread = started["result"]["thread"]["id"]
    notes += process.until(lambda m: False, 4)
    trusted["startup_notifications"] = [n for n in notes if n["method"] == "mcpServer/startupStatus/updated"]
    trusted["mcpServerStatus/list with threadId"] = request(
        "mcpServerStatus/list", {"threadId": thread, "detail": "toolsAndAuthOnly"})[0]
    process.close()
    result["files"] = {k: v for folder in (codex_home, home, project / ".codex", project / ".agents")
                       for k, v in files(folder, root).items()}
    result["untrusted_project"] = untrusted
    result["trusted_project"] = trusted
    return result


def resolve(path: str, name: str) -> str:
    """The CLI's own executable: a mise shim needs the real HOME, which the probe hides."""
    real = os.path.realpath(path)
    if os.path.basename(real) == "mise":
        real = subprocess.run(["mise", "which", name], capture_output=True, text=True,
                              check=True).stdout.strip()
    return real


def scrub(value, root: Path):
    return json.loads(json.dumps(value).replace(str(root), "<root>"))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--claude", default=shutil.which("claude"))
    parser.add_argument("--codex", default=shutil.which("codex"))
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="keystone-integrations-probe-"))
    try:
        (root / "project").mkdir()
        subprocess.run(["git", "init", "-q", str(root / "project")], check=True)
        write(root / "server.py", SERVER)
        claude, codex = resolve(args.claude, "claude"), resolve(args.codex, "codex")
        versions = {name: subprocess.run([path, "--version"], capture_output=True, text=True).stdout.strip()
                    for name, path in (("claude", claude), ("codex", codex))}
        result = {"versions": versions, "claude": probe_claude(claude, root),
                  "codex": probe_codex(codex, root)}
        Path(args.out).write_text(json.dumps(scrub(result, root), indent=1) + "\n")
    finally:
        shutil.rmtree(root)


if __name__ == "__main__":
    sys.exit(main())
