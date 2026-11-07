#!/usr/bin/env python3
"""Worktree setup script journey for Keystone on macOS (TOB-298).

Launches the real debug app with the scripted providers, a throwaway home, workspace
and git repository as the project, and drives it through the automation inbox the
way scripts/macos-smoke.py does. Through clicks and keys only it:

- sets a passing setup script in Settings > Project, creates a worktree session from
  the welcome screen's "In worktree" pill, sees the running card, then the finished
  line, and sends a message;
- sets a failing script from the palette's "Edit worktree setup script…", creates a
  second worktree session with Cmd+Alt+G, sees the failure card, checks that Send
  keeps the draft, clicks Skip and sends;
- quits with Cmd+Q, relaunches, and finds the script in Settings and the database;
- runs the AGENTS.md draft journey: Cmd+N, a draft, Cmd+Q, relaunch, the draft is back.

Run from the workspace root: python3 scripts/setup-script-journey.py [--no-build]
Evidence goes to .audit/setup-journey-<time>/: numbered PNGs and report.json.
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
REPLY = 'Hello from the fake Claude in a worktree.'
PASSING = 'echo installing; sleep 3; echo done'
FAILING = 'echo "missing .env" >&2; exit 1'
HELD = 'Held until the script is skipped'
DRAFT = 'Draft kept across restart'


class Failure(Exception):
    pass


class Run:
    def __init__(self, binary, evidence):
        self.binary = binary
        self.evidence = evidence
        self.scratch = Path(tempfile.mkdtemp(prefix='keystone-setup-journey-'))
        self.home = self.scratch / 'home'
        self.project = self.scratch / 'project'
        self.inbox = self.scratch / 'inbox'
        self.geometry = self.scratch / 'elements.json'
        self.database = self.scratch / 'workspace.sqlite3'
        for folder in (self.home / 'claude', self.home / 'codex', self.project, self.inbox):
            folder.mkdir(parents=True)
        git = ['git', '-C', str(self.project)]
        subprocess.run(['git', 'init', '-q', '--initial-branch=main', str(self.project)], check=True)
        subprocess.run(git + ['config', 'user.name', 'Setup journey'], check=True)
        subprocess.run(git + ['config', 'user.email', 'setup@example.invalid'], check=True)
        (self.project / 'README').write_text('fixture\n')
        subprocess.run(git + ['add', 'README'], check=True)
        subprocess.run(git + ['commit', '-q', '-m', 'base'], check=True)
        self.project = self.project.resolve()
        self.app = None
        self.launches = 0
        self.shots = 0
        self.sequence = 0
        self.steps = []

    def env(self):
        return {
            'HOME': str(self.home), 'USER': os.environ.get('USER', ''), 'SHELL': '/bin/zsh',
            'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'TMPDIR': os.environ.get('TMPDIR', '/tmp'),
            'TERM': 'dumb',
            'KEYSTONE_STATE_PATH': str(self.database),
            'KEYSTONE_AUTOMATION_PATH': str(self.geometry),
            'KEYSTONE_AUTOMATION_INBOX': str(self.inbox),
            'KEYSTONE_CLAUDE_PATH': str(self.binary.parent / 'keystone-fake-claude'),
            'KEYSTONE_CODEX_PATH': str(self.binary.parent / 'keystone-fake-codex'),
            'KEYSTONE_PROJECT_DIR': str(self.project),
            'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '5', 'KEYSTONE_FAKE_CODEX_DELAY_MS': '5',
            'KEYSTONE_FAKE_CLAUDE_REPLY': REPLY,
            'CLAUDE_CONFIG_DIR': str(self.home / 'claude'), 'CODEX_HOME': str(self.home / 'codex'),
        }

    def step(self, name, ok, detail=''):
        self.steps.append({'step': name, 'ok': bool(ok), 'detail': detail})
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

    def until(self, what, check, seconds=30):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if check():
                return True
            time.sleep(0.2)
        return False

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

    def read(self, sql, *args):
        connection = sqlite3.connect(self.database.as_uri() + '?mode=ro', uri=True, timeout=1)
        try:
            return connection.execute(sql, args).fetchall()
        finally:
            connection.close()

    def saved_script(self):
        rows = self.read('SELECT payload FROM projects WHERE id = ?', str(self.project))
        return json.loads(rows[0][0]).get('setup_script') if rows else None

    def sessions(self):
        return [json.loads(row[0]) for row in self.read('SELECT payload FROM sessions ORDER BY position')]

    def worktree_sessions(self):
        return [s for s in self.sessions()
                if (s.get('execution_context') or {}).get('worktree')]

    def messages(self, session):
        rows = self.read('SELECT payload FROM log_entries WHERE session_id = ?', str(session))
        return [row[0] for row in rows]

    def replied(self, session):
        return any(REPLY in payload for payload in self.messages(session))

    def set_script(self, script):
        self.wait_for('settings-setup-script')
        self.click('settings-setup-script')
        self.key('secondary-a')
        self.type(script)
        self.click('settings-setup-script-save')
        ok = self.until('saved', lambda: self.saved_script() == script, 10)
        self.step('Save writes the setup script to the workspace database', ok,
                  repr(self.saved_script()))

    def send(self, text):
        self.click('composer-field')
        self.type(text)
        self.key('secondary-enter')

    def journey(self):
        self.launch('welcome-claude')
        self.step('launch shows the welcome screen', True)

        self.key('secondary-,')
        self.wait_for('settings-page')
        self.click('settings-section-project')
        self.wait_for('settings-project')
        self.step('Settings > Project opens by click', True)
        self.set_script(PASSING)
        self.shot('settings-passing-script')
        self.click('settings-back')
        self.wait_for('welcome-worktree')

        self.click('welcome-worktree')
        self.wait_for('worktree-dialog')
        self.wait_for('worktree-setup-script')
        self.step('the worktree dialog names the setup script', True)
        self.shot('dialog-names-script')
        self.key('enter')
        self.wait_for('setup', 30)
        self.wait_for('setup-cancel', 10)
        self.step('the session shows the running card with Cancel', True)
        self.wait_for('setup-output', 10)
        self.shot('setup-running')
        self.wait_for('setup-cancel', 30, present=False)
        first = self.worktree_sessions()
        self.step('one worktree session exists', len(first) == 1, f'{len(first)}')
        first_id = first[0]['id']
        self.shot('setup-finished')
        self.send('Hello after setup')
        ok = self.until('reply', lambda: self.replied(first_id), 30)
        self.step('a message goes after the script succeeds and Claude replies', ok)
        self.wait_for('setup', 10, present=False)
        self.step('the finished line goes once the first message is sent', True)
        self.shot('sent-after-setup')

        self.key('secondary-k')
        self.wait_for('palette')
        self.type('edit worktree setup')
        self.key('enter')
        self.wait_for('settings-project')
        self.step('the palette opens Settings > Project', True)
        self.set_script(FAILING)
        self.click('settings-back')
        self.wait_for('composer-field')

        self.key('secondary-alt-g')
        self.wait_for('worktree-dialog')
        self.key('enter')
        self.wait_for('setup-retry', 30)
        self.wait_for('setup-skip')
        self.wait_for('setup-open')
        self.wait_for('setup-output')
        self.step('a failing script shows Retry, Skip, Open folder and its output', True)
        second = [s for s in self.worktree_sessions() if s['id'] != first_id]
        self.step('a second worktree session exists', len(second) == 1, f'{len(second)}')
        second_id = second[0]['id']
        self.shot('setup-failed')

        self.send(HELD)
        self.wait_for('conversation-notice', 10)
        time.sleep(1.5)
        draft = next(s for s in self.sessions() if s['id'] == second_id).get('draft')
        self.step('Send is refused and the draft stays', draft == HELD and not self.messages(second_id),
                  repr(draft))
        self.shot('send-held')

        self.click('setup-skip')
        self.wait_for('setup', 10, present=False)
        self.click('composer-field')
        self.key('secondary-enter')
        ok = self.until('reply', lambda: self.replied(second_id), 30)
        self.step('after Skip the held draft sends and Claude replies', ok)
        self.shot('sent-after-skip')

        self.quit()
        self.step('the setting is in the database after quitting', self.saved_script() == FAILING,
                  repr(self.saved_script()))
        self.launch('composer-field')
        self.key('secondary-,')
        self.wait_for('settings-page')
        self.click('settings-section-project')
        self.wait_for('settings-setup-script')
        self.shot('settings-after-relaunch')
        self.step('the setting survives a relaunch', self.saved_script() == FAILING)
        self.click('settings-back')
        self.wait_for('composer-field')

        sessions_before = len(self.sessions())
        self.key('secondary-n')
        ok = self.until('new session', lambda: len(self.sessions()) == sessions_before + 1, 10)
        self.step('Cmd+N creates a Claude session', ok)
        self.wait_for('composer-field')
        self.click('composer-field')
        self.type(DRAFT)
        time.sleep(1.5)
        self.shot('draft')
        self.quit()
        latest = self.sessions()[-1]
        self.step('the draft is saved', latest.get('draft') == DRAFT, repr(latest.get('draft')))
        self.launch('composer-field')
        self.shot('draft-restored')
        latest = self.sessions()[-1]
        self.step('the draft survives the relaunch', latest.get('draft') == DRAFT)
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


def main():
    if sys.platform != 'darwin':
        sys.exit('setup-script-journey runs on macOS. On Linux, use the verify skill.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    binary = ROOT / 'target/debug/keystone-desktop'
    evidence = ROOT / '.audit' / time.strftime('setup-journey-%Y%m%d-%H%M%S')
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
        sys.exit(f'setup-script-journey failed: {error}')
    print('setup-script-journey passed')


if __name__ == '__main__':
    main()
