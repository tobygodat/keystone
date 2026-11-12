#!/usr/bin/env python3
"""Terminal journey for Keystone on Linux (TOB-301).

Drives the real debug app on the verify skill's private headless Sway, with real
Wayland keyboard and pointer input, the scripted providers, and the user's login
shell. The journey: Ctrl+N makes a Claude session; the terminal opens by Ctrl+J, by
the palette and by the top bar button; the shell starts in the session's folder;
`seq` output and a scrolled-back screen; `vi` and `less` take keys and are left;
Ctrl+Shift+D splits, Ctrl+Alt+Left and Right move between panes, Ctrl+Alt+W closes
one along with its shell; a drag selects, Ctrl+Shift+C copies to the Wayland
clipboard and Ctrl+Shift+V pastes it; a second window opens and closes and a new one
shows the same shell and its scrollback; the drawer at the default 1100x720 and at
860x560, in light and dark; the Browser beside the open drawer at both sizes, with
Send whole above it; a draft, Ctrl+Q from inside a pane, and no shell, job or Chrome process survives,
including jobs that ignore SIGHUP; a relaunch brings back the session and its draft.

The harness reads state from files the shell writes, from /proc and from the
workspace database. It cannot read text off the screen, so look at the screenshots.

Run from the workspace root after `cargo build --workspace --locked`:
    python3 scripts/terminal-journey-linux.py [--driver PATH]
Evidence goes to the verify run's .audit/verify-*/ folder: numbered PNGs, actions.jsonl,
and journey.json with every step.
"""

import argparse
import importlib.util
import json
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parent.parent
DRAFT = 'Draft kept across the terminal journey'
DEFAULT = (1100, 720)
SMALL = (860, 560)


def load_driver():
    spec = importlib.util.spec_from_file_location('native_driver', ROOT / 'scripts/native-driver.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Failure(Exception):
    pass


def descendants(root):
    children = {}
    for entry in Path('/proc').iterdir():
        if entry.name.isdigit():
            try:
                fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
            except (FileNotFoundError, ProcessLookupError):
                continue
            children.setdefault(int(fields[1]), []).append(int(entry.name))
    found, queue = [], [root]
    while queue:
        for child in children.get(queue.pop(), []):
            found.append(child)
            queue.append(child)
    return found


def command_line(pid):
    try:
        return Path(f'/proc/{pid}/cmdline').read_bytes().replace(b'\0', b' ').decode(errors='replace')
    except (FileNotFoundError, ProcessLookupError):
        return ''


def alive(pid):
    try:
        state = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0]
    except (FileNotFoundError, ProcessLookupError):
        return False
    return state not in ('Z', 'X')


class Journey:
    def __init__(self, driver, binary):
        self.native = load_driver()
        self.server = self.native.Server(SimpleNamespace(driver=driver, binary=binary,
                                                         trace_wayland=False))
        self.steps = []
        self.shots = []
        self.scratch = ROOT / '.audit' / f'terminal-journey-linux-{time.strftime("%Y%m%dT%H%M%S")}'
        self.project = self.scratch / 'project'
        self.receipts = self.scratch / 'receipts'
        for folder in (self.project, self.receipts, self.scratch / 'claude', self.scratch / 'codex'):
            folder.mkdir(parents=True)
        subprocess.run(['git', 'init', '-q', str(self.project)], check=True)
        (self.project / 'README.md').write_text(''.join(f'line {n}\n' for n in range(1, 201)))
        self.evidence = None
        self.sway = None

    def call(self, name, **arguments):
        value, _ = self.server.handle(name, arguments)
        return value

    def step(self, name, ok, detail=''):
        self.steps.append({'step': name, 'ok': bool(ok), 'detail': detail})
        print(('ok   ' if ok else 'FAIL ') + name + (f'  ({detail})' if detail else ''), flush=True)
        if not ok:
            raise Failure(name)

    def wait(self, what, check, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            try:
                if check():
                    return
            except self.native.NotReady:
                pass
            time.sleep(0.1)
        raise Failure(f'timed out waiting for {what}')

    def wait_for(self, name, present=True, seconds=15, window=None):
        arguments = {'id': name, 'present': present, 'timeout_ms': int(seconds * 1000)}
        if window:
            arguments['window'] = window
        return self.call('wait_for', **arguments)

    def elements(self, window=None):
        return self.call('elements', **({'window': window} if window else {}))['elements']

    def element(self, name, window=None):
        found = [e for e in self.elements(window) if e['id'] == name and e['visible']]
        return found[0] if len(found) == 1 else None

    def windows(self):
        return sorted({e.get('window') for e in self.elements()} - {None})

    def keys(self, *keys):
        self.call('input', actions=[{'action': 'keypress', 'keys': list(keys)}])

    def type(self, text):
        self.call('input', actions=[{'action': 'type', 'text': text}])

    def click(self, name, window=None):
        self.call('click', id=name, **({'window': window} if window else {}))

    def shell(self, line):
        self.type(line)
        self.keys('Return')

    def receipt(self, name, seconds=20):
        path = self.receipts / name
        self.wait(f'receipt {name}', lambda: path.exists() and path.read_text().endswith('\n'), seconds)
        return path.read_text().strip()

    def shot(self, label):
        value = self.call('screenshot')
        target = self.scratch / f'{len(self.shots) + 1:02d}-{label}.png'
        shutil.copyfile(value['path'], target)
        self.shots.append(str(target))
        self.step(f'screenshot {target.name}', target.stat().st_size > 0,
                  f'{value["width"]}x{value["height"]}')

    def resize(self, size):
        width, height = size
        subprocess.run(['swaymsg', '-s', self.sway, '--', 'output', '*', 'mode', '--custom',
                        f'{width}x{height}'], check=True, capture_output=True)
        self.wait(f'a {width}x{height} window', lambda: self.window_size() == (width, height))
        time.sleep(0.5)

    def window_size(self):
        tree = json.loads(subprocess.run(['swaymsg', '-s', self.sway, '-t', 'get_tree'],
                                         check=True, capture_output=True, text=True).stdout)
        found = []

        def walk(node):
            if node.get('pid') and node.get('type') == 'con':
                found.append((node['rect']['width'], node['rect']['height']))
            for child in node.get('nodes', []) + node.get('floating_nodes', []):
                walk(child)
        walk(tree)
        return found[0] if len(found) == 1 else None

    def stubborn_job(self, name, background=False):
        command = f"sh -c 'trap \"\" HUP; echo $$ > {self.receipts}/{name}; exec sleep 100000'"
        self.shell(command + (' &' if background else ''))
        pid = int(self.receipt(name))
        self.step(f'a job that ignores SIGHUP runs ({name})', alive(pid), f'pid {pid}')
        return pid

    def theme(self, option):
        self.keys('ctrl', 'comma')
        self.wait_for(f'settings-theme-option-{option}')
        self.click(f'settings-theme-option-{option}')
        time.sleep(0.4)
        self.keys('ctrl', 'comma')
        self.wait_for('terminal-drawer')

    def browser_with_terminal(self):
        (self.project / 'index.html').write_text(
            '<!doctype html><title>Journey page</title><h1>Browser beside the terminal</h1>'
            '<p>' + 'A paragraph of the project page. ' * 20 + '</p>')
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', 0))
            port = probe.getsockname()[1]
        server = subprocess.Popen([sys.executable, '-m', 'http.server', '--bind', '127.0.0.1', str(port),
                                   '--directory', str(self.project)], stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL)
        try:
            self.wait('the page server', lambda: socket.socket().connect_ex(('127.0.0.1', port)) == 0, 10)
            self.resize(DEFAULT)
            self.click('composer-field')
            self.keys('ctrl', 'alt', 'b')
            self.wait_for('browser-address')
            self.step('Ctrl+Alt+B opens the Browser beside the open terminal', True)
            self.click('browser-address')
            self.type(f'http://127.0.0.1:{port}/')
            self.keys('Return')
            self.wait_for('browser-page', seconds=30)
            app = self.app_pid()
            self.wait('Chrome to start', lambda: any('chrom' in command_line(p) for p in descendants(app)), 30)
            time.sleep(2)
            chrome = [p for p in descendants(app) if 'chrom' in command_line(p)]
            self.step('the page loads in headless Chrome', chrome, f'{len(chrome)} Chrome processes')
            for size in (DEFAULT, SMALL):
                self.resize(size)
                self.wait_for('terminal-drawer')
                time.sleep(0.8)
                tabs = sorted(e['id'] for e in self.elements() if e['id'].startswith('fitted-tab-'))
                if tabs and not self.element('browser-address'):
                    # A narrow window shows each panel as a tab; the session's comes first.
                    self.click(tabs[-1])
                    self.step(f'at {size[0]}x{size[1]} the Browser is a tab beside the session', True,
                              json.dumps(tabs))
                self.wait_for('browser-page')
                time.sleep(0.8)
                self.shot(f'browser-and-terminal-{size[0]}x{size[1]}')
                drawer, send, page = (self.element(n) for n in ('terminal-drawer', 'send', 'browser-page'))
                bounds = {n: e and e['bounds'] for n, e in
                          (('drawer', drawer), ('send', send), ('page', page))}
                fits = (drawer and send and page
                        and send['visible'] == send['bounds']
                        and send['bounds']['x'] + send['bounds']['w'] <= size[0]
                        and send['bounds']['y'] + send['bounds']['h'] <= drawer['bounds']['y']
                        and page['bounds']['y'] + page['bounds']['h'] <= drawer['bounds']['y'] + 1
                        and page['bounds']['h'] > 40)
                self.step(f'Browser and terminal at {size[0]}x{size[1]}: Send is whole and above the '
                          'drawer, the page sits above it', fits, json.dumps(bounds))
            self.resize(DEFAULT)
            return chrome
        finally:
            server.terminate()
            server.wait(5)

    def app_pid(self):
        return self.call('doctor')['processes'][-1]['pid']

    def quit(self, name='Ctrl+Q quits gracefully'):
        pid = self.app_pid()
        self.keys('ctrl', 'q')
        self.wait('the app to exit', lambda: not alive(pid), 30)

        def exit_record():
            return next((p.get('exit') for p in self.call('doctor')['processes']
                         if p['pid'] == pid), None)
        self.wait('the exit record', exit_record, 10)
        code = exit_record()
        self.step(name, code.get('returncode') == 0, json.dumps(code))

    def run(self):
        env = {
            'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
            'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
            'KEYSTONE_PROJECT_DIR': str(self.project),
            'CLAUDE_CONFIG_DIR': str(self.scratch / 'claude'),
            'CODEX_HOME': str(self.scratch / 'codex'),
        }
        started = self.call('start', width=DEFAULT[0], height=DEFAULT[1], env=env)
        self.evidence = started['evidence']
        self.sway = started['session'].get('sway_socket') or started['session'].get('swaysock')
        self.step('the private desktop has a Sway IPC socket', self.sway, json.dumps(started['session']))
        self.wait_for('welcome-claude', seconds=60)
        doctor = self.call('doctor')
        self.step('doctor: the launched build runs', doctor.get('binary_unchanged') is True)
        receipts = self.receipts

        self.keys('ctrl', 'n')
        self.wait('a Claude session', lambda: len(self.sessions()) == 1)
        self.step('Ctrl+N makes a Claude session', True)
        self.wait_for('send')

        self.keys('ctrl', 'j')
        self.wait_for('terminal-drawer')
        self.wait_for('terminal-pane-0')
        self.step('Ctrl+J opens the terminal', True)
        self.shell(f'pwd > {receipts}/pwd.txt; echo $$ > {receipts}/shell.pid')
        folder = self.receipt('pwd.txt')
        self.step('the shell starts in the session folder', Path(folder).resolve() == self.project.resolve(),
                  folder)
        pid = int(self.receipt('shell.pid'))
        self.step('the shell is the login shell, alive', alive(pid), f'pid {pid}')
        self.shell(f'seq 1 300; echo seq-done > {receipts}/seq.txt')
        self.receipt('seq.txt')
        self.shot('seq-output')
        self.call('wheel', id='terminal-pane-0', vertical=-6)
        time.sleep(0.4)
        self.shot('scrolled-back')
        self.type('x')
        self.keys('BackSpace')

        # A short name: ex-vi stops at "Hit return" when the status line wraps.
        self.shell(f'cd {receipts} && vi vi.txt')
        time.sleep(1.5)
        self.type('ihello from vi')
        self.shot('vi')
        self.keys('Escape')
        self.type(':wq')
        self.keys('Return')
        self.wait('vi to save', lambda: (receipts / 'vi.txt').exists())
        self.step('vi took the keys and saved', (receipts / 'vi.txt').read_text() == 'hello from vi\n',
                  repr((receipts / 'vi.txt').read_text()))
        self.shell('cd -')
        self.shell(f'less {self.project}/README.md')
        time.sleep(0.8)
        self.keys('space')
        time.sleep(0.4)
        self.shot('less')
        self.type('q')
        self.shell(f'echo after-less > {receipts}/after-less.txt')
        self.step('the shell takes input again after less', self.receipt('after-less.txt') == 'after-less')

        self.keys('ctrl', 'j')
        self.wait_for('terminal-drawer', present=False)
        self.step('Ctrl+J hides the focused terminal', alive(pid), f'shell {pid} still runs')
        self.keys('ctrl', 'k')
        time.sleep(0.4)
        self.type('Toggle terminal')
        time.sleep(0.3)
        self.shot('palette-toggle-terminal')
        self.keys('Return')
        self.wait_for('terminal-drawer')
        self.step('the palette opens it', True)
        self.keys('ctrl', 'j')
        self.wait_for('terminal-drawer', present=False)
        self.click('terminal-toggle')
        self.wait_for('terminal-drawer')
        self.step('the top bar button opens it', True)
        self.click('terminal-pane-0')
        self.shell(f'echo $$ > {receipts}/same.pid')
        self.step('it is the same shell', int(self.receipt('same.pid')) == pid)

        self.keys('ctrl', 'shift', 'd')
        self.wait_for('terminal-pane-1')
        self.step('Ctrl+Shift+D splits the terminal', True)
        self.shell(f'echo $$ > {receipts}/second.pid')
        second = int(self.receipt('second.pid'))
        self.step('the split runs its own shell', second != pid and alive(second), f'pid {second}')
        self.keys('ctrl', 'alt', 'Left')
        self.shell(f'echo $$ > {receipts}/left.pid')
        self.step('Ctrl+Alt+Left focuses the first pane', int(self.receipt('left.pid')) == pid)
        self.keys('ctrl', 'alt', 'Right')
        self.shell(f'echo $$ > {receipts}/right.pid')
        self.step('Ctrl+Alt+Right focuses the second pane', int(self.receipt('right.pid')) == second)
        split_job = self.stubborn_job('split-job.pid', background=True)
        self.shot('split')
        self.keys('ctrl', 'alt', 'w')
        self.wait_for('terminal-pane-1', present=False)
        self.wait('the closed pane\'s shell to end', lambda: not alive(second))
        self.step('Ctrl+Alt+W closes the pane and ends its shell', True, f'pid {second}')
        self.wait('its job to end', lambda: not alive(split_job))
        self.step('and its job that ignores SIGHUP', True, f'pid {split_job}')

        self.click('terminal-pane-0')
        self.shell('clear; printf "SEL%s\\n" ME-FROM-THE-SHELL')
        time.sleep(0.5)
        pane = self.element('terminal-pane-0')
        b = pane['bounds']
        start = {'x': b['x'] + 2, 'y': b['y'] + 22}
        end = {'x': b['x'] + min(b['w'] - 10, 420), 'y': b['y'] + 30}
        self.call('input', actions=[{'action': 'drag', 'path': [start, end], 'duration_ms': 500}])
        time.sleep(0.3)
        self.shot('selection')
        self.keys('ctrl', 'shift', 'c')
        time.sleep(0.3)
        self.shell(f'wl-paste --no-newline > {receipts}/clipboard.txt; echo >> {receipts}/clipboard.txt')
        copied = self.receipt('clipboard.txt')
        self.step('Ctrl+Shift+C puts the selection on the Wayland clipboard',
                  'SELME-FROM-THE-SHELL' in copied, repr(copied))
        self.shell(f'cat > {receipts}/pasted.txt')
        time.sleep(0.3)
        self.keys('ctrl', 'shift', 'v')
        self.keys('Return')
        self.keys('ctrl', 'd')
        self.wait('the paste', lambda: (receipts / 'pasted.txt').exists()
                  and 'SELME' in (receipts / 'pasted.txt').read_text())
        self.step('Ctrl+Shift+V pastes it into the shell', True,
                  repr((receipts / 'pasted.txt').read_text()))
        self.shell(f'seq 1 120 > /dev/stdout; echo scroll-mark > {receipts}/mark.txt')
        self.receipt('mark.txt')

        first = self.windows()
        self.keys('ctrl', 'alt', 'shift', 'n')
        self.wait('a second window', lambda: len(self.windows()) == 2)
        self.step('the session opens in a second window', True, json.dumps(self.windows()))
        self.keys('ctrl', 'shift', 'w')
        self.wait('the second window to close', lambda: self.windows() == first)
        self.wait_for('terminal-drawer')
        self.step('closing a window keeps the shell', alive(pid), f'pid {pid}')
        self.keys('ctrl', 'alt', 'shift', 'n')
        self.wait('a new window', lambda: len(self.windows()) == 2)
        new = [w for w in self.windows() if w not in first][0]
        self.wait_for('terminal-drawer', window=new)
        self.shot('reopened-window')
        self.step('a new window shows the terminal with its output', True, new)
        self.click('terminal-pane-0', window=new)
        self.shell(f'echo $$ > {receipts}/reopened.pid')
        self.step('it is the same shell', int(self.receipt('reopened.pid')) == pid)
        self.keys('ctrl', 'shift', 'w')
        self.wait('back to one window', lambda: len(self.windows()) == 1)

        for theme, option in (('dark', 2), ('light', 1)):
            self.theme(option)
            for size in (DEFAULT, SMALL):
                self.resize(size)
                self.wait_for('terminal-drawer')
                drawer = self.element('terminal-drawer')
                send = self.element('send')
                self.step(f'{theme} {size[0]}x{size[1]}: the send button shows above the drawer',
                          send is not None and drawer is not None
                          and send['bounds']['y'] + send['bounds']['h'] <= drawer['bounds']['y'],
                          json.dumps({'send': send and send['bounds'],
                                      'drawer': drawer and drawer['bounds']}))
                self.shot(f'{theme}-{size[0]}x{size[1]}')
        self.resize(DEFAULT)
        self.keys('ctrl', 'shift', 'd')
        self.wait_for('terminal-pane-1')
        self.shot('light-split-default')
        self.theme(2)
        self.shot('dark-split-default')

        chrome = self.browser_with_terminal()

        self.click('terminal-pane-0')
        job = self.stubborn_job('job.pid')
        self.click('terminal-pane-1')
        third = None
        self.shell(f'echo $$ > {receipts}/third.pid')
        third = int(self.receipt('third.pid'))
        background_job = self.stubborn_job('background-job.pid', background=True)
        self.click('composer-field')
        self.type(DRAFT)
        time.sleep(1.5)
        self.click('terminal-pane-1')
        self.shell(f'echo $$ > {receipts}/focus.pid')
        self.step('a pane has focus before Quit', int(self.receipt('focus.pid')) == third)
        self.shot('before-quit')
        self.quit('Ctrl+Q from inside a pane quits gracefully')
        for name, value in (('the first shell', pid), ('the second shell', third),
                            ('a foreground job that ignores SIGHUP', job),
                            ('a background job that ignores SIGHUP', background_job)):
            self.wait(f'{name} to end', lambda: not alive(value), 10)
            self.step(f'Quit ends {name}', not alive(value), f'pid {value}')
        self.wait('Chrome to end', lambda: not any(alive(p) for p in chrome), 10)
        left = [p for p in chrome if alive(p)]
        self.step('Quit ends every Chrome process', not left, f'{len(chrome)} found, alive: {left}')
        sessions = self.sessions()
        self.step('one session is saved', len(sessions) == 1, f'{len(sessions)} sessions')

        self.call('launch')
        self.wait_for('send', seconds=60)
        self.step('relaunch reopens the session', len(self.sessions()) == 1)
        self.wait_for('terminal-drawer', present=False, seconds=5)
        self.step('the terminal starts closed after a relaunch', True)
        draft = self.draft()
        self.step('the draft survives the relaunch', draft == DRAFT, repr(draft))
        self.shot('relaunched')
        self.quit()

    def sessions(self):
        return self.call('workspace').get('workspace', {}).get('sessions', [])

    def draft(self):
        sessions = self.sessions()
        return sessions[0].get('draft') if sessions else None

    def finish(self, error):
        stopped = None
        try:
            stopped = self.call('stop')
        except Exception as stop_error:
            error = error or stop_error
        report = {'ok': error is None, 'error': str(error) if error else None,
                  'evidence': self.evidence, 'screenshots': self.shots, 'steps': self.steps,
                  'stop': stopped}
        (self.scratch / 'journey.json').write_text(json.dumps(report, indent=2))
        print(json.dumps({'ok': report['ok'], 'error': report['error'],
                          'steps': f'{sum(s["ok"] for s in self.steps)}/{len(self.steps)}',
                          'folder': str(self.scratch), 'evidence': self.evidence}, indent=2))
        return 0 if report['ok'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    native = load_driver()
    parser.add_argument('--driver', default=str(native.default_driver()))
    parser.add_argument('--binary', default=str(ROOT / 'target/debug/keystone-desktop'))
    args = parser.parse_args()
    journey = Journey(args.driver, args.binary)
    error = None
    try:
        journey.run()
    except Exception as failure:
        error = failure
        print(f'FAIL {type(failure).__name__}: {failure}', file=sys.stderr)
    return journey.finish(error)


if __name__ == '__main__':
    sys.exit(main())
