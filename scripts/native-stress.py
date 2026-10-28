#!/usr/bin/env python3
"""Repeatable native typing stress on the private headless desktop (KS-005, KS-009).

Each run starts its own nested Sway through native-driver.py, creates a Claude
session with Ctrl+N through the driver, and types a long draft with `wtype` at a
fixed per-character delay against that desktop's Wayland socket. It then quits with
`wtype` Ctrl+Q, so the temporary virtual keyboard is the last one the
compositor saw, and relaunches. That is the ordering that crashed GPUI in
KS-009. `--no-prime` skips the driver's pre-launch keyboard event to show the
crash. Every quit must end with exit code 0, and the draft must be saved
within --max-persist-lag of the last key. Evidence goes to .audit/native-stress-*/run-NN/ and a summary.json.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location('native_driver', ROOT / 'scripts/native-driver.py')
driver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(driver)

SENTENCE = ' A long draft should keep its caret visible.'
DRAFT = 'Native stress draft survives restart.'


def alive(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().split(') ')[1][0] not in 'ZX'
    except FileNotFoundError:
        return False


def wait(description, check, timeout):
    deadline = time.monotonic() + timeout
    while True:
        value = check()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise TimeoutError(f'Timed out after {timeout} s waiting for {description}')
        time.sleep(0.1)


class Stress:
    def __init__(self, args, evidence):
        self.args = args
        self.run = driver.Run(args.driver, ROOT, evidence, args.binary)
        self.run.prime_keyboard = not args.no_prime
        self.socket = None
        self.checks = []

    def wtype(self, *arguments, timeout=30):
        env = dict(os.environ, WAYLAND_DISPLAY=self.socket)
        subprocess.run(['wtype', *arguments], env=env, check=True, timeout=timeout,
                       capture_output=True, text=True)

    def grim(self, name):
        path = self.run.evidence / f'grim-{name}.png'
        env = dict(os.environ, WAYLAND_DISPLAY=self.socket)
        subprocess.run(['grim', str(path)], env=env, check=True, timeout=20, capture_output=True)
        return str(path)

    def app(self):
        return self.run.app_pids[-1]

    def draft(self):
        snapshot = driver.read_workspace(self.run.state)
        if snapshot.get('state_absent'):
            return None
        workspace = snapshot['workspace']
        sessions = workspace['sessions']
        return sessions[0]['draft'] if sessions else None

    def window(self):
        windows = self.run.call('windows_list').get('structuredContent', {})
        return any(w.get('pid') == self.app() for w in windows.get('windows', []))

    def check(self, name):
        self.checks.append(name)

    def ended(self, pid):
        status = self.run.exit_status(pid)
        if status is None:
            return 'no exit status recorded'
        if status['signal']:
            return f'killed by {status["signal"]}'
        return f'exit code {status["returncode"]}'

    def quit_gracefully(self, label, pid):
        status = wait(label, lambda: self.run.exit_status(pid), 20)
        if status['returncode'] != 0:
            raise RuntimeError(f'{label}: app PID {pid} {self.ended(pid)}; '
                               f'see {self.run.app_logs[pid].name}')

    def launched(self, label, ready):
        pid = self.app()
        try:
            wait(f'{label} window', lambda: not alive(pid) or self.window(), 20)
            if not alive(pid):
                raise RuntimeError(f'{label}: app PID {pid} {self.ended(pid)} before its '
                                   f'window appeared; see {self.run.app_logs[pid].name}')
            self.run.wait_for(ready, True, 20000)
        except Exception:
            if not alive(pid):
                raise RuntimeError(f'{label}: app PID {pid} {self.ended(pid)}; '
                                   f'see {self.run.app_logs[pid].name}')
            raise
        self.check(f'{label}: window shown by PID {pid}')

    def execute(self, result):
        started = self.run.start(self.args.width, self.args.height, True)
        self.socket = started['session']['wayland_socket']
        result['wayland_socket'] = self.socket
        # The welcome screen shows provider pills and no composer field.
        self.launched('first launch', 'welcome-claude')
        self.run.call('input', {'actions': [{'action': 'keypress', 'keys': ['ctrl', 'n']}]})
        self.run.wait_for('composer-field', True, 20000)
        wait('Claude session', lambda: self.draft() is not None, 10)
        self.check('Claude session created by Ctrl+N')
        self.grim('01-created')

        text = DRAFT + SENTENCE * self.args.repeat
        result['characters'] = len(text)
        began = time.monotonic()
        typing = subprocess.Popen(['wtype', '-d', str(self.args.delay_ms), '--', text],
                                  env=dict(os.environ, WAYLAND_DISPLAY=self.socket),
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        # Timestamp wtype's exit as it happens; the polling below can lag by 250 ms.
        exited = []
        waiter = threading.Thread(target=lambda: (typing.wait(), exited.append(time.monotonic())),
                                  daemon=True)
        waiter.start()
        while typing.poll() is None:
            if not alive(self.app()):
                typing.kill()
                raise RuntimeError(f'App {self.ended(self.app())} while typing '
                                   f'({time.monotonic() - began:.1f} s in)')
            if time.monotonic() - began > len(text) * self.args.delay_ms / 1000 * 4 + 60:
                typing.kill()
                raise TimeoutError('wtype did not finish')
            time.sleep(0.25)
        if typing.returncode:
            raise RuntimeError(f'wtype failed: {typing.stdout.read()}')
        waiter.join()
        typed = exited[0]
        result['typing_s'] = round(typed - began, 2)
        wait('typed draft persisted', lambda: not alive(self.app()) or self.draft() == text,
             self.args.settle_timeout)
        if not alive(self.app()):
            raise RuntimeError(f'App {self.ended(self.app())} after typing')
        # The app saves 250 ms after the last edit, so a key backlog shows here.
        elapsed = time.monotonic() - typed
        lag = result['persist_lag_s'] = round(elapsed, 2)
        if elapsed > self.args.max_persist_lag:
            raise RuntimeError(f'KS-005: draft saved {lag} s after the last key, over the '
                               f'{self.args.max_persist_lag} s limit')
        self.check(f'{len(text)} characters persisted exactly, {lag} s after the last key')
        self.grim('02-typed')

        # KS-009: quit through the temporary virtual keyboard, then relaunch.
        pid = self.app()
        self.wtype('-M', 'ctrl', '-k', 'q', '-m', 'ctrl')
        self.quit_gracefully('quit through wtype', pid)
        wait('empty window list', lambda: not self.window(), 10)
        self.check('Ctrl+Q through wtype quit the app with exit code 0')
        self.run.launch()
        self.launched('relaunch after wtype', 'composer-field')
        time.sleep(1)
        if not alive(self.app()):
            raise RuntimeError(f'Relaunched app {self.ended(self.app())} within a second '
                               'of its window')
        if self.draft() != text:
            raise RuntimeError('Draft changed across restart')
        self.check('Relaunch after wtype kept running with the draft')
        self.grim('03-restored')

        pid = self.app()
        self.run.call('input', {'actions': [{'action': 'keypress', 'keys': ['ctrl', 'q']}]})
        self.quit_gracefully('quit through the driver', pid)
        self.check('Ctrl+Q through the driver quit the app with exit code 0')

    def __call__(self):
        result = {'status': 'failed', 'delay_ms': self.args.delay_ms, 'prime': not self.args.no_prime,
                  'evidence': str(self.run.evidence)}
        try:
            self.execute(result)
            result['status'] = 'passed'
        except Exception as error:
            result['error'] = f'{type(error).__name__}: {error}'
            result['app_alive'] = bool(self.run.app_pids) and alive(self.app())
            try:
                if self.socket:
                    self.grim('failure')
            except Exception:
                pass
        finally:
            result['checks'] = self.checks
            # Read before stop, which tears down whatever is still running.
            result['exits'] = [{'pid': pid, **(self.run.exit_status(pid) or {'running': True})}
                               for pid in self.run.app_pids]
            try:
                result['cleanup'] = self.run.stop()
            except Exception as error:
                result['status'] = 'failed'
                result['cleanup_error'] = str(error)
        (self.run.evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runs', type=int, default=1)
    parser.add_argument('--delay-ms', type=int, default=10, help='wtype delay per character')
    parser.add_argument('--repeat', type=int, default=24,
                        help='Sentences appended to the draft; 6 matches native-smoke.py')
    parser.add_argument('--settle-timeout', type=float, default=60,
                        help='Seconds to wait for the typed draft to persist after wtype exits, '
                             'so a slow run still records its lag')
    # The app saves 250 ms after the last edit and headless runs saved within
    # 0.3 s. One second still fails a backlog of about 65 keys at 10 ms each.
    parser.add_argument('--max-persist-lag', type=float, default=1.0,
                        help='Seconds after wtype exits by which the draft must be saved (KS-005)')
    parser.add_argument('--width', type=int, default=1280)
    parser.add_argument('--height', type=int, default=900)
    parser.add_argument('--no-prime', action='store_true',
                        help='Skip the pre-launch driver key event; reproduces KS-009')
    parser.add_argument('--driver', default=str(driver.default_driver()))
    driver.add_binary_argument(parser)
    args = parser.parse_args()

    directory = ROOT / '.audit' / f'native-stress-{time.strftime("%Y%m%dT%H%M%S")}-{os.getpid()}'
    directory.mkdir(parents=True)
    results = []
    for index in range(1, args.runs + 1):
        result = Stress(args, directory / f'run-{index:02d}')()
        results.append(result)
        print(json.dumps({'run': index, **{k: result.get(k) for k in (
            'status', 'typing_s', 'persist_lag_s', 'error')}}), flush=True)
    passed = sum(r['status'] == 'passed' for r in results)
    summary = {'runs': len(results), 'passed': passed, 'delay_ms': args.delay_ms,
               'repeat': args.repeat, 'prime': not args.no_prime, 'results': results}
    (directory / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({'summary': str(directory / 'summary.json'), 'runs': len(results),
                      'passed': passed}))
    return 0 if passed == len(results) else 1


if __name__ == '__main__':
    sys.exit(main())
