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
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
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


def timestamp():
    return time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())


def digest(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


class Run:
    def __init__(self, driver, root, evidence):
        self.env = {}
        self.prime_keyboard = True
        self.binary = Path(root).resolve() / 'target/debug/keystone-desktop'
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
        self.responses = queue.Queue()
        self.log = (self.evidence / 'actions.jsonl').open('a')
        self.stderr = (self.evidence / 'driver.stderr.log').open('w')
        try:
            self.process = subprocess.Popen([str(Path(driver).resolve())], stdin=subprocess.PIPE,
                                            stdout=subprocess.PIPE, stderr=self.stderr, text=True,
                                            bufsize=1, cwd=root)
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
                                'driver_pid': self.process.pid})

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
        self.env = app_env(env)
        return {'session': session.get('structuredContent'), 'launch': self.launch()}

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
        number = len(self.exit_files) + 1
        status = self.evidence / f'exit-{number:02d}.json'
        log = self.evidence / f'app-{number:02d}.stderr.log'
        result = self.call('app_launch', {'argv': [
            '/usr/bin/env', '-u', 'KEYSTONE_VISUAL_FIXTURE', f'KEYSTONE_STATE_PATH={self.state}',
            f'KEYSTONE_AUTOMATION_PATH={self.geometry}', *extra,
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

    def window_rect(self, pid):
        windows = self.call('windows_list').get('structuredContent', {})
        for window in windows.get('windows', windows if isinstance(windows, list) else []):
            if window.get('pid') == pid:
                return window['rect']
        raise RuntimeError(f'No window for app PID {pid}: {json.dumps(windows)}')

    def elements(self):
        """Named elements in desktop pixels, read from the live app's latest frame."""
        if not self.geometry.exists():
            raise NotReady('The app has not written element geometry yet')
        frame = json.loads(self.geometry.read_text())
        if frame['pid'] not in self.app_pids:
            raise NotReady(f'Geometry is from PID {frame["pid"]}, not a launched app')
        rect = self.window_rect(frame['pid'])
        ox, oy = rect['x'], rect['y']
        # The first frame can predate the compositor's configure, and its
        # layout then belongs to another size. Clicking it misses. The
        # tolerance covers the window's own inset, 1 px a side under Sway.
        viewport = frame.get('viewport')
        if viewport and (abs(viewport['w'] - rect['width']) > 8
                         or abs(viewport['h'] - rect['height']) > 8):
            self.record('elements_not_ready', {'revision': frame['revision'],
                                               'viewport': viewport, 'window': rect})
            raise NotReady(f'Frame is {viewport["w"]}x{viewport["h"]}, '
                           f'window is {rect["width"]}x{rect["height"]}')

        def place(b):
            return None if b is None else {'x': round(ox + b['x']), 'y': round(oy + b['y']),
                                           'w': round(b['w']), 'h': round(b['h'])}

        elements = []
        for element in frame['elements']:
            visible = place(element['visible'])
            elements.append({'id': element['id'], 'bounds': place(element['bounds']),
                             'visible': visible,
                             'center': None if visible is None else
                             {'x': visible['x'] + visible['w'] // 2,
                              'y': visible['y'] + visible['h'] // 2}})
        result = {'revision': frame['revision'], 'pid': frame['pid'], 'elements': elements}
        self.record('elements', result)
        return result

    def find(self, element_id):
        matches = [e for e in self.elements()['elements'] if e['id'] == element_id]
        if not matches:
            raise RuntimeError(f'No element named {element_id!r} in the current frame')
        if len(matches) > 1:
            raise RuntimeError(f'{len(matches)} elements are named {element_id!r}')
        if matches[0]['center'] is None:
            raise RuntimeError(f'{element_id!r} is laid out but fully clipped')
        return matches[0]

    def wait_for(self, element_id, present, timeout_ms):
        deadline = time.monotonic() + timeout_ms / 1000
        while True:
            # Absence counts only from an observed frame, never from a failed read.
            try:
                found = any(e['id'] == element_id and e['visible']
                            for e in self.elements()['elements'])
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
        if self.state.exists():
            shutil.copyfile(self.state, target)
            result = {'path': str(target), 'sha256': digest(target)}
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
        if not errors:
            shutil.rmtree(self.scratch)
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
           'KEYSTONE_FAKE_CODEX_DELAY_MS', 'KEYSTONE_FAKE_CLAUDE_DELAY_MS'}


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
TOOLS = [
    ('start', 'Build nothing; launch target/debug/keystone-desktop on a new private desktop with an '
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
     'unclipped part; menus that overlap an element are not subtracted. Unnamed controls need a screenshot.',
     schema()),
    ('click', 'Left-click the center of a named element, or explicit desktop pixels.',
     schema({'id': ID, 'x': {'type': 'integer'}, 'y': {'type': 'integer'},
             'button': {'type': 'string', 'enum': ['left', 'right', 'middle'], 'default': 'left'},
             'count': {'type': 'integer', 'default': 1}})),
    ('input', 'Pass ordered actions to the driver: keypress {keys}, type {text}, wait {duration_ms}, '
     'scroll, drag, move. Re-observe after state changes rather than batching across them.',
     schema({'actions': {'type': 'array', 'items': {'type': 'object'}}}, ['actions'])),
    ('wait_for', 'Wait until a named element is visible, or gone with present=false.',
     schema({'id': ID, 'present': {'type': 'boolean', 'default': True},
             'timeout_ms': {'type': 'integer', 'default': 5000}}, ['id'])),
    ('workspace', 'Read the real saved workspace file.', schema()),
    ('snapshot', 'Copy the saved workspace file into the evidence directory.', schema()),
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
            self.run = Run(self.args.driver, ROOT, evidence)
            started = self.run.start(arguments.get('width', 1280), arguments.get('height', 900),
                                     arguments.get('headless', True), arguments.get('env'))
            return {**started, 'evidence': str(evidence), 'state': str(self.run.state)}, None
        run = self.require()
        if name == 'doctor':
            return run.doctor(), None
        if name == 'screenshot':
            result = run.call('screenshot')
            image = next(c for c in result['content'] if c.get('type') == 'image')
            saved = next(c for c in result['_evidence']['content'] if c.get('type') == 'image')
            return {**result.get('structuredContent', {}), 'path': saved['path']}, image
        if name == 'elements':
            return run.elements(), None
        if name == 'click':
            if 'id' in arguments:
                target = run.find(arguments['id'])['center']
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
        if name == 'wait_for':
            return run.wait_for(arguments['id'], arguments.get('present', True),
                                arguments.get('timeout_ms', 5000)), None
        if name == 'workspace':
            result = json.loads(run.state.read_text()) if run.state.exists() else {'state_absent': True}
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
    parser.add_argument('--driver', default=str(ROOT / '.audit/tools/computer-use-mcp'))
    args = parser.parse_args()
    server = Server(args)
    server.serve()
    return int(server.failures > 0)


if __name__ == '__main__':
    sys.exit(main())
