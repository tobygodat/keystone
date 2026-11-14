#!/usr/bin/env python3
"""Floods a terminal pane on the private headless desktop and measures the app (TOB-331).

It starts its own nested Sway through native-driver.py, creates a Claude session with
Ctrl+N and opens its terminal with Ctrl+J. Then:

1. Throughput: `cat` of a large file in the pane, timed by the shell.
2. Flood: `yes` for --seconds while the composer is focused (F6, then Ctrl+L) and a
   draft is typed into it with `wtype`. The draft must be saved; the time from the
   last key to the save is the input lag (the app saves 250 ms after the last edit).
   Ctrl+N then switches to a new session and a click on the first one's sidebar row
   back, each timed until the saved workspace shows it.
3. The app's resident memory and CPU are sampled every half second throughout.

It needs no element names, so it runs a release build too: pass --binary. Evidence
and result.json go to .audit/terminal-flood-*/.
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

DRAFT = 'Typed while the terminal floods.' + ' The composer keeps up.' * 8
TICKS = os.sysconf('SC_CLK_TCK')


def alive(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().split(') ')[1][0] not in 'ZX'
    except FileNotFoundError:
        return False


def wait(description, check, timeout, step=0.05):
    deadline = time.monotonic() + timeout
    while True:
        value = check()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise TimeoutError(f'Timed out after {timeout} s waiting for {description}')
        time.sleep(step)


def rss_mb(pid):
    for line in Path(f'/proc/{pid}/status').read_text().splitlines():
        if line.startswith('VmRSS:'):
            return int(line.split()[1]) / 1024
    return 0.0


def cpu_s(pid):
    fields = Path(f'/proc/{pid}/stat').read_text().split(') ')[1].split()
    return (int(fields[11]) + int(fields[12])) / TICKS


class Sampler(threading.Thread):
    """Samples the app's RSS and CPU time until stopped."""

    def __init__(self, pid):
        super().__init__(daemon=True)
        self.pid = pid
        self.samples = []
        self.done = threading.Event()

    def run(self):
        while not self.done.is_set() and alive(self.pid):
            self.samples.append((time.monotonic(), rss_mb(self.pid), cpu_s(self.pid)))
            self.done.wait(0.5)

    def between(self, start, end):
        return [s for s in self.samples if start <= s[0] <= end]


class Flood:
    def __init__(self, args, evidence):
        self.args = args
        self.evidence = evidence
        self.run = driver.Run(args.driver, ROOT, evidence / 'run', args.binary)
        self.socket = None
        self.checks = []

    def keys(self, *keys):
        self.run.call('input', {'actions': [{'action': 'keypress', 'keys': list(keys)}]})

    def wtype(self, text, delay_ms=0):
        env = dict(os.environ, WAYLAND_DISPLAY=self.socket)
        delay = ['-d', str(delay_ms)] if delay_ms else []
        subprocess.run(['wtype', *delay, '--', text], env=env, check=True, timeout=120,
                       capture_output=True, text=True)

    def command(self, line):
        self.wtype(line)
        self.keys('Return')

    def grim(self, name):
        path = self.run.evidence / f'grim-{name}.png'
        env = dict(os.environ, WAYLAND_DISPLAY=self.socket)
        subprocess.run(['grim', str(path)], env=env, check=True, timeout=20, capture_output=True)
        return str(path)

    def app(self):
        return self.run.app_pids[-1]

    def workspace(self):
        snapshot = driver.read_workspace(self.run.state)
        return None if snapshot.get('state_absent') else snapshot['workspace']

    def sessions(self):
        workspace = self.workspace()
        return workspace['sessions'] if workspace else []

    def selected(self):
        workspace = self.workspace() or {}
        return workspace.get('selected')

    def window(self):
        windows = self.run.call('windows_list').get('structuredContent', {})
        return any(w.get('pid') == self.app() for w in windows.get('windows', []))

    def execute(self, result):
        scratch = self.evidence / 'scratch'
        scratch.mkdir(parents=True)
        big = scratch / 'big.txt'
        line = b'the quick brown fox jumps over the lazy dog 0123456789 ABCDEFGHIJ\n'
        with big.open('wb') as file:
            file.write(line * (self.args.cat_mb * 1024 * 1024 // len(line)))
        (scratch / 'cat.sh').write_text(
            's=$(date +%s%N); cat "$1"; e=$(date +%s%N)\n'
            'echo $(( (e - s) / 1000000 )) > "$2"\n')
        (scratch / 'flood.sh').write_text(f'timeout {self.args.seconds} yes; echo done > "$1"\n')

        started = self.run.start(self.args.width, self.args.height, True)
        self.socket = started['session']['wayland_socket']
        pid = self.app()
        wait('the window', lambda: not alive(pid) or self.window(), 30)
        time.sleep(3)
        sampler = Sampler(pid)
        sampler.start()
        result['rss_idle_mb'] = round(rss_mb(pid), 1)

        self.keys('ctrl', 'n')
        wait('a Claude session', lambda: len(self.sessions()) == 1, 15)
        first = self.selected()
        self.keys('ctrl', 'j')
        time.sleep(1.5)
        self.command('echo ready')
        time.sleep(0.5)
        self.grim('01-terminal')
        result['rss_terminal_mb'] = round(rss_mb(pid), 1)

        took = scratch / 'cat-ms.txt'
        self.command(f'sh {scratch / "cat.sh"} {big} {took}')
        wait('cat to finish', lambda: took.exists() and took.read_text().strip(), 600, 0.2)
        ms = int(took.read_text())
        result['cat_mb'] = self.args.cat_mb
        result['cat_ms'] = ms
        result['cat_mb_per_s'] = round(self.args.cat_mb / (ms / 1000), 1)
        self.checks.append(f'cat of {self.args.cat_mb} MB took {ms} ms')
        self.grim('02-after-cat')

        done = scratch / 'flood-done.txt'
        self.command(f'sh {scratch / "flood.sh"} {done}')
        time.sleep(2)
        flood_began = time.monotonic()
        # Out of the pane: F6 to the transcript, then the composer's shortcut.
        self.keys('F6')
        time.sleep(0.3)
        self.keys('ctrl', 'l')
        time.sleep(0.5)
        typed_began = time.monotonic()
        self.wtype(DRAFT, self.args.delay_ms)
        typed = time.monotonic()
        result['typing_s'] = round(typed - typed_began, 2)
        saved = lambda: (self.sessions() or [{}])[0].get('draft') == DRAFT
        wait('the draft to save', saved, 60)
        result['draft_lag_s'] = round(time.monotonic() - typed, 2)
        self.checks.append(f'draft of {len(DRAFT)} characters saved '
                           f'{result["draft_lag_s"]} s after the last key, during yes')
        self.grim('03-typed-during-flood')

        began = time.monotonic()
        self.keys('ctrl', 'n')
        wait('a second session', lambda: len(self.sessions()) == 2 and self.selected() != first,
             30)
        result['new_session_s'] = round(time.monotonic() - began, 2)
        began = time.monotonic()
        # The first session's sidebar row; release builds name no elements.
        self.run.call('input', {'actions': [
            {'action': 'click', 'x': 130, 'y': 205, 'button': 'left', 'count': 1}]})
        wait('back to the first session', lambda: self.selected() == first, 30)
        result['switch_back_s'] = round(time.monotonic() - began, 2)
        time.sleep(0.5)
        self.grim('04-switched-back-during-flood')

        wait('yes to end', lambda: done.exists(), self.args.seconds + 120, 0.2)
        flood_ended = time.monotonic()
        time.sleep(1)
        self.grim('05-after-flood')
        samples = sampler.between(flood_began, flood_ended)
        sampler.done.set()
        sampler.join()
        if len(samples) >= 2:
            span = samples[-1][0] - samples[0][0]
            result['flood_s'] = round(span, 1)
            result['flood_cpu_percent'] = round(100 * (samples[-1][2] - samples[0][2]) / span, 1)
            result['flood_rss_mb'] = {'start': round(samples[0][1], 1),
                                      'max': round(max(s[1] for s in samples), 1),
                                      'end': round(samples[-1][1], 1)}
        result['rss_after_mb'] = round(rss_mb(pid), 1)

        self.keys('ctrl', 'q')
        status = wait('quit', lambda: self.run.exit_status(pid), 30)
        result['quit_exit_code'] = status['returncode']
        if status['returncode'] != 0:
            raise RuntimeError(f'quit ended with {status}')
        self.checks.append('Ctrl+Q quit with exit code 0')

    def __call__(self):
        result = {'status': 'failed', 'binary': self.args.binary,
                  'evidence': str(self.run.evidence)}
        try:
            self.execute(result)
            result['status'] = 'passed'
        except Exception as error:
            result['error'] = f'{type(error).__name__}: {error}'
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
            for path in (self.evidence / 'scratch').glob('big.txt'):
                path.unlink()
        (self.run.evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seconds', type=int, default=30, help='How long yes runs')
    parser.add_argument('--cat-mb', type=int, default=100, help='Size of the file cat prints')
    parser.add_argument('--delay-ms', type=int, default=10, help='wtype delay per character')
    parser.add_argument('--width', type=int, default=1280)
    parser.add_argument('--height', type=int, default=900)
    parser.add_argument('--driver', default=str(driver.default_driver()))
    driver.add_binary_argument(parser)
    args = parser.parse_args()
    directory = ROOT / '.audit' / f'terminal-flood-{time.strftime("%Y%m%dT%H%M%S")}-{os.getpid()}'
    directory.mkdir(parents=True)
    result = Flood(args, directory)()
    print(json.dumps({k: v for k, v in result.items() if k != 'cleanup'}, indent=2))
    return 0 if result['status'] == 'passed' else 1


if __name__ == '__main__':
    sys.exit(main())
