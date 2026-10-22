#!/usr/bin/env python3
"""MCP server that drives the real Keystone binary on a private nested desktop.

Each `start` owns one computer-use-mcp connection, one Sway desktop, one
disposable workspace file, and one evidence directory. Every driver request
and sanitized result goes to actions.jsonl; screenshots and state snapshots are
kept beside it. The app reports named element geometry through
KEYSTONE_AUTOMATION_PATH, so `click` and `wait_for` can target names.
"""
import argparse
import base64
import copy
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

PROTOCOL = '2025-03-26'


class NotReady(RuntimeError):
    """The app has not reported a frame yet. Retrying can succeed."""
ROOT = Path(__file__).resolve().parent.parent

# computer-use-mcp spawns the app but never reports how it exited, so each
# launch runs under this parent. It writes the app's PID, then its exit
# status, to the first file named; a negative returncode is a signal. The app's
# stderr, where a panic goes, is kept in the second. The parent ignores
# termination signals so a teardown still records the app's status.
REAPER = """
import json, os, signal, subprocess, sys
path, log = sys.argv[1:3]
stops = (signal.SIGTERM, signal.SIGINT, signal.SIGHUP)
def write(value):
    with open(path + '.tmp', 'w') as stream:
        json.dump({'launcher_pid': os.getpid(), 'stderr': log, **value}, stream)
    os.replace(path + '.tmp', path)
def handle(disposition):
    for number in stops:
        signal.signal(number, disposition)
handle(signal.SIG_IGN)
try:
    with open(log, 'ab') as stderr:
        child = subprocess.Popen(sys.argv[3:], stderr=stderr,
                                 preexec_fn=lambda: handle(signal.SIG_DFL))
except OSError as error:
    write({'pid': None, 'error': str(error)})
    sys.exit(127)
write({'pid': child.pid, 'returncode': None})
code = child.wait()
write({'pid': child.pid, 'returncode': code,
       'signal': signal.Signals(-code).name if code < 0 else None})
"""

# Each run's app gets this private session bus: a dbus-daemon with no activatable
# services, so nothing the app sends over D-Bus, such as a desktop notification,
# reaches the user's own session. A journey may own names on it, as a fake
# notification server does.
BUS_CONFIG = """<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={path}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"""


def default_driver():
    """This checkout's driver, else the main checkout's, which git worktrees share."""
    local = ROOT / '.audit/tools/computer-use-mcp'
    if local.exists():
        return local
    try:
        common = subprocess.run(['git', 'rev-parse', '--path-format=absolute', '--git-common-dir'],
                                cwd=ROOT, capture_output=True, text=True).stdout.strip()
    except OSError:
        return local
    shared = Path(common).parent / '.audit/tools/computer-use-mcp' if common else local
    return shared if shared.exists() else local


def add_binary_argument(parser):
    parser.add_argument('--binary', default=os.environ.get(
        'KEYSTONE_BINARY', str(ROOT / 'target/debug/keystone-desktop')),
        help='App executable to launch; defaults to $KEYSTONE_BINARY, then this checkout\'s debug build')


def timestamp():
    return time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())


def digest(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def database_path(state):
    state = Path(state)
    header = b''
    try:
        if state.is_file():
            with state.open('rb') as stream:
                header = stream.read(16)
                if header == b'SQLite format 3\0':
                    return state
                while header and not header.strip():
                    following = stream.read(16)
                    if not following:
                        break
                    header = following
    except OSError:
        header = b''
    if state.suffix == '.json':
        return state.with_suffix('.sqlite3')
    sibling = Path(str(state) + '.sqlite3')
    if state.suffix in ('.sqlite3', '.sqlite', '.db') and not header.lstrip().startswith(b'{'):
        if (not state.exists() or state.stat().st_size == 0) and sibling.exists():
            return sibling
        return state
    return sibling


def open_database(path):
    return sqlite3.connect(Path(path).resolve().as_uri() + '?mode=ro', uri=True, timeout=0.1)


def workspace_projection(connection):
    connection.execute('BEGIN')
    try:
        snapshot = json.loads(connection.execute(
            'SELECT payload FROM workspace WHERE singleton=1').fetchone()[0])
        snapshot['workspace']['sessions'] = [json.loads(row[0]) for row in connection.execute(
            'SELECT payload FROM sessions ORDER BY position')]
        for table in ('accounts', 'projects', 'prompts', 'deliveries'):
            if connection.execute(f"SELECT 1 FROM sqlite_master WHERE type='table' AND name='{table}'").fetchone():
                rows = [json.loads(row[0]) for row in connection.execute(
                    f'SELECT payload FROM {table} ORDER BY position')]
                if rows:
                    snapshot['workspace'][table] = rows
        drafts = {key[len('commit_draft/'):]: value for key, value in connection.execute(
            "SELECT key, value FROM metadata WHERE substr(key, 1, 13) = 'commit_draft/'")}
        if drafts:
            snapshot['workspace']['commit_drafts'] = drafts
        return snapshot
    finally:
        connection.rollback()


def read_workspace(state):
    database = database_path(state)
    if not database.exists():
        return {'state_absent': True}
    with closing(open_database(database)) as connection:
        return workspace_projection(connection)


class Run:
    def __init__(self, driver, root, evidence, binary, trace_wayland=False):
        self.env = {}
        self.trace_wayland = trace_wayland
        self.prime_keyboard = True
        self.binary = Path(binary).resolve()
        self.binary_sha = digest(self.binary)
        driver_sha = digest(driver)
        self.evidence = Path(evidence).resolve()
        self.evidence.mkdir(parents=True, exist_ok=False)
        self.scratch = Path(tempfile.mkdtemp(prefix='keystone-verify-'))
        self.state = self.scratch / 'workspace.json'
        self.geometry = self.scratch / 'elements.json'
        self.sequence = 0
        self.images = 0
        self.app_pids = []
        self.exit_files = {}
        self.app_logs = {}
        self.closed = False
        self.bus = None
        self.bus_address = None
        self.responses = queue.Queue()
        self.log = (self.evidence / 'actions.jsonl').open('a')
        self.stderr = (self.evidence / 'driver.stderr.log').open('w')
        try:
            driver_env = {**os.environ, **({'WAYLAND_DEBUG': 'client'} if trace_wayland else {})}
            self.process = subprocess.Popen([str(Path(driver).resolve())], stdin=subprocess.PIPE,
                                            stdout=subprocess.PIPE, stderr=self.stderr, text=True,
                                            bufsize=1, cwd=root, env=driver_env)
        except Exception as error:
            shutil.rmtree(self.scratch)
            self.record('cleanup', {'startup_error': str(error), 'scratch_retained': None,
                                    'errors': [], 'evidence': str(self.evidence)})
            self.log.close()
            self.stderr.close()
            raise
        threading.Thread(target=self.read_responses, daemon=True).start()
        self.record('identity', {'binary': str(self.binary), 'binary_sha256': self.binary_sha,
                                'driver': str(Path(driver).resolve()),
                                'driver_sha256': driver_sha, 'scratch': str(self.scratch),
                                'driver_pid': self.process.pid, 'trace_wayland': trace_wayland})

    def read_responses(self):
        try:
            for line in self.process.stdout:
                self.responses.put(json.loads(line))
        except Exception as error:
            self.responses.put({'reader_error': str(error)})
        finally:
            self.responses.put(None)

    def record(self, kind, value):
        self.log.write(json.dumps({'time': timestamp(), 'kind': kind, 'value': value}) + '\n')
        self.log.flush()

    def sanitize(self, value):
        """Moves image payloads into numbered evidence files, in place."""
        if isinstance(value, dict):
            if value.get('type') == 'image' and 'data' in value:
                self.images += 1
                extension = '.png' if value.get('mimeType') == 'image/png' else '.bin'
                path = self.evidence / f'{self.images:04d}{extension}'
                path.write_bytes(base64.b64decode(value['data'], validate=True))
                value.pop('data')
                value.update(path=str(path), sha256=digest(path))
                return
            for item in value.values():
                self.sanitize(item)
        elif isinstance(value, list):
            for item in value:
                self.sanitize(item)

    def request(self, method, params=None):
        """Returns the driver's raw result; the log gets a copy without image data."""
        self.sequence += 1
        request = {'jsonrpc': '2.0', 'id': self.sequence, 'method': method}
        if params is not None:
            request['params'] = params
        self.record('request', request)
        self.process.stdin.write(json.dumps(request) + '\n')
        self.process.stdin.flush()
        deadline = time.monotonic() + 90
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f'MCP request {self.sequence} timed out')
            response = self.responses.get(timeout=remaining)
            if response is None or 'reader_error' in response:
                raise RuntimeError(f'MCP stream closed: {response}')
            logged = copy.deepcopy(response)
            self.sanitize(logged)
            self.record('response', logged)
            if response.get('id') != self.sequence:
                if 'id' in response and 'method' in response:
                    reply = {'jsonrpc': '2.0', 'id': response['id'],
                             'error': {'code': -32601, 'message': 'Client requests unsupported'}}
                    self.process.stdin.write(json.dumps(reply) + '\n')
                    self.process.stdin.flush()
                continue
            if 'error' in response:
                raise RuntimeError(json.dumps(response['error']))
            result = response.get('result', {})
            if result.get('isError'):
                raise RuntimeError(json.dumps(logged.get('result')))
            result['_evidence'] = logged.get('result', {})
            return result

    def call(self, tool, arguments=None):
        return self.request('tools/call', {'name': tool, 'arguments': arguments or {}})

    def start(self, width, height, headless, env=None):
        self.request('initialize', {'protocolVersion': PROTOCOL, 'capabilities': {},
                                   'clientInfo': {'name': 'keystone-verifier', 'version': '2'}})
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
        self.process.stdin.flush()
        session = self.call('session_start', {'width': width, 'height': height, 'xwayland': True,
                                              'sandbox': False, 'headless': headless})
        # HOME, and each provider folder the driver was not itself started with, start
        # empty, so a run that names no fixture never reads or shows the user's own
        # provider config or skills.
        homes = {key: self.scratch / folder for key, folder in
                 (('CLAUDE_CONFIG_DIR', 'claude-config'), ('CODEX_HOME', 'codex-home'),
                  ('HOME', 'home'))
                 if key == 'HOME' or key not in os.environ}
        for folder in homes.values():
            folder.mkdir(exist_ok=True)
        self.env = {**{key: str(folder) for key, folder in homes.items()}, **app_env(env)}
        self.start_bus()
        return {'session': session.get('structuredContent'), 'bus': self.bus_address,
                'launch': self.launch()}

    def start_bus(self):
        config = self.scratch / 'bus.conf'
        config.write_text(BUS_CONFIG.format(path=self.scratch / 'bus'))
        # setpriv ends the daemon if the server dies without stopping it.
        self.bus = subprocess.Popen(['setpriv', '--pdeathsig', 'TERM', 'dbus-daemon',
                                     f'--config-file={config}', '--nofork', '--print-address=1'],
                                    stdout=subprocess.PIPE, stderr=self.stderr, text=True)
        self.bus_address = self.bus.stdout.readline().strip()
        if not self.bus_address:
            raise RuntimeError(f'The private session bus exited with {self.bus.wait()}')
        self.record('bus', {'address': self.bus_address, 'pid': self.bus.pid})

    def stop_bus(self):
        if self.bus is None:
            return
        self.bus.terminate()
        try:
            self.bus.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.bus.kill()
            self.bus.wait()
        self.bus.stdout.close()

    def launch(self):
        # KS-009: once a temporary virtual keyboard such as wtype's goes away,
        # Sway's seat has no keyboard. A new client then gets no keymap, and
        # GPUI 0.2.2 unwraps the missing keymap on its first modifiers event.
        # A key event from the driver's own keyboard makes it the seat
        # keyboard again; F24 has no binding in the private desktop or the app.
        if self.prime_keyboard:
            self.call('input', {'actions': [{'action': 'keypress', 'keys': ['F24']}]})
        self.geometry.unlink(missing_ok=True)
        extra = [f'{key}={value}' for key, value in sorted(self.env.items())]
        if self.trace_wayland:
            extra.append('WAYLAND_DEBUG=client')
        number = len(self.exit_files) + 1
        status = self.evidence / f'exit-{number:02d}.json'
        log = self.evidence / f'app-{number:02d}.stderr.log'
        result = self.call('app_launch', {'argv': [
            '/usr/bin/env', '-u', 'KEYSTONE_VISUAL_FIXTURE', f'KEYSTONE_STATE_PATH={self.state}',
            f'KEYSTONE_AUTOMATION_PATH={self.geometry}',
            f'DBUS_SESSION_BUS_ADDRESS={self.bus_address}', *extra,
            sys.executable, '-I', '-c', REAPER, str(status), str(log),
            str(self.binary)]})
        launched = result.get('structuredContent', {})
        deadline = time.monotonic() + 10
        while (record := read_exit(status)) is None:
            if time.monotonic() >= deadline:
                raise RuntimeError(f'Launcher {launched.get("pid")} never reported the app PID')
            time.sleep(0.02)
        if record['pid'] is None:
            raise RuntimeError(f'App failed to start: {record["error"]}')
        self.app_pids.append(record['pid'])
        self.exit_files[record['pid']] = status
        self.app_logs[record['pid']] = log
        self.record('app_pid', record)
        return {**launched, 'pid': record['pid'], 'launcher_pid': record['launcher_pid']}

    def exit_status(self, pid):
        """The app's exit record once it has exited and been reaped, else None."""
        record = read_exit(self.exit_files[pid])
        return record if record and record.get('returncode') is not None else None

    def doctor(self):
        result = {name: self.call(name).get('structuredContent')
                  for name in ('server_info', 'session_status', 'windows_list')}
        identities = []
        for pid in self.app_pids:
            executable = Path(f'/proc/{pid}/exe')
            try:
                actual = str(executable.resolve(strict=True))
                identities.append({'pid': pid, 'exe': actual, 'sha256': digest(executable),
                                   'matches_launch': digest(executable) == self.binary_sha})
            except FileNotFoundError:
                identities.append({'pid': pid, 'exited': True, 'exit': self.exit_status(pid)})
        result['processes'] = identities
        result['binary_sha256_now'] = digest(self.binary)
        result['binary_unchanged'] = result['binary_sha256_now'] == self.binary_sha
        self.record('doctor', result)
        return result

    def window_rects(self, pid):
        """The compositor's windows for `pid`, as {title, rect}. None while the app runs
        with no window open."""
        windows = self.call('windows_list').get('structuredContent', {})
        return [w for w in windows.get('windows', windows if isinstance(windows, list) else [])
                if w.get('pid') == pid]

    def elements(self, window=None):
        """Named elements in desktop pixels, read from the live app's latest frame of each
        of its windows. Each element names its window's title. `window` keeps one title.
        A window the compositor does not list shows no elements."""
        if not self.geometry.exists():
            raise NotReady('The app has not written element geometry yet')
        frame = json.loads(self.geometry.read_text())
        if frame['pid'] not in self.app_pids:
            raise NotReady(f'Geometry is from PID {frame["pid"]}, not a launched app')
        rects = self.window_rects(frame['pid'])
        frames = frame['windows'] if 'windows' in frame else [{'title': None, **frame}]
        if not rects:
            frames = []
        elements = []
        for drawn in frames:
            title = drawn.get('title')
            if window is not None and title != window:
                continue
            matches = [w for w in rects if title and w.get('title') == title]
            if not matches and len(rects) == 1 and len(frames) == 1:
                matches = rects
            if len(matches) != 1:
                raise NotReady(f'No single compositor window titled {title!r}')
            rect = matches[0]['rect']
            ox, oy = rect['x'], rect['y']
            # The first frame can predate the compositor's configure, and its
            # layout then belongs to another size. Clicking it misses. The
            # tolerance covers the window's own inset, 1 px a side under Sway.
            viewport = drawn.get('viewport')
            if viewport and (abs(viewport['w'] - rect['width']) > 8
                             or abs(viewport['h'] - rect['height']) > 8):
                self.record('elements_not_ready', {'revision': frame['revision'], 'window': title,
                                                   'viewport': viewport, 'rect': rect})
                raise NotReady(f'Frame of {title!r} is {viewport["w"]}x{viewport["h"]}, '
                               f'window is {rect["width"]}x{rect["height"]}')

            def place(b):
                return None if b is None else {'x': round(ox + b['x']), 'y': round(oy + b['y']),
                                               'w': round(b['w']), 'h': round(b['h'])}

            for element in drawn['elements']:
                visible = place(element['visible'])
                elements.append({'id': element['id'], 'window': title,
                                 'bounds': place(element['bounds']), 'visible': visible,
                                 'center': None if visible is None else
                                 {'x': visible['x'] + visible['w'] // 2,
                                  'y': visible['y'] + visible['h'] // 2}})
        if (window is not None and not any(d.get('title') == window for d in frames)
                and any(r.get('title') == window for r in rects)):
            raise NotReady(f'No frame from a window titled {window!r}')
        result = {'revision': frame['revision'], 'pid': frame['pid'],
                  'windows': [d.get('title') for d in frames], 'elements': elements}
        self.record('elements', result)
        return result

    def find(self, element_id, window=None):
        matches = [e for e in self.elements(window)['elements'] if e['id'] == element_id]
        if not matches:
            raise RuntimeError(f'No element named {element_id!r} in the current frame')
        if len(matches) > 1:
            raise RuntimeError(f'{len(matches)} elements are named {element_id!r}; pass window')
        if matches[0]['center'] is None:
            raise RuntimeError(f'{element_id!r} is laid out but fully clipped')
        return matches[0]

    def wait_for(self, element_id, present, timeout_ms, window=None):
        deadline = time.monotonic() + timeout_ms / 1000
        while True:
            # Absence counts only from an observed frame, never from a failed read.
            try:
                found = any(e['id'] == element_id and e['visible']
                            for e in self.elements(window)['elements'])
            except NotReady as error:
                found, unready = None, error
            if found == present:
                return {'id': element_id, 'present': found}
            if time.monotonic() >= deadline:
                if found is None:
                    raise TimeoutError(f'No observable frame after {timeout_ms} ms: {unready}')
                raise TimeoutError(f'{element_id!r} present={found} after {timeout_ms} ms')
            time.sleep(0.1)

    def snapshot(self):
        target = self.evidence / f'state-{self.sequence:04d}-{time.time_ns()}.json'
        database = database_path(self.state)
        if database.exists():
            backup = target.with_suffix('.sqlite3')
            deadline = time.monotonic() + 5

            def progress(status, remaining, total):
                if time.monotonic() >= deadline:
                    raise TimeoutError('SQLite snapshot backup exceeded five seconds')

            with closing(open_database(database)) as source, \
                    closing(sqlite3.connect(backup)) as destination:
                source.backup(destination, pages=256, progress=progress, sleep=0.05)
                snapshot = workspace_projection(destination)
            target.write_text(json.dumps(snapshot, indent=2) + '\n')
            result = {'path': str(target), 'sha256': digest(target),
                      'database_path': str(backup), 'database_sha256': digest(backup)}
        else:
            result = {'state_absent': True}
        self.record('state_snapshot', result)
        return result

    def stop(self):
        if self.closed:
            return {'already_closed': True}
        self.closed = True
        errors = []
        try:
            self.call('session_stop')
        except Exception as error:
            errors.append(str(error))
        try:
            snapshot = self.snapshot()
        except Exception as error:
            snapshot = None
            errors.append(str(error))
        self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.stop_bus()
        if not errors:
            try:
                shutil.rmtree(self.scratch)
            except OSError as error:
                errors.append(str(error))
        result = {'snapshot': snapshot, 'evidence': str(self.evidence), 'errors': errors,
                  'scratch_retained': str(self.scratch) if errors else None}
        self.record('cleanup', result)
        self.log.close()
        self.stderr.close()
        if errors:
            raise RuntimeError(json.dumps(result))
        return result


def read_exit(path):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return None


# Variables a run may pass to the app. The state and automation paths stay owned by the run.
APP_ENV = {'KEYSTONE_CODEX_PATH', 'KEYSTONE_CLAUDE_PATH', 'KEYSTONE_PROJECT_DIR',
           'KEYSTONE_FAKE_CODEX_DELAY_MS', 'KEYSTONE_FAKE_CLAUDE_DELAY_MS',
           'KEYSTONE_VISUAL_FIXTURE', 'KEYSTONE_TEST_CLAUDE_IDLE_MS', 'KEYSTONE_TEST_LOG_BUDGET_BYTES', 'VISUAL', 'EDITOR',
           'KEYSTONE_FAKE_MCP_CONFIG', 'KEYSTONE_FAKE_IMAGE_RECEIPT', 'KEYSTONE_ATTACHMENT_MAX_MB',
           'KEYSTONE_ATTACHMENT_KEEP_DAYS', 'CLAUDE_CONFIG_DIR', 'CODEX_HOME', 'HOME', 'PATH',
           'GH_CONFIG_DIR'}


def app_env(env):
    env = env or {}
    unknown = sorted(set(env) - APP_ENV)
    if unknown:
        raise ValueError(f'env may only set {sorted(APP_ENV)}; got {unknown}')
    return {key: str(value) for key, value in env.items()}


def schema(properties=None, required=()):
    return {'type': 'object', 'properties': properties or {}, 'required': list(required),
            'additionalProperties': False}


ID = {'type': 'string', 'description': 'Element name, as listed by `elements`'}
WINDOW = {'type': 'string', 'description': 'Window title, such as "Keystone 2", when several '
          'windows show an element of that name'}
TOOLS = [
    ('start', 'Build nothing; launch the --binary build (default target/debug/keystone-desktop) on a new '
     'private desktop with an '
     'empty disposable workspace and a new evidence directory. One run at a time.',
     schema({'width': {'type': 'integer', 'default': 1280}, 'height': {'type': 'integer', 'default': 900},
             'headless': {'type': 'boolean', 'default': True,
                          'description': 'false shows the desktop as a host window'},
             'env': {'type': 'object', 'additionalProperties': {'type': 'string'},
                     'description': 'App variables for this run and its relaunches: '
                                    + ', '.join(sorted(APP_ENV))}})),
    ('doctor', 'Read desktop, window, and process identity. Require matches_launch and binary_unchanged.',
     schema()),
    ('screenshot', 'Capture the whole private desktop as a PNG and save it as evidence.', schema()),
    ('elements', 'List named elements from the app\'s latest frame in desktop pixels. `visible` is the '
     'unclipped part; menus that overlap an element are not subtracted. Unnamed controls need a screenshot. '
     'Each element names its window; pass window to list one.',
     schema({'window': WINDOW})),
    ('click', 'Left-click the center of a named element, or explicit desktop pixels.',
     schema({'id': ID, 'window': WINDOW, 'x': {'type': 'integer'}, 'y': {'type': 'integer'},
             'button': {'type': 'string', 'enum': ['left', 'right', 'middle'], 'default': 'left'},
             'count': {'type': 'integer', 'default': 1}})),
    ('input', 'Pass ordered actions to the driver: keypress {keys}, type {text}, wait {duration_ms}, '
     'scroll (signed axis distances; 15 units per wheel notch), drag, move. '
     'Re-observe after state changes rather than batching across them.',
     schema({'actions': {'type': 'array', 'items': {'type': 'object'}}}, ['actions'])),
    ('wheel', 'Scroll a named element or desktop pixels by signed whole wheel notches. '
     'Positive vertical scrolls down; positive horizontal scrolls right. Requires the wheel driver patch.',
     schema({'id': ID, 'window': WINDOW, 'x': {'type': 'integer'}, 'y': {'type': 'integer'},
             'horizontal': {'type': 'integer', 'minimum': -100, 'maximum': 100, 'default': 0},
             'vertical': {'type': 'integer', 'minimum': -100, 'maximum': 100, 'default': 0}})),
    ('wait_for', 'Wait until a named element is visible, or gone with present=false.',
     schema({'id': ID, 'window': WINDOW, 'present': {'type': 'boolean', 'default': True},
             'timeout_ms': {'type': 'integer', 'default': 5000}}, ['id'])),
    ('workspace', 'Read saved SQLite workspace projections in one read-only transaction.', schema()),
    ('snapshot', 'Back up SQLite including WAL commits and export its workspace JSON into evidence.',
     schema()),
    ('launch', 'Relaunch the app against the same workspace after it has quit. Never while it runs.',
     schema()),
    ('call', 'Call any computer-use-mcp tool directly, such as windows_list.',
     schema({'tool': {'type': 'string'}, 'arguments': {'type': 'object'}}, ['tool'])),
    ('stop', 'Stop the desktop and app, snapshot state, and remove the scratch directory. '
     'Keeps evidence. Call it even after failures.', schema()),
]


class Server:
    def __init__(self, args):
        self.args = args
        self.run = None
        self.failures = 0

    def require(self):
        if self.run is None or self.run.closed:
            raise RuntimeError('No active run; call start first')
        return self.run

    def handle(self, name, arguments):
        if name == 'start':
            if self.run is not None and not self.run.closed:
                raise RuntimeError('A run is active; stop it first')
            evidence = ROOT / '.audit' / f'verify-{time.strftime("%Y%m%dT%H%M%S")}-{os.getpid()}'
            self.run = Run(self.args.driver, ROOT, evidence, self.args.binary,
                           self.args.trace_wayland)
            started = self.run.start(arguments.get('width', 1280), arguments.get('height', 900),
                                     arguments.get('headless', True), arguments.get('env'))
            return {**started, 'evidence': str(evidence), 'state': str(self.run.state),
                    'database': str(database_path(self.run.state))}, None
        run = self.require()
        if name == 'doctor':
            return run.doctor(), None
        if name == 'screenshot':
            result = run.call('screenshot')
            image = next(c for c in result['content'] if c.get('type') == 'image')
            saved = next(c for c in result['_evidence']['content'] if c.get('type') == 'image')
            return {**result.get('structuredContent', {}), 'path': saved['path']}, image
        if name == 'elements':
            return run.elements(arguments.get('window')), None
        if name == 'click':
            if 'id' in arguments:
                target = run.find(arguments['id'], arguments.get('window'))['center']
            elif 'x' in arguments and 'y' in arguments:
                target = {'x': arguments['x'], 'y': arguments['y']}
            else:
                raise ValueError('Pass id, or both x and y')
            action = {'action': 'click', 'x': target['x'], 'y': target['y'],
                      'button': arguments.get('button', 'left'), 'count': arguments.get('count', 1)}
            run.call('input', {'actions': [action]})
            return {'clicked': arguments.get('id'), **target}, None
        if name == 'input':
            return run.call('input', {'actions': arguments['actions']}).get('structuredContent'), None
        if name == 'wheel':
            horizontal, vertical = arguments.get('horizontal', 0), arguments.get('vertical', 0)
            if any(type(value) is not int or not -100 <= value <= 100
                   for value in (horizontal, vertical)):
                raise ValueError('Wheel notches must be integers within +/-100')
            if 'id' in arguments:
                target = run.find(arguments['id'], arguments.get('window'))['center']
            elif 'x' in arguments and 'y' in arguments:
                target = {'x': arguments['x'], 'y': arguments['y']}
            else:
                raise ValueError('Pass id, or both x and y')
            action = {'action': 'scroll', **target, 'scroll_x': horizontal * 15,
                      'scroll_y': vertical * 15}
            return run.call('input', {'actions': [action]}).get('structuredContent'), None
        if name == 'wait_for':
            return run.wait_for(arguments['id'], arguments.get('present', True),
                                arguments.get('timeout_ms', 5000), arguments.get('window')), None
        if name == 'workspace':
            result = read_workspace(run.state)
            run.record('workspace', result)
            return result, None
        if name == 'snapshot':
            return run.snapshot(), None
        if name == 'launch':
            return run.launch(), None
        if name == 'call':
            result = run.call(arguments['tool'], arguments.get('arguments', {}))
            images = [c for c in result.get('content', []) if c.get('type') == 'image']
            return result['_evidence'], images[0] if images else None
        if name == 'stop':
            return run.stop(), None
        raise ValueError(f'Unknown tool {name}')

    def respond(self, message):
        method, params = message.get('method'), message.get('params', {})
        if method == 'initialize':
            return {'protocolVersion': PROTOCOL, 'capabilities': {'tools': {}},
                    'serverInfo': {'name': 'keystone-verify', 'version': '2'},
                    'instructions': 'Read crates/keystone-desktop/.agents/skills/verify/SKILL.md first.'}
        if method == 'ping':
            return {}
        if method == 'tools/list':
            return {'tools': [{'name': n, 'description': d, 'inputSchema': s} for n, d, s in TOOLS]}
        if method == 'tools/call':
            try:
                value, image = self.handle(params['name'], params.get('arguments') or {})
                content = [{'type': 'text', 'text': json.dumps(value)}]
                if image:
                    content.append({k: image[k] for k in ('type', 'data', 'mimeType')})
                return {'content': content, 'isError': False}
            except Exception as error:
                self.failures += 1
                return {'content': [{'type': 'text', 'text': f'{type(error).__name__}: {error}'}],
                        'isError': True}
        raise LookupError(method)

    def reply(self, message):
        try:
            message = json.loads(message)
        except json.JSONDecodeError as error:
            return {'jsonrpc': '2.0', 'id': None, 'error': {'code': -32700, 'message': str(error)}}
        if not isinstance(message, dict) or 'method' not in message:
            return {'jsonrpc': '2.0', 'id': message.get('id') if isinstance(message, dict) else None,
                    'error': {'code': -32600, 'message': 'Invalid request'}}
        if 'id' not in message:
            return None
        try:
            return {'jsonrpc': '2.0', 'id': message['id'], 'result': self.respond(message)}
        except LookupError as error:
            return {'jsonrpc': '2.0', 'id': message['id'],
                    'error': {'code': -32601, 'message': f'Unknown method {error}'}}
        except Exception as error:
            return {'jsonrpc': '2.0', 'id': message['id'],
                    'error': {'code': -32602, 'message': f'{type(error).__name__}: {error}'}}

    def serve(self):
        try:
            for line in sys.stdin:
                if line.strip() and (reply := self.reply(line)) is not None:
                    sys.stdout.write(json.dumps(reply) + '\n')
                    sys.stdout.flush()
        finally:
            self.cleanup()

    def cleanup(self):
        if self.run is not None and not self.run.closed:
            try:
                self.run.stop()
            except Exception as error:
                print(f'cleanup failed: {error}', file=sys.stderr)
                self.failures += 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--driver', default=str(default_driver()))
    parser.add_argument('--trace-wayland', action='store_true',
                        help='Record native driver requests and app Wayland events in evidence stderr logs')
    add_binary_argument(parser)
    args = parser.parse_args()
    server = Server(args)
    server.serve()
    return int(server.failures > 0)


if __name__ == '__main__':
    sys.exit(main())
