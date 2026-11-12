#!/usr/bin/env python3
"""Settings > Shortcuts and palette audit for Keystone on Linux (TOB-301).

Screenshots every screenful of Settings > Shortcuts, in dark and light, in the default
state and after rebinding New Claude to Ctrl+G, Split terminal to Ctrl+K and Quit to
Ctrl+P, then after resetting each. Records which rows show a clash. Then screenshots
the palette's hints with focus outside the terminal and inside a pane.

It drives the real app on the verify skill's private Sway, as
scripts/terminal-journey-linux.py does, and checks clashes by element name. Read the
screenshots for the hints. Run from the workspace root after
`cargo build --workspace --locked`: python3 scripts/shortcuts-audit-linux.py
"""
import importlib.util
import json
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('journey', ROOT / 'scripts/terminal-journey-linux.py')
journey = importlib.util.module_from_spec(spec)
spec.loader.exec_module(journey)


class Audit(journey.Journey):
    def __init__(self):
        native = journey.load_driver()
        super().__init__(str(native.default_driver()), str(ROOT / 'target/debug/keystone-desktop'))

    def screens(self, label):
        """Screenshots the Shortcuts page from top to bottom; returns the rows with a clash."""
        for _ in range(40):
            self.call('wheel', id='settings-shortcuts', vertical=-10)
        time.sleep(0.3)
        warnings, rows, n = set(), set(), 0
        while True:
            n += 1
            self.shot(f'{label}-{n:02d}')
            for e in self.elements():
                if e['id'].startswith('settings-shortcut-') and e['visible']:
                    if e['id'].endswith('-warning'):
                        warnings.add(e['id'][len('settings-shortcut-'):-len('-warning')])
                    elif not e['id'].endswith('-reset'):
                        rows.add(e['id'][len('settings-shortcut-'):])
            before = [(e['id'], e['bounds']['y']) for e in self.elements()
                      if e['id'].startswith('settings-shortcut-')][:1]
            self.call('wheel', id='settings-shortcuts', vertical=6)
            time.sleep(0.4)
            after = [(e['id'], e['bounds']['y']) for e in self.elements()
                     if e['id'].startswith('settings-shortcut-')][:1]
            if before == after or n > 30:
                break
        return sorted(warnings), sorted(rows)

    def record(self, command, *keys):
        self.scroll_into_view(f'settings-shortcut-{command}')
        self.click(f'settings-shortcut-{command}')
        time.sleep(0.3)
        self.keys(*keys)
        time.sleep(0.6)

    def reset(self, command):
        self.scroll_into_view(f'settings-shortcut-{command}-reset')
        self.click(f'settings-shortcut-{command}-reset')
        time.sleep(0.6)

    def scroll_into_view(self, name):
        for _ in range(40):
            self.call('wheel', id='settings-shortcuts', vertical=-10)
        for _ in range(40):
            e = self.element(name)
            if e and e['visible'] == e['bounds']:
                return
            self.call('wheel', id='settings-shortcuts', vertical=3)
            time.sleep(0.15)
        raise journey.Failure(f'{name} never came into view')

    def palette(self, label, open_keys, queries):
        for query in queries:
            self.keys(*open_keys)
            time.sleep(0.4)
            if query:
                self.type(query)
                time.sleep(0.4)
            self.shot(f'palette-{label}-{query or "all"}'.replace(' ', '-'))
            self.keys('Escape')
            time.sleep(0.3)

    def run(self):
        env = {
            'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
            'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
            'KEYSTONE_PROJECT_DIR': str(self.project),
            'CLAUDE_CONFIG_DIR': str(self.scratch / 'claude'),
            'CODEX_HOME': str(self.scratch / 'codex'),
        }
        started = self.call('start', width=1100, height=720, env=env)
        self.evidence = started['evidence']
        self.wait_for('welcome-claude', seconds=60)
        self.keys('ctrl', 'n')
        self.wait_for('send')
        self.keys('ctrl', 'j')
        self.wait_for('terminal-pane-0')
        expected = {
            'default': [],
            'rebound': ['split-terminal'],
            'reset': [],
        }
        for theme, option in (('dark', 2), ('light', 1)):
            self.keys('ctrl', 'comma')
            self.wait_for(f'settings-theme-option-{option}')
            self.click(f'settings-theme-option-{option}')
            time.sleep(0.4)
            self.click('settings-section-shortcuts')
            self.wait_for('settings-shortcuts')
            warnings, rows = self.screens(f'{theme}-default')
            self.step(f'{theme}: default page names no clash', warnings == expected['default'],
                      json.dumps(warnings))
            self.step(f'{theme}: every command has a row', len(rows) >= 69, f'{len(rows)} rows')
            self.record('new-claude', 'ctrl', 'g')
            self.record('split-terminal', 'ctrl', 'k')
            self.record('quit', 'ctrl', 'p')
            saved = self.call('workspace').get('workspace', {}).get('preferences', {}).get('shortcuts')
            self.step(f'{theme}: the three rebinds are saved', saved == {
                'new-claude': 'secondary-g', 'split-terminal': 'secondary-k', 'quit': 'secondary-p'},
                json.dumps(saved))
            warnings, _ = self.screens(f'{theme}-rebound')
            self.step(f'{theme}: only Split terminal names a clash after the rebinds',
                      warnings == expected['rebound'], json.dumps(warnings))
            for command in ('new-claude', 'split-terminal', 'quit'):
                self.reset(command)
            warnings, _ = self.screens(f'{theme}-reset')
            self.step(f'{theme}: after Reset the page names no clash', warnings == expected['reset'],
                      json.dumps(warnings))
            self.keys('ctrl', 'comma')
            self.wait_for('terminal-drawer')
            self.click('composer-field')
            self.palette(f'{theme}-outside', ('ctrl', 'k'), ['', 'terminal', 'browser', 'quit'])
            self.click('terminal-pane-0')
            self.palette(f'{theme}-in-pane', ('ctrl', 'shift', 'p'), ['', 'terminal', 'browser', 'quit'])
        self.quit()


if __name__ == '__main__':
    audit = Audit()
    error = None
    try:
        audit.run()
    except Exception as failure:
        error = failure
        print(f'FAIL {type(failure).__name__}: {failure}', file=sys.stderr)
    sys.exit(audit.finish(error))
