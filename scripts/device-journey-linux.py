#!/usr/bin/env python3
"""Device panel journey for Keystone on Linux (TOB-307).

Drives the real debug app on the verify skill's private headless Sway, with real
Wayland keyboard and pointer input and the scripted providers. Linux has no iOS
Simulator, and this machine has no Android SDK, so the Android flow runs against
stand-in `adb` and `emulator` programs this script writes and puts first on the
app's PATH. They are not an emulator: they answer the commands Keystone's Android
host runs, keep a sleeping process as the "emulator", draw PNG frames that show
each tap, and log every call.

Run 1, no device tools: Ctrl+N, a draft, Ctrl+Q and a relaunch bring back the
session and its draft; the Device panel opens by Ctrl+Alt+V, the palette and a tab
strip's + menu, and says which tools are missing.

Run 2, the stand-ins: the picker lists the AVD beside a stale offline adb entry
(TOB-337); Open boots it and frames show; a click on the frame becomes a tap at the
same point and a drag a swipe; Close shuts it down; the scripted Claude's device_*
tools open it again, launch an app, take a screenshot and tap; the panel beside the
Browser, Files and the terminal drawer at 1920x1080, 1100x720 and 720x900, dark and
light, with Send whole; a capture costs one adb call; device_open of an id no listing
names, or one that starts with -, is refused and leaves the open device alone; Ctrl+Q
with a capture hung leaves no stand-in process and no capture running.

Run 3, a session on another machine: a stand-in `ssh` runs the real `keystone-host
attach`; beside the remote session the Device panel, by shortcut, palette and + menu,
says devices aren't available for sessions on that machine and runs nothing local,
while a local session in the same window still opens its device.

Run from the workspace root after `cargo build --workspace --locked`:
    python3 scripts/device-journey-linux.py [--driver PATH]
Evidence: .audit/device-journey-linux-*/ with numbered PNGs and journey.json.
"""

import argparse
import importlib.util
import json
import os
import shutil
import signal
import re
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parent.parent
DRAFT = 'Draft kept across the device journey'
SIZES = ((1920, 1080), (1100, 720), (720, 900))
SCREEN = (540, 1200)
MACHINE = 'build-box-in-the-back-office.example.internal'

ADB = r'''#!/usr/bin/env python3
import os, signal, struct, sys, time, zlib
from pathlib import Path
STATE = Path(__file__).resolve().parent.parent / 'state'
W, H = __SCREEN__
NAMED = 'getprop ro.boot.qemu.avd_name; getprop ro.kernel.qemu.avd_name; screencap -p'
args = sys.argv[1:]
if not args[0].startswith('__'):
    with open(STATE / 'adb.log', 'a') as log:
        log.write(' '.join(args) + '\n')

def emulator_pid():
    try:
        pid = int((STATE / 'emulator.pid').read_text())
        os.kill(pid, 0)
        return pid
    except (FileNotFoundError, ValueError, ProcessLookupError):
        return None

def frame():
    taps = [line.split() for line in (STATE / 'input.log').read_text().splitlines()] \
        if (STATE / 'input.log').exists() else []
    app = (STATE / 'app').read_text().strip() if (STATE / 'app').exists() else ''
    background = (40, 64, 112) if app else (28, 28, 30)
    if (STATE / 'reused').exists():
        background = (170, 30, 30)
    rows = [bytearray(bytes(background) * W) for _ in range(H)]
    for y in range(0, 60):
        rows[y][:] = bytes((60, 60, 64)) * W
    points = [(int(t[1]), int(t[2])) for t in taps if t[0] == 'tap'] + \
             [(int(t[3]), int(t[4])) for t in taps if t[0] == 'swipe']
    for index, (cx, cy) in enumerate(points):
        radius, color = (22, (255, 255, 255)) if index == len(points) - 1 else (8, (150, 150, 150))
        for y in range(max(cy - radius, 0), min(cy + radius + 1, H)):
            for x in range(max(cx - radius, 0), min(cx + radius + 1, W)):
                if (x - cx) ** 2 + (y - cy) ** 2 <= radius * radius:
                    rows[y][x * 3:x * 3 + 3] = bytes(color)
    raw = b''.join(b'\0' + bytes(row) for row in rows)
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', W, H, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw, 1)) + chunk(b'IEND', b''))

def current():
    return 'Pixel_9' if (STATE / 'reused').exists() else 'Pixel_8'

if args == ['__frame']:
    sys.stdout.buffer.write(frame())
elif args == ['__booted']:
    started = float((STATE / 'started').read_text())
    print('1' if time.time() - started > 4 else '')
elif args[:2] == ['devices', '-l']:
    print('List of devices attached')
    if emulator_pid():
        print('emulator-5554          device product:sdk_gphone64_x86_64 model:Pixel_8 '
              'device:emu64x transport_id:1')
    print('emulator-5556          offline transport_id:2')
    print()
elif args[:1] == ['-s'] and args[2:] == ['emu', 'avd', 'name']:
    if args[1] == 'emulator-5554' and emulator_pid():
        sys.stdout.write(f'{current()}\r\nOK\r\n')
    else:
        port = args[1].split('-')[-1]
        sys.stderr.write(f'error: could not connect to TCP port {port}: Connection refused\n')
        sys.exit(1)
elif args[:1] == ['-s'] and args[1] != 'emulator-5554':
    sys.stderr.write(f"adb: device '{args[1]}' not found\n")
    sys.exit(1)
elif not emulator_pid():
    sys.stderr.write("adb: device 'emulator-5554' not found\n")
    sys.exit(1)
elif args[2:] == ['emu', 'kill']:
    with open(STATE / 'device.log', 'a') as log:
        log.write(f'{current()} killed\n')
    os.kill(emulator_pid(), signal.SIGTERM)
    (STATE / 'emulator.pid').unlink()
    print('OK: killing emulator, bye bye')
elif args[2] in ('exec-out', 'shell'):
    # The device's own shell: the command runs with these as its programs.
    me = Path(__file__).resolve()
    shell = f"""
cur={current()}
getprop() {{
  case "$1" in
    ro.boot.qemu.avd_name) echo "$cur" ;;
    ro.kernel.qemu.avd_name) echo ;;
    sys.boot_completed) python3 -I {me} __booted ;;
  esac
}}
screencap() {{
  echo "$cur screencap" >> {STATE}/device.log
  if [ -f {STATE}/hang-shot ]; then echo $$ > {STATE}/shot.pid; exec sleep 100000; fi
  python3 -I {me} __frame
}}
input() {{
  echo "$cur input $*" >> {STATE}/device.log
  if [ "$cur" = Pixel_8 ]; then echo "$*" >> {STATE}/input.log; fi
}}
monkey() {{ echo "$cur monkey $*" >> {STATE}/device.log; printf %s "$2" > {STATE}/app; echo "Events injected: 1"; }}
wm() {{ echo "Physical size: {W}x{H}"; }}
"""
    os.execvp('sh', ['sh', '-c', shell + ' '.join(args[3:])])
else:
    sys.stderr.write('stand-in adb: unhandled ' + ' '.join(args) + '\n')
    sys.exit(1)
'''

EMULATOR = r'''#!/usr/bin/env python3
import os, sys, time
from pathlib import Path
STATE = Path(__file__).resolve().parent.parent / 'state'
with open(STATE / 'emulator.log', 'a') as log:
    log.write(' '.join(sys.argv[1:]) + '\n')
if sys.argv[1:] == ['-list-avds']:
    print('Pixel_8')
    sys.exit(0)
if sys.argv[1:3] == ['-avd', 'Pixel_8']:
    (STATE / 'emulator.pid').write_text(str(os.getpid()))
    (STATE / 'started').write_text(str(time.time()))
    os.execvp('sleep', ['sleep', '100000'])
print(f'ERROR   | Unknown AVD name [{sys.argv[2] if len(sys.argv) > 2 else ""}]')
sys.exit(1)
'''


def load_driver():
    spec = importlib.util.spec_from_file_location('native_driver', ROOT / 'scripts/native-driver.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Failure(Exception):
    pass


def fitted(bounds):
    """Where the screen's image sits in the frame area: fitted whole and centred."""
    scale = min(bounds['w'] / SCREEN[0], bounds['h'] / SCREEN[1])
    w, h = SCREEN[0] * scale, SCREEN[1] * scale
    return {'x': bounds['x'] + (bounds['w'] - w) / 2, 'y': bounds['y'] + (bounds['h'] - h) / 2,
            'w': w, 'h': h}


def alive(pid):
    try:
        state = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0]
    except (FileNotFoundError, ProcessLookupError):
        return False
    return state not in ('Z', 'X')


def running_from(folder):
    """Processes whose command line runs a program in `folder`."""
    found = []
    for entry in Path('/proc').iterdir():
        if not entry.name.isdigit():
            continue
        try:
            line = (entry / 'cmdline').read_bytes().replace(b'\0', b' ').decode(errors='replace')
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
        if str(folder) in line and alive(int(entry.name)):
            found.append((int(entry.name), line.strip()))
    return found


class Journey:
    def __init__(self, driver, binary):
        self.native = load_driver()
        self.server = self.native.Server(SimpleNamespace(driver=driver, binary=binary,
                                                         trace_wayland=False))
        self.steps = []
        self.shots = []
        self.scratch = ROOT / '.audit' / f'device-journey-linux-{time.strftime("%Y%m%dT%H%M%S")}'
        self.project = self.scratch / 'project'
        self.sdk = self.scratch / 'sdk'
        self.state = self.sdk / 'state'
        for folder in (self.project, self.sdk / 'bin', self.state, self.scratch / 'claude',
                       self.scratch / 'codex'):
            folder.mkdir(parents=True)
        subprocess.run(['git', 'init', '-q', str(self.project)], check=True)
        (self.project / 'README.md').write_text('A project for the device journey.\n')
        for name, body in (('adb', ADB.replace('__SCREEN__', repr(SCREEN))), ('emulator', EMULATOR)):
            path = self.sdk / 'bin' / name
            path.write_text(body)
            path.chmod(0o755)
        self.evidence = None
        self.sway = None
        self.database = None
        self.run_name = ''

    def call(self, name, **arguments):
        value, _ = self.server.handle(name, arguments)
        return value

    def step(self, name, ok, detail=''):
        name = f'[{self.run_name}] {name}'
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

    def wait_for(self, name, present=True, seconds=15):
        return self.call('wait_for', id=name, present=present, timeout_ms=int(seconds * 1000))

    def elements(self):
        return self.call('elements')['elements']

    def element(self, name):
        found = [e for e in self.elements() if e['id'] == name and e['visible']]
        return found[0] if len(found) == 1 else None

    def keys(self, *keys):
        self.call('input', actions=[{'action': 'keypress', 'keys': list(keys)}])

    def type(self, text):
        self.call('input', actions=[{'action': 'type', 'text': text}])

    def click(self, name):
        self.call('click', id=name)

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
        time.sleep(0.6)

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

    def theme(self, option):
        self.keys('ctrl', 'comma')
        self.wait_for(f'settings-theme-option-{option}')
        self.click(f'settings-theme-option-{option}')
        time.sleep(0.4)
        self.keys('ctrl', 'comma')
        self.wait_for('send')

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

    def start(self, name, path):
        self.run_name = name
        env = {
            'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
            'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
            'KEYSTONE_PROJECT_DIR': str(self.project),
            'CLAUDE_CONFIG_DIR': str(self.scratch / 'claude'),
            'CODEX_HOME': str(self.scratch / 'codex'),
            'PATH': path,
        }
        started = self.call('start', width=SIZES[0][0], height=SIZES[0][1], env=env)
        self.evidence = started['evidence']
        self.database = started.get('database')
        self.sway = started['session'].get('sway_socket') or started['session'].get('swaysock')
        self.step('the private desktop has a Sway IPC socket', self.sway)
        self.wait_for('welcome-claude', seconds=60)
        self.step('doctor: the launched build runs',
                  self.call('doctor').get('binary_unchanged') is True)

    def stop(self):
        stopped = self.call('stop')
        self.step('the verify run stops cleanly', not stopped.get('errors'), json.dumps(stopped)[:300])

    def sessions(self):
        return self.call('workspace').get('workspace', {}).get('sessions', [])

    def log_text(self):
        connection = sqlite3.connect(Path(self.database).resolve().as_uri() + '?mode=ro', uri=True,
                                     timeout=1)
        try:
            return '\n'.join(row[0] for row in connection.execute('SELECT payload FROM log_entries'))
        finally:
            connection.close()

    def adb_calls(self):
        log = self.state / 'adb.log'
        return log.read_text().splitlines() if log.exists() else []

    def inputs(self):
        log = self.state / 'input.log'
        return log.read_text().splitlines() if log.exists() else []

    def toggle_off(self, keys, name):
        for _ in range(3):
            if not self.element(name):
                return
            self.keys(*keys)
            time.sleep(0.5)
        self.step(f'{name} closes', not self.element(name))

    def device_shown(self, name):
        """Shows the Device surface when the window folded its surfaces into tabs."""
        if self.element(name):
            return []
        tabs = sorted(e['id'] for e in self.elements()
                      if e['id'].startswith('fitted-tab-') or re.fullmatch(r'[a-z0-9-]+-tab-\d+', e['id']))
        for tab in tabs:
            self.click(tab)
            time.sleep(0.5)
            if self.element(name):
                return tabs
        return tabs

    def no_tools(self):
        self.start('no device tools', os.environ['PATH'])
        found = [p for p in os.environ['PATH'].split(':') if (Path(p) / 'adb').exists()]
        self.step('this machine has no adb on PATH', not found, json.dumps(found))
        self.keys('ctrl', 'n')
        self.wait('a Claude session', lambda: len(self.sessions()) == 1)
        self.wait_for('send')
        self.step('Ctrl+N makes a Claude session', True)

        self.keys('ctrl', 'alt', 'v')
        self.wait_for('device-picker')
        self.wait_for('device-refresh')
        self.wait('the listing', lambda: sum(e['id'] == 'device-note' for e in self.elements()) >= 2)
        notes = sum(e['id'] == 'device-note' for e in self.elements())
        rows = [e['id'] for e in self.elements() if e['id'].startswith('device-row-')]
        self.step('Ctrl+Alt+V opens the Device panel; it lists no device and a note per platform',
                  notes == 2 and not rows, f'{notes} notes, rows {rows}')
        self.shot('no-tools-light')
        self.click('composer-field')
        self.keys('ctrl', 'alt', 'v')
        self.keys('ctrl', 'alt', 'v')
        self.wait_for('device-picker', present=False)
        self.step('Ctrl+Alt+V closes the focused Device panel', True)

        self.keys('ctrl', 'k')
        time.sleep(0.4)
        self.type('Toggle device')
        time.sleep(0.3)
        self.shot('palette-toggle-device')
        self.keys('Return')
        self.wait_for('device-picker')
        self.step('the palette opens it', True)
        self.keys('ctrl', 'alt', 'v')
        self.wait_for('device-picker', present=False)

        self.keys('ctrl', 'shift', 'e')
        self.wait_for('surface-add')
        self.click('surface-add')
        self.wait_for('surface-add-device')
        self.shot('plus-menu')
        self.click('surface-add-device')
        self.wait_for('device-picker')
        self.step("a tab strip's + menu opens it as a tab beside Files", True)
        self.theme(2)
        self.shot('no-tools-dark-beside-files')

        self.click('composer-field')
        self.type(DRAFT)
        time.sleep(1.5)
        self.quit()
        self.call('launch')
        self.wait_for('send', seconds=60)
        self.step('relaunch reopens the session', len(self.sessions()) == 1)
        draft = self.sessions()[0].get('draft')
        self.step('the draft survives the relaunch', draft == DRAFT, repr(draft))
        self.shot('relaunched')
        self.quit()
        self.stop()

    def stand_ins(self):
        self.start('stand-in adb and emulator', f'{self.sdk / "bin"}:{os.environ["PATH"]}')
        self.keys('ctrl', 'n')
        self.wait('a Claude session', lambda: len(self.sessions()) == 1)
        self.wait_for('send')

        self.keys('ctrl', 'alt', 'v')
        self.wait_for('device-row-0', seconds=20)
        rows = [e['id'] for e in self.elements() if e['id'].startswith('device-row-')]
        self.step('the picker lists the AVD despite the stale offline entry (TOB-337)',
                  rows == ['device-row-0'], json.dumps(rows))
        self.step('Keystone asked the stale entry, which refused',
                  '-s emulator-5556 emu avd name' in self.adb_calls())
        self.shot('picker-android')

        self.click('device-open-0')
        self.wait_for('device-phase')
        self.wait('the stand-in emulator to start', lambda: (self.state / 'emulator.pid').exists(), 10)
        emulator = int((self.state / 'emulator.pid').read_text())
        launched = [line for line in (self.state / 'emulator.log').read_text().splitlines()
                    if line.startswith('-avd')]
        self.step('Open launches the emulator headless',
                  launched == ['-avd Pixel_8 -no-window -no-audio -no-boot-anim'], json.dumps(launched))
        self.wait_for('device-frame', seconds=30)
        time.sleep(0.8)
        self.shot('booted-frame')
        self.step('frames come from screencap',
                  any('screencap -p' in c for c in self.adb_calls()))

        frame = self.element('device-frame')
        self.step('the frame shows whole', frame and frame['visible'] == frame['bounds'],
                  json.dumps(frame and frame['bounds']))
        b = fitted(frame['bounds'])
        at = (0.25, 0.3)
        self.call('click', x=round(b['x'] + b['w'] * at[0]), y=round(b['y'] + b['h'] * at[1]))
        self.wait('the tap', lambda: any(i.startswith('tap ') for i in self.inputs()), 10)
        tap = [i for i in self.inputs() if i.startswith('tap ')][-1].split()
        x, y = int(tap[1]) / (SCREEN[0] - 1), int(tap[2]) / (SCREEN[1] - 1)
        self.step('a click on the frame taps the same point of the screen',
                  abs(x - at[0]) < 0.02 and abs(y - at[1]) < 0.02, ' '.join(tap))
        time.sleep(1.2)
        self.shot('frame-shows-tap')
        start = {'x': round(b['x'] + b['w'] * 0.5), 'y': round(b['y'] + b['h'] * 0.75)}
        end = {'x': round(b['x'] + b['w'] * 0.5), 'y': round(b['y'] + b['h'] * 0.35)}
        self.call('input', actions=[{'action': 'drag', 'path': [start, end],
                                     'duration_ms': 400}])
        self.wait('the swipe', lambda: any(i.startswith('swipe ') for i in self.inputs()), 10)
        self.step('a drag on the frame swipes', True,
                  [i for i in self.inputs() if i.startswith('swipe ')][-1])

        self.click('device-close')
        self.wait('the emulator to stop', lambda: not alive(emulator), 15)
        self.step('Close shuts down the emulator Keystone booted',
                  any(c.endswith('emu kill') for c in self.adb_calls()), f'pid {emulator} gone')
        self.wait_for('device-picker')
        self.shot('closed-back-to-picker')
        self.toggle_off(('ctrl', 'alt', 'v'), 'device-picker')

        before = len(self.inputs())
        self.click('composer-field')
        self.type('orchestrator test: device android:Pixel_8 com.example.app')
        self.click('send')
        self.wait('the scripted Claude to finish',
                  lambda: 'Scripted device' in self.log_text(), 90)
        text = self.log_text()
        reply = text[text.index('Scripted device'):][:240]
        self.step("the scripted Claude's device_* tools list, open, launch, screenshot and tap",
                  'Scripted device success' in reply and 'mime=image/png' in reply
                  and 'screenshot=540x1200' in reply, reply)
        self.step('the agent opened the Device panel beside the session', self.element('device-frame'))
        agent = self.inputs()[before:]
        self.step('the agent tapped the centre of its screenshot', 'tap 270 600' in agent,
                  json.dumps(agent))
        self.step('the agent launched the app', (self.state / 'app').read_text() == 'com.example.app')
        emulator = int((self.state / 'emulator.pid').read_text())
        time.sleep(1.2)
        self.shot('agent-drove-device')

        start = len(self.adb_calls())
        time.sleep(5)
        window = self.adb_calls()[start:]
        frames = sum('screencap -p' in c for c in window)
        self.step('a running device costs one adb call per frame',
                  frames >= 5 and len(window) <= frames + 1,
                  f'{len(window)} adb calls for {frames} frames')

        for asked, says in (('android:-help', 'Unknown device id'),
                            ('android:No_Such_Phone',
                             'No device android:No_Such_Phone here. Available: android:Pixel_8')):
            answered = self.log_text().count('Scripted device failure')
            self.click('composer-field')
            self.type(f'orchestrator test: device open {asked}')
            self.click('send')
            self.wait(f'the answer to {asked}',
                      lambda: self.log_text().count('Scripted device failure') > answered, 60)
            text = self.log_text()
            reply = text[text.rindex('Scripted device failure'):][:200]
            self.step(f'device_open {asked} is refused and says so', says in reply, reply)
            self.step(f'device_open {asked} leaves the open emulator running beside the session',
                      alive(emulator) and self.element('device-name')
                      and not any(c.endswith('emu kill') for c in self.adb_calls()[start:]))

        for theme, option in (('dark', 2), ('light', 1)):
            self.theme(option)
            for beside, keys, name in (('browser', ('ctrl', 'alt', 'b'), 'browser-servers'),
                                       ('files', ('ctrl', 'shift', 'e'), 'files-tree-row-0'),
                                       ('terminal', ('ctrl', 'j'), 'terminal-drawer')):
                self.resize(SIZES[0])
                self.click('composer-field')
                self.keys(*keys)
                self.wait_for(name)
                for size in SIZES:
                    self.resize(size)
                    tabs = self.device_shown('device-frame')
                    time.sleep(0.6)
                    self.shot(f'{theme}-{beside}-{size[0]}x{size[1]}')
                    send, frame = self.element('send'), self.element('device-frame')
                    drawer = self.element('terminal-drawer')
                    fits = (send and frame and send['visible'] == send['bounds']
                            and send['bounds']['x'] + send['bounds']['w'] <= size[0]
                            and send['bounds']['y'] + send['bounds']['h'] <= size[1]
                            and frame['visible'] == frame['bounds'] and frame['bounds']['h'] > 60
                            and (beside != 'terminal' or drawer is None
                                 or send['bounds']['y'] + send['bounds']['h'] <= drawer['bounds']['y']))
                    self.step(f'{theme} {beside} {size[0]}x{size[1]}: Send whole, the frame whole',
                              fits, json.dumps({'send': send and send['bounds'],
                                                'frame': frame and frame['bounds'],
                                                'drawer': drawer and drawer['bounds'],
                                                'tabs': tabs}))
                self.resize(SIZES[0])
                if beside == 'terminal':
                    self.click('composer-field')
                    self.keys('ctrl', 'j')
                    self.keys('ctrl', 'j')
                    self.wait_for('terminal-drawer', present=False)
                else:
                    self.toggle_off(keys, name)
        self.theme(2)

        self.click('composer-field')
        self.type(DRAFT)
        time.sleep(1.5)
        (self.state / 'hang-shot').write_text('')
        self.wait('a capture to hang', lambda: (self.state / 'shot.pid').exists(), 10)
        hung = int((self.state / 'shot.pid').read_text())
        self.step('a capture hangs before Quit', alive(hung), f'pid {hung}')
        self.quit('Ctrl+Q with a device open quits gracefully')
        (self.state / 'hang-shot').unlink()
        self.wait('the hung capture to end', lambda: not alive(hung), 10)
        self.step('Quit kills the capture that hung', not alive(hung), f'pid {hung}')
        self.wait('the emulator to stop', lambda: not alive(emulator), 10)
        self.step('Quit shuts down the emulator Keystone booted', not alive(emulator), f'pid {emulator}')
        left = running_from(self.sdk)
        self.step('no stand-in process is left', not left, json.dumps(left))
        calls = len(self.adb_calls())
        time.sleep(3)
        self.step('no capture runs after Quit', len(self.adb_calls()) == calls,
                  f'{len(self.adb_calls()) - calls} adb calls in 3 s')

        self.call('launch')
        self.wait_for('send', seconds=60)
        draft = self.sessions()[0].get('draft')
        self.step('relaunch brings back the session and its draft', draft == DRAFT, repr(draft))
        time.sleep(1)
        self.step('no device is shown as open after a relaunch', not self.element('device-name'))
        self.reused_serial()
        self.quit()
        self.stop()

    def device_log(self):
        log = self.state / 'device.log'
        return log.read_text() if log.exists() else ''

    def open_pixel(self):
        self.click('composer-field')
        if not self.element('device-picker') and not self.element('device-name'):
            self.keys('ctrl', 'alt', 'v')
        self.wait_for('device-row-0', seconds=20)
        self.click('device-open-0')
        self.wait_for('device-frame', seconds=30)
        time.sleep(1)

    def reused_serial(self):
        """After adb kill-server, the new server gives Pixel_9 the serial and transport id
        Pixel_8 had. Nothing of Pixel_9 may show as Pixel_8, and nothing may reach it."""
        self.open_pixel()
        pixel = int((self.state / 'emulator.pid').read_text())
        self.toggle_off(('ctrl', 'alt', 'v'), 'device-frame')
        (self.state / 'reused').write_text('')
        swapped = len(self.device_log())
        answered = self.log_text().count('Scripted device')
        self.click('composer-field')
        self.keys('ctrl', 'a')
        self.keys('BackSpace')
        self.type('orchestrator test: device close')
        self.click('send')
        self.wait('the close to answer', lambda: self.log_text().count('Scripted device') > answered, 60)
        self.step('device_close after an adb restart kills nothing on the reused serial',
                  'killed' not in self.device_log()[swapped:] and alive(pixel),
                  repr(self.device_log()[swapped:][-200:]))

        (self.state / 'reused').unlink()
        self.open_pixel()
        self.toggle_off(('ctrl', 'alt', 'v'), 'device-frame')
        before = self.device_log()
        (self.state / 'reused').write_text('')
        self.click('composer-field')
        self.keys('ctrl', 'alt', 'v')
        self.wait_for('device-frame', seconds=10)
        frame = self.element('device-frame')
        self.call('click', x=frame['center']['x'], y=frame['center']['y'])
        time.sleep(3)
        self.shot('reused-serial-not-running')
        after = self.device_log()[len(before):]
        self.step('after an adb restart the panel neither takes nor sends anything to Pixel_9',
                  'Pixel_9' not in after, repr(after[-300:]))
        notes = [e for e in self.elements() if e['id'] == 'device-note']
        self.step('the panel no longer shows the last Pixel_8 frame as live: it carries its age',
                  notes or self.element('device-frame') is None, f'{len(notes)} notes')
        os.kill(pixel, signal.SIGTERM)
        (self.state / 'emulator.pid').unlink()
        (self.state / 'reused').unlink()

    def descendants(self, root):
        children = {}
        for entry in Path('/proc').iterdir():
            if entry.name.isdigit():
                try:
                    fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
                    line = (entry / 'cmdline').read_bytes().replace(b'\0', b' ').decode(errors='replace')
                except (FileNotFoundError, ProcessLookupError, PermissionError):
                    continue
                children.setdefault(int(fields[1]), []).append((int(entry.name), line.strip()))
        found, queue = [], [root]
        while queue:
            for pid, line in children.get(queue.pop(), []):
                found.append((pid, line))
                queue.append(pid)
        return found

    def remote(self):
        """A session on a linked machine beside a local one in the same window."""
        host_dir = Path(tempfile.mkdtemp(prefix='kh', dir='/tmp'))
        host_dir.chmod(0o700)
        ssh = self.sdk / 'bin' / 'ssh'
        ssh.write_text(f'''#!/bin/sh
echo "ssh $*" >> "{self.state}/ssh.log"
exec env -i HOME={host_dir} KEYSTONE_HOST_DIR={host_dir} \\
  KEYSTONE_CLAUDE_PATH={ROOT / 'target/debug/keystone-fake-claude'} PATH=/usr/bin:/bin \\
  {ROOT / 'target/debug/keystone-host'} attach
''')
        ssh.chmod(0o755)
        try:
            self.remote_run(host_dir)
        finally:
            lock = host_dir / 'run' / 'host.lock'
            if lock.exists():
                try:
                    os.kill(int(lock.read_text().strip()), 9)
                except (ValueError, ProcessLookupError):
                    pass
            shutil.rmtree(host_dir, ignore_errors=True)
            ssh.unlink()

    def remote_run(self, host_dir):
        self.start('a session on another machine', f'{self.sdk / "bin"}:{os.environ["PATH"]}')
        self.keys('ctrl', 'n')
        self.wait('a local Claude session', lambda: len(self.sessions()) == 1)
        self.wait_for('send')

        self.keys('ctrl', 'k')
        time.sleep(0.4)
        self.type('Link a machine')
        time.sleep(0.3)
        self.keys('Return')
        self.wait_for('machine-destination')
        self.click('machine-destination')
        self.type(MACHINE)
        self.click('machine-link')
        self.wait('the machine to link',
                  lambda: any(e['id'].startswith('machine-new-session-') for e in self.elements()), 30)
        new = next(e['id'] for e in self.elements() if e['id'].startswith('machine-new-session-'))
        self.step('the machine links through the stand-in ssh', (self.state / 'ssh.log').exists())
        self.click(new)
        time.sleep(0.5)
        self.type(str(self.project))
        time.sleep(0.4)
        self.keys('Return')
        self.wait('a session on the machine', lambda: len(self.sessions()) == 2, 20)
        time.sleep(0.8)
        if self.element('settings-theme-option-1'):
            self.keys('ctrl', 'comma')
        self.wait_for('send')
        self.step('a Claude session starts on the machine', True)
        app = self.app_pid()
        calls = len(self.adb_calls())

        self.click('composer-field')
        self.keys('ctrl', 'alt', 'v')
        self.wait_for('device-elsewhere')
        self.step('Ctrl+Alt+V beside the remote session shows the refusal, no picker',
                  not self.element('device-picker'))
        self.shot('remote-refusal-light')
        self.toggle_off(('ctrl', 'alt', 'v'), 'device-elsewhere')
        self.keys('ctrl', 'k')
        time.sleep(0.4)
        self.type('Toggle device')
        time.sleep(0.3)
        self.keys('Return')
        self.wait_for('device-elsewhere')
        self.step('the palette shows the same refusal', not self.element('device-picker'))
        self.toggle_off(('ctrl', 'alt', 'v'), 'device-elsewhere')
        self.keys('ctrl', 'shift', 'e')
        self.wait_for('surface-add')
        adds = sorted(e['id'] for e in self.elements() if e['id'] == 'surface-add')
        self.click(adds[0])
        self.wait_for('surface-add-device')
        self.click('surface-add-device')
        self.wait_for('device-elsewhere')
        self.step("the + menu shows the same refusal", not self.element('device-picker'))
        self.theme(2)
        self.shot('remote-refusal-dark')
        for size in ((1100, 720), (720, 900)):
            self.resize(size)
            for name in ('files-empty', 'device-elsewhere'):
                tabs = self.device_shown(name)
                time.sleep(0.5)
                self.shot(f'remote-{name}-{size[0]}x{size[1]}')
                self.step(f'{size[0]}x{size[1]}: {name} shows', self.element(name), json.dumps(tabs))
        self.resize(SIZES[0])
        time.sleep(2)
        local_tools = [(pid, line) for pid, line in self.descendants(app)
                       if any(name in line for name in ('adb', 'emulator', 'simctl', 'axe'))]
        self.step('nothing local ran for the remote session',
                  len(self.adb_calls()) == calls and not local_tools,
                  f'{len(self.adb_calls()) - calls} adb calls, children {local_tools}')

        rows = sorted(e['id'] for e in self.elements() if re.fullmatch(r'session-\d+', e['id']))
        local = next(row for row in rows if not self.element(f'session-machine-{row.split("-")[1]}'))
        self.click(local)
        self.wait_for('device-row-0', seconds=20)
        self.step('a local session in the same window lists the local device',
                  not self.element('device-elsewhere'))
        self.click('device-open-0')
        self.wait_for('device-frame', seconds=30)
        time.sleep(0.8)
        self.shot('local-device-beside-remote-dark')
        self.theme(1)
        self.shot('local-device-beside-remote-light')
        remote = next(row for row in rows if row != local)
        self.click(remote)
        self.wait_for('device-elsewhere')
        time.sleep(0.6)
        calls = len(self.adb_calls())
        time.sleep(2)
        self.step('back on the remote session, no capture runs',
                  len(self.adb_calls()) == calls, f'{len(self.adb_calls()) - calls} adb calls in 2 s')
        self.shot('remote-refusal-light-again')
        emulator = int((self.state / 'emulator.pid').read_text())
        self.quit('Ctrl+Q with a local device and a remote session quits gracefully')
        self.wait('the emulator to stop', lambda: not alive(emulator), 10)
        self.step('Quit shuts down the local emulator Keystone booted', not alive(emulator))
        self.stop()

    def finish(self, error):
        try:
            self.call('stop')
        except Exception:
            pass
        report = {'ok': error is None, 'error': str(error) if error else None,
                  'screenshots': self.shots, 'steps': self.steps}
        (self.scratch / 'journey.json').write_text(json.dumps(report, indent=2))
        print(json.dumps({'ok': report['ok'], 'error': report['error'],
                          'steps': f'{sum(s["ok"] for s in self.steps)}/{len(self.steps)}',
                          'folder': str(self.scratch)}, indent=2))
        return 0 if report['ok'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    native = load_driver()
    parser.add_argument('--driver', default=str(native.default_driver()))
    parser.add_argument('--binary', default=str(ROOT / 'target/debug/keystone-desktop'))
    parser.add_argument('--only', choices=('no-tools', 'stand-ins', 'remote'))
    args = parser.parse_args()
    journey = Journey(args.driver, args.binary)
    error = None
    try:
        if args.only in (None, 'no-tools'):
            journey.no_tools()
        if args.only in (None, 'stand-ins'):
            journey.stand_ins()
        if args.only in (None, 'remote'):
            journey.remote()
    except Exception as failure:
        error = failure
        print(f'FAIL {type(failure).__name__}: {failure}', file=sys.stderr)
    return journey.finish(error)


if __name__ == '__main__':
    sys.exit(main())
