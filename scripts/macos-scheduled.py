#!/usr/bin/env python3
"""Scheduled tasks journey on macOS, on the harness of scripts/macos-smoke.py.

Launches the real debug app with the scripted providers, adds a task in Settings →
Scheduled that runs every minute in a new Claude session, waits for it to fire, and
checks it fired within seconds of its saved next run as a session titled after the
task with the fake Claude's reply. Opens that session from the run's Open button.
Quits, waits until the next run has passed while Keystone is closed, relaunches and
checks the missed run ran once at launch, recorded as late. Then Run now starts a
third session and Pause clears the next run. Every click goes through the automation
inbox at the element's center.

Run from the workspace root: python3 scripts/macos-scheduled.py
Evidence goes to .audit/macos-scheduled-<time>/: numbered PNGs and report.json.
"""

import argparse
import importlib.util
import json
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('macos_smoke', ROOT / 'scripts' / 'macos-smoke.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

TITLE = 'Minute summary'
PROMPT = 'Summarize what changed today'
ON_TIME_MS = 3_000


class Run(smoke.Run):
    def launch(self):
        super().launch()
        # A key sent the moment the first frame lands can arrive before the window takes it.
        time.sleep(1)

    def tasks(self):
        rows = self.read("SELECT value FROM metadata WHERE key LIKE 'scheduled_task/%'")
        return [json.loads(row[0]) for row in rows]

    def finished_runs(self):
        """Runs past the record saved before they start, which says Keystone stopped."""
        return [run for run in self.tasks()[0]['runs']
                if run['outcome'].get('reason') != 'Keystone stopped during this run.']

    def replies(self):
        return sum(smoke.REPLY in row[0] for row in self.read('SELECT payload FROM log_entries'))

    def wait(self, what, check, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if self.app.poll() is not None:
                raise smoke.Failure(f'the app exited with {self.app.returncode} waiting for {what}')
            if check():
                return
            time.sleep(0.2)
        raise smoke.Failure(f'{what} did not happen in {seconds} s')

    def open_scheduled(self):
        self.key('secondary-,')
        self.wait_for('settings-section-scheduled')
        self.click('settings-section-scheduled')
        self.wait_for('scheduled-form')

    def journey(self):
        self.launch()
        self.step('launch shows the welcome screen', True)

        self.open_scheduled()
        self.step('Cmd+, then a click on Scheduled opens the section', True)
        self.click('scheduled-title')
        self.type(TITLE)
        self.click('scheduled-prompt')
        self.type(PROMPT)
        self.click('scheduled-when')
        self.type('1')
        self.shot('form-filled')
        self.click('scheduled-add')
        self.wait('the task to be saved', lambda: len(self.tasks()) == 1, 10)
        task = self.tasks()[0]
        self.step('Add saves the task', task['title'] == TITLE and task['schedule'] == {'kind': 'every', 'minutes': 1},
                  json.dumps(task['schedule']))
        task_id = task['id']
        self.wait_for(f'scheduled-row-{task_id}')
        self.shot('task-added')

        due = task['next_run']
        self.step('the first run is due a minute after adding', due is not None)
        self.wait('the first run', lambda: self.finished_runs(), 90)
        run = self.finished_runs()[0]
        late = run['at'] - due
        self.step('the task fires on time', 0 <= late <= ON_TIME_MS, f'{late} ms after its next run')
        self.step('its run started a session', run['outcome']['kind'] == 'started', json.dumps(run['outcome']))
        session = run['outcome']['session']
        sessions = {s['id']: s for s in self.sessions()}
        self.step('the session is titled after the task', sessions.get(session, {}).get('title') == TITLE,
                  repr(sessions.get(session, {}).get('title')))
        self.wait('the fake Claude to reply', lambda: self.replies() >= 1, 30)
        self.step('the session got the reply', True)
        self.wait_for(f'scheduled-open-{task_id}-0')
        self.shot('first-run-in-history')

        self.click(f'scheduled-open-{task_id}-0')
        self.wait_for('settings-page', present=False)
        self.wait_for('composer-field')
        self.step('Open shows the run\'s session', True)
        self.shot('run-session')

        next_run = self.tasks()[0]['next_run']
        self.quit()
        wait = max(0, next_run / 1000 - time.time()) + 3
        print(f'waiting {wait:.0f} s with Keystone closed', flush=True)
        time.sleep(wait)
        before = len(self.sessions())
        self.launch()
        self.wait('the missed run to catch up', lambda: len(self.finished_runs()) == 2, 30)
        caught = self.finished_runs()[1]
        self.step('the missed run ran once at launch', len(self.sessions()) == before + 1,
                  f'{len(self.sessions())} sessions')
        self.step('it is recorded as late', caught['at'] - caught['due'] >= 3_000,
                  f'due {caught["due"]}, ran {caught["at"]}')
        self.step('and started', caught['outcome']['kind'] == 'started', json.dumps(caught['outcome']))

        self.open_scheduled()
        self.wait_for(f'scheduled-history-{task_id}-0')
        self.shot('caught-up-run')
        scheduled_next = self.tasks()[0]['next_run']
        self.click(f'scheduled-run-{task_id}')
        self.wait('Run now', lambda: len(self.finished_runs()) == 3, 15)
        now_run = self.finished_runs()[2]
        self.step('Run now starts another session', now_run['due'] is None and now_run['outcome']['kind'] == 'started',
                  json.dumps(now_run))
        self.step('Run now leaves the next run', self.tasks()[0]['next_run'] == scheduled_next)
        self.click(f'scheduled-pause-{task_id}')
        self.wait('Pause', lambda: self.tasks()[0]['next_run'] is None, 10)
        self.step('Pause clears the next run', True)
        self.wait('three replies', lambda: self.replies() >= 3, 30)
        self.shot('run-now-and-paused')
        self.quit()


def main():
    if sys.platform != 'darwin':
        sys.exit('macos-scheduled runs on macOS.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    evidence = ROOT / '.audit' / time.strftime('macos-scheduled-%Y%m%d-%H%M%S')
    evidence.mkdir(parents=True)
    run = Run(ROOT / 'target/debug/keystone-desktop', evidence)
    error = None
    try:
        run.journey()
    except smoke.Failure as failure:
        error = str(failure)
    finally:
        run.finish(error)
    print(f'evidence: {evidence}')
    if error:
        sys.exit(f'macos-scheduled failed: {error}')
    print('macos-scheduled passed')


if __name__ == '__main__':
    main()
