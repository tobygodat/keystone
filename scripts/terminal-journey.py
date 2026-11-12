#!/usr/bin/env python3
"""Terminal journey for Keystone on macOS (TOB-301).

Launches the real debug app with the scripted providers and a throwaway home, state
and project, and drives it through the automation inbox the way macos-smoke.py does
(see crates/keystone-ui/src/automation.rs). The journey: a new Claude session with
Cmd+N, open in a second window too; the terminal opened with Cmd+J; with the first
window made smaller, the shell sees one terminal size that holds still, and no
SIGWINCH, while both windows draw; pwd and seq 1 300 run in the user's login shell;
the wheel scrolls back; Cmd+Shift+D splits, Cmd+Alt+Left and Right move between the
panes, and a pane's own button closes it along with a job there that ignores SIGHUP;
Cmd+J hides it and the top bar button shows it again; the second window closes and
the first still shows the shell and its scrollback, and a new window shows them
again; Cmd+Q quits and the shell and a job that ignores SIGHUP are gone; a relaunch
brings back the session and its draft.

The harness reads state from files the shell writes and from the workspace database.
It cannot read text off the screen, so look at the screenshots.

Run from the workspace root: python3 scripts/terminal-journey.py
Evidence goes to .audit/terminal-journey-<time>/: numbered PNGs and report.json.
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
DRAFT = 'Draft kept across the terminal journey'


class Failure(Exception):
    pass


class Run:
    def __init__(self, binary, evidence):
        self.binary = binary
        self.evidence = evidence
        self.scratch = Path(tempfile.mkdtemp(prefix='keystone-terminal-journey-'))
        self.home = self.scratch / 'home'
        self.project = self.scratch / 'project'
        self.inbox = self.scratch / 'inbox'
        self.geometry = self.scratch / 'elements.json'
        self.database = self.scratch / 'workspace.sqlite3'
        self.receipts = self.scratch / 'receipts'
        for folder in (self.home / 'claude', self.home / 'codex', self.project,
                       self.inbox, self.receipts):
            folder.mkdir(parents=True)
        # A plain prompt that names the folder, and no first-run wizard.
        (self.home / '.zshrc').write_text("PROMPT='%1~ %# '\n")
        subprocess.run(['git', 'init', '-q', str(self.project)], check=True)
        self.app = None
        self.launches = 0
        self.shots = 0
        self.sequence = 0
        self.steps = []

    def env(self):
        return {
            'HOME': str(self.home), 'USER': os.environ.get('USER', ''), 'SHELL': '/bin/zsh',
            'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'TMPDIR': os.environ.get('TMPDIR', '/tmp'),
            'KEYSTONE_STATE_PATH': str(self.database),
            'KEYSTONE_AUTOMATION_PATH': str(self.geometry),
            'KEYSTONE_AUTOMATION_INBOX': str(self.inbox),
            'KEYSTONE_CLAUDE_PATH': str(self.binary.parent / 'keystone-fake-claude'),
            'KEYSTONE_CODEX_PATH': str(self.binary.parent / 'keystone-fake-codex'),
            'KEYSTONE_PROJECT_DIR': str(self.project),
            'CLAUDE_CONFIG_DIR': str(self.home / 'claude'), 'CODEX_HOME': str(self.home / 'codex'),
            **background(),
        }

    def step(self, name, ok, detail=''):
        self.steps.append({'step': name, 'ok': bool(ok), 'detail': detail})
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

    def frame(self):
        try:
            frame = json.loads(self.geometry.read_text())
        except (OSError, ValueError):
            return {}
        return frame if frame.get('pid') == self.app.pid else {}

    def elements(self):
        return {e['id']: e for e in self.frame().get('elements', []) if e.get('visible')}

    def windows(self):
        return len(self.frame().get('windows', []))

    def wait(self, what, check, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if self.app.poll() is not None:
                raise Failure(f'the app exited with {self.app.returncode} waiting for {what}')
            if check():
                return
            time.sleep(0.1)
        raise Failure(f'{what} did not happen in {seconds} s')

    def wait_for(self, name, seconds=20, present=True):
        self.wait(f'{name} {"appearing" if present else "going"}',
                  lambda: (name in self.elements()) == present, seconds)

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

    def center(self, name):
        element = self.elements().get(name)
        if not element:
            raise Failure(f'{name} is not on screen')
        box = element['visible']
        return box['x'] + box['w'] / 2, box['y'] + box['h'] / 2

    def click(self, name):
        x, y = self.center(name)
        self.command(f'click {x} {y}')

    def shell(self, line):
        self.type(line)
        self.key('enter')

    def receipt(self, name, seconds=20):
        path = self.receipts / name
        self.wait(f'the shell writing {name}', lambda: path.exists() and path.read_text().strip(),
                  seconds)
        return path.read_text().strip()

    def stubborn_job(self, name):
        """Starts a background job in the focused pane that ignores SIGHUP."""
        self.shell(f"sh -c 'trap \"\" HUP; echo $$ > {self.receipts}/{name}; exec sleep 1000' &")
        job = int(self.receipt(name))
        self.step('a job that ignores SIGHUP runs', alive(job), f'pid {job}')
        return job

    def shot(self, label):
        time.sleep(0.6)
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

    def sessions(self):
        connection = sqlite3.connect(self.database.as_uri() + '?mode=ro', uri=True, timeout=1)
        try:
            rows = connection.execute('SELECT payload FROM sessions ORDER BY position').fetchall()
        finally:
            connection.close()
        return [json.loads(row[0]) for row in rows]

    def journey(self):
        receipts = self.receipts
        self.launch()
        self.key('secondary-n')
        self.wait_for('composer-field')
        self.step('Cmd+N opens a new Claude session', True)

        self.key('secondary-alt-shift-n')
        self.wait(f'a second window', lambda: self.windows() == 2)
        self.wait_for('composer-field')
        self.step('the session opens in a second window', True)

        self.key('secondary-j')
        self.wait_for('terminal-drawer')
        self.wait_for('terminal-pane-0')
        self.step('Cmd+J opens the terminal', True)
        self.shell(f'echo $$ > {receipts}/shell.pid; pwd > {receipts}/pwd.txt; pwd')
        pid = int(self.receipt('shell.pid'))
        folder = self.receipt('pwd.txt')
        self.step('the shell starts in the session folder',
                  os.path.realpath(folder) == os.path.realpath(self.project),
                  f'{folder} for {self.project}')
        self.step('the shell is the login zsh', subprocess.run(
            ['ps', '-o', 'comm=', '-p', str(pid)], capture_output=True, text=True
        ).stdout.strip().endswith('zsh'), f'pid {pid}')
        self.shell(f'stty size > {receipts}/size-before.txt')
        before = self.receipt('size-before.txt')
        self.command('resize 860 560')
        time.sleep(1)
        self.shell(f'stty size > {receipts}/size-1.txt')
        first = self.receipt('size-1.txt')
        self.shell("sh -c 'trap \"echo w >> " + str(receipts) + "/winch.txt\" WINCH; "
                   "i=0; while [ $i -lt 30 ]; do sleep 0.1; i=$((i+1)); done'; "
                   f'stty size > {receipts}/size-2.txt')
        second_size = self.receipt('size-2.txt', 30)
        winched = (receipts / 'winch.txt').exists()
        self.step('with two windows of different sizes the PTY size holds still',
                  first == second_size and not winched,
                  f'{first} then {second_size} 3 s later, SIGWINCH {"seen" if winched else "never"}')
        self.step('the smaller window sets the size',
                  int(first.split()[1]) < int(before.split()[1]),
                  f'{before} with equal windows, {first} with this one smaller')
        self.shot('smaller-window-sets-the-size')
        self.command('resize 1100 720')
        time.sleep(1)

        self.shell(f'seq 1 300; echo seq-done > {receipts}/seq.txt')
        self.receipt('seq.txt')
        self.shot('terminal-after-seq')

        x, y = self.center('terminal-pane-0')
        self.command(f'wheel {x} {y} 120')
        self.shot('terminal-scrolled-back')
        self.command(f'wheel {x} {y} -400')

        self.key('secondary-shift-d')
        self.wait_for('terminal-pane-1')
        self.step('Cmd+Shift+D splits the terminal', True)
        self.shell(f'echo $$ > {receipts}/second.pid')
        second = int(self.receipt('second.pid'))
        self.key('secondary-alt-left')
        self.shell(f'echo $$ > {receipts}/left.pid')
        self.step('Cmd+Alt+Left focuses the first pane', int(self.receipt('left.pid')) == pid)
        self.key('secondary-alt-right')
        self.shell(f'echo $$ > {receipts}/right.pid')
        self.step('Cmd+Alt+Right focuses the second pane',
                  int(self.receipt('right.pid')) == second)
        split_job = self.stubborn_job('split-job.pid')
        self.shot('terminal-split')
        self.click('terminal-pane-1')
        self.wait_for('terminal-pane-1-close')
        self.click('terminal-pane-1-close')
        self.wait_for('terminal-pane-1', present=False)
        self.wait(f'the closed pane\'s shell {second} ending', lambda: not alive(second), 5)
        self.step('a pane\'s close button ends its shell', True, f'pid {second}')
        self.wait(f'its job {split_job} that ignores SIGHUP ending',
                  lambda: not alive(split_job), 5)
        self.step('and its job that ignores SIGHUP', True, f'pid {split_job}')

        self.click('terminal-pane-0')
        self.key('secondary-j')
        self.wait_for('terminal-drawer', present=False)
        self.step('Cmd+J hides the focused terminal', alive(pid), f'shell {pid} still running')
        self.shot('terminal-hidden')
        self.click('terminal-toggle')
        self.wait_for('terminal-drawer')
        self.step('the top bar button shows it again', True)
        self.shot('terminal-shown-by-click')

        self.key('secondary-shift-w')
        self.wait('the second window closing', lambda: self.windows() == 1)
        self.wait_for('terminal-drawer')
        self.step('closing a window keeps the shell', alive(pid), f'pid {pid}')
        self.shot('first-window-after-close')

        self.key('secondary-alt-shift-n')
        self.wait('a reopened window', lambda: self.windows() == 2)
        self.wait_for('terminal-drawer')
        self.step('a new window shows the same terminal', True)
        self.click('terminal-pane-0')
        self.shell(f'echo $$ > {receipts}/reopened.pid')
        self.step('it is the same shell', int(self.receipt('reopened.pid')) == pid)
        self.shot('reopened-window')

        job = self.stubborn_job('job.pid')
        self.click('composer-field')
        self.type(DRAFT)
        time.sleep(1)
        self.quit()
        time.sleep(0.2)
        self.step('Quit ends the shell', not alive(pid), f'pid {pid}')
        self.step('Quit ends a job that ignores SIGHUP', not alive(job), f'pid {job}')
        sessions = self.sessions()
        self.step('one session is saved', len(sessions) == 1, f'{len(sessions)} sessions')
        self.step('the draft is saved', sessions[0].get('draft') == DRAFT,
                  repr(sessions[0].get('draft')))

        self.launch()
        self.step('relaunch reopens the session', True)
        self.shot('restored')
        self.step('the terminal starts closed after a relaunch',
                  'terminal-drawer' not in self.elements())
        sessions = self.sessions()
        self.step('the draft survives the relaunch',
                  len(sessions) == 1 and sessions[0].get('draft') == DRAFT)
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


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def background():
    key = 'KEYSTONE_AUTOMATION_BACKGROUND'
    return {key: os.environ[key]} if key in os.environ else {}


def main():
    if sys.platform != 'darwin':
        sys.exit('terminal-journey runs on macOS. On Linux, use the verify skill.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    binary = ROOT / 'target/debug/keystone-desktop'
    evidence = ROOT / '.audit' / time.strftime('terminal-journey-%Y%m%d-%H%M%S')
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
        sys.exit(f'terminal-journey failed: {error}')
    print('terminal-journey passed')


if __name__ == '__main__':
    main()
