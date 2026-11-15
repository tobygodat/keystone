#!/usr/bin/env python3
"""Behavior tests for scripts/dev-timing.py and the check.sh wrapper.

Run: python3 scripts/test_dev_timing.py
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / 'scripts/dev-timing.py'
PY = sys.executable


def gone(pid):
    try:
        state = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0]
    except FileNotFoundError:
        return True
    return state == 'Z'


class DevTimingTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.records = self.tmp / 'records'
        self.env = {**os.environ, 'KEYSTONE_DEV_TIMING_DIR': str(self.records)}
        self.env.pop('KEYSTONE_CACHE_STATE', None)

    def tearDown(self):
        subprocess.run(['chmod', '-R', 'u+w', self.tmp])
        subprocess.run(['rm', '-rf', self.tmp])

    def timed(self, *command, label='test', extra=(), env=None, cwd=ROOT):
        return subprocess.run([PY, TOOL, 'run', '--label', label, *extra, '--', *command],
                              capture_output=True, text=True, env=env or self.env, cwd=cwd)

    def saved(self, directory=None):
        return [json.loads(p.read_text()) for p in sorted((directory or self.records).glob('*.json'))]

    def summary(self):
        done = subprocess.run([PY, TOOL, 'summary'], capture_output=True, text=True, env=self.env)
        self.assertEqual(done.returncode, 0, done.stderr)
        return done.stdout, done.stderr

    def test_success_records_child_wall_time_and_context(self):
        done = self.timed(PY, '-c', 'import time; time.sleep(0.3); print("child ran")')
        self.assertEqual((done.returncode, done.stdout), (0, 'child ran\n'))
        [record] = self.saved()
        self.assertEqual(record['exit_code'], 0)
        self.assertEqual(record['cache_state'], 'unknown')
        self.assertGreaterEqual(record['wall_s'], 0.3)
        self.assertLess(record['wall_s'], 5)
        head = subprocess.run(['git', 'rev-parse', 'HEAD'], capture_output=True, text=True, cwd=ROOT)
        self.assertEqual(record['head'], head.stdout.strip())
        self.assertIsInstance(record['dirty'], bool)

    def test_nonzero_exit_is_preserved_and_recorded_as_failure(self):
        done = self.timed(PY, '-c', 'import sys; sys.exit(3)')
        self.assertEqual(done.returncode, 3)
        [record] = self.saved()
        self.assertEqual(record['exit_code'], 3)
        out, _ = self.summary()
        self.assertIn('runs 1  ok 0  failed 1', out)
        self.assertNotIn('median', out)

    def test_missing_command_reports_launch_failure(self):
        done = self.timed('keystone-no-such-command')
        self.assertEqual(done.returncode, 127)
        self.assertIn('keystone-no-such-command', done.stderr)
        [record] = self.saved()
        self.assertIsNone(record['exit_code'])
        self.assertTrue(record['launch_error'])
        self.assertIn('launch failed', self.summary()[0])

    def test_arguments_pass_through_unchanged(self):
        args = ['a b', '', '--', '$HOME', '*', '--label', "it's;rm -rf x"]
        done = self.timed(PY, '-c', 'import json, sys; print(json.dumps(sys.argv[1:]))', *args)
        self.assertEqual(json.loads(done.stdout), args)
        self.assertEqual(self.saved()[0]['argv'][3:], args)

    def test_output_streams_while_child_runs(self):
        gate = self.tmp / 'gate'
        script = ('import os, time\nprint("started", flush=True)\n'
                  f'while not os.path.exists({str(gate)!r}): time.sleep(0.02)\nprint("finished")')
        wrapper = subprocess.Popen([PY, TOOL, 'run', '--label', 'stream', '--', PY, '-c', script],
                                   stdout=subprocess.PIPE, text=True, env=self.env)
        self.assertEqual(wrapper.stdout.readline(), 'started\n')
        gate.touch()
        self.assertEqual(wrapper.stdout.read(), 'finished\n')
        self.assertEqual(wrapper.wait(), 0)

    def test_concurrent_runs_each_publish_one_complete_record(self):
        wrappers = [subprocess.Popen([PY, TOOL, 'run', '--label', f'job{i}', '--', PY, '-c',
                                      'import time; time.sleep(0.2)'], env=self.env)
                    for i in range(8)]
        self.assertEqual([w.wait() for w in wrappers], [0] * 8)
        self.assertEqual(sorted(r['label'] for r in self.saved()), sorted(f'job{i}' for i in range(8)))
        self.assertEqual(list(self.records.glob('.*')), [])

    def test_invalid_record_dir_warns_and_keeps_status(self):
        blocker = self.tmp / 'file'
        blocker.write_text('')
        env = {**self.env, 'KEYSTONE_DEV_TIMING_DIR': str(blocker / 'sub')}
        done = self.timed(PY, '-c', 'import sys; print("out"); sys.exit(4)', env=env)
        self.assertEqual((done.returncode, done.stdout), (4, 'out\n'))
        self.assertIn('dev-timing: could not save', done.stderr)

    @unittest.skipIf(os.geteuid() == 0, 'root ignores directory permissions')
    def test_unwritable_record_dir_warns_and_keeps_status(self):
        self.records.mkdir()
        self.records.chmod(0o500)
        done = self.timed(PY, '-c', 'pass')
        self.assertEqual(done.returncode, 0)
        self.assertIn('dev-timing: could not save', done.stderr)

    def test_outside_git_without_override_runs_command_unrecorded(self):
        env = {k: v for k, v in self.env.items() if k != 'KEYSTONE_DEV_TIMING_DIR'}
        env['GIT_CEILING_DIRECTORIES'] = str(self.tmp.parent)
        done = self.timed(PY, '-c', 'import sys; sys.exit(5)', env=env, cwd=self.tmp)
        self.assertEqual(done.returncode, 5)
        self.assertIn('dev-timing: could not save', done.stderr)

    def test_outside_git_with_override_records_missing_revision(self):
        env = {**self.env, 'GIT_CEILING_DIRECTORIES': str(self.tmp.parent)}
        self.assertEqual(self.timed(PY, '-c', 'pass', env=env, cwd=self.tmp).returncode, 0)
        [record] = self.saved()
        self.assertIsNone(record['head'])
        self.assertIn('no-git', self.summary()[0])

    def test_default_dir_is_shared_by_worktrees_and_survives_removal(self):
        repo, tree = self.tmp / 'repo', self.tmp / 'tree'
        git = ['git', '-c', 'user.name=t', '-c', 'user.email=t@example.com']
        subprocess.run([*git, 'init', '-q', repo], check=True)
        subprocess.run([*git, '-C', repo, 'commit', '-q', '--allow-empty', '-m', 'x'], check=True)
        subprocess.run([*git, '-C', repo, 'worktree', 'add', '-q', tree], check=True)
        env = {k: v for k, v in self.env.items() if k != 'KEYSTONE_DEV_TIMING_DIR'}
        self.assertEqual(self.timed(PY, '-c', 'pass', env=env, cwd=tree).returncode, 0)
        subprocess.run([*git, '-C', repo, 'worktree', 'remove', tree], check=True)
        [record] = self.saved(repo / '.git/dev-timings')
        self.assertEqual(record['worktree'], str(tree))

    def test_signals_reach_the_whole_command_and_are_reraised(self):
        for signum in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signal=signum.name):
                pidfile = self.tmp / f'{signum.name}.pid'
                script = ('import subprocess\np = subprocess.Popen(["sleep", "60"])\n'
                          f'open({str(pidfile)!r}, "w").write(str(p.pid))\np.wait()')
                wrapper = subprocess.Popen([PY, TOOL, 'run', '--label', signum.name, '--',
                                            PY, '-c', script],
                                           stderr=subprocess.DEVNULL, env=self.env)
                deadline = time.monotonic() + 10
                while not (pidfile.exists() and pidfile.read_text()):
                    self.assertLess(time.monotonic(), deadline, 'child never started')
                    time.sleep(0.02)
                grandchild = int(pidfile.read_text())
                wrapper.send_signal(signum)
                self.assertEqual(wrapper.wait(timeout=10), -signum)
                while not gone(grandchild):
                    self.assertLess(time.monotonic(), deadline, 'grandchild was orphaned')
                    time.sleep(0.02)
        self.assertEqual(sorted(r['signal'] for r in self.saved()),
                         sorted([signal.SIGTERM, signal.SIGINT]))

    def test_summary_separates_incomparable_runs(self):
        exit_with = ['-c', 'import os, sys; sys.exit(int(os.environ["CODE"]))']
        for code in ('0', '0', '0', '7'):
            self.timed(PY, *exit_with, env={**self.env, 'CODE': code})
        ok = {**self.env, 'CODE': '0'}
        self.timed(PY, *exit_with, extra=['--cache-state', 'warm'], env=ok)
        self.timed(PY, *exit_with, env={**ok, 'RUSTFLAGS': '-Copt-level=1'})
        self.timed(PY, *exit_with, label='other', env=ok)
        (self.records / 'corrupt.json').write_text('{not json')
        (self.records / 'future.json').write_text('{"version": 99}')
        out, err = self.summary()
        self.assertEqual(out.count('  runs '), 4, out)
        self.assertIn('runs 4  ok 3  failed 1  ok wall median', out)
        self.assertIn('failures (not timings): exit 7', out)
        self.assertIn('cache=warm', out)
        self.assertIn('RUSTFLAGS=-Copt-level=1', out)
        self.assertIn('load1 ', out)
        self.assertIn('skipping corrupt.json', err)
        self.assertIn('skipping future.json', err)

    def test_wrong_typed_records_do_not_hide_valid_history(self):
        self.assertEqual(self.timed(PY, '-c', 'pass').returncode, 0)
        [valid] = self.saved()
        for index, (field, value) in enumerate([
            ('env', []), ('env', {'RUSTFLAGS': []}), ('argv', [None]),
            ('wall_s', 'fast'), ('wall_s', float('nan')), ('dirty', []),
            ('load_before', None), ('exit_code', False),
        ]):
            bad = {**valid, field: value}
            (self.records / f'bad-{index}.json').write_text(json.dumps(bad))
        out, err = self.summary()
        self.assertIn('runs 1  ok 1  failed 0', out)
        self.assertEqual(err.count('skipping bad-'), 8)

    def test_unknown_source_state_is_visible(self):
        self.assertEqual(self.timed(PY, '-c', 'pass').returncode, 0)
        [path] = list(self.records.glob('*.json'))
        record = json.loads(path.read_text())
        record['dirty'] = None
        path.write_text(json.dumps(record))
        out, _ = self.summary()
        self.assertIn('+state-unknown', out)


class CheckScriptTest(unittest.TestCase):
    NEXTEST_CORE = 'nextest run -p keystone-core -p keystone-agent --locked --retries 0'
    CASE_INSENSITIVE = 'test -p keystone-agent --locked --lib --no-run --message-format=json'
    DOCTESTS = 'test -p keystone-core -p keystone-agent --locked --doc'

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.records = self.tmp / 'records'
        self.bin = self.tmp / 'bin'
        self.bin.mkdir()
        self.calls = self.tmp / 'cargo-calls'
        cargo = self.bin / 'cargo'
        # The case-insensitive lane asks cargo where the agent's test binary is and runs it.
        cargo.write_text('#!/bin/sh\necho "$*" >> "$FAKE_CARGO_CALLS"\n'
                         '[ "$1" = nextest ] && exit "${FAKE_CARGO_NEXTEST_EXIT:-0}"\n'
                         'case "$*" in\n'
                         '  *--doc*) exit "${FAKE_CARGO_DOC_EXIT:-0}" ;;\n'
                         '  *--message-format=json*)\n'
                         """    echo '{"executable":"/bin/true","target":{"name":"keystone_agent"}}' ;;\n"""
                         'esac\nexit 0\n')
        cargo.chmod(0o755)
        self.nextest = self.bin / 'cargo-nextest'
        self.nextest.write_text('#!/bin/sh\nexit 0\n')
        self.nextest.chmod(0o755)
        self.env = {**os.environ, 'KEYSTONE_DEV_TIMING_DIR': str(self.records),
                    'FAKE_CARGO_CALLS': str(self.calls),
                    'PATH': f'{self.bin}:{os.environ["PATH"]}'}

    def tearDown(self):
        subprocess.run(['rm', '-rf', self.tmp])

    def check(self, *args, env=None):
        return subprocess.run(['/usr/bin/env', 'bash', ROOT / 'scripts/check.sh', *args],
                              capture_output=True, text=True, env=env or self.env, cwd=self.tmp)

    def cargo_calls(self):
        return self.calls.read_text().splitlines()

    def test_core_runs_each_step_once_and_records_the_mode(self):
        done = self.check('core', env={**self.env, 'KEYSTONE_CACHE_STATE': 'warm'})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.cargo_calls(), ['fmt --all --check', self.NEXTEST_CORE,
                                              self.CASE_INSENSITIVE, self.DOCTESTS])
        [record] = [json.loads(p.read_text()) for p in self.records.glob('*.json')]
        self.assertEqual((record['label'], record['exit_code'], record['cache_state']),
                         ('check core', 0, 'warm'))

    def test_ui_builds_the_fakes_then_runs_nextest_with_the_ui_features(self):
        done = self.check('ui')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.cargo_calls(), [
            'fmt --all --check',
            'build -p keystone-agent --bins --locked',
            'nextest run -p keystone-desktop -p keystone-ui'
            ' --features keystone-desktop/ui-tests,keystone-ui/ui-tests --locked --retries 0'])

    def test_check_failure_status_is_recorded_and_returned(self):
        done = self.check('ui', env={**self.env, 'FAKE_CARGO_NEXTEST_EXIT': '100'})
        self.assertEqual(done.returncode, 100)
        [record] = [json.loads(p.read_text()) for p in self.records.glob('*.json')]
        self.assertEqual((record['label'], record['exit_code']), ('check ui', 100))

    def test_a_failed_test_run_skips_the_steps_after_it(self):
        done = self.check('core', env={**self.env, 'FAKE_CARGO_NEXTEST_EXIT': '100'})
        self.assertEqual(done.returncode, 100)
        self.assertEqual(self.cargo_calls(), ['fmt --all --check', self.NEXTEST_CORE])

    def test_a_doctest_failure_propagates(self):
        done = self.check('core', env={**self.env, 'FAKE_CARGO_DOC_EXIT': '101'})
        self.assertEqual(done.returncode, 101)
        self.assertEqual(self.cargo_calls()[-1], self.DOCTESTS)

    def test_without_nextest_the_check_says_how_to_install_it(self):
        self.nextest.unlink()
        for tool in ('bash', 'dirname', 'grep', 'env', 'python3'):
            (self.bin / tool).symlink_to(shutil.which(tool))
        done = self.check('core', env={**self.env, 'PATH': str(self.bin)})
        self.assertEqual(done.returncode, 2)
        self.assertIn('https://nexte.st/docs/installation/pre-built-binaries/', done.stderr)
        self.assertEqual(self.cargo_calls(), ['fmt --all --check'])

    def test_check_usage_error_is_preserved(self):
        done = self.check('nope')
        self.assertEqual(done.returncode, 2)
        self.assertIn('Usage', done.stderr)


if __name__ == '__main__':
    unittest.main()
