#!/usr/bin/env python3
"""Browser preview journey on macOS (TOB-302).

Serves a small demo app from a throwaway project, launches the real debug app on it
through the automation inbox (see scripts/macos-smoke.py), and checks:

- Cmd+Alt+B and the palette open the Browser surface, which lists the project's dev
  server; a click on it loads the page beside the session.
- The user's own clicks and typing reach the page.
- A real Claude session drives the same page through the keystone browser_* tools.
- Quitting stops the browser, and the surface comes back after a relaunch.

The agent step runs the user's own `claude` with their login and spends a little of
their usage; pass --no-agent to skip it. Keystone's workspace is a scratch database.

Run from the workspace root: python3 scripts/macos-browser.py
Evidence goes to .audit/macos-browser-<time>/: numbered PNGs and report.json.
"""

import argparse
import json
import os
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

DEMO = """<!doctype html>
<title>Orders demo</title>
<style>
  body { margin: 0; font: 15px -apple-system, system-ui, sans-serif; background: #f6f6f4; color: #222; }
  h1 { position: absolute; left: 24px; top: 8px; font-size: 22px; }
  #name { position: absolute; left: 24px; top: 72px; width: 220px; height: 30px; font: inherit; padding: 0 8px; }
  #add { position: absolute; left: 268px; top: 72px; height: 34px; font: inherit; }
  #list { position: absolute; left: 24px; top: 120px; margin: 0; padding-left: 20px; }
  #count { position: absolute; left: 24px; top: 260px; color: #666; }
</style>
<h1>Orders</h1>
<form id="form"><input id="name" aria-label="Customer" placeholder="Customer"><button id="add">Add order</button></form>
<ul id="list"></ul>
<p id="count">0 orders</p>
<script>
  form.onsubmit = (event) => {
    event.preventDefault();
    const field = document.getElementById('name');
    if (!field.value.trim()) return;
    const item = document.createElement('li');
    item.textContent = field.value;
    list.append(item);
    count.textContent = list.children.length + (list.children.length === 1 ? ' order' : ' orders');
    field.value = '';
  };
</script>
"""

AGENT_MESSAGE = (
    "Use only Keystone's browser tools (mcp__keystone__browser_*), no shell. "
    "Call browser_open with no url to show the project's page, take a browser_snapshot, "
    "type Ada Lovelace into the Customer field, click Add order, take another snapshot to "
    "confirm, then reply with the order count it shows."
)


class Failure(Exception):
    pass


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


class Run:
    def __init__(self, binary, evidence, agent):
        self.binary = binary
        self.evidence = evidence
        self.agent = agent
        self.scratch = Path(tempfile.mkdtemp(prefix='keystone-macos-browser-'))
        self.project = self.scratch / 'orders-demo'
        self.inbox = self.scratch / 'inbox'
        self.geometry = self.scratch / 'elements.json'
        self.database = self.scratch / 'workspace.sqlite3'
        for folder in (self.project, self.inbox):
            folder.mkdir(parents=True)
        subprocess.run(['git', 'init', '-q', str(self.project)], check=True)
        (self.project / 'index.html').write_text(DEMO)
        self.port = free_port()
        self.server = subprocess.Popen(
            [sys.executable, '-m', 'http.server', str(self.port), '--bind', '127.0.0.1'],
            cwd=self.project, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.app = None
        self.launches = 0
        self.shots = 0
        self.sequence = 0
        self.steps = []

    def env(self):
        env = dict(os.environ)
        env.update({
            'KEYSTONE_STATE_PATH': str(self.database),
            'KEYSTONE_AUTOMATION_PATH': str(self.geometry),
            'KEYSTONE_AUTOMATION_INBOX': str(self.inbox),
            'KEYSTONE_PROJECT_DIR': str(self.project),
        })
        if not self.agent:
            env['KEYSTONE_CLAUDE_PATH'] = str(self.binary.parent / 'keystone-fake-claude')
            env['KEYSTONE_CODEX_PATH'] = str(self.binary.parent / 'keystone-fake-codex')
        return env

    def step(self, name, ok, detail=''):
        self.steps.append({'step': name, 'ok': ok, 'detail': detail})
        print(f'{"ok  " if ok else "FAIL"} {name} {detail}', flush=True)
        if not ok:
            raise Failure(f'{name}: {detail}')

    def launch(self, ready):
        self.launches += 1
        self.geometry.unlink(missing_ok=True)
        log = open(self.evidence / f'app-{self.launches:02}.stderr.log', 'w')
        self.app = subprocess.Popen([str(self.binary)], env=self.env(), cwd='/',
                                    stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        self.wait_for(ready, 60)

    def elements(self):
        try:
            frame = json.loads(self.geometry.read_text())
        except (OSError, ValueError):
            return {}
        if frame.get('pid') != self.app.pid:
            return {}
        return {e['id']: e for e in frame.get('elements', []) if e.get('visible')}

    def wait_for(self, name, seconds=20, present=True):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if self.app.poll() is not None:
                raise Failure(f'the app exited with {self.app.returncode} waiting for {name}')
            if (name in self.elements()) == present:
                return
            time.sleep(0.1)
        raise Failure(f'{name} {"never appeared" if present else "never went"} in {seconds} s')

    def command(self, text, seconds=10):
        self.sequence += 1
        path = self.inbox / f'{self.sequence:05}.cmd'
        staging = path.with_suffix('.tmp')
        staging.write_text(text)
        staging.rename(path)
        done = path.with_suffix('.done')
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if done.exists():
                answer = done.read_text()
                if answer != 'ok':
                    raise Failure(f'{text!r}: {answer}')
                return
            time.sleep(0.05)
        raise Failure(f'{text!r} got no answer in {seconds} s')

    def key(self, keys):
        self.command(f'key {keys}')

    def type(self, text):
        self.command(f'type {text}')

    def click_at(self, x, y):
        self.command(f'click {x} {y}')

    def click(self, name, dx=None, dy=None):
        element = self.elements().get(name)
        if not element:
            raise Failure(f'{name} is not on screen')
        box = element['visible']
        x = box['x'] + (box['w'] / 2 if dx is None else dx)
        y = box['y'] + (box['h'] / 2 if dy is None else dy)
        self.click_at(x, y)

    def shot(self, label):
        time.sleep(0.8)
        self.shots += 1
        path = self.evidence / f'{self.shots:02}-{label}.png'
        self.command(f'capture {path}', 30)
        self.step(f'screenshot {path.name}', path.exists() and path.stat().st_size > 0)

    def quit(self):
        self.key('secondary-q')
        try:
            code = self.app.wait(30)
        except subprocess.TimeoutExpired:
            self.app.kill()
            raise Failure('the app did not quit within 30 s of Cmd+Q')
        self.step('Cmd+Q quits gracefully', code == 0, f'exit code {code}')

    def read(self, sql):
        connection = sqlite3.connect(self.database.as_uri() + '?mode=ro', uri=True, timeout=1)
        try:
            return connection.execute(sql).fetchall()
        finally:
            connection.close()

    def browsers_running(self):
        found = subprocess.run(['pgrep', '-f', f'user-data-dir={self.database.parent / "browser"}'],
                               capture_output=True, text=True)
        return [line for line in found.stdout.split() if line]

    def wait_until(self, check, seconds, what):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if check():
                return True
            time.sleep(0.25)
        raise Failure(f'{what} not seen in {seconds} s')

    def journey(self):
        self.launch('welcome-claude')
        self.key('secondary-n')
        self.wait_for('composer-field')
        self.step('Cmd+N opens a Claude session', True)

        self.key('secondary-alt-b')
        self.wait_for('browser-open-server-0', 20)
        self.step('Cmd+Alt+B opens the Browser surface listing the dev server', True)
        self.shot('browser-surface-dev-server')

        self.click('browser-address')
        self.key('secondary-alt-b')
        self.wait_for('browser-open-server-0', 5, present=False)
        self.step('Cmd+Alt+B again closes it while it has focus', True)
        self.key('secondary-k')
        self.wait_for('palette')
        self.type('Toggle browser')
        self.click('palette-row-0')
        self.wait_for('browser-open-server-0', 20)
        self.step('the palette, by mouse, opens it again', True)

        self.click('browser-open-server-0')
        self.wait_for('browser-page', 30)
        time.sleep(1.5)
        self.step('clicking the dev server loads the page beside the session', True)
        self.shot('page-loaded')

        self.click('browser-page', 60, 87)
        self.type('Grace Hopper')
        self.key('enter')
        time.sleep(1.5)
        self.shot('user-typed-and-submitted')

        if self.agent:
            self.key('secondary-alt-b')
            self.wait_for('browser-page', 5, present=False)
            self.step('Cmd+Alt+B closes the surface while the page has focus', True)
            self.click('composer-field')
            self.type(AGENT_MESSAGE)
            self.key('secondary-enter')
            self.wait_for('turn-status', 30)
            self.wait_for('browser-page', 120)
            self.step("the agent's browser_open brings the surface back beside the session", True)
            self.wait_for('turn-status', 300, present=False)
            rows = ' '.join(row[0] for row in self.read('SELECT payload FROM log_entries'))
            self.step('the agent called browser_snapshot', 'browser_snapshot' in rows)
            self.step('the agent called browser_act', 'browser_act' in rows)
            self.wait_for('browser-agent-note', 10)
            self.step('the surface shows what the agent did', True)
            self.shot('agent-drove-the-page')

        self.quit()
        self.wait_until(lambda: not self.browsers_running(), 15, 'the browser stopping after quit')
        self.step('quitting stops the headless browser', True)

        self.launch('composer-field')
        self.wait_for('browser-open-server-0', 20)
        self.step('the Browser surface comes back after a relaunch', True)
        self.shot('relaunched')
        self.quit()

    def finish(self, error):
        if self.app and self.app.poll() is None:
            self.app.kill()
            self.app.wait()
        self.server.terminate()
        report = {'binary': str(self.binary), 'agent': self.agent, 'steps': self.steps, 'error': error,
                  'screenshots': sorted(p.name for p in self.evidence.glob('*.png'))}
        (self.evidence / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        if self.database.exists():
            shutil.copy(self.database, self.evidence / 'workspace.sqlite3')
        shutil.rmtree(self.scratch, ignore_errors=True)


def main():
    if sys.platform != 'darwin':
        sys.exit('macos-browser runs on macOS.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    parser.add_argument('--no-agent', action='store_true', help='skip the real Claude step')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    binary = ROOT / 'target/debug/keystone-desktop'
    evidence = ROOT / '.audit' / time.strftime('macos-browser-%Y%m%d-%H%M%S')
    evidence.mkdir(parents=True)
    run = Run(binary, evidence, agent=not args.no_agent)
    error = None
    try:
        run.journey()
    except Failure as failure:
        error = str(failure)
    finally:
        run.finish(error)
    print(f'evidence: {evidence}')
    if error:
        sys.exit(f'macos-browser failed: {error}')
    print('macos-browser passed')


if __name__ == '__main__':
    main()
