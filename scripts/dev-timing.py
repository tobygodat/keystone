#!/usr/bin/env python3
"""Time development commands and keep one JSON record per run.

  python3 scripts/dev-timing.py run --label NAME [--cache-state STATE] -- COMMAND...
  python3 scripts/dev-timing.py summary

Records go to $KEYSTONE_DEV_TIMING_DIR, else <git common dir>/dev-timings.
"""
import argparse
import datetime
import json
import math
import os
import platform
import shlex
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

VERSION = 1
CACHE_STATES = ('unknown', 'warm', 'cold', 'touched')
FORWARDED = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
FIELDS = {
    'label': (str,), 'argv': (list,), 'head': (str, type(None)),
    'dirty': (bool, type(None)), 'host': (str,), 'cpu_count': (int, type(None)),
    'rustc': (str, type(None)), 'env': (dict,), 'cache_state': (str,),
    'wall_s': (int, float), 'load_before': (int, float), 'load_after': (int, float),
    'exit_code': (int, type(None)), 'signal': (int, type(None)),
    'launch_error': (str, type(None)),
}
BUILD_ENV = ('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_JOBS', 'RUSTFLAGS',
             'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS', 'RUSTC_WRAPPER',
             'RUSTC_WORKSPACE_WRAPPER', 'CARGO_INCREMENTAL', 'RUST_TEST_THREADS')


def warn(message):
    print(f'dev-timing: {message}', file=sys.stderr, flush=True)


def output(argv):
    try:
        done = subprocess.run(argv, capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return done.stdout.strip() if done.returncode == 0 else None


def cpu_model():
    try:
        for line in Path('/proc/cpuinfo').read_text().splitlines():
            if line.startswith('model name'):
                return line.split(':', 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or None


def git_context():
    head = output(['git', 'rev-parse', 'HEAD'])
    status = output(['git', 'status', '--porcelain'])
    return {
        'worktree': output(['git', 'rev-parse', '--show-toplevel']),
        'head': head,
        'dirty': None if head is None or status is None else bool(status),
    }


def record_dir():
    if os.environ.get('KEYSTONE_DEV_TIMING_DIR'):
        return Path(os.environ['KEYSTONE_DEV_TIMING_DIR'])
    common = output(['git', 'rev-parse', '--git-common-dir'])
    if common is None:
        raise OSError('not in a git checkout and KEYSTONE_DEV_TIMING_DIR is unset')
    return Path.cwd() / common / 'dev-timings'


def new_record(label, argv, cache_state):
    return {
        'version': VERSION,
        'label': label,
        'argv': argv,
        'cwd': os.getcwd(),
        **git_context(),
        'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='seconds'),
        'host': platform.node(),
        'cpu': cpu_model(),
        'cpu_count': os.cpu_count(),
        'kernel': platform.release(),
        'rustc': output(['rustc', '-V']),
        'env': {name: os.environ[name] for name in BUILD_ENV if name in os.environ},
        'cache_state': cache_state,
        'load_before': os.getloadavg()[0],
    }


def publish(record):
    directory = record_dir()
    directory.mkdir(parents=True, exist_ok=True)
    fd, temp = tempfile.mkstemp(dir=directory, prefix='.', suffix='.tmp')
    try:
        with os.fdopen(fd, 'w') as file:
            json.dump(record, file, indent=1)
            file.write('\n')
        stamp = record['started_utc'].replace(':', '').replace('+0000', 'Z')
        path = directory / f'{stamp}-{uuid.uuid4().hex[:12]}.json'
        os.replace(temp, path)
    except BaseException:
        os.unlink(temp)
        raise


def run(label, argv, cache_state):
    try:
        record = new_record(label, argv, cache_state)
    except Exception as error:
        warn(f'metadata failed, this run will not be recorded: {error}')
        record = None

    pending, child = [], None

    def forward(signum, _frame):
        if child is None:
            pending.append(signum)
        else:
            try:
                os.killpg(child.pid, signum)
            except ProcessLookupError:
                pass

    for signum in FORWARDED:
        signal.signal(signum, forward)
    start = time.monotonic()
    try:
        child = subprocess.Popen(argv, start_new_session=True)
    except OSError as error:
        wall, code, signum, launch_error = time.monotonic() - start, None, None, str(error)
        warn(f'cannot run {argv[0]}: {error.strerror}')
        status = 126 if isinstance(error, PermissionError) else 127
    else:
        for early in pending:
            forward(early, None)
        returncode = child.wait()
        wall, launch_error = time.monotonic() - start, None
        code, signum = (returncode, None) if returncode >= 0 else (None, -returncode)
        status = returncode if returncode >= 0 else 128 - returncode

    if record is not None:
        try:
            record.update(load_after=os.getloadavg()[0], wall_s=round(wall, 3), exit_code=code,
                          signal=signum, launch_error=launch_error)
            publish(record)
        except Exception as error:
            warn(f'could not save timing record: {error}')

    if signum is not None:
        signal.signal(signum, signal.SIG_DFL)
        os.kill(os.getpid(), signum)
    return status


def load_records(directory):
    records = []
    for path in sorted(directory.glob('*.json')):
        try:
            record = json.loads(path.read_text())
            if record.get('version') != VERSION:
                raise ValueError(f"unsupported version {record.get('version')!r}")
            for name, types in FIELDS.items():
                if name not in record or type(record[name]) not in types:
                    raise ValueError(f'invalid or missing {name}')
            if not record['argv'] or not all(isinstance(arg, str) for arg in record['argv']):
                raise ValueError('argv must contain command strings')
            if not all(isinstance(k, str) and isinstance(v, str)
                       for k, v in record['env'].items()):
                raise ValueError('env must contain string keys and values')
            for name in ('wall_s', 'load_before', 'load_after'):
                if not math.isfinite(record[name]) or record[name] < 0:
                    raise ValueError(f'invalid {name}')
            records.append(record)
        except (OSError, ValueError, AttributeError) as error:
            warn(f'skipping {path.name}: {error}')
    return records


def group_key(record):
    state = '+state-unknown' if record['dirty'] is None else '+dirty' if record['dirty'] else ''
    revision = f"{record['head'] or 'no-git'}{state}"
    return (
        record['label'],
        shlex.join(record['argv']),
        revision,
        f"{record['host']} ({record['cpu_count']} cpu)",
        record['rustc'] or 'rustc unknown',
        ' '.join(f'{k}={v}' for k, v in sorted(record['env'].items())) or '(default build env)',
        record['cache_state'],
    )


def summarize(records):
    groups = {}
    for record in records:
        groups.setdefault(group_key(record), []).append(record)
    lines = []
    for key in sorted(groups):
        label, command, revision, host, rustc, env, cache_state = key
        runs = groups[key]
        ok = [r['wall_s'] for r in runs if r['exit_code'] == 0]
        failed = [r for r in runs if r['exit_code'] != 0]
        lines += [f'{label}  cache={cache_state}  {revision}',
                  f'  {command}', f'  {host}  {rustc}', f'  {env}']
        line = f'  runs {len(runs)}  ok {len(ok)}  failed {len(failed)}'
        if ok:
            line += (f'  ok wall median {statistics.median(ok):.2f}s'
                     f'  min {min(ok):.2f}s  max {max(ok):.2f}s')
            loads = [r[key] for r in runs if r['exit_code'] == 0
                     for key in ('load_before', 'load_after')]
            line += f'  load1 {min(loads):.2f} to {max(loads):.2f}'
        lines.append(line)
        if failed:
            outcomes = ', '.join(failure(r) for r in failed)
            lines.append(f'  failures (not timings): {outcomes}')
        lines.append('')
    return '\n'.join(lines) or 'No timing records.\n'


def failure(record):
    if record['launch_error']:
        return 'launch failed'
    if record['signal']:
        return f"signal {record['signal']} after {record['wall_s']:.2f}s"
    return f"exit {record['exit_code']} after {record['wall_s']:.2f}s"


def main(argv):
    command = []
    if '--' in argv:
        command = argv[argv.index('--') + 1:]
        argv = argv[:argv.index('--')]
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='action', required=True)
    run_parser = sub.add_parser('run', help='run COMMAND after -- and record its wall time')
    run_parser.add_argument('--label', required=True)
    run_parser.add_argument('--cache-state', choices=CACHE_STATES)
    sub.add_parser('summary', help='compare recorded runs')
    args = parser.parse_args(argv)

    if args.action == 'summary':
        try:
            directory = record_dir()
        except OSError as error:
            parser.error(str(error))
        print(summarize(load_records(directory)), end='')
        return 0
    if not command:
        parser.error('run needs a command after --')
    cache_state = args.cache_state or os.environ.get('KEYSTONE_CACHE_STATE') or 'unknown'
    if cache_state not in CACHE_STATES:
        warn(f'ignoring KEYSTONE_CACHE_STATE={cache_state!r}; expected one of {CACHE_STATES}')
        cache_state = 'unknown'
    return run(args.label, command, cache_state)


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
