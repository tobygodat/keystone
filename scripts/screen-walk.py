#!/usr/bin/env python3
"""Screenshot Keystone's surfaces for review, at each width, with the scripted providers.

    python3 scripts/screen-walk.py walk [--width 1920 --width 720] [--height 1080] [--out DIR] [--driver PATH]
    python3 scripts/screen-walk.py sheet DIR [AFTER_DIR] [--out DIR]

`walk` launches target/debug/keystone-desktop on a private headless Sway through
scripts/native-driver.py, with keystone-fake-claude and keystone-fake-codex and empty provider
homes, so no real provider runs and no real config is read. Build first:
`cargo build -p keystone-desktop -p keystone-agent --bins --locked`. It saves DIR/<width>/<surface>.png
and DIR/manifest.json, which records each surface as shot, skipped (not in this build) or failed.

`sheet` tiles one walk, or pairs two walks surface by surface as before and after, into PNG
contact sheets with ImageMagick's montage.
"""
import argparse
import fcntl
import json
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
COMMANDS = ROOT / 'crates/keystone-desktop/src/commands.rs'
# Palette commands from features not on every branch: command id, label, surface, the element that
# proves the surface opened, and whether running the command again closes it. Each runs only when
# commands.rs has its id. With no saved prompts, Insert prompt opens Settings → Prompts.
OPTIONAL = [
    ('toggle-changes', 'Toggle changes', 'changes', 'changes-branch', True),
    ('toggle-notes', 'Toggle notes', 'notes', 'notes-editor', True),
    ('insert-prompt', 'Insert prompt…', 'insert-prompt', 'settings-section-prompts', False),
    ('attach-files', 'Attach files…', 'attach-files', 'attach-path', False),
    ('new-worktree-session', 'New session in worktree…', 'worktree-dialog', 'worktree-dialog', False),
    ('second-opinion', 'Ask for a second opinion…', 'second-opinion', 'opinion-of', False),
]


class Skip(Exception):
    pass


class Walk:
    def __init__(self, width, height, out, driver):
        self.width, self.height = width, height
        self.results = []
        self.lock = open(Path(tempfile.gettempdir()) / f'keystone-walk-{width}.lock', 'w')
        try:
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            sys.exit(f'Another screen walk at {width} px is running; run one at a time.')
        self.out = out / str(width)
        shutil.rmtree(self.out, ignore_errors=True)
        self.out.mkdir(parents=True)
        self.scratch = Path(tempfile.gettempdir()) / f'keystone-walk-{width}'
        shutil.rmtree(self.scratch, ignore_errors=True)
        self.project = self.scratch / 'project'
        (self.project / 'src').mkdir(parents=True)
        (self.project / '.claude/commands').mkdir(parents=True)
        (self.project / 'README.md').write_text('# Sample project\n')
        (self.project / 'src/main.rs').write_text('fn main() {}\n')
        (self.project / '.claude/commands/review.md').write_text('Review the open changes.\n')
        subprocess.run(['git', 'init', '-q', str(self.project)], check=True)
        self.homes = {name: self.scratch / 'homes' / name for name in ('claude', 'codex')}
        for home in self.homes.values():
            home.mkdir(parents=True)
        self.command = [sys.executable, str(ROOT / 'scripts/native-driver.py'),
                        '--binary', str(ROOT / 'target/debug/keystone-desktop')]
        if driver:
            self.command += ['--driver', driver]
        self.seq = 0

    def rpc(self, method, params):
        self.seq += 1
        self.server.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': self.seq, 'method': method,
                                            'params': params}) + '\n')
        self.server.stdin.flush()
        deadline = time.monotonic() + 120
        while True:
            try:
                line = self.lines.get(timeout=max(0, deadline - time.monotonic()))
            except queue.Empty:
                raise RuntimeError(f'native-driver.py did not answer {method} in 120 s') from None
            if line is None:
                raise RuntimeError('native-driver.py closed')
            message = json.loads(line)
            if message.get('id') == self.seq:
                return message

    def read(self):
        for line in self.server.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def tool(self, name, **args):
        result = self.rpc('tools/call', {'name': name, 'arguments': args})['result']
        text = result['content'][0]['text']
        if result.get('isError'):
            raise RuntimeError(f'{name}: {text}')
        return json.loads(text)

    def ids(self):
        """The named elements of the latest frame, or None while the app has not drawn one."""
        try:
            return [e['id'] for e in self.tool('elements')['elements']]
        except RuntimeError as error:
            if 'NotReady' in str(error):
                return None
            raise

    def find(self, name, ids=None):
        """The element called `name`, or its instance in a conversation view (`name-3-view-0`)."""
        deadline = time.monotonic() + 30
        while ids is None:
            ids = self.ids()
            if ids is None:
                if time.monotonic() > deadline:
                    raise RuntimeError('the app drew no frame in 30 s')
                time.sleep(0.25)
        return next((i for i in ids if i == name or re.fullmatch(re.escape(name) + r'(?:-\d+)?(?:-view-\d+)?', i)),
                    None)

    def until(self, test, what, seconds=10):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ids = self.ids()
            if ids is not None:
                result = test(ids)
                if result:
                    return result
            time.sleep(0.25)
        raise RuntimeError(f'timed out waiting for {what}')

    def wait(self, name, seconds=10, present=True):
        return self.until(lambda ids: (self.find(name, ids) or False) if present else not self.find(name, ids),
                          f'{name} {"to show" if present else "to go"}', seconds)

    def tabs(self, ids):
        return sum(1 for i in ids if re.fullmatch(r'fitted-tab-\d+|surface-tabs-\d+-tab-\d+', i))

    def click(self, prefix, seconds=5):
        self.tool('click', id=self.wait(prefix, seconds))
        time.sleep(0.4)

    def key(self, *keys):
        self.tool('input', actions=[{'action': 'keypress', 'keys': list(keys)}])
        time.sleep(0.5)

    def type(self, text):
        self.tool('input', actions=[{'action': 'type', 'text': text}])
        time.sleep(0.3)

    def send(self, text):
        self.click('composer-field')
        self.type(text)
        self.key('ctrl', 'Return')

    def idle(self, seconds=20):
        time.sleep(0.5)
        self.wait('turn-status', seconds, present=False)

    def shot(self, surface, proof=None):
        if proof:
            self.wait(proof, 5)
        x, y = self.width - 1, self.height - 1
        self.tool('input', actions=[{'action': 'move', 'x': x - 1, 'y': y - 1}, {'action': 'move', 'x': x, 'y': y}])
        time.sleep(0.6)
        shutil.copy(self.tool('screenshot')['path'], self.out / f'{surface}.png')
        self.results.append({'surface': surface, 'width': self.width, 'status': 'shot'})
        print(f'{self.width} {surface}', flush=True)

    def step(self, surface, action):
        try:
            action()
        except Skip as reason:
            self.results.append({'surface': surface, 'width': self.width, 'status': 'skipped', 'reason': str(reason)})
            print(f'{self.width} {surface} skipped: {reason}', flush=True)
        except Exception as error:
            self.results.append({'surface': surface, 'width': self.width, 'status': 'failed', 'reason': str(error)})
            print(f'{self.width} {surface} FAILED: {error}', flush=True)
            try:
                for _ in range(2):
                    self.key('Escape')
                if self.find('stop'):
                    self.click('stop')
                    self.idle()
            except Exception as recovery:
                print(f'{self.width} could not recover after {surface}: {recovery}', flush=True)

    def sidebar(self, shown):
        if bool(self.find('filter')) != shown:
            self.key('ctrl', 'b')
            self.wait('filter', 5, present=shown)

    def palette(self, label):
        self.key('ctrl', 'k')
        self.wait('palette', 5)
        self.type(label)
        self.key('Return')
        self.wait('palette', 5, present=False)
        time.sleep(1)

    def run(self):
        claude = self.scratch / 'claude'
        claude.write_text('#!/bin/sh\nexport KEYSTONE_FAKE_CLAUDE_STORE=1\n'
                          f'exec "{ROOT}/target/debug/keystone-fake-claude" "$@"\n')
        claude.chmod(0o755)
        codex = ROOT / 'target/debug/keystone-fake-codex'
        self.server = subprocess.Popen(self.command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                                       cwd=ROOT)
        self.lines = queue.Queue()
        threading.Thread(target=self.read, daemon=True).start()
        try:
            self.rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                    'clientInfo': {'name': 'screen-walk', 'version': '1'}})
            self.tool('start', width=self.width, height=self.height, env={
                'KEYSTONE_CLAUDE_PATH': str(claude), 'KEYSTONE_CODEX_PATH': str(codex),
                'KEYSTONE_PROJECT_DIR': str(self.project), 'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '5',
                'KEYSTONE_FAKE_CODEX_DELAY_MS': '5',
                'CLAUDE_CONFIG_DIR': str(self.homes['claude']), 'CODEX_HOME': str(self.homes['codex'])})
            self.wait('welcome-claude', 30)
            self.walk()
        finally:
            try:
                stop = self.tool('stop')
                if stop.get('errors'):
                    print('stop errors', stop['errors'], file=sys.stderr)
            except Exception as error:
                print(f'{self.width} stop failed: {error}', file=sys.stderr)
            finally:
                self.server.stdin.close()
                try:
                    self.server.wait(30)
                except subprocess.TimeoutExpired:
                    self.server.kill()
                shutil.rmtree(self.scratch, ignore_errors=True)
                self.lock.close()

    def walk(self):
        step, shot, key = self.step, self.shot, self.key
        step('welcome', lambda: shot('welcome'))

        def sidebar_menus():
            self.sidebar(True)
            self.click('new-session')
            shot('menu-new-session', 'new-claude')
            key('Escape')
            self.click('filter')
            shot('menu-filter', 'filter-all')
            key('Escape')
        step('menu-new-session', sidebar_menus)

        def empty_session():
            self.click('welcome-claude')
            self.wait('composer-field', 10)
            time.sleep(1)
            shot('session-empty')
        step('session-empty', empty_session)

        def menu(target, surface, proof):
            def run():
                self.click(target)
                shot(surface, proof)
                key('Escape')
            return run
        step('menu-model', menu('model-selector', 'menu-model', 'model-column'))
        step('menu-approval', menu('approval-mode', 'menu-approval', 'approval-ask'))

        def full_access():
            self.click('approval-mode')
            self.click('approval-full-access')
            self.wait('full-access-dialog', 5)
            shot('dialog-full-access')
            key('Escape')
        step('dialog-full-access', full_access)

        def slash_menu():
            self.click('composer-field')
            self.type('/')
            self.wait('slash-menu-list-row', 5)
            time.sleep(1)
            shot('menu-slash')
            key('BackSpace')
        step('menu-slash', slash_menu)

        def streaming():
            delay = self.project / '.fake-claude-delay-ms'
            delay.write_text('250')
            try:
                self.send('Explain the layout of this project')
                time.sleep(2.5)
                shot('transcript-streaming')
            finally:
                delay.unlink()
            self.idle(60)
            shot('transcript-reply')
        step('transcript-streaming', streaming)

        def approval():
            self.send('Please run the tests')
            self.wait('approve', 20)
            shot('card-approval')
            self.click('approve')
            self.idle()
            shot('transcript-command')
        step('card-approval', approval)

        def widget():
            self.send('Please show a widget form')
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and not self.find('widget'):
                if self.find('approve'):
                    self.click('approve')
                time.sleep(0.5)
            self.wait('widget', 5)
            shot('card-widget')
            self.click('stop')
            self.idle()
        step('card-widget', widget)

        def usage():
            key('ctrl', 'n')
            self.wait('composer-field', 10)
            time.sleep(1)
            self.send('Report usage 85')
            self.idle()
            shot('composer-usage', 'usage-meter')
        step('composer-usage', usage)

        def recovery():
            self.send('Please crash now')
            self.wait('recovery', 30)
            shot('card-recovery')
            if self.find('recovery-continue'):
                self.click('recovery-continue')
                self.wait('recovery-continue', 5, present=False)
                time.sleep(1)
                shot('transcript-recovery-report')
        step('card-recovery', recovery)

        def codex():
            key('ctrl', 'shift', 'n')
            self.wait('recovery-report', 10, present=False)
            self.send('Hello from Codex')
            self.idle()
            shot('session-codex', 'copy-reply')
        step('session-codex', codex)

        def sidebar_states():
            self.sidebar(True)
            key('ctrl', 'shift', 's')
            shot('sidebar-sections', 'settled-section')
            self.click('search-field')
            self.type('zzzz')
            self.wait('session', 5, present=False)
            shot('sidebar-search-empty')
            key('ctrl', 'a')
            key('BackSpace')
            key('Escape')
            if self.width < 1000:
                self.sidebar(False)
        step('sidebar-sections', sidebar_states)

        def palette():
            key('ctrl', 'k')
            shot('palette', 'palette-row')
            self.type('zzzz')
            self.wait('palette-row', 5, present=False)
            shot('palette-empty')
            key('Escape')
        step('palette', palette)

        def projects():
            key('ctrl', 'o')
            shot('palette-projects', 'projects-row')
            key('Escape')
        step('palette-projects', projects)

        def open_folder():
            key('ctrl', 'shift', 'o')
            shot('open-folder', 'workspace-error')
            for _ in range(3):
                if not self.find('projects'):
                    break
                key('Escape')
        step('open-folder', open_folder)

        def files():
            key('ctrl', 'shift', 'e')
            self.wait('files-tree-row', 5)
            time.sleep(0.5)
            shot('files')
            key('ctrl', 'shift', 'e')
        step('files', files)

        def rename():
            key('F2')
            shot('dialog-rename', 'rename-dialog')
            key('Escape')
        step('dialog-rename', rename)

        def handoff():
            key('ctrl', 'shift', 'h')
            self.wait('handoff-from', 5)
            time.sleep(1.5)
            shot('handoff')
            key('Escape')
        step('handoff', handoff)

        def tabs():
            before = self.tabs(self.ids())
            key('ctrl', 't')
            self.until(lambda ids: self.tabs(ids) > before, 'a new tab', 5)
            time.sleep(0.5)
            shot('tabs')
            if self.width >= 1000:
                key('ctrl', 'backslash')
                self.until(lambda ids: any(i.startswith('split-grip-') for i in ids), 'a split', 5)
                time.sleep(0.5)
                shot('split')
        step('tabs', tabs)

        def move_tab():
            key('ctrl', 'alt', 'm')
            shot('palette-move-tab', 'palette-row')
            key('Escape')
        step('palette-move-tab', move_tab)

        def windows():
            key('ctrl', 'alt', 'n')
            self.wait('welcome-heading', 10)
            time.sleep(1.5)
            shot('windows')
            key('ctrl', 'shift', 'w')
            time.sleep(1.5)
        step('windows', windows)

        def customize():
            key('ctrl', 'shift', 'l')
            self.wait('customize-close', 5)
            shot('customize')
            self.click('customize-borrow')
            shot('dialog-borrow-layout', 'borrow-dialog')
            key('Escape')
            if self.find('borrow-close'):
                self.click('borrow-close')
            self.click('customize-close')
        step('customize', customize)

        def settings():
            key('ctrl', 'comma')
            self.wait('settings-section-appearance', 5)
            elements = self.tool('elements')['elements']
            sections = sorted((e for e in elements if e['id'].startswith('settings-section-')),
                              key=lambda e: e['bounds']['y'])
            for index, section in enumerate(sections):
                name = section['id'].removeprefix('settings-section-')

                def page():
                    if not self.find(section['id']):
                        key('ctrl', 'comma')
                    self.wait(section['id'], 5)
                    before = set(self.ids() or [])
                    self.click(section['id'])
                    if index:
                        self.until(lambda ids: set(ids) != before, f'the {name} page', 5)
                    time.sleep(1)
                    shot(f'settings-{name}')
                self.step(f'settings-{name}', page)
            key('Escape')
        step('settings', settings)

        def conversation():
            key('ctrl', 'n')
            self.wait('composer-field', 10)
            time.sleep(1)
            self.send('Explain the layout of this project')
            self.idle()
            self.wait('copy-reply', 10)
        step('conversation', conversation)

        source = COMMANDS.read_text()
        for command, label, surface, element, toggle in OPTIONAL:
            def optional(command=command, label=label, surface=surface, element=element, toggle=toggle):
                if f'id: "{command}"' not in source:
                    raise Skip(f'no {command} command in this build')
                self.palette(label)
                self.wait(element, 10)
                shot(surface)
                if toggle:
                    self.palette(label)
                else:
                    key('Escape')
            step(surface, optional)


def walk(args):
    missing = [p for p in ('keystone-desktop', 'keystone-fake-claude', 'keystone-fake-codex')
               if not (ROOT / 'target/debug' / p).exists()]
    if missing:
        sys.exit(f'Build first: cargo build -p keystone-desktop -p keystone-agent --bins --locked '
                 f'(missing {", ".join(missing)})')
    out = Path(args.out or ROOT / f'.audit/screen-walk-{time.strftime("%Y%m%dT%H%M%S")}').resolve()
    results = []
    for width in args.width or [1920, 720]:
        walker = None
        try:
            walker = Walk(width, args.height, out, args.driver)
            walker.run()
        except Exception as error:
            results.append({'surface': 'walk', 'width': width, 'status': 'failed', 'reason': str(error)})
            print(f'{width} FAILED: {error}', file=sys.stderr)
        results += walker.results if walker else []
    (out / 'manifest.json').write_text(json.dumps(results, indent=1) + '\n')
    failed = [r for r in results if r['status'] == 'failed']
    count = {status: sum(r['status'] == status for r in results) for status in ('shot', 'skipped', 'failed')}
    print(f'{count["shot"]} shot, {count["skipped"]} skipped, {count["failed"]} failed; screenshots in {out}')
    return 1 if failed else 0


def sheet(args):
    if not shutil.which('montage'):
        sys.exit('sheet needs ImageMagick montage on PATH')
    dirs = [Path(d) for d in args.dirs]
    missing = [str(d) for d in dirs if not (d / 'manifest.json').exists()]
    if missing:
        sys.exit(f'No manifest.json in {", ".join(missing)}; sheet needs a finished walk')
    manifests = [json.loads((d / 'manifest.json').read_text()) for d in dirs]
    shots = [{(str(r['width']), r['surface']) for r in m if r['status'] == 'shot'} for m in manifests]
    order = list(dict.fromkeys((str(r['width']), r['surface']) for m in manifests for r in m if r['status'] == 'shot'))
    out = Path(args.out or dirs[-1] / 'sheets')
    out.mkdir(parents=True, exist_ok=True)
    for width in sorted({w for w, _ in order}, key=int, reverse=True):
        surfaces = [s for w, s in order if w == width]
        tiles = []
        for surface in surfaces:
            for d, found, name in zip(dirs, shots, ['before', 'after'] if len(dirs) == 2 else ['']):
                label = f'{surface} · {name}'.strip(' ·')
                if (width, surface) in found:
                    tiles.append((label, d / width / f'{surface}.png'))
                else:
                    tiles.append((f'{label} (none)', 'xc:#eeeeee'))
        per_sheet = 16 if len(dirs) == 2 else 24
        columns = 2 if len(dirs) == 2 else 4
        for index in range(0, len(tiles), per_sheet):
            command = ['montage', '-pointsize', '16', '-background', '#d8d8d8']
            for label, path in tiles[index:index + per_sheet]:
                command += ['-label', label, str(path)]
            target = out / f'sheet-{width}-{index // per_sheet + 1}.png'
            command += ['-tile', f'{columns}x', '-geometry', '640x+8+8', str(target)]
            subprocess.run(command, check=True)
            print(target)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='command', required=True)
    walk_parser = sub.add_parser('walk')
    walk_parser.add_argument('--width', type=int, action='append')
    walk_parser.add_argument('--height', type=int, default=1080)
    walk_parser.add_argument('--out')
    walk_parser.add_argument('--driver')
    sheet_parser = sub.add_parser('sheet')
    sheet_parser.add_argument('dirs', nargs='+', metavar='DIR')
    sheet_parser.add_argument('--out')
    args = parser.parse_args()
    if args.command == 'sheet' and len(args.dirs) > 2:
        parser.error('sheet takes one walk, or a before and an after walk')
    return walk(args) if args.command == 'walk' else sheet(args)


if __name__ == '__main__':
    sys.exit(main())
