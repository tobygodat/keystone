#!/usr/bin/env python3
"""Behavior tests for scripts/preview.py.

Run: python3 scripts/test_preview.py
"""
import importlib.util
import os
import queue
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / 'scripts/preview.py'
spec = importlib.util.spec_from_file_location('preview', TOOL)
preview = importlib.util.module_from_spec(spec)
spec.loader.exec_module(preview)

GIT_ENV = {'GIT_AUTHOR_NAME': 't', 'GIT_AUTHOR_EMAIL': 't@t', 'GIT_COMMITTER_NAME': 't',
           'GIT_COMMITTER_EMAIL': 't@t', 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1'}
BUILD_SH = '''mkdir -p "$CARGO_TARGET_DIR/debug"
app="$CARGO_TARGET_DIR/debug/keystone-desktop"
printf '#!/bin/sh\\necho "%s $KEYSTONE_STATE_PATH"\\n%s\\n' "$(cat version)" "$(cat behavior)" > "$app"
chmod +x "$app"
'''


def alive(pid):
    try:
        state = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0]
    except FileNotFoundError:
        return False
    return state != 'Z'


class Repo:
    def __init__(self, root):
        self.main = root / 'main'
        self.main.mkdir()
        self.env = {**os.environ, **GIT_ENV}
        self.git('init', '-q', '-b', 'main')
        self.write('README', 'hi\n')
        self.commit('first', when=1_000_000)

    def git(self, *args, cwd=None):
        return subprocess.run(['git', *args], cwd=cwd or self.main, env=self.env, check=True,
                              capture_output=True, text=True).stdout

    def write(self, name, text, cwd=None, mtime=None):
        path = (cwd or self.main) / name
        path.write_text(text)
        if mtime is not None:
            os.utime(path, (mtime, mtime))

    def commit(self, message, cwd=None, when=None):
        self.git('add', '-A', cwd=cwd)
        env = {**self.env, **({'GIT_COMMITTER_DATE': f'@{when} +0000'} if when else {})}
        subprocess.run(['git', 'commit', '-qm', message], cwd=cwd or self.main, env=env, check=True)

    def worktree(self, branch, root):
        path = root / branch
        self.git('worktree', 'add', '-q', '-b', branch, str(path))
        return path


class PickWorktreeTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.repo = Repo(self.tmp)
        self.a = self.repo.worktree('agent-a', self.tmp)
        self.b = self.repo.worktree('agent-b', self.tmp)

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def followed(self, target=None):
        return preview.locate(self.repo.main, target)

    def test_follows_the_worktree_with_the_newest_edit(self):
        self.repo.write('note', 'a', cwd=self.a, mtime=2_000_000)
        self.repo.write('note', 'b', cwd=self.b, mtime=3_000_000)
        self.assertEqual(self.followed().branch, 'agent-b')
        self.repo.write('note', 'a again', cwd=self.a, mtime=4_000_000)
        self.assertEqual(self.followed().branch, 'agent-a')

    def test_a_commit_counts_as_activity(self):
        self.repo.write('note', 'b', cwd=self.b, mtime=2_000_000)
        self.repo.write('feature', 'x', cwd=self.a)
        self.repo.commit('feature', cwd=self.a, when=3_000_000)
        self.assertEqual(self.followed().branch, 'agent-a')

    def test_deleted_dirty_files_are_ignored(self):
        self.repo.write('note', 'b', cwd=self.b, mtime=2_000_000)
        (self.a / 'README').unlink()
        self.assertEqual(self.followed().branch, 'agent-b')

    def test_target_pins_by_branch_or_path(self):
        self.repo.write('note', 'b', cwd=self.b)
        self.assertEqual(self.followed('agent-a').path, str(self.a))
        self.assertEqual(self.followed(str(self.a)).branch, 'agent-a')
        self.assertEqual(self.followed(f'{self.a}/').branch, 'agent-a')
        self.assertIsNone(self.followed('no-such-branch'))


class FingerprintTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.repo = Repo(self.tmp)

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def fingerprint(self):
        return preview.fingerprint(preview.snapshot((str(self.repo.main), 'main')))

    def test_stable_without_changes(self):
        self.repo.write('draft', 'one', mtime=2_000_000)
        self.assertEqual(self.fingerprint(), self.fingerprint())

    def test_changes_on_each_edit_of_a_dirty_file(self):
        self.repo.write('draft', 'one', mtime=2_000_000)
        before = self.fingerprint()
        self.repo.write('draft', 'two', mtime=2_000_001)
        self.assertNotEqual(self.fingerprint(), before)

    def test_changes_on_an_edit_inside_a_new_directory(self):
        (self.repo.main / 'new').mkdir()
        self.repo.write('new/file', 'one', mtime=2_000_000)
        before = self.fingerprint()
        self.repo.write('new/file', 'two', mtime=2_000_001)
        self.assertNotEqual(self.fingerprint(), before)

    def test_changes_on_a_commit(self):
        before = self.fingerprint()
        self.repo.write('README', 'changed\n')
        self.repo.commit('second')
        after = self.fingerprint()
        self.assertNotEqual(after, before)
        self.assertEqual(self.fingerprint(), after)


class SettleTest(unittest.TestCase):
    def test_a_burst_of_edits_builds_once_after_it_goes_quiet(self):
        state, builds = preview.Waiting(('v0',)), 0
        timeline = [(0, 'v1'), (1, 'v2'), (2, 'v3'), (4, 'v3'), (5, 'v3'), (6, 'v3'), (8, 'v3')]
        for now, fp in timeline:
            state = preview.advance(state, (fp,), now, quiet_s=3)
            if isinstance(state, preview.Building):
                builds += 1
                state = preview.Waiting(state.fp)
        self.assertEqual(builds, 1)


class FollowTest(unittest.TestCase):
    """Runs preview.py against a throwaway repo with a fake build and a fake app."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.repo = Repo(self.tmp)
        (self.repo.main / 'scripts').mkdir()
        shutil.copy(TOOL, self.repo.main / 'scripts/preview.py')
        self.repo.write('build.sh', BUILD_SH)
        self.repo.write('version', 'v1')
        self.repo.write('behavior', 'exec sleep 600')
        self.repo.commit('fake app')
        self.lines = queue.Queue()
        self.proc = None

    def tearDown(self):
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        if self.proc:
            self.proc.stdout.close()
        shutil.rmtree(self.tmp)

    def start(self, *extra):
        env = {**self.repo.env, 'HOME': str(self.tmp), 'KEYSTONE_PREVIEW_HOME': str(self.tmp / 'cache'),
               'KEYSTONE_PREVIEW_BUILD_CMD': 'sh build.sh'}
        self.proc = subprocess.Popen(
            [sys.executable, str(self.repo.main / 'scripts/preview.py'), '--poll', '0.1', '--quiet', '0.3', *extra],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
        threading.Thread(target=lambda: [self.lines.put(line) for line in self.proc.stdout], daemon=True).start()

    def wait_for(self, text, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                line = self.lines.get(timeout=0.1)
            except queue.Empty:
                continue
            if text in line:
                return line
        self.fail(f'no line containing {text!r}')

    def app_pid(self):
        return int(self.wait_for('app (pid').rsplit('pid ', 1)[1].rstrip(')\n'))

    def app_output(self):
        deadline = time.monotonic() + 5
        log = self.tmp / 'cache/app.log'
        while time.monotonic() < deadline:
            if log.exists() and log.read_text():
                return log.read_text()
            time.sleep(0.05)
        return ''

    def test_failed_build_keeps_the_running_app_until_a_good_build(self):
        self.start()
        self.wait_for(f'following {self.repo.main} (main)')
        first = self.app_pid()
        self.assertIn(f'v1 {self.tmp}/.local/share/keystone-preview/workspace.json', self.app_output())

        self.repo.write('build.sh', 'echo warming up\necho "error[E0001]: broken"\nexit 1\n')
        self.wait_for('error[E0001]: broken')
        self.wait_for('build failed, keeping last good build')
        self.assertTrue(alive(first), 'the old app survives a failed build')

        self.repo.write('build.sh', BUILD_SH)
        self.repo.write('version', 'v2')
        second = self.app_pid()
        self.assertNotEqual(second, first)
        self.assertFalse(alive(first), 'the old app is stopped before the new one starts')
        self.assertIn('v2', self.app_output())

        self.proc.send_signal(signal.SIGHUP)
        self.proc.wait(10)
        self.assertFalse(alive(second), 'no app left behind after SIGHUP')

    def test_switches_to_a_worktree_that_becomes_more_active(self):
        self.start()
        self.wait_for(f'following {self.repo.main} (main)')
        self.app_pid()
        other = self.repo.worktree('agent', self.tmp)
        self.repo.write('version', 'from agent', cwd=other)
        self.wait_for(f'switching to {other} (agent)')
        self.app_pid()
        self.assertIn('from agent', self.app_output())
        self.proc.send_signal(signal.SIGTERM)
        self.proc.wait(10)

    def test_quitting_the_app_ends_the_preview(self):
        self.repo.write('behavior', 'sleep 0.5; exit 0')
        self.start()
        self.app_pid()
        self.wait_for('app closed, exiting')
        self.assertEqual(self.proc.wait(10), 0)

    def test_a_crashed_app_is_relaunched_on_the_next_change(self):
        self.repo.write('behavior', 'exit 3')
        self.start()
        self.app_pid()
        self.wait_for('app crashed (exit 3)')
        self.repo.write('behavior', 'exec sleep 600')
        self.assertIn('started app', self.wait_for('app (pid'))
        self.assertIsNone(self.proc.poll())
        self.proc.send_signal(signal.SIGTERM)
        self.proc.wait(10)

    def test_once_builds_runs_and_exits_with_the_app(self):
        self.repo.write('behavior', 'exit 0')
        self.start('--once')
        self.assertEqual(self.proc.wait(10), 0)


if __name__ == '__main__':
    unittest.main()
