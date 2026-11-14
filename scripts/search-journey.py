#!/usr/bin/env python3
"""Find in a session and search across sessions, in the real app (TOB-456).

Drives the real debug app on the verify skill's private headless Sway with real Wayland
input and the scripted providers. The journey: Ctrl+N, a draft, Ctrl+Q and a relaunch
bring the session and its draft back; a machine is linked through a stand-in `ssh`, as
the verify skill's machines page describes, and a Claude session there is asked about a
needle; then, with the app closed, `seed_search` adds a 1,500-row session and 220 more
sessions, a quarter of them settled. After a relaunch: find in the long session by
keyboard and by mouse with the terminal drawer and Files open, the terminal keeping
Ctrl+F; the sidebar's search listing message results, opening one by Down and Enter and
another, the remote session's, by a click; light and dark at 1920x1080 and 1100x720.

The harness reads state from elements and the workspace database. It cannot read text
off the screen, so look at the screenshots.

Run from the workspace root after
`cargo build -p keystone-desktop -p keystone-agent --bins --examples --locked`:
    python3 scripts/search-journey.py [--driver PATH]
Evidence goes to .audit/search-journey-<time>/: numbered PNGs and journey.json.
"""

import argparse
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DRAFT = 'Draft kept across the search journey'
SEEDED = 220
TURNS = 300
LARGE, SMALL = (1920, 1080), (1100, 720)

SSH = '''#!/bin/sh
# Skips ssh's options up to "--", then runs the remote command as a login shell on a
# clean host environment.
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--" ]; then shift; break; fi
    shift
done
shift
exec env -i HOME={home} KEYSTONE_HOST_DIR={hostdir} KEYSTONE_CLAUDE_PATH={claude} \\
    KEYSTONE_FAKE_CLAUDE_DELAY_MS=2 PATH=/usr/bin:/bin sh -c "$*"
'''


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Search:
    def __init__(self, j, out):
        self.j, self.out = j, out
        self.sway = None
        self.state = None

    def palette(self, command):
        j = self.j
        j.keys('ctrl', 'k')
        j.wait_for('palette-input')
        j.type(command)
        j.settle()
        j.keys('Return')
        j.settle()

    def ids(self, pattern):
        return self.j.ids(pattern)

    def resize(self, size):
        width, height = size
        subprocess.run(['swaymsg', '-s', self.sway, '--', 'output', '*', 'mode', '--custom',
                        f'{width}x{height}'], check=True, capture_output=True)
        time.sleep(0.5)
        self.j.settle()

    def element(self, name):
        return next((e for e in self.j.elements() if e['id'] == name and e['visible']), None)

    def find_count_shown(self):
        return self.element('find-count') is not None

    def standard(self):
        j = self.j
        j.keys('ctrl', 'n')
        j.wait('a Claude session', lambda: len(j.sessions()) == 1)
        j.wait_for('composer-field')
        j.click('composer-field')
        j.type(DRAFT)
        session = j.sessions()[0]['id']
        j.wait('the draft to save', lambda: j.draft(session) == DRAFT, 10)
        j.shot('draft-typed')
        j.quit()
        j.relaunch()
        j.step('after a relaunch the session and its draft come back', j.draft(session) == DRAFT)
        j.shot('draft-restored')

    def remote(self, folder):
        j = self.j
        self.palette('Link a machine')
        j.wait_for('machine-destination')
        j.click('machine-destination')
        j.type('desk')
        j.click('machine-link')
        j.wait('the machine to connect', lambda: any(
            e['id'].startswith('machine-state-') for e in j.elements()), 30)
        j.settle()
        j.keys('Escape')
        self.palette('New Claude session on a machine')
        j.keys('Return')
        j.settle()
        j.type(str(folder))
        j.settle()
        j.keys('Return')
        j.wait('a session on the machine', lambda: len(j.sessions()) == 2, 20)
        remote = j.sessions()[-1]['id']
        j.wait_for('composer-field')
        j.send('Where did the remote needle go?')
        j.wait_for('send', seconds=30)
        j.settle()
        # The first message names the session, so a word only a later message holds
        # finds it by its messages.
        j.send('Look under the haystack on the desk.')
        j.wait_for('send', seconds=30)
        j.settle()
        j.shot('remote-session-replied')
        return remote

    def seed(self):
        j = self.j
        j.quit()
        result = subprocess.run(
            [str(ROOT / 'target/debug/examples/seed_search'), self.state, str(j.project),
             str(SEEDED), str(TURNS)], check=True, capture_output=True, text=True)
        long = int(re.search(r'long session (\d+)', result.stdout).group(1))
        j.relaunch()
        count = len(j.sessions())
        j.step(f'the workspace holds {SEEDED + 3} sessions', count == SEEDED + 3, f'{count}')
        return long

    def open_session(self, session, title):
        j = self.j
        j.keys('ctrl', 'shift', 'f')
        j.keys('ctrl', 'a')
        j.type(title)
        j.settle()
        j.wait_for(f'session-{session}')
        j.click(f'session-{session}')
        j.wait('the session to open', lambda: j.workspace()['workspace'].get('selected') == session)
        j.keys('ctrl', 'shift', 'f')
        j.keys('ctrl', 'a')
        j.keys('BackSpace')
        j.settle()

    def find_by_keyboard(self, label):
        j = self.j
        j.keys('ctrl', 'l')
        j.keys('ctrl', 'f')
        j.wait_for('find-bar')
        j.type('needle')
        j.wait('the count', self.find_count_shown)
        j.settle()
        j.shot(f'{label}-find-typed')
        rows = []
        for _ in range(3):
            j.keys('Return')
            j.settle()
            rows.append(self.ids(r'transcript-row-(\d+)'))
        j.step(f'{label}: Enter steps through matches across the session',
               rows[0] != rows[1] != rows[2], json.dumps([r[:1] for r in rows]))
        j.shot(f'{label}-find-stepped')
        j.keys('shift', 'Return')
        j.settle()
        j.step(f'{label}: Shift+Enter goes back to the previous match',
               self.ids(r'transcript-row-(\d+)') == rows[1])
        j.keys('Escape')
        j.wait_for('find-bar', present=False)
        j.settle()
        j.shot(f'{label}-find-closed')

    def find_by_mouse(self, label):
        j = self.j
        self.palette('Find in session')
        j.wait_for('find-bar')
        j.type('needle')
        j.wait('the count', self.find_count_shown)
        before = self.ids(r'transcript-row-(\d+)')
        j.click('find-next')
        j.settle()
        j.click('find-next')
        j.settle()
        after = self.ids(r'transcript-row-(\d+)')
        j.step(f'{label}: the next button moves to another match', before != after)
        j.click('find-previous')
        j.settle()
        j.shot(f'{label}-find-mouse')
        j.click('find-close')
        j.wait_for('find-bar', present=False)
        j.step(f'{label}: the close button closes the bar', True)

    def terminal_keeps_ctrl_f(self):
        j = self.j
        j.click('terminal-drawer')
        j.settle()
        j.keys('ctrl', 'f')
        j.settle()
        j.step('in a terminal pane Ctrl+F stays the shell\'s', self.element('find-bar') is None)

    def sidebar(self, remote, label):
        j = self.j
        j.keys('ctrl', 'shift', 'f')
        j.keys('ctrl', 'a')
        j.type('needle')
        j.settle()
        j.wait_for('message-results', seconds=20)
        j.settle()
        j.shot(f'{label}-sidebar-results')
        results = [e for e in j.elements() if e['id'].startswith('message-result-') and e['visible']]
        j.step(f'{label}: message results list under the title matches', len(results) > 3,
               f'{len(results)} on screen')
        top = min(results, key=lambda e: e['bounds']['y'])
        target = int(top['id'].rsplit('-', 1)[1])
        j.keys('Down')
        j.settle()
        j.shot(f'{label}-sidebar-highlighted')
        j.keys('Return')
        j.wait_for('find-bar')
        j.wait('the count', self.find_count_shown)
        j.settle()
        selected = j.workspace()['workspace'].get('selected')
        j.step(f'{label}: Enter opens the first result with find on the same words',
               selected == target, f'selected {selected}, wanted {target}')
        j.shot(f'{label}-result-opened')
        j.keys('Escape')
        j.keys('ctrl', 'shift', 'f')
        j.keys('ctrl', 'a')
        j.type('haystack')
        j.settle()
        j.wait_for(f'message-result-{remote}', seconds=20)
        j.shot(f'{label}-sidebar-remote-result')
        j.settle()
        j.click(f'message-result-{remote}')
        j.wait('the remote session to open',
               lambda: j.workspace()['workspace'].get('selected') == remote)
        j.wait_for('find-bar')
        j.wait('the count', self.find_count_shown)
        j.settle()
        j.shot(f'{label}-remote-result-opened')
        j.step(f'{label}: a click opens the session on the other machine at its message', True)
        j.keys('Escape')

    def looks(self, long, size, label):
        j = self.j
        self.resize(size)
        for option, theme in ((1, 'light'), (2, 'dark')):
            j.theme(option)
            self.open_session(long, 'Long session')
            j.keys('ctrl', 'l')
            j.keys('ctrl', 'f')
            j.wait_for('find-bar')
            j.keys('ctrl', 'a')
            j.type('needle')
            j.wait('the count', self.find_count_shown)
            j.keys('Return')
            j.settle()
            j.shot(f'{label}-{theme}-find')
            j.keys('Escape')
            j.keys('ctrl', 'shift', 'f')
            j.keys('ctrl', 'a')
            j.type('needle')
            j.wait_for('message-results', seconds=20)
            j.keys('Down')
            j.settle()
            j.shot(f'{label}-{theme}-sidebar')
            j.keys('ctrl', 'a')
            j.keys('BackSpace')
            j.settle()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--driver', default=None)
    args = parser.parse_args()
    journey = load('edit_diff_journey', ROOT / 'scripts/edit-diff-journey.py')
    driver = args.driver or str(journey.load_driver().default_driver())
    j = journey.Journey(driver, str(ROOT / 'target/debug/keystone-desktop'))
    out = ROOT / '.audit' / f'search-journey-{time.strftime("%Y%m%dT%H%M%S")}'
    out.mkdir(parents=True)
    bin_dir, home, folder = out / 'bin', out / 'host-home', out / 'remote-folder'
    for path in (bin_dir, home, home / 'bin', folder):
        path.mkdir()
    # The host's socket path must fit SUN_LEN, so its folder is short.
    hostdir = Path(tempfile.mkdtemp(prefix='kh', dir='/tmp'))
    shutil.copy(ROOT / 'target/debug/keystone-host', home / 'bin/keystone-host')
    (home / '.profile').write_text(f'export PATH={home}/bin:$PATH\n')
    ssh = bin_dir / 'ssh'
    ssh.write_text(SSH.format(home=home, hostdir=hostdir,
                              claude=ROOT / 'target/debug/keystone-fake-claude'))
    ssh.chmod(0o755)
    env = {
        'PATH': f'{bin_dir}:{os.environ["PATH"]}',
        'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
        'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
        'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '2',
        'KEYSTONE_PROJECT_DIR': str(j.project),
        'CLAUDE_CONFIG_DIR': str(j.scratch / 'claude'),
        'CODEX_HOME': str(j.scratch / 'codex'),
    }
    s = Search(j, out)
    error = None
    try:
        started = j.call('start', width=LARGE[0], height=LARGE[1], env=env)
        s.sway = started['session'].get('sway_socket') or started['session'].get('swaysock')
        s.state = started['state']
        j.wait_for('welcome-claude', seconds=60)
        s.standard()
        remote = s.remote(folder)
        long = s.seed()
        j.settle()
        j.shot('seeded-sidebar')
        s.open_session(long, 'Long session')
        j.keys('ctrl', 'j')
        j.wait_for('terminal-drawer', seconds=20)
        j.keys('ctrl', 'shift', 'e')
        j.settle()
        j.shot('long-session-with-terminal-and-files')
        s.find_by_keyboard('drawer')
        s.find_by_mouse('drawer')
        s.terminal_keeps_ctrl_f()
        j.keys('ctrl', 'j')
        j.keys('ctrl', 'shift', 'e')
        j.settle()
        s.sidebar(remote, 'large')
        s.looks(long, LARGE, 'large')
        s.looks(long, SMALL, 'small')
        j.quit()
    except Exception as failure:
        error = f'{type(failure).__name__}: {failure}'
        print(f'FAIL {error}', flush=True)
        try:
            j.shot('failure')
        except Exception:
            pass
    finally:
        j.call('stop')
        for shot in j.shots:
            shutil.copy(shot, out)
        (out / 'journey.json').write_text(json.dumps({'steps': j.steps, 'shots': j.shots,
                                                       'error': error}, indent=2))
        shutil.rmtree(hostdir, ignore_errors=True)
        print(f'evidence: {out}')
    if error:
        sys.exit(f'search-journey failed: {error}')
    print('search-journey passed')


if __name__ == '__main__':
    main()
