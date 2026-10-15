#!/usr/bin/env python3
"""Own one nested desktop and accept newline-delimited JSON commands on stdin."""
import argparse
import base64
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


def timestamp():
    return time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())


def digest(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


class Run:
    def __init__(self, args):
        self.binary = Path(args.root).resolve() / 'target/debug/keystone-desktop'
        self.binary_sha = digest(self.binary)
        driver_sha = digest(args.driver)
        self.evidence = Path(args.evidence).resolve()
        self.evidence.mkdir(parents=True, exist_ok=False)
        self.scratch = Path(tempfile.mkdtemp(prefix='keystone-verify-'))
        self.state = self.scratch / 'workspace.json'
        self.sequence = 0
        self.images = 0
        self.app_pids = []
        self.closed = False
        self.responses = queue.Queue()
        self.log = (self.evidence / 'actions.jsonl').open('a')
        self.stderr = (self.evidence / 'driver.stderr.log').open('w')
        try:
            self.process = subprocess.Popen([str(Path(args.driver).resolve())], stdin=subprocess.PIPE,
                                            stdout=subprocess.PIPE, stderr=self.stderr, text=True,
                                            bufsize=1, cwd=args.root)
        except Exception as error:
            shutil.rmtree(self.scratch)
            self.record('cleanup', {'startup_error': str(error), 'scratch_retained': None,
                                    'errors': [], 'evidence': str(self.evidence)})
            self.log.close()
            self.stderr.close()
            raise
        threading.Thread(target=self.read_responses, daemon=True).start()
        self.record('identity', {'binary': str(self.binary), 'binary_sha256': self.binary_sha,
                                'driver': str(Path(args.driver).resolve()),
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
        if isinstance(value, dict):
            if value.get('type') == 'image' and 'data' in value:
                self.images += 1
                extension = '.png' if value.get('mimeType') == 'image/png' else '.bin'
                path = self.evidence / f'{self.images:04d}{extension}'
                path.write_bytes(base64.b64decode(value['data'], validate=True))
                return {**{k: v for k, v in value.items() if k != 'data'},
                        'path': str(path), 'sha256': digest(path)}
            return {key: self.sanitize(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.sanitize(item) for item in value]
        return value

    def request(self, method, params=None):
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
            response = self.sanitize(response)
            self.record('response', response)
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
                raise RuntimeError(json.dumps(result))
            return result

    def call(self, tool, arguments=None):
        return self.request('tools/call', {'name': tool, 'arguments': arguments or {}})

    def start(self, args):
        self.request('initialize', {'protocolVersion': '2025-03-26', 'capabilities': {},
                                   'clientInfo': {'name': 'keystone-verifier', 'version': '1'}})
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
        self.process.stdin.flush()
        self.call('session_start', {'width': args.width, 'height': args.height,
                                    'xwayland': True, 'sandbox': False})
        return self.launch()

    def launch(self):
        result = self.call('app_launch', {'argv': ['/usr/bin/env', '-u', 'KEYSTONE_VISUAL_FIXTURE',
                                                   f'KEYSTONE_STATE_PATH={self.state}', str(self.binary)]})
        pid = result.get('structuredContent', {}).get('pid')
        if pid:
            self.app_pids.append(pid)
        return result

    def doctor(self):
        result = {name: self.call(name) for name in ('server_info', 'session_status', 'windows_list')}
        identities = []
        for pid in self.app_pids:
            executable = Path(f'/proc/{pid}/exe')
            try:
                actual = str(executable.resolve(strict=True))
                identities.append({'pid': pid, 'exe': actual, 'sha256': digest(executable),
                                   'matches_launch': digest(executable) == self.binary_sha})
            except FileNotFoundError:
                identities.append({'pid': pid, 'exited': True})
        result['processes'] = identities
        result['binary_sha256_now'] = digest(self.binary)
        result['binary_unchanged'] = result['binary_sha256_now'] == self.binary_sha
        self.record('doctor', result)
        return result

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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--driver', required=True)
    parser.add_argument('--root', required=True)
    parser.add_argument('--evidence', required=True, help='New directory; refuses an existing directory')
    parser.add_argument('--width', type=int, default=1280)
    parser.add_argument('--height', type=int, default=900)
    args = parser.parse_args()
    run = None
    failed = False
    try:
        run = Run(args)
        print(json.dumps({'ok': True, 'started': run.start(args), 'state': str(run.state)}), flush=True)
        for line in sys.stdin:
            try:
                command = json.loads(line)
                kind = command['kind']
                if kind == 'call':
                    result = run.call(command['tool'], command.get('arguments', {}))
                elif kind == 'doctor':
                    result = run.doctor()
                elif kind == 'workspace':
                    result = json.loads(run.state.read_text()) if run.state.exists() else {'state_absent': True}
                    run.record('workspace', result)
                elif kind == 'snapshot':
                    result = run.snapshot()
                elif kind == 'launch':
                    result = run.launch()
                elif kind == 'stop':
                    result = run.stop()
                else:
                    raise ValueError(f'Unknown command kind {kind}')
                print(json.dumps({'ok': True, 'result': result}), flush=True)
                if kind == 'stop':
                    break
            except Exception as error:
                failed = True
                print(json.dumps({'ok': False, 'error': str(error)}), flush=True)
    except Exception as error:
        failed = True
        print(json.dumps({'ok': False, 'error': str(error)}), flush=True)
    finally:
        if run and not run.closed:
            try:
                print(json.dumps({'ok': True, 'cleanup': run.stop()}), flush=True)
            except Exception as error:
                failed = True
                print(json.dumps({'ok': False, 'cleanup_error': str(error)}), flush=True)
    return int(failed)


if __name__ == '__main__':
    sys.exit(main())
