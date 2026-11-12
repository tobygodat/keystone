#!/usr/bin/env python3
"""Edit diff journey for Keystone on Linux (TOB-310).

Drives the real debug app on the verify skill's private headless Sway, with real
Wayland keyboard and pointer input and the scripted providers, which replay the edits
recorded in crates/keystone-agent/tests/fixtures/edits. The journey: Ctrl+N, a draft,
Ctrl+Q and a relaunch bring the session and its draft back; a Claude session's Edit and
Write rows and a Codex session's fileChange row each start collapsed, open to their own
diff by mouse and by keyboard (Enter, then Space), and close again; a 1,500-line edit
draws a 40-line preview, opens whole in the diff view by mouse and by Ctrl+Shift+D,
says there where it was cut, and costs typing and a Down key no more than when closed; after a quit and relaunch the diffs are still there; the
open rows in light and dark at the default 1100x720; and with the terminal drawer open,
an edit row still opens by mouse and keyboard and the transcript still scrolls.

The harness reads state from elements and the workspace database. It cannot read text
off the screen, so look at the screenshots.

Run from the workspace root after `cargo build -p keystone-desktop -p keystone-agent --bins --locked`:
    python3 scripts/edit-diff-journey.py [--driver PATH]
Evidence goes to .audit/edit-diff-journey-<time>/: numbered PNGs and journey.json.
"""

import argparse
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parent.parent
DRAFT = 'Draft kept across the edit diff journey'
DEFAULT = (1100, 720)


def load_driver():
    spec = importlib.util.spec_from_file_location('native_driver', ROOT / 'scripts/native-driver.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Failure(Exception):
    pass


class Journey:
    def __init__(self, driver, binary):
        self.native = load_driver()
        self.server = self.native.Server(SimpleNamespace(driver=driver, binary=binary,
                                                         trace_wayland=False))
        self.steps = []
        self.shots = []
        self.scratch = ROOT / '.audit' / f'edit-diff-journey-{time.strftime("%Y%m%dT%H%M%S")}'
        self.project = self.scratch / 'project'
        for folder in (self.project, self.scratch / 'claude', self.scratch / 'codex'):
            folder.mkdir(parents=True)
        subprocess.run(['git', 'init', '-q', str(self.project)], check=True)
        self.evidence = None
        self.sway = None

    def call(self, name, **arguments):
        value, _ = self.server.handle(name, arguments)
        return value

    def step(self, name, ok, detail=''):
        self.steps.append({'step': name, 'ok': bool(ok), 'detail': detail})
        print(('ok   ' if ok else 'FAIL ') + name + (f'  ({detail})' if detail else ''), flush=True)
        if not ok:
            raise Failure(name)

    def poll(self, what, check, seconds=20):
        """Like `wait`, polling every 5 ms, for timings."""
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if check():
                return
            time.sleep(0.005)
        raise Failure(f'timed out waiting for {what}')

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

    def visible(self, name):
        return any(e['id'] == name and e['visible'] for e in self.elements())

    def whole(self, name):
        """`name` is on screen with none of it clipped, so a click lands on it."""
        for element in self.elements():
            if element['id'] == name and element['visible']:
                return (element['visible']['w'], element['visible']['h']) == \
                    (element['bounds']['w'], element['bounds']['h'])
        return False

    def ids(self, pattern, drawn=False):
        """Numbers in the names matching `pattern`: on screen, or with `drawn` anywhere
        in the frame, clipped or not."""
        found = []
        for element in self.elements():
            match = re.fullmatch(pattern, element['id'])
            if match and (drawn or element['visible']):
                found.append(int(match.group(1)))
        return sorted(set(found))

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

    def workspace(self):
        return self.call('workspace')

    def sessions(self):
        return self.workspace()['workspace']['sessions']

    def theme(self, option):
        self.keys('ctrl', 'comma')
        self.wait_for(f'settings-theme-option-{option}')
        self.click(f'settings-theme-option-{option}')
        self.settle()
        self.keys('ctrl', 'comma')
        self.wait_for('composer-field')

    def app_pid(self):
        return self.call('doctor')['processes'][-1]['pid']

    def quit(self):
        pid = self.app_pid()
        self.keys('ctrl', 'q')

        def exit_record():
            return next((p.get('exit') for p in self.call('doctor')['processes']
                         if p['pid'] == pid), None)
        self.wait('the app to exit', exit_record, 30)
        code = exit_record()
        self.step('Ctrl+Q quits gracefully', code.get('returncode') == 0, json.dumps(code))

    def relaunch(self):
        self.call('launch')
        self.wait_for('composer-field', seconds=60)
        doctor = self.call('doctor')
        self.step('the relaunched build runs', doctor.get('binary_unchanged') is True)

    def send(self, text):
        self.click('composer-field')
        self.type(text)
        self.keys('ctrl', 'Return')

    def edit_rows(self, count, seconds=30):
        self.wait(f'{count} edit rows', lambda: len(self.ids(r'activity-collapsed-(\d+)')) == count, seconds)
        return self.ids(r'activity-collapsed-(\d+)')

    def lines(self, row):
        return len(self.ids(rf'edit-{row}-line-(\d+)', drawn=True))

    def by_mouse(self, row, label):
        self.step(f'{label}: row {row} starts collapsed',
                  self.visible(f'activity-collapsed-{row}') and not self.visible(f'activity-body-{row}'))
        self.click(f'activity-collapsed-{row}')
        self.wait_for(f'activity-body-{row}')
        drawn = self.lines(row)
        self.step(f'{label}: a click opens row {row} to its diff', drawn > 0, f'{drawn} lines drawn')
        return drawn

    def revision(self):
        """The app's frame count, from the geometry it writes each frame."""
        return json.loads(self.server.run.geometry.read_text())['revision']

    def settle(self, quiet=0.1, seconds=10):
        """Waits until the app draws no new frame for `quiet` seconds: it has handled what
        it was sent."""
        deadline = time.monotonic() + seconds
        last, since = self.revision(), time.monotonic()
        while time.monotonic() < deadline:
            time.sleep(0.01)
            now = self.revision()
            if now != last:
                last, since = now, time.monotonic()
            elif time.monotonic() - since >= quiet:
                return
        raise Failure('the app kept drawing')

    def closed(self, row):
        """Row `row` is drawn closed: its toggle says collapsed and no body is drawn, as
        opposed to an open body that merely scrolled out of view."""
        drawn = {e['id'] for e in self.elements()}
        return f'activity-collapsed-{row}' in drawn and f'activity-body-{row}' not in drawn

    def close_by_mouse(self, row, label):
        self.click(f'activity-expanded-{row}')
        self.wait(f'row {row} to close', lambda: self.closed(row))
        self.step(f'{label}: a click closes row {row}', True)

    def by_keyboard(self, row, label):
        self.keys('ctrl', 'shift', 't')
        self.keys('Home')
        for _ in range(row):
            self.keys('Down')
        self.keys('Tab')
        self.settle()
        self.shot(f'{label}-focus-ring')
        self.keys('Return')
        self.wait_for(f'activity-body-{row}')
        self.step(f'{label}: Enter opens row {row}', self.lines(row) > 0)
        self.keys('space')
        self.wait(f'row {row} to close', lambda: self.closed(row))
        self.step(f'{label}: Space closes row {row}', True)
        self.keys('Escape')

    def reach(self, name, over='conversation', vertical=10):
        """Wheels `over`, down by default, until `name` shows whole and stays put."""
        for _ in range(400):
            self.settle()
            if self.whole(name):
                return
            self.call('wheel', id=over, vertical=vertical)
        raise Failure(f'{name} never scrolled into view')

    def select(self, row):
        """Selects transcript row `row` from the keyboard, which scrolls it into view."""
        self.keys('ctrl', 'shift', 't')
        self.keys('Home')
        for _ in range(row):
            self.keys('Down')
        self.settle()

    def key_toggle(self, row, open_after, label):
        self.select(row)
        self.keys('Tab')
        self.keys('Return')
        if open_after:
            self.wait_for(f'activity-body-{row}')
        else:
            self.wait(f'row {row} to close', lambda: self.closed(row))
        self.step(f'{label}: Enter {"opens" if open_after else "closes"} row {row}', True)
        self.keys('Escape')

    def typing_ms(self, text):
        """Types `text` into the composer and times it until the saved draft holds it."""
        session = self.sessions()[-1]['id']
        self.click('composer-field')
        self.keys('ctrl', 'a')
        self.keys('BackSpace')
        self.wait('the draft to clear', lambda: not self.draft(session), 10)
        start = time.monotonic()
        self.type(text)
        self.poll('the typed draft to save', lambda: self.draft(session) == text, 20)
        return round((time.monotonic() - start) * 1000)

    def draft(self, session):
        return next(s for s in self.sessions() if s['id'] == session).get('draft')

    def down_ms(self, row):
        """From transcript row `row`, times a Down key until the app draws its next frame."""
        self.select(row)
        before = self.revision()
        start = time.monotonic()
        self.keys('Down')
        self.poll('a frame after Down', lambda: self.revision() != before, 5)
        took = round((time.monotonic() - start) * 1000)
        self.keys('Escape')
        return took

    def full_edit(self, row, how, label):
        """Opens the whole edit at `row` in the diff view, by mouse or keyboard, checks it
        ends with the note that it was cut, and goes back."""
        if how == 'mouse':
            self.click(f'edit-open-{row}')
        elif how == 'tab':
            self.select(row)
            self.keys('Tab')
            self.keys('Tab')
            self.settle()
            self.shot(f'{label}-open-full-edit-focused')
            self.keys('Return')
        else:
            self.select(row)
            self.keys('ctrl', 'shift', 'd')
        self.wait_for('diff-lines')
        self.step(f'{label}: the {how} opens the full edit in the diff view', True)
        self.step(f'{label}: the full edit takes no comments', not self.visible('diff-comment'))
        self.settle()
        self.shot(f'{label}-full-edit-{how}')
        self.reach('diff-truncated', over='diff-lines')
        self.shot(f'{label}-full-edit-end')
        self.step(f'{label}: the full edit says where its kept lines end', True)
        self.keys('Escape')
        self.wait_for('diff-lines', present=False)
        self.wait(f'row {row} back, still open', lambda: self.visible(f'edit-more-{row}')
                  or self.visible(f'activity-expanded-{row}'))

    def copied(self, row, label):
        """The reviewer's route: Ctrl+Shift+T, Home, Down to `row`, Ctrl+Shift+C, then
        what `wl-paste` reads from the app's own desktop."""
        self.select(row)
        socket = self.server.run.session['wayland_socket']
        subprocess.run(['wl-copy', '--clear'], env={**os.environ, 'WAYLAND_DISPLAY': str(socket)})
        self.keys('ctrl', 'shift', 'c')
        self.keys('Escape')

        def paste():
            return subprocess.run(['wl-paste', '--no-newline'], capture_output=True, text=True,
                                  env={**os.environ, 'WAYLAND_DISPLAY': str(socket)}).stdout
        self.wait('the copy', lambda: paste().strip() != '', 10)
        text = paste()
        (self.scratch / f'copied-{label}.diff').write_text(text)
        return text

    def copied_paths_are_relative(self, row, label):
        text = self.copied(row, label)
        headers = [line for line in text.splitlines() if line.startswith(('--- ', '+++ '))]
        relative = all(h[4:] == '/dev/null' or (h[4:6] in ('a/', 'b/') and not h[6:].startswith('/'))
                       for h in headers)
        self.step(f'{label}: Ctrl+Shift+C copies a diff with paths from the session folder',
                  headers and relative and str(self.project) not in text, '; '.join(headers))
        return text

    def scroll_timing(self, notches):
        """Wheel steps over the transcript, each timed until a frame shows the rows moved.
        A step at an end of the transcript moves nothing and is left out."""
        times = []
        for direction in notches:
            self.settle()
            rows = lambda: {e['id']: e['bounds']['y'] for e in self.elements()
                            if e['id'].startswith('transcript-row-')}
            before = rows()
            start = time.monotonic()
            self.call('wheel', id='conversation', vertical=direction)
            try:
                self.poll('the transcript to scroll', lambda: rows() != before, 1)
            except Failure:
                continue
            times.append(round((time.monotonic() - start) * 1000))
        return times

    def run(self):
        env = {
            'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
            'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
            'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '2',
            'KEYSTONE_FAKE_CODEX_DELAY_MS': '2',
            'KEYSTONE_PROJECT_DIR': str(self.project),
            'CLAUDE_CONFIG_DIR': str(self.scratch / 'claude'),
            'CODEX_HOME': str(self.scratch / 'codex'),
        }
        started = self.call('start', width=DEFAULT[0], height=DEFAULT[1], env=env)
        self.evidence = started['evidence']
        self.wait_for('welcome-claude', seconds=60)
        doctor = self.call('doctor')
        self.step('doctor: the launched build runs', doctor.get('binary_unchanged') is True)

        self.keys('ctrl', 'n')
        self.wait('a Claude session', lambda: len(self.sessions()) == 1)
        self.wait_for('composer-field')
        self.click('composer-field')
        self.type(DRAFT)
        self.wait('the draft to save', lambda: self.sessions()[0].get('draft') == DRAFT, 10)
        self.step('Ctrl+N makes a Claude session, and its draft saves', True)
        self.quit()
        self.relaunch()
        self.step('after a relaunch the session and its draft come back',
                  len(self.sessions()) == 1 and self.sessions()[0].get('draft') == DRAFT)
        self.shot('relaunched-with-draft')
        self.click('composer-field')
        self.keys('ctrl', 'a')
        self.keys('BackSpace')

        self.send('replay edits')
        claude = self.edit_rows(5)
        self.wait_for('send')
        self.shot('claude-edits-collapsed')
        expected = [6, 4, 2, 2, 4]
        drawn = []
        for row in claude:
            drawn.append(self.by_mouse(row, 'Claude'))
            if row == claude[0]:
                self.shot('claude-edit-open')
            self.close_by_mouse(row, 'Claude')
        self.step('each Claude row draws its own edit', drawn == expected, f'{drawn}')
        self.by_keyboard(claude[0], 'claude')
        self.copied_paths_are_relative(claude[0], 'claude')

        self.keys('ctrl', 'shift', 'n')
        self.wait('a Codex session', lambda: len(self.sessions()) == 2)
        self.wait_for('composer-field')
        self.send('replay edits')
        codex = self.edit_rows(1)
        self.wait_for('send')
        lines = self.by_mouse(codex[0], 'Codex')
        self.step('the Codex row draws all three files', lines == 7, f'{lines} lines')
        self.shot('codex-edit-open-dark')
        self.theme(1)
        self.shot('codex-edit-open-light')
        self.theme(2)
        self.close_by_mouse(codex[0], 'Codex')
        self.by_keyboard(codex[0], 'codex')
        self.copied_paths_are_relative(codex[0], 'codex')

        self.send('long edit 1500')
        rows = self.edit_rows(2)
        long = rows[-1]
        self.wait_for('send')
        typing = {'closed': [], 'open': []}
        down = {'closed': [], 'open': []}
        for state in ['closed', 'open'] * 3:
            if state == 'open':
                self.click(f'activity-collapsed-{long}')
                self.wait_for(f'activity-body-{long}')
            typing[state].append(self.typing_ms('Thirty characters typed here..'))
            down[state].append(self.down_ms(codex[0]))
            if state == 'open':
                self.reach(f'activity-expanded-{long}', vertical=-3)
                self.click(f'activity-expanded-{long}')
                self.wait(f'row {long} to close', lambda: self.closed(long))
        self.step('typing 30 characters until the draft is saved, edit closed vs open',
                  True, f'ms closed {typing["closed"]} open {typing["open"]}')
        self.step('a Down key until the next frame, edit closed vs open',
                  True, f'ms closed {down["closed"]} open {down["open"]}')
        self.click('composer-field')
        self.keys('ctrl', 'a')
        self.keys('BackSpace')
        self.reach(f'activity-collapsed-{long}', vertical=-3)
        self.click(f'activity-collapsed-{long}')
        self.wait_for(f'activity-body-{long}')
        drawn = self.lines(long)
        self.step('the open 1,000-line edit draws its 40-line preview', drawn == 40, f'{drawn} lines')
        self.reach(f'edit-open-{long}')
        self.step('it ends with how many lines are left', self.visible(f'edit-more-{long}'))
        self.shot('long-edit-preview')
        floor = []
        for _ in range(10):
            start = time.monotonic()
            self.elements()
            floor.append(round((time.monotonic() - start) * 1000))
        times = self.scroll_timing([-2] * 6 + [2] * 6)
        self.step('the transcript scrolls over the open long edit, a frame per wheel step',
                  len(times) >= 6 and max(times) < 500,
                  f'ms per moving step {times}; one elements read takes {floor} ms')
        self.reach(f'edit-open-{long}')
        self.full_edit(long, 'mouse', 'long')
        self.full_edit(long, 'keyboard', 'long')
        self.full_edit(long, 'tab', 'long')
        text = self.copied(long, 'long')
        self.step('a capped edit copies with a line saying it was cut',
                  text.startswith('Keystone kept only the first 1000 lines of this edit'),
                  text.splitlines()[0] if text else '')

        self.keys('ctrl', 'j')
        self.wait_for('terminal-drawer')
        self.wait_for('terminal-pane-0')
        self.step('Ctrl+J opens the terminal drawer under the transcript', True)
        # A plain prompt, so screenshots carry no user or host name.
        self.type("PS1='$ '; clear")
        self.keys('Return')
        # Ctrl+Shift+T belongs to the shell while it has focus (TOB-301), so leave it first.
        self.click('composer-field')
        self.key_toggle(long, open_after=False, label='with the terminal open')
        self.select(codex[0])
        lines = self.by_mouse(codex[0], 'with the terminal open')
        self.step('with the terminal open the Codex row draws all three files', lines == 7, f'{lines} lines')
        self.shot('codex-edit-open-with-terminal')
        self.close_by_mouse(codex[0], 'with the terminal open')
        self.by_keyboard(codex[0], 'with-terminal')
        self.step('the terminal drawer stayed open through the keys', self.visible('terminal-drawer'))
        self.key_toggle(long, open_after=True, label='with the terminal open')
        times = self.scroll_timing([-2] * 6 + [2] * 6)
        self.step('with the terminal open the transcript still scrolls over the open edit',
                  len(times) >= 6 and max(times) < 500, f'ms per moving step {times}')
        self.shot('long-edit-open-with-terminal')

        self.quit()
        self.relaunch()
        sessions = self.sessions()
        claude_session = next(s for s in sessions if s.get('provider') == 'Claude')['id']
        self.wait_for(f'session-{claude_session}')
        self.click(f'session-{claude_session}')
        restored = self.edit_rows(5)
        again = []
        for row in restored:
            again.append(self.by_mouse(row, 'restored Claude'))
            if row == restored[0]:
                self.shot('restored-claude-edit-open-dark')
                self.theme(1)
                self.shot('restored-claude-edit-open-light')
                self.theme(2)
            self.close_by_mouse(row, 'restored Claude')
        self.step('after a relaunch each Claude row still draws its own edit', again == expected, f'{again}')
        codex_session = next(s for s in sessions if s.get('provider') == 'Codex')['id']
        self.click(f'session-{codex_session}')
        rows = self.edit_rows(2)
        self.step('after a relaunch the Codex row still draws its three files',
                  self.by_mouse(rows[0], 'restored Codex') == 7)
        self.close_by_mouse(rows[0], 'restored Codex')
        self.click(f'activity-collapsed-{rows[-1]}')
        self.wait_for(f'activity-body-{rows[-1]}')
        self.reach(f'edit-open-{rows[-1]}')
        self.full_edit(rows[-1], 'mouse', 'restored')

        # An edit to a file with no final newline, as Claude Code's jsdiff writes it.
        self.keys('ctrl', 'n')
        self.wait('a third session', lambda: len(self.sessions()) == 3)
        self.wait_for('composer-field')
        self.send('edit without newline')
        rows = self.edit_rows(1)
        self.wait_for('send')
        lines = self.by_mouse(rows[0], 'no final newline')
        self.step('the edit draws its four lines; the marker is not a line', lines == 4, f'{lines}')
        self.shot('no-newline-preview')
        self.select(rows[0])
        self.keys('ctrl', 'shift', 'd')
        self.wait_for('diff-lines')
        self.settle()
        self.shot('no-newline-full-edit')
        self.keys('Escape')
        self.wait_for('diff-lines', present=False)
        text = self.copied(rows[0], 'no-newline')
        self.step('the copy ends with the marker after the last line',
                  text.endswith(' three\n\\ No newline at end of file\n'), text.splitlines()[-1])
        target = self.scratch / 'apply'
        target.mkdir()
        (target / 'a.txt').write_bytes(b'one\ntwo\nthree')
        git = ['git', '-c', 'core.autocrlf=false', 'apply']
        check = subprocess.run([*git, '--check'], input=text, text=True, cwd=target, capture_output=True)
        applied = subprocess.run(git, input=text, text=True, cwd=target, capture_output=True)
        result = (target / 'a.txt').read_bytes()
        (self.scratch / 'git-apply-no-newline.txt').write_text(
            f'check: {check.returncode} {check.stderr}\napply: {applied.returncode} {applied.stderr}\n'
            f'a.txt after: {result!r}\n')
        self.step('git apply --check, then git apply, of the copy to a file with no final newline',
                  check.returncode == 0 and applied.returncode == 0, f'{check.stderr}{applied.stderr}')
        self.step('the file is byte for byte what the edit made', result == b'one\nTWO\nthree', repr(result))
        self.quit()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--driver', default=None, help='computer-use-mcp with the verify patches')
    parser.add_argument('--binary', default=str(ROOT / 'target/debug/keystone-desktop'))
    args = parser.parse_args()
    native = load_driver()
    driver = args.driver or str(native.default_driver())
    journey = Journey(driver, args.binary)
    error = None
    try:
        journey.run()
    except Exception as failure:
        error = f'{type(failure).__name__}: {failure}'
        print(f'FAIL {error}', flush=True)
        try:
            journey.shot('failure')
        except Exception:
            pass
    finally:
        try:
            stopped = journey.call('stop')
            journey.steps.append({'step': 'stop', 'ok': not stopped.get('errors'), 'detail': json.dumps(stopped)})
        except Exception as stop_error:
            journey.steps.append({'step': 'stop', 'ok': False, 'detail': str(stop_error)})
        report = {'error': error, 'evidence': journey.evidence, 'steps': journey.steps, 'shots': journey.shots}
        (journey.scratch / 'journey.json').write_text(json.dumps(report, indent=1) + '\n')
        print(f'evidence: {journey.scratch}')
    if error:
        sys.exit(f'edit-diff-journey failed: {error}')
    print('edit-diff-journey passed')


if __name__ == '__main__':
    main()
