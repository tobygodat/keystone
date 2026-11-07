#!/usr/bin/env python3
"""Shortcuts journey for Keystone on macOS, through the same automation inbox as
macos-smoke.py.

Rebinds New Claude session from Settings, Shortcuts with the mouse, uses the new keys,
checks the old ones no longer run it, sees the new hint in the palette and the new
session menu, quits with Cmd+Q, relaunches, and uses the binding again. Then it refuses
a key the composer types, resets the binding, and finally launches a workspace whose
saved shortcuts hold bad entries: the app starts, binds the good entry and lists the
skipped ones.

Run from the workspace root: python3 scripts/macos-shortcuts.py
Evidence goes to .audit/macos-shortcuts-<time>/: numbered PNGs and report.json.
"""

import argparse
import importlib.util
import json
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('macos_smoke', ROOT / 'scripts/macos-smoke.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)
Failure = smoke.Failure

BAD = {'new-claude': 'secondary-j', 'quit': 7, 'no-such-command': 'secondary-y',
       'focus-search': 'a'}


class Run(smoke.Run):
    def __init__(self, binary, evidence, seed=None):
        super().__init__(binary, evidence)
        if seed is not None:
            legacy = {'version': 1, 'next_id': 1,
                      'workspace': {'sessions': [], 'preferences': {'shortcuts': seed}}}
            self.database.with_suffix('.json').write_text(json.dumps(legacy))

    def shortcuts(self):
        rows = self.read('SELECT payload FROM workspace')
        if not rows:
            return None
        return json.loads(rows[0][0])['workspace'].get('preferences', {}).get('shortcuts')

    def settle(self, check, seconds=5):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if check():
                return True
            time.sleep(0.1)
        return check()

    def open_shortcuts(self):
        self.key('secondary-,')
        self.wait_for('settings-page')
        self.click('settings-section-shortcuts')
        self.wait_for('settings-shortcuts')

    def close_settings(self):
        self.key('escape')
        self.wait_for('settings-page', present=False)

    def journey(self):
        self.launch()
        self.open_shortcuts()
        self.step('the mouse opens Settings, Shortcuts', True)
        self.shot('shortcuts-page')

        self.click('settings-shortcut-new-claude')
        self.shot('recording')
        self.key('secondary-j')
        self.step('Cmd+J is saved for New Claude session',
                  self.settle(lambda: self.shortcuts() == {'new-claude': 'secondary-j'}),
                  json.dumps(self.shortcuts()))
        self.wait_for('settings-shortcut-new-claude-reset')
        self.shot('rebound')
        self.close_settings()

        self.key('secondary-n')
        time.sleep(1.5)
        self.step('Cmd+N no longer starts a Claude session', len(self.sessions()) == 0,
                  f'{len(self.sessions())} sessions')
        self.key('secondary-j')
        self.wait_for('composer-field')
        self.step('Cmd+J starts a Claude session',
                  self.settle(lambda: [s.get('provider') for s in self.sessions()] == ['Claude']),
                  json.dumps([s.get('provider') for s in self.sessions()]))
        self.key('secondary-k')
        self.wait_for('palette')
        self.shot('palette-hint')
        self.key('escape')
        self.wait_for('palette', present=False)
        self.click('new-session')
        self.wait_for('new-claude')
        self.shot('new-session-menu-hint')
        self.key('escape')

        self.quit()
        self.launch()
        self.key('secondary-j')
        self.step('after relaunch Cmd+J still starts a Claude session',
                  self.settle(lambda: len(self.sessions()) == 2), f'{len(self.sessions())} sessions')
        self.open_shortcuts()
        self.shot('relaunched-shortcuts')

        self.click('settings-shortcut-focus-search')
        self.key('a')
        self.wait_for('settings-shortcut-focus-search-warning')
        self.step('a key the composer types is refused and not saved',
                  self.shortcuts() == {'new-claude': 'secondary-j'}, json.dumps(self.shortcuts()))
        self.shot('refused')

        self.click('settings-shortcut-new-claude-reset')
        self.step('Reset to default removes the override',
                  self.settle(lambda: not self.shortcuts()), json.dumps(self.shortcuts()))
        self.close_settings()
        before = len(self.sessions())
        self.key('secondary-n')
        self.step('Cmd+N starts a Claude session again',
                  self.settle(lambda: len(self.sessions()) == before + 1),
                  f'{len(self.sessions())} sessions')
        self.quit()


class BadConfig(Run):
    def journey(self):
        self.launch()
        self.step('a workspace with bad saved shortcuts starts', True)
        self.key('secondary-j')
        self.wait_for('composer-field')
        self.step('its good entry binds Cmd+J', self.settle(lambda: len(self.sessions()) == 1))
        self.open_shortcuts()
        self.wait_for('settings-shortcuts-skipped')
        self.step('the skipped entries are listed on the page', True)
        self.shot('skipped')
        stderr = (self.evidence / f'app-{self.launches:02}.stderr.log').read_text()
        self.step('each skipped entry is reported at startup',
                  stderr.count('Skipped a saved shortcut.') == 3, stderr.strip())
        self.quit()


def main():
    if sys.platform != 'darwin':
        sys.exit('macos-shortcuts runs on macOS.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    binary = ROOT / 'target/debug/keystone-desktop'
    evidence = ROOT / '.audit' / time.strftime('macos-shortcuts-%Y%m%d-%H%M%S')
    error = None
    for name, run in (('rebind', lambda e: Run(binary, e)),
                      ('bad-config', lambda e: BadConfig(binary, e, BAD))):
        folder = evidence / name
        folder.mkdir(parents=True)
        current = run(folder)
        try:
            current.journey()
        except Failure as failure:
            error = f'{name}: {failure}'
        finally:
            current.finish(error)
        if error:
            break
    print(f'evidence: {evidence}')
    if error:
        sys.exit(f'macos-shortcuts failed: {error}')
    print('macos-shortcuts passed')


if __name__ == '__main__':
    main()
