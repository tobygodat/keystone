#!/usr/bin/env python3
"""Repeatable native typing stress on the private headless desktop (KS-005, KS-009).

Each run starts its own nested Sway through native-driver.py, creates a Claude
session with a named click, and types a long draft with `wtype` at a fixed
per-character delay against that desktop's Wayland socket. It then quits with
`wtype` Ctrl+Q, so the temporary virtual keyboard is the last one the
compositor saw, and relaunches. That is the ordering that crashed GPUI in
KS-009. `--no-prime` skips the driver's pre-launch keyboard event to show the
crash. Evidence goes to .audit/native-stress-*/run-NN/ and a summary.json.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
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
        self.run = driver.Run(args.driver, ROOT, evidence)
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
        try:
            workspace = json.loads(self.run.state.read_text())['workspace']
        except (FileNotFoundError, json.JSONDecodeError):
            return None
        sessions = workspace['sessions']
        return sessions[0]['draft'] if sessions else None

    def window(self):
        windows = self.run.call('windows_list').get('structuredContent', {})
        return any(w.get('pid') == self.app() for w in windows.get('windows', []))

    def check(self, name):
        self.checks.append(name)

    def launched(self, label):
        pid = self.app()
        try:
            wait(f'{label} window', lambda: not alive(pid) or self.window(), 20)
            if not alive(pid):
                raise RuntimeError(f'{label}: app PID {pid} exited before its window appeared')
            self.run.wait_for('composer-field', True, 20000)
        except Exception:
            if not alive(pid):
                raise RuntimeError(f'{label}: app PID {pid} exited; see driver.stderr.log')
            raise
        self.check(f'{label}: window shown by PID {pid}')

    def execute(self, result):
        started = self.run.start(self.args.width, self.args.height, True)
        self.socket = started['session']['wayland_socket']
        result['wayland_socket'] = self.socket
        self.launched('first launch')
        self.run.wait_for('welcome-claude', True, 20000)
        target = self.run.find('welcome-claude')['center']
        self.run.call('input', {'actions': [{'action': 'click', 'button': 'left', **target}]})
        wait('Claude session', lambda: self.draft() is not None, 10)
        self.check('Claude session created by named click')
        self.grim('01-created')

        text = DRAFT + SENTENCE * self.args.repeat
        result['characters'] = len(text)
        began = time.monotonic()
        typing = subprocess.Popen(['wtype', '-d', str(self.args.delay_ms), '--', text],
                                  env=dict(os.environ, WAYLAND_DISPLAY=self.socket),
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        while typing.poll() is None:
            if not alive(self.app()):
                typing.kill()
                raise RuntimeError(f'App exited while typing ({time.monotonic() - began:.1f} s in)')
            if time.monotonic() - began > len(text) * self.args.delay_ms / 1000 * 4 + 60:
                typing.kill()
                raise TimeoutError('wtype did not finish')
            time.sleep(0.25)
        if typing.returncode:
            raise RuntimeError(f'wtype failed: {typing.stdout.read()}')
        typed = time.monotonic()
        result['typing_s'] = round(typed - began, 2)
        wait('typed draft persisted', lambda: not alive(self.app()) or self.draft() == text,
             self.args.settle_timeout)
        if not alive(self.app()):
            raise RuntimeError('App exited after typing')
        # The app saves 250 ms after the last edit, so a key backlog shows here.
        result['persist_lag_s'] = round(time.monotonic() - typed, 2)
        self.check(f'{len(text)} characters persisted exactly')
        self.grim('02-typed')

        # KS-009: quit through the temporary virtual keyboard, then relaunch.
        pid = self.app()
        self.wtype('-M', 'ctrl', '-k', 'q', '-m', 'ctrl')
        wait('graceful quit', lambda: not alive(pid), 20)
        wait('empty window list', lambda: not self.window(), 10)
        self.check('Ctrl+Q through wtype quit the app')
        self.run.launch()
        self.launched('relaunch after wtype')
        time.sleep(1)
        if not alive(self.app()):
            raise RuntimeError('Relaunched app exited within a second of its window')
        if self.draft() != text:
            raise RuntimeError('Draft changed across restart')
        self.check('Relaunch after wtype kept running with the draft')
        self.grim('03-restored')

        pid = self.app()
        self.run.call('input', {'actions': [{'action': 'keypress', 'keys': ['ctrl', 'q']}]})
        wait('second graceful quit', lambda: not alive(pid), 20)
        self.check('Ctrl+Q through the driver quit the app')

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
                        help='Seconds the typed draft may take to persist after wtype exits')
    parser.add_argument('--width', type=int, default=1280)
    parser.add_argument('--height', type=int, default=900)
    parser.add_argument('--no-prime', action='store_true',
                        help='Skip the pre-launch driver key event; reproduces KS-009')
    parser.add_argument('--driver', default=str(ROOT / '.audit/tools/computer-use-mcp'))
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
