#!/usr/bin/env python3
"""The native journey for checkpoints and Restore to here (TOB-58).

    python3 scripts/checkpoint-journey.py [--out DIR] [--width 1920] [--height 1080] [--driver PATH]

Build first: `cargo build -p keystone-desktop -p keystone-agent --bins --locked`. It makes a
fixture repository with a commit, a staged file, a stash and ignored files, launches
target/debug/keystone-desktop on a private headless Sway through scripts/native-driver.py with
the scripted Claude in store mode and empty provider homes, and drives two turns whose prompts
make the fake write, edit and delete files. It then checks the refs, that a save expires
another workspace's refs older than 30 days, that a restore during a running turn is refused,
that Cancel stops a restore stuck in a git filter, that a failing filter or a fork that can't
open (the workspace database held by another writer) changes no file, that Restore to here on
turn one brings its files back without touching the index, HEAD, stash or ignored files, that
a linked fork holds turn one, and that retention prunes the oldest ref. With --outside-git it
uses a folder outside any repository instead and checks that Restore says checkpoints need one
only in its tooltip. --theme picks Settings → Appearance first, and --record saves the run as
DIR/journey.mp4. Screenshots, DIR/frames for a step-by-step video and DIR/report.json record
each check; the script exits non-zero when one fails.
"""
import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('screen_walk', ROOT / 'scripts/screen-walk.py')
screen_walk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(screen_walk)

TURN_ONE = 'Turn one; write one.txt: turn one; write edited.txt: turn one; delete deleted.txt'
TURN_TWO = ('Turn two; write edited.txt: turn two; write deleted.txt: back in turn two; '
            'write two/nested.txt: turn two; delete one.txt; write debug.log: written in turn two')


# Another workspace's store, whose refs the fixture dates back.
AGED = 'aged'


def git(repo, *args, env=None):
    return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True,
                          text=True, env=env).stdout


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else None


class Journey(screen_walk.Walk):
    def __init__(self, width, height, out, driver, plain=False, theme=None, record=False):
        self.width, self.height = width, height
        self.plain, self.theme, self.record = plain, theme, record
        self.out = out
        shutil.rmtree(out, ignore_errors=True)
        out.mkdir(parents=True)
        self.scratch = Path(tempfile.mkdtemp(prefix='keystone-checkpoints-'))
        self.lock = open(self.scratch / 'lock', 'w')
        self.repo = self.scratch / 'repo'
        self.homes = {name: self.scratch / 'homes' / name for name in ('claude', 'codex')}
        for home in self.homes.values():
            home.mkdir(parents=True)
        self.command = [sys.executable, str(ROOT / 'scripts/native-driver.py'),
                        '--binary', str(ROOT / 'target/debug/keystone-desktop')]
        if driver:
            self.command += ['--driver', driver]
        self.seq = 0
        self.results = []
        self.checks = []
        self.evidence = None

    def fixture(self):
        repo = self.repo
        repo.mkdir()
        if self.plain:
            (repo / 'notes.txt').write_text('a folder outside any repository\n')
            return
        git(repo, 'init', '-q', '-b', 'main')
        for key, value in [('user.name', 'Fixture'), ('user.email', 'fixture@example.com'),
                           ('commit.gpgsign', 'false')]:
            git(repo, 'config', key, value)
        (repo / '.gitignore').write_text('*.log\nbuild/\n.fake-claude-*\n')
        for name, text in [('kept.txt', 'kept\n'), ('edited.txt', 'first\n'), ('deleted.txt', 'deleted\n')]:
            (repo / name).write_text(text)
        git(repo, 'add', '--all')
        git(repo, 'commit', '-q', '-m', 'fixture')
        (repo / 'kept.txt').write_text('stashed\n')
        git(repo, 'stash', '-q')
        (repo / 'staged.txt').write_text('staged\n')
        git(repo, 'add', 'staged.txt')
        (repo / 'debug.log').write_text('ignored\n')
        (repo / 'build').mkdir()
        (repo / 'build/out.bin').write_bytes(b'\x00ignored\x01')
        tree = git(repo, 'rev-parse', 'HEAD^{tree}').strip()
        for session, days in [(1, 31), (2, 29)]:
            date = f'@{int(time.time()) - days * 24 * 60 * 60} +0000'
            env = {'PATH': '/usr/bin:/bin', 'GIT_AUTHOR_DATE': date, 'GIT_COMMITTER_DATE': date,
                   'GIT_AUTHOR_NAME': 'Fixture', 'GIT_AUTHOR_EMAIL': 'fixture@example.com',
                   'GIT_COMMITTER_NAME': 'Fixture', 'GIT_COMMITTER_EMAIL': 'fixture@example.com'}
            commit = git(repo, 'commit-tree', tree, '-m', f'{days} days old', env=env).strip()
            git(repo, 'update-ref', f'refs/keystone/checkpoints/{AGED}/{session}/1', commit)

    def untouched(self):
        return {'index': digest(self.repo / '.git/index'), 'HEAD': git(self.repo, 'rev-parse', 'HEAD').strip(),
                'stash': git(self.repo, 'stash', 'list', '--format=%H').strip()}

    def ignored(self):
        return {name: digest(self.repo / name) for name in ('debug.log', 'build/out.bin')}

    def files(self):
        """Every file of the working tree outside .git and ignored files, by hash."""
        listed = git(self.repo, 'ls-files', '-z', '--cached', '--others', '--exclude-standard').split('\0')
        return {name: digest(self.repo / name) for name in listed if name and (self.repo / name).exists()}

    def checkpoint(self, ref):
        """The files a checkpoint ref holds, by hash."""
        out = git(self.repo, 'ls-tree', '-r', '-z', ref)
        tree = {}
        for line in filter(None, out.split('\0')):
            meta, name = line.split('\t', 1)
            tree[name] = hashlib.sha256(git(self.repo, 'cat-file', 'blob', meta.split()[2]).encode()).hexdigest()
        return tree

    def aged(self):
        """The refs left of the other workspace's dated store, as <session>/<name>."""
        names = git(self.repo, 'for-each-ref', '--format=%(refname)', f'refs/keystone/checkpoints/{AGED}').split()
        return [name.split('/', 4)[4] for name in names]

    def leftovers(self):
        return sorted(p.name for p in (self.repo / '.git').iterdir() if p.name.startswith('keystone-'))

    def refs(self):
        """The checkpoint refs as <session>/<name>, after the workspace's store name."""
        names = [name for name in git(self.repo, 'for-each-ref', '--format=%(refname)', 'refs/keystone').split()
                 if name.split('/')[3] != AGED]
        self.stores = {name.split('/')[3] for name in names}
        if self.stores:
            self.store = min(self.stores)
        return [name.split('/', 4)[4] for name in names]

    def ref(self, name):
        return f'refs/keystone/checkpoints/{self.store}/{name}'

    def check(self, name, ok, detail=None):
        self.checks.append({'check': name, 'ok': bool(ok), 'detail': detail})
        print(f'{"PASS" if ok else "FAIL"} {name}' + (f': {detail}' if detail is not None else ''), flush=True)

    def snap(self, name):
        x, y = self.width - 1, self.height - 1
        self.tool('input', actions=[{'action': 'move', 'x': x - 1, 'y': y - 1}, {'action': 'move', 'x': x, 'y': y}])
        time.sleep(0.6)
        shutil.copy(self.tool('screenshot')['path'], self.out / f'{name}.png')
        self.frame()
        print(f'shot {name}', flush=True)

    def frame(self):
        """A numbered screenshot for the step-by-step video in DIR/frames."""
        frames = self.out / 'frames'
        frames.mkdir(exist_ok=True)
        shutil.copy(self.tool('screenshot')['path'], frames / f'{len(list(frames.iterdir())):03}.png')

    def restores(self):
        ids = self.ids() or []
        found = sorted((int(m.group(1)), i) for i in ids if (m := re.fullmatch(r'restore-(\d+)(?:-view-\d+)?', i)))
        return found

    def sessions(self):
        return self.tool('workspace')['workspace']['sessions']

    def run(self):
        self.fixture()
        claude = self.scratch / 'claude'
        claude.write_text('#!/bin/sh\nexport KEYSTONE_FAKE_CLAUDE_STORE=1\n'
                          f'exec "{ROOT}/target/debug/keystone-fake-claude" "$@"\n')
        claude.chmod(0o755)
        self.server = subprocess.Popen(self.command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, cwd=ROOT)
        self.lines = screen_walk.queue.Queue()
        screen_walk.threading.Thread(target=self.read, daemon=True).start()
        try:
            self.rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                    'clientInfo': {'name': 'checkpoint-journey', 'version': '1'}})
            started = self.tool('start', width=self.width, height=self.height, env={
                'KEYSTONE_CLAUDE_PATH': str(claude),
                'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
                'KEYSTONE_PROJECT_DIR': str(self.repo), 'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '5',
                'KEYSTONE_TEST_CHECKPOINT_KEEP': '3',
                'CLAUDE_CONFIG_DIR': str(self.homes['claude']), 'CODEX_HOME': str(self.homes['codex'])})
            self.evidence = started.get('evidence') or started.get('evidence_dir')
            self.database = started.get('database')
            self.wait('welcome-claude', 30)
            if self.theme:
                self.choose_theme()
            if self.record:
                self.tool('record_start')
            self.journey()
            if self.record:
                shutil.copy(self.tool('record_stop')['path'], self.out / 'journey.mp4')
        finally:
            try:
                stop = self.tool('stop')
                self.check('driver stopped cleanly', not stop.get('errors'), stop.get('errors'))
            finally:
                self.server.stdin.close()
                try:
                    self.server.wait(30)
                except subprocess.TimeoutExpired:
                    self.server.kill()
                report = {'checks': self.checks, 'evidence': self.evidence}
                (self.out / 'report.json').write_text(json.dumps(report, indent=2))
                shutil.rmtree(self.scratch, ignore_errors=True)

    def outside_git(self):
        self.click('welcome-claude')
        self.wait('composer-field', 10)
        self.send('Turn one; write notes.txt: edited')
        self.idle(30)
        self.until(lambda ids: self.restores() or None, 'the Restore control', 10)
        first = self.restores()[0][0]
        self.send('Turn two; write notes.txt: again')
        self.idle(30)
        later = self.until(lambda ids: [r for r in self.restores() if r[0] != first] or None,
                           'the Restore control on turn two', 10)
        self.check('one unavailable control outside git, on the latest turn', len(self.restores()) == 1,
                   self.restores())
        control = later[0][1]
        center = next(e['center'] for e in self.tool('elements')['elements'] if e['id'] == control)
        self.tool('input', actions=[{'action': 'move', 'x': center['x'], 'y': center['y']}])
        time.sleep(1.5)
        shutil.copy(self.tool('screenshot')['path'], self.out / '10-outside-git-tooltip.png')
        self.click(control)
        time.sleep(0.5)
        self.check('no dialog outside git', self.find('restore-dialog-confirm') is None)
        self.check('no conversation line outside git', self.find('conversation-notice') is None,
                   [i for i in self.ids() or [] if i.startswith('conversation-notice')])
        self.check('no files count outside git', not any(i.startswith('checkpoint-files') for i in self.ids() or []))
        self.check('no .git was made', not (self.repo / '.git').exists())
        self.snap('11-outside-git-pressed')

    def confirm_turn_one(self):
        self.click(self.restores()[0][1])
        self.wait('restore-dialog-confirm', 5)
        self.click('restore-dialog-confirm')

    def cancel_stuck_restore(self):
        pid = self.repo / '.git/filter-pid'
        git(self.repo, 'config', 'filter.slow.smudge', f"echo $$ > '{pid}'; sleep 60; cat")
        (self.repo / '.git/info/attributes').write_text('*.txt filter=slow\n')
        before = self.files()
        self.confirm_turn_one()
        self.wait('restore-busy', 10)
        deadline = time.monotonic() + 10
        while not (pid.exists() and pid.read_text().strip().isdigit()):
            if time.monotonic() > deadline:
                raise RuntimeError('the filter did not start')
            time.sleep(0.1)
        filter_pid = int(pid.read_text())
        self.snap('4-restore-stuck-in-a-filter')
        self.check('a restore stuck in a filter changes no file', self.files() == before)
        self.click('restore-dialog-cancel')
        self.wait('restore-dialog-confirm', 10, present=False)
        self.frame()
        time.sleep(0.5)
        try:
            os.kill(filter_pid, 0)
            alive = True
        except ProcessLookupError:
            alive = False
        self.check('Cancel stops the filter', not alive, filter_pid)
        self.check('a cancelled restore changes no file', self.files() == before)
        self.check('a cancelled restore leaves nothing in .git', self.leftovers() == [], self.leftovers())
        git(self.repo, 'config', '--unset', 'filter.slow.smudge')
        (self.repo / '.git/info/attributes').unlink()
        pid.unlink()
        self.snap('5-restore-cancelled')

    def failing_filter(self):
        git(self.repo, 'config', 'filter.broken.clean', 'cat')
        git(self.repo, 'config', 'filter.broken.smudge', 'false')
        git(self.repo, 'config', 'filter.broken.required', 'true')
        (self.repo / '.git/info/attributes').write_text('edited.txt filter=broken\n')
        before = self.files()
        self.confirm_turn_one()
        self.wait('restore-error', 15)
        self.snap('6-restore-failing-filter')
        self.check('a failing filter changes no file', self.files() == before)
        self.check('a failed restore leaves nothing in .git', self.leftovers() == [], self.leftovers())
        self.key('Escape')
        self.wait('restore-dialog-confirm', 5, present=False)
        git(self.repo, 'config', '--remove-section', 'filter.broken')
        (self.repo / '.git/info/attributes').unlink()

    def fork_cannot_open(self):
        before, sessions = self.files(), len(self.sessions())
        holder = sqlite3.connect(self.database, isolation_level=None, timeout=10)
        try:
            holder.execute('BEGIN EXCLUSIVE')
            self.confirm_turn_one()
            self.wait('restore-error', 20)
            self.snap('7-restore-fork-cannot-open')
            self.check('a fork that cannot open changes no file', self.files() == before)
        finally:
            holder.execute('ROLLBACK')
            holder.close()
        self.check('no fork was made', len(self.sessions()) == sessions, len(self.sessions()))
        self.check('nothing left in .git after the fork failed', self.leftovers() == [], self.leftovers())
        self.key('Escape')
        self.wait('restore-dialog-confirm', 5, present=False)

    def journey(self):
        if self.plain:
            return self.outside_git()
        self.click('welcome-claude')
        self.wait('composer-field', 10)
        before = self.untouched()

        self.frame()
        self.send(TURN_ONE)
        self.frame()
        self.idle(30)
        self.frame()
        session = self.sessions()[0]['id']
        self.until(lambda ids: len(self.refs()) == 1 or None, 'turn one checkpoint', 15)
        self.check('a save expires refs older than 30 days in every store', self.aged() == ['2/1'], self.aged())
        after_one = self.files()
        self.check('turn one changed the files', (self.repo / 'one.txt').exists()
                   and (self.repo / 'edited.txt').read_text() == 'turn one\n'
                   and not (self.repo / 'deleted.txt').exists())

        self.send(TURN_TWO)
        self.frame()
        self.idle(30)
        self.until(lambda ids: len(self.refs()) == 2 or None, 'turn two checkpoint', 15)
        self.until(lambda ids: len(self.restores()) == 2 or None, 'two Restore controls', 10)
        refs = self.refs()
        print('git for-each-ref refs/keystone:\n  ' + '\n  '.join(refs), flush=True)
        self.check('refs after two turns', refs == [f'{session}/{n}' for n in (1, 2)] and len(self.stores) == 1,
                   [self.ref(name) for name in refs])
        self.check('turn one checkpoint holds turn one files', self.checkpoint(self.ref(refs[0])) == after_one)
        counts = {i: self.find(f'checkpoint-files-{n}') for n, i in self.restores()}
        self.check('file counts shown', all(counts.values()), counts)
        self.check('index, HEAD and stash unchanged by checkpoints', self.untouched() == before)
        self.snap('1-two-turns-with-checkpoints')

        ignored = self.ignored()
        before_refused = self.files()
        (self.repo / '.fake-claude-delay-ms').write_text('300')
        self.send('Turn three runs long')
        self.wait('stop', 10)
        first = self.restores()[0][1]
        self.click(first)
        self.wait('restore-dialog-confirm', 5)
        self.snap('2-restore-confirmation')
        self.click('restore-dialog-confirm')
        self.wait('restore-error', 5)
        self.snap('3-restore-refused-while-running')
        self.check('restore during a running turn is refused', self.find('restore-error') is not None)
        self.check('a refused restore changes no file', self.files() == before_refused)
        self.key('Escape')
        self.wait('restore-dialog-confirm', 5, present=False)
        self.frame()
        self.click('stop')
        self.idle(30)
        (self.repo / '.fake-claude-delay-ms').unlink()

        self.until(lambda ids: len(self.restores()) >= 2 or None, 'Restore controls after the stop', 10)
        self.cancel_stuck_restore()
        self.failing_filter()
        self.fork_cannot_open()
        before_restore = self.files()
        self.click(self.restores()[0][1])
        self.wait('restore-dialog-confirm', 5)
        self.frame()
        self.click('restore-dialog-confirm')
        self.until(lambda ids: len(self.sessions()) == 2 or None, 'the fork', 15)
        self.wait('restore-dialog-confirm', 10, present=False)
        time.sleep(1)
        self.snap('8-restored-fork-open')
        restored = self.files()
        self.check('files hash as checkpoint 1', restored == self.checkpoint(self.ref(f'{session}/1')),
                   {'restored': restored, 'checkpoint': self.checkpoint(self.ref(f'{session}/1'))})
        self.check('the files the restore replaced are kept',
                   self.checkpoint(self.ref(f'{session}/before-restore')) == before_restore)
        self.check('files created later are deleted',
                   not (self.repo / 'two').exists() and not (self.repo / 'deleted.txt').exists())
        self.check('ignored files untouched by the restore', self.ignored() == ignored, self.ignored())
        self.check('index, HEAD and stash unchanged by the restore', self.untouched() == before,
                   {'before': before, 'after': self.untouched()})
        sessions = self.sessions()
        fork = next((s for s in sessions if s.get('forked_from') == session), None)
        self.check('a linked fork exists', fork is not None, [{k: s.get(k) for k in ('id', 'title', 'forked_from')} for s in sessions])
        self.until(lambda ids: self.find('fork-from') or None, 'the fork link row', 10)
        rows = [i for i in (self.ids() or []) if re.fullmatch(r'transcript-row-\d+(?:-view-\d+)?', i)]
        self.check('the fork shows turn one only', len(rows) == 2, rows)

        refs_now = self.refs()
        print('git for-each-ref refs/keystone (after restore):\n  ' + '\n  '.join(refs_now), flush=True)
        self.check('restoring keeps every checkpoint',
                   refs_now == [f'{session}/{n}' for n in (1, 2, 3, 'before-restore')], refs_now)

        self.sidebar(True)
        self.click(f'session-{session}')
        self.wait('composer-field', 10)
        self.send('Turn four; write four.txt: turn four')
        self.idle(30)
        self.until(lambda ids: f'{session}/4' in self.refs() or None, 'turn four checkpoint', 15)
        pruned = self.refs()
        print('git for-each-ref refs/keystone (keep 3, after turn four):\n  ' + '\n  '.join(pruned), flush=True)
        self.check('retention prunes the oldest',
                   pruned == [f'{session}/{n}' for n in (2, 3, 4, 'before-restore')], pruned)
        self.snap('9-source-after-turn-four')
        return session


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--out', type=Path, default=ROOT / '.audit/checkpoint-journey')
    parser.add_argument('--width', type=int, default=1920)
    parser.add_argument('--height', type=int, default=1080)
    parser.add_argument('--driver')
    parser.add_argument('--outside-git', action='store_true',
                        help='use a folder outside any repository and check what Restore says there')
    parser.add_argument('--theme', choices=screen_walk.THEMES)
    parser.add_argument('--record', action='store_true', help='save the run as DIR/journey.mp4')
    args = parser.parse_args()
    journey = Journey(args.width, args.height, args.out, args.driver, args.outside_git, args.theme, args.record)
    journey.run()
    failed = [c['check'] for c in journey.checks if not c['ok']]
    print(f'{len(journey.checks) - len(failed)} of {len(journey.checks)} checks passed')
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
