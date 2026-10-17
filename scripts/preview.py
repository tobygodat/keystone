#!/usr/bin/env python3
"""Follow a Keystone worktree, rebuilding and restarting the app when it changes.

  python3 scripts/preview.py [TARGET] [--once]

From another machine, show the window there with waypipe:

  waypipe ssh HOST ~/keystone/scripts/preview.py

Without TARGET it follows whichever worktree of this repository changed most
recently, uncommitted edits included. TARGET pins a branch name or worktree path.
"""
import argparse
import ctypes
import os
import shutil
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
HOME = Path(os.environ.get('KEYSTONE_PREVIEW_HOME') or Path.home() / '.cache/keystone-preview')
TARGET_DIR = HOME / 'target'
BINARY = HOME / 'keystone-desktop'
STATE = Path.home() / '.local/share/keystone-preview/workspace.json'
CARGO = ['cargo', 'build', '-p', 'keystone-desktop', '--locked']
STOP_TIMEOUT_S = 5
LIBC = ctypes.CDLL(None, use_errno=True)
PR_SET_PDEATHSIG = 1


@dataclass(frozen=True)
class Snapshot:
    path: str
    branch: str | None
    head: str
    committed: float
    status: str
    mtimes: tuple


def activity(snap):
    return max([snap.committed, *(mtime for _, mtime in snap.mtimes)])


def fingerprint(snap):
    return (snap.path, snap.head, snap.status, snap.mtimes)


def most_active(snaps):
    return max(snaps, key=activity, default=None)


def matches(worktree, target):
    path, branch = worktree
    return target == branch or os.path.realpath(target) == os.path.realpath(path)


def parse_worktrees(porcelain):
    found = []
    for block in porcelain.strip().split('\n\n'):
        fields = dict(line.partition(' ')[::2] for line in block.splitlines())
        if 'worktree' in fields and 'bare' not in fields:
            found.append((fields['worktree'], fields.get('branch', '').removeprefix('refs/heads/') or None))
    return found


def dirty_paths(status_z):
    paths, tokens = [], iter(status_z.split('\0'))
    for token in tokens:
        if token:
            paths.append(token[3:])
            if token[0] in 'RC':
                next(tokens, None)
    return paths


def error_tail(log, limit=15):
    lines = log.splitlines()
    first = next((i for i, line in enumerate(lines) if line.startswith('error')), max(len(lines) - limit, 0))
    return lines[first:first + limit]


@dataclass(frozen=True)
class Waiting:
    done: tuple


@dataclass(frozen=True)
class Settling:
    fp: tuple
    since: float


@dataclass(frozen=True)
class Building:
    fp: tuple
    started: float


def advance(state, fp, now, quiet_s):
    if isinstance(state, Waiting):
        return state if fp == state.done else Settling(fp, now)
    if isinstance(state, Settling):
        if fp != state.fp:
            return Settling(fp, now)
        return Building(fp, now) if now - state.since >= quiet_s else state
    return state


def say(message):
    print(f'{time.strftime("%H:%M:%S")} {message}', flush=True)


def git(path, *args):
    return subprocess.run(['git', '-C', str(path), *args], capture_output=True, text=True, check=True).stdout


def snapshot(worktree):
    path, branch = worktree
    try:
        head, committed = git(path, 'log', '-1', '--format=%H %ct').split()
        status = git(path, 'status', '--porcelain', '-z', '--untracked-files=all')
    except (subprocess.CalledProcessError, ValueError):
        return None
    mtimes = []
    for rel in dirty_paths(status):
        try:
            mtimes.append((rel, os.stat(os.path.join(path, rel)).st_mtime))
        except OSError:
            pass
    return Snapshot(path, branch, head, float(committed), status, tuple(mtimes))


def locate(repo, target):
    worktrees = parse_worktrees(git(repo, 'worktree', 'list', '--porcelain'))
    if target is not None:
        worktrees = [w for w in worktrees if matches(w, target)]
    return most_active([s for s in map(snapshot, worktrees) if s])


def die_with_parent():
    LIBC.prctl(PR_SET_PDEATHSIG, signal.SIGTERM)


class Group:
    """A child in its own process group, so stopping it also stops what it spawned."""
    proc = None

    def spawn(self, argv, cwd, env, log):
        with open(log, 'w') as out:
            self.proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=out,
                                         stderr=subprocess.STDOUT, start_new_session=True,
                                         preexec_fn=die_with_parent)

    def poll(self):
        return None if self.proc is None else self.proc.poll()

    def stop(self):
        if self.proc is None:
            return
        try:
            os.killpg(self.proc.pid, signal.SIGTERM)
            self.proc.wait(STOP_TIMEOUT_S)
        except ProcessLookupError:
            pass
        except subprocess.TimeoutExpired:
            os.killpg(self.proc.pid, signal.SIGKILL)
            self.proc.wait()
        self.proc = None


def start_build(build, path):
    custom = os.environ.get('KEYSTONE_PREVIEW_BUILD_CMD')
    env = {**os.environ, 'CARGO_TARGET_DIR': str(TARGET_DIR),
           'PATH': f'{Path.home() / ".cargo/bin"}:{os.environ.get("PATH", "")}'}
    build.spawn(['nice', '-n', '19', *(['sh', '-c', custom] if custom else CARGO)], path, env, HOME / 'build.log')


def restart(app, path):
    staged = BINARY.with_suffix('.new')
    shutil.copy2(TARGET_DIR / 'debug/keystone-desktop', staged)
    os.replace(staged, BINARY)
    app.stop()
    STATE.parent.mkdir(parents=True, exist_ok=True)
    app.spawn([str(BINARY)], path, {**os.environ, 'KEYSTONE_STATE_PATH': str(STATE)}, HOME / 'app.log')
    return app.proc.pid


def label(snap):
    return f'{snap.path} ({snap.branch or "detached"})'


def finish(state, code, app):
    path = state.fp[0]
    if code == 0:
        say(f'built in {time.monotonic() - state.started:.0f}s')
        say(f'{"started" if app.proc is None else "restarted"} app (pid {restart(app, path)})')
    else:
        log = (HOME / 'build.log').read_text(errors='replace')
        for line in error_tail(log):
            print(f'    {line}', flush=True)
        say('build failed, keeping last good build' if app.proc else 'build failed, no app running')
    return Waiting(state.fp)


def run(args, app, build):
    HOME.mkdir(parents=True, exist_ok=True)
    followed, state = None, None
    while True:
        snap = locate(REPO, args.target)
        if snap is None:
            say(f'no worktree of {REPO} matches {args.target}' if args.target else f'no worktree in {REPO}')
            return 2
        if followed is None:
            say(f'following {label(snap)}')
        elif snap.path != followed:
            say(f'switching to {label(snap)}, the most recently changed worktree')
        followed, fp, now = snap.path, fingerprint(snap), time.monotonic()

        if state is None:
            state = Building(fp, now)
        elif isinstance(state, Building):
            code = build.poll()
            if code is not None:
                build.proc = None
                state = finish(state, code, app)
                if args.once:
                    return app.proc.wait() if code == 0 else 1
        else:
            state = advance(state, fp, now, args.quiet)
        if isinstance(state, Building) and build.proc is None:
            say(f'building {snap.branch or "detached"} at {snap.head[:7]}{" with edits" if snap.status else ""}')
            start_build(build, snap.path)

        code = app.poll()
        if code == 0:
            say('app closed, exiting')
            return 0
        if code is not None:
            app.proc = None
            how = f'exit {code}' if code > 0 else f'signal {-code}'
            say(f'app crashed ({how}), relaunching on the next change; log in {HOME / "app.log"}')
        time.sleep(args.poll)


def interrupt(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('target', nargs='?', help='branch name or worktree path to pin; default follows the most active')
    parser.add_argument('--once', action='store_true', help='build and run once, without following changes')
    parser.add_argument('--poll', type=float, default=2, help='seconds between checks (default 2)')
    parser.add_argument('--quiet', type=float, default=3,
                        help='seconds a change must stay unchanged before a build (default 3)')
    args = parser.parse_args()
    app, build = Group(), Group()
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupt)
    try:
        return run(args, app, build)
    except BrokenPipeError:
        return 1
    finally:
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(signum, signal.SIG_IGN)
        build.stop()
        app.stop()


if __name__ == '__main__':
    sys.exit(main())
