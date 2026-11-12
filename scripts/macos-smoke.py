#!/usr/bin/env python3
"""Smoke journey for Keystone on macOS, where no private Sway desktop exists.

Launches the real debug app with the scripted providers and a throwaway home, state
and project. It drives the app through the automation inbox (see
crates/keystone-ui/src/automation.rs): keystrokes and clicks enter through GPUI's
Window::dispatch_keystroke and Window::dispatch_event, and screenshots are the
window's frames rendered by GPUI's Metal renderer (Window::render_to_image). The
journey: launch, new Claude session, send to the fake Claude, see the reply, leave a
draft, quit with Cmd+Q, relaunch, and find the session and draft restored. It also
checks that a launch without TERM, as from Finder, adopts the login shell's environment.

Run from the workspace root: python3 scripts/macos-smoke.py
Evidence goes to .audit/macos-smoke-<time>/: numbered PNGs and report.json.
"""

import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPLY = 'Hello from the fake Claude on macOS.'
MESSAGE = 'Hello from macOS'
DRAFT = 'Draft kept across restart'


class Failure(Exception):
    pass


class Run:
    def __init__(self, binary, evidence):
        self.binary = binary
        self.evidence = evidence
        self.scratch = Path(tempfile.mkdtemp(prefix='keystone-macos-smoke-'))
        self.home = self.scratch / 'home'
        self.project = self.scratch / 'project'
        self.inbox = self.scratch / 'inbox'
        self.geometry = self.scratch / 'elements.json'
        self.database = self.scratch / 'workspace.sqlite3'
        self.path_receipt = self.scratch / 'claude-path.txt'
        self.login_receipt = self.scratch / 'claude-login.txt'
        for folder in (self.home / 'login-bin', self.home / 'claude', self.home / 'codex',
                       self.project, self.inbox):
            folder.mkdir(parents=True)
        (self.home / '.zprofile').write_text('export PATH="$HOME/login-bin:$PATH"\n'
                                             'export KEYSTONE_SMOKE_LOGIN=from-zprofile\n')
        subprocess.run(['git', 'init', '-q', str(self.project)], check=True)
        claude = self.scratch / 'claude'
        claude.write_text('#!/bin/sh\n'
                          f'printf "%s" "$PATH" > "{self.path_receipt}"\n'
                          f'printf "%s" "$KEYSTONE_SMOKE_LOGIN" > "{self.login_receipt}"\n'
                          'export KEYSTONE_FAKE_CLAUDE_STORE=1\n'
                          f'exec "{binary.parent}/keystone-fake-claude" "$@"\n')
        claude.chmod(0o755)
        self.app = None
        self.launches = 0
        self.shots = 0
        self.sequence = 0
        self.steps = []

    def env(self):
        # No TERM: the app must find its PATH the way a Finder launch does.
        return {
            'HOME': str(self.home), 'USER': os.environ.get('USER', ''), 'SHELL': '/bin/zsh',
            'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'TMPDIR': os.environ.get('TMPDIR', '/tmp'),
            'KEYSTONE_STATE_PATH': str(self.database),
            'KEYSTONE_AUTOMATION_PATH': str(self.geometry),
            'KEYSTONE_AUTOMATION_INBOX': str(self.inbox),
            'KEYSTONE_CLAUDE_PATH': str(self.scratch / 'claude'),
            'KEYSTONE_CODEX_PATH': str(self.binary.parent / 'keystone-fake-codex'),
            'KEYSTONE_PROJECT_DIR': str(self.project),
            'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '5', 'KEYSTONE_FAKE_CODEX_DELAY_MS': '5',
            'KEYSTONE_FAKE_CLAUDE_REPLY': REPLY,
            'CLAUDE_CONFIG_DIR': str(self.home / 'claude'), 'CODEX_HOME': str(self.home / 'codex'),
            **background(),
        }

    def step(self, name, ok, detail=''):
        self.steps.append({'step': name, 'ok': ok, 'detail': detail})
        print(f'{"ok  " if ok else "FAIL"} {name} {detail}', flush=True)
        if not ok:
            raise Failure(f'{name}: {detail}')

    def launch(self):
        self.launches += 1
        self.geometry.unlink(missing_ok=True)
        log = open(self.evidence / f'app-{self.launches:02}.stderr.log', 'w')
        self.app = subprocess.Popen([str(self.binary)], env=self.env(), cwd='/',
                                    stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        self.wait_for('welcome-claude' if self.launches == 1 else 'composer-field', 60)

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

    def click(self, name):
        element = self.elements().get(name)
        if not element:
            raise Failure(f'{name} is not on screen')
        box = element['visible']
        self.command(f'click {box["x"] + box["w"] / 2} {box["y"] + box["h"] / 2}')

    def shot(self, label):
        time.sleep(0.5)
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

    def sessions(self):
        return [json.loads(row[0]) for row in self.read('SELECT payload FROM sessions ORDER BY position')]

    def replied(self):
        return any(REPLY in row[0] for row in self.read('SELECT payload FROM log_entries'))

    def journey(self):
        self.launch()
        self.step('launch shows the welcome screen', True)
        self.shot('welcome')

        self.key('secondary-n')
        self.wait_for('composer-field')
        self.step('Cmd+N opens a new Claude session', True)
        self.click('composer-field')
        self.type(MESSAGE)
        self.key('secondary-enter')
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and not self.replied():
            time.sleep(0.2)
        self.step('the fake Claude replies', self.replied(), REPLY)
        self.wait_for('turn-status', 30, present=False)
        self.shot('reply')
        self.key('secondary-k')
        self.wait_for('palette')
        self.step('Cmd+K opens the palette, its hints in macOS notation', True)
        self.shot('palette')
        self.key('escape')
        self.wait_for('palette', present=False)

        login_path = self.path_receipt.read_text() if self.path_receipt.exists() else ''
        self.step('a launch without TERM adopts the login shell PATH',
                  str(self.home / 'login-bin') in login_path.split(':'), login_path)
        login_variable = self.login_receipt.read_text() if self.login_receipt.exists() else ''
        self.step('it also adopts variables the login shell exports',
                  login_variable == 'from-zprofile', login_variable)

        self.click('composer-field')
        self.type(DRAFT)
        time.sleep(1)
        self.shot('draft')
        self.quit()
        sessions = self.sessions()
        self.step('one session is saved', len(sessions) == 1, f'{len(sessions)} sessions')
        self.step('the draft is saved', sessions[0].get('draft') == DRAFT, repr(sessions[0].get('draft')))

        self.launch()
        self.step('relaunch reopens the session', True)
        self.shot('restored')
        sessions = self.sessions()
        self.step('the draft survives the relaunch', len(sessions) == 1 and sessions[0].get('draft') == DRAFT)
        self.step('the reply survives the relaunch', self.replied())
        self.quit()

    def finish(self, error):
        if self.app and self.app.poll() is None:
            self.app.kill()
            self.app.wait()
        report = {'binary': str(self.binary), 'steps': self.steps, 'error': error,
                  'screenshots': sorted(p.name for p in self.evidence.glob('*.png'))}
        (self.evidence / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        if self.database.exists():
            shutil.copy(self.database, self.evidence / 'workspace.sqlite3')
        shutil.rmtree(self.scratch, ignore_errors=True)


def background():
    key = 'KEYSTONE_AUTOMATION_BACKGROUND'
    return {key: os.environ[key]} if key in os.environ else {}


def main():
    if sys.platform != 'darwin':
        sys.exit('macos-smoke runs on macOS. On Linux, use the verify skill.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    binary = ROOT / 'target/debug/keystone-desktop'
    evidence = ROOT / '.audit' / time.strftime('macos-smoke-%Y%m%d-%H%M%S')
    evidence.mkdir(parents=True)
    run = Run(binary, evidence)
    error = None
    try:
        run.journey()
    except Failure as failure:
        error = str(failure)
    finally:
        run.finish(error)
    print(f'evidence: {evidence}')
    if error:
        sys.exit(f'macos-smoke failed: {error}')
    print('macos-smoke passed')


if __name__ == '__main__':
    main()
