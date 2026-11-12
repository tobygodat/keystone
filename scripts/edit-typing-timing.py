#!/usr/bin/env python3
"""Typing cost of an open long edit in the real app, as users run it (TOB-310).

Verification launches draw cached views uncached, so they do not measure what a user
pays (ADR 0007). This script prepares a 1,500-line edit with the scripted Codex under
the verify driver, quits, then launches the same debug build on the same private
desktop without KEYSTONE_AUTOMATION_PATH and drives it by keyboard alone. It types 30
characters into the composer, timed until the saved draft holds them, with the edit
row closed and open in turn, and screenshots each state, since nothing can name the
app's elements in this mode.

Run from the workspace root after `cargo build -p keystone-desktop -p keystone-agent --bins --locked`:
    python3 scripts/edit-typing-timing.py [--driver PATH] [--runs N]
Prints each run and writes timing.json beside the screenshots in .audit/edit-typing-timing-<time>/.
"""

import argparse
import importlib.util
import json
import os
import shutil
import statistics
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TEXT = 'Thirty characters typed here..'


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def alive(pid):
    try:
        state = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0]
    except (FileNotFoundError, ProcessLookupError):
        return False
    return state not in ('Z', 'X')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--driver', default=None)
    parser.add_argument('--runs', type=int, default=5)
    args = parser.parse_args()
    journey_module = load('edit_diff_journey', ROOT / 'scripts/edit-diff-journey.py')
    driver = args.driver or str(journey_module.load_driver().default_driver())
    j = journey_module.Journey(driver, str(ROOT / 'target/debug/keystone-desktop'))
    out = ROOT / '.audit' / f'edit-typing-timing-{time.strftime("%Y%m%dT%H%M%S")}'
    out.mkdir(parents=True)
    env = {
        'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
        'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
        'KEYSTONE_FAKE_CODEX_DELAY_MS': '2',
        'KEYSTONE_PROJECT_DIR': str(j.project),
        'CLAUDE_CONFIG_DIR': str(j.scratch / 'claude'),
        'CODEX_HOME': str(j.scratch / 'codex'),
    }
    shots = []
    pid = None

    def shot(label):
        value = j.call('screenshot')
        target = out / f'{len(shots) + 1:02d}-{label}.png'
        shutil.copyfile(value['path'], target)
        shots.append(str(target))

    try:
        j.call('start', width=1100, height=720, env=env)
        j.wait_for('welcome-claude', seconds=60)
        j.keys('ctrl', 'shift', 'n')
        j.wait('a Codex session', lambda: len(j.sessions()) == 1)
        j.wait_for('composer-field')
        j.send('long edit 1500')
        j.edit_rows(1)
        j.wait_for('send')
        composer = next(e['center'] for e in j.elements() if e['id'] == 'composer-field')
        j.quit()

        run = j.server.run
        argv = ['/usr/bin/env', '-u', 'KEYSTONE_VISUAL_FIXTURE', '-u', 'KEYSTONE_AUTOMATION_PATH',
                f'KEYSTONE_STATE_PATH={run.state}', f'DBUS_SESSION_BUS_ADDRESS={run.bus_address}',
                *[f'{key}={value}' for key, value in sorted(run.env.items())],
                str(ROOT / 'target/debug/keystone-desktop')]
        # KS-009: a key from the driver's own keyboard first, as the driver's launch does.
        j.keys('F24')
        launched = j.call('call', tool='app_launch', arguments={'argv': argv})
        pid = launched.get('structuredContent', {}).get('pid')
        session = j.sessions()[0]['id']

        def draft():
            return next(s for s in j.sessions() if s['id'] == session).get('draft')

        def wait(what, check, seconds=20):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if check():
                    return
                time.sleep(0.005)
            raise RuntimeError(f'timed out waiting for {what}')

        windows = lambda: j.call('call', tool='windows_list').get('structuredContent', {}).get('windows', [])
        wait('the app window', lambda: any(w.get('pid') == pid for w in windows()))
        # Its log loads on a worker after the window maps; the closed run waits for it.
        wait('the draft to load', lambda: draft() is not None)
        shot('relaunched-without-automation')

        def toggle():
            j.keys('ctrl', 'shift', 't')
            j.keys('End')
            j.keys('Tab')
            j.keys('Return')
            j.keys('Escape')

        def typing():
            j.call('click', x=composer['x'], y=composer['y'])
            j.keys('ctrl', 'a')
            j.keys('BackSpace')
            wait('the draft to clear', lambda: not draft())
            start = time.monotonic()
            j.type(TEXT)
            wait('the typed draft to save', lambda: draft() == TEXT)
            return round((time.monotonic() - start) * 1000)

        times = {'closed': [], 'open': []}
        state = 'closed'
        for n in range(args.runs * 2):
            times[state].append(typing())
            print(f'{state}: {times[state][-1]} ms', flush=True)
            if n < 2:
                shot(f'typed-with-the-edit-{state}')
            toggle()
            state = 'open' if state == 'closed' else 'closed'
        report = {name: {'runs': values, 'median': statistics.median(values),
                         'range': [min(values), max(values)]} for name, values in times.items()}
        report['shots'] = shots
        (out / 'timing.json').write_text(json.dumps(report, indent=1) + '\n')
        print(json.dumps({k: v for k, v in report.items() if k != 'shots'}))
        print(f'evidence: {out}')
    finally:
        if pid and alive(pid):
            j.keys('ctrl', 'q')
            deadline = time.monotonic() + 15
            while alive(pid) and time.monotonic() < deadline:
                time.sleep(0.1)
            if alive(pid):
                os.kill(pid, 9)
        j.call('stop')


if __name__ == '__main__':
    sys.exit(main())
