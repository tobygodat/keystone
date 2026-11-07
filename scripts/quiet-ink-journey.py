#!/usr/bin/env python3
"""Screenshots of the quiet-ink composer, side panels and Settings on macOS (TOB-300).

Drives the real debug app through the automation inbox, in the background when
KEYSTONE_AUTOMATION_BACKGROUND is set, with the
scripted Claude, scripts/fake-gh.py and a throwaway home, state and repository that has
staged, unstaged and untracked files and an open pull request. In the default 1100×720
window it checks that Files, Changes and Pull request each open beside the composer's
column, and captures each with the sidebar's group labels.
At 1440×900, in dark, then light, it captures the composer alone (and with a long draft
in dark), the composer with Changes open, the Pull request panel and Settings →
Appearance. It then quits and relaunches, and checks that the draft came back.

Run from the workspace root after `python3 scripts/macos-smoke.py` has built the app:
    python3 scripts/quiet-ink-journey.py
Evidence goes to .audit/quiet-ink-<time>/: numbered PNGs and report.json.
"""

import importlib.util
import json
import os
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('smoke', ROOT / 'scripts/macos-smoke.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

REPLY = ('I traced the flake to the retry loop in `src/upload.rs`. It retries on every error, '
         'including a 401, so a revoked token spins until the test times out.')
DRAFT = 'Also check the retry limit in the CLI'


class Run(smoke.Run):
    def __init__(self, binary, evidence):
        super().__init__(binary, evidence)
        project = self.project
        git = lambda *a: subprocess.run(['git', *a], cwd=project, check=True, capture_output=True)
        git('checkout', '-q', '-b', 'retry-uploads')
        for k, v in [('user.name', 'Test'), ('user.email', 't@example.com'), ('commit.gpgsign', 'false')]:
            git('config', k, v)
        (project / 'src').mkdir()
        for name in ('src/upload.rs', 'src/store.rs', 'README.md'):
            (project / name).write_text('pub fn upload() {}\n')
        git('add', '-A')
        git('commit', '-qm', 'first')
        (project / 'src/store.rs').write_text('pub fn store() {}\n')
        git('add', 'src/store.rs')
        (project / 'src/upload.rs').write_text('pub fn retry_upload() {}\n')
        (project / 'README.md').write_text('# Retry uploads\n')
        (project / 'tests').mkdir()
        (project / 'tests/upload.rs').write_text('#[test]\nfn retries_three_times() {}\n')
        (project / 'docs').mkdir()
        (project / 'docs/retry.md').write_text('Retries back off.\n')
        self.gh = self.scratch / 'gh'
        self.gh_bin = subprocess.run([sys.executable, str(ROOT / 'scripts/fake-gh.py'), 'install',
                                      str(self.gh), str(project)],
                                     check=True, capture_output=True, text=True).stdout.strip()
        (self.gh / 'stage').write_text('review\n')
        # The app adopts the login shell's PATH, which puts login-bin first. fake-gh finds
        # its stage beside its own link, so login-bin gets a wrapper, not another link.
        wrapper = self.home / 'login-bin' / 'gh'
        wrapper.write_text(f'#!/bin/sh\nexec "{self.gh_bin}/gh" "$@"\n')
        wrapper.chmod(0o755)
        (self.scratch / 'gh-config').mkdir()

    def env(self):
        env = super().env()
        env['PATH'] = f'{self.gh_bin}:{env["PATH"]}'
        env['GH_CONFIG_DIR'] = str(self.scratch / 'gh-config')
        env['KEYSTONE_FAKE_CLAUDE_REPLY'] = REPLY
        if os.environ.get('KEYSTONE_AUTOMATION_BACKGROUND'):
            env['KEYSTONE_AUTOMATION_BACKGROUND'] = '1'
        return env

    def replied(self):
        return any('retry loop' in row[0] for row in self.read('SELECT payload FROM log_entries'))

    def shown(self, prefix):
        return any(name.startswith(prefix) for name in self.elements())

    def close(self, keys, prefix):
        for _ in range(3):
            if not self.shown(prefix):
                return
            self.key(keys)
            time.sleep(1)
        self.step(f'{prefix} panel closes', not self.shown(prefix))

    def theme(self, option):
        self.key('secondary-,')
        self.wait_for('settings-page')
        self.click('settings-section-appearance')
        self.wait_for('settings-theme')
        self.click(f'settings-theme-option-{option}')
        time.sleep(1)

    def sized(self, width, height):
        payload = json.loads(self.read('SELECT payload FROM workspace')[0][0])
        for window in payload['workspace']['preferences']['windows']:
            window['placement'].update(width=width, height=height)
        connection = sqlite3.connect(self.database)
        connection.execute('UPDATE workspace SET payload = ?', (json.dumps(payload),))
        connection.commit()
        connection.close()

    def beside_column(self, component):
        payload = json.loads(self.read('SELECT payload FROM workspace')[0][0])
        preferences = payload['workspace']['preferences']
        layouts = [layout for window in preferences.get('windows', [])
                   for layout in window.get('layouts', {}).values()]
        for layout in layouts + list(preferences.get('project_layouts', {}).values()):
            views = {view['id']: view['component'] for view in layout['views']}
            if component in views.values():
                # Alone, or as a tab with another side panel, in the window's top row.
                def holds(node):
                    ids = [node.get('id')] if node.get('kind') == 'view' else node.get('views', [])
                    return node.get('kind') in ('view', 'tabs') and any(
                        views.get(view) == component for view in ids)
                return any(holds(child['node']) for child in layout['root'].get('children', []))
        raise smoke.Failure(f'no saved layout holds {component}')

    def narrow(self):
        for keys, shown, component in [('secondary-shift-e', 'files-', 'files'),
                                       ('secondary-shift-g', 'changes-branch', 'changes'),
                                       ('secondary-alt-p', 'pull-request-title', 'pull_request')]:
            self.key(keys)
            deadline = time.monotonic() + 30
            while not self.shown(shown) and time.monotonic() < deadline:
                time.sleep(0.2)
            time.sleep(1)
            self.step(f'1100 px wide: {component} opens beside the composer column',
                      self.beside_column(component))
            if component == 'pull_request':
                self.wait_for('pull-request-check-0', 30)
                time.sleep(2)
            self.shot(f'1100-{component}')
            self.close(keys, shown)

    def pass_for(self, mode, option):
        self.theme(option)
        self.key('escape')
        self.wait_for('settings-page', present=False)
        self.close('secondary-shift-g', 'changes-')
        self.close('secondary-alt-p', 'pull-request-')
        self.shot(f'{mode}-composer')
        if mode == 'dark':
            self.click('composer-field')
            self.key('secondary-a')
            self.type(' '.join(['Also check the retry limit in the CLI and the backoff cap.'] * 6))
            time.sleep(1)
            self.shot('dark-composer-long-draft')
            self.key('secondary-a')
            self.type(DRAFT)
            time.sleep(1)
        self.key('secondary-shift-g')
        self.wait_for('changes-branch', 20)
        time.sleep(1)
        self.shot(f'{mode}-composer-changes')
        self.key('secondary-alt-p')
        try:
            self.wait_for('pull-request-title', 30)
        except smoke.Failure:
            self.shot(f'{mode}-pull-request-missing')
            print(sorted(n for n in self.elements() if 'pull' in n or 'tab' in n), flush=True)
            print((self.gh / 'calls.log').read_text() if (self.gh / 'calls.log').exists() else 'no gh calls', flush=True)
            raise
        self.wait_for('pull-request-review-0', 30)
        self.shot(f'{mode}-pull-request')
        self.theme(option)
        self.shot(f'{mode}-settings-appearance')
        self.key('escape')
        self.wait_for('settings-page', present=False)

    def journey(self):
        self.launch()
        self.key('secondary-n')
        self.wait_for('composer-field')
        self.step('Cmd+N opens a new Claude session', True)
        self.click('composer-field')
        self.type('Why does the upload test flake on CI?')
        self.key('secondary-enter')
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and not self.replied():
            time.sleep(0.2)
        self.step('the fake Claude replies', self.replied())
        self.wait_for('turn-status', 30, present=False)
        self.click('composer-field')
        self.type(DRAFT)
        time.sleep(1)
        self.quit()
        self.sized(1100, 720)
        self.launch()
        self.theme(2)
        self.key('escape')
        try:
            self.wait_for('settings-page', 10, present=False)
        except smoke.Failure:
            self.shot('settings-stuck')
            print(sorted(self.elements()), flush=True)
            raise
        self.narrow()
        self.quit()
        self.sized(1440, 900)
        self.launch()
        self.step('the draft survives a relaunch', self.sessions()[0].get('draft') == DRAFT)
        self.pass_for('dark', 2)
        self.pass_for('light', 1)
        self.quit()
        self.launch()
        self.shot('light-restored')
        self.step('the draft survives a second relaunch', self.sessions()[0].get('draft') == DRAFT)
        self.quit()


def main():
    binary = ROOT / 'target/debug/keystone-desktop'
    evidence = ROOT / '.audit' / time.strftime('quiet-ink-%Y%m%d-%H%M%S')
    evidence.mkdir(parents=True)
    run = Run(binary, evidence)
    error = None
    try:
        run.journey()
    except smoke.Failure as failure:
        error = str(failure)
    finally:
        run.finish(error)
    print(f'evidence: {evidence}')
    if error:
        sys.exit(f'quiet-ink journey failed: {error}')
    print('quiet-ink journey passed')


if __name__ == '__main__':
    main()
