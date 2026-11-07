#!/usr/bin/env python3
"""Diff viewer journey for Keystone on macOS (TOB-305), on macos-smoke.py's harness.

A fixture repository with a 120,000-line file. One turn of the scripted Claude edits
three files and replaces the large one. The journey opens that turn's diff from the
transcript with the mouse and with the keyboard, steps through its files and lines by
keyboard, comments on lines and sends the comments to the session, then opens the
uncommitted changes from Changes and comments on them with the mouse alone.

Run from the workspace root: python3 scripts/macos-diff.py
Evidence goes to .audit/macos-diff-<time>/: numbered PNGs and report.json.
--background keeps the window behind other apps (KEYSTONE_AUTOMATION_BACKGROUND), so the
run never takes focus from the person at the Mac. A fully covered window draws no frames,
so a background run can stall where a front one passes.
"""

import argparse
import importlib.util
import json
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('macos_smoke', ROOT / 'scripts/macos-smoke.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

LARGE_LINES = 120_000
EDIT = ('write notes.md: Ship the diff viewer; '
        'write src/lib.rs: pub fn greet() -> &str { "hello" }; '
        'write vendor/large.txt: replaced')
FIRST_COMMENT = 'Who reads these notes?'
SECOND_COMMENT = 'Keep the other functions.'
MOUSE_COMMENT = 'Commented with the mouse.'


class Journey(smoke.Run):
    def __init__(self, binary, evidence, background=False):
        super().__init__(binary, evidence)
        self.background = background
        git = lambda *args: subprocess.run(['git', '-C', str(self.project), *args], check=True,
                                           capture_output=True)
        git('config', 'user.name', 'Fixture')
        git('config', 'user.email', 'fixture@example.com')
        git('config', 'commit.gpgsign', 'false')
        (self.project / 'notes.md').write_text('Draft notes\n')
        (self.project / 'src').mkdir()
        (self.project / 'src/lib.rs').write_text('pub fn a() {}\npub fn b() {}\npub fn c() {}\n')
        (self.project / 'vendor').mkdir()
        (self.project / 'vendor/large.txt').write_text(
            ''.join(f'generated line {n}\n' for n in range(LARGE_LINES)))
        git('add', '-A')
        git('commit', '-q', '-m', 'fixture')

    def env(self):
        env = super().env()
        if self.background:
            env['KEYSTONE_AUTOMATION_BACKGROUND'] = '1'
        return env

    def named(self, prefix):
        return sorted(name for name in self.elements() if name.startswith(prefix))

    def messages(self):
        entries = [json.loads(row[0]) for row in self.read('SELECT payload FROM log_entries')]
        return [entry['item']['text'] for entry in entries
                if entry.get('entry') == 'item' and entry['item']['kind']['type'] == 'user_message'
                and entry['item']['text'].startswith('Comments on')]

    def wait_message(self, needle, seconds=30):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            found = [text for text in self.messages() if needle in text]
            if found:
                return found[-1]
            time.sleep(0.2)
        raise smoke.Failure(f'no sent message holds {needle!r}')

    def open_timed(self, action, label):
        started = time.monotonic()
        action()
        self.wait_for('diff-view', 10)
        self.wait_for('diff-lines', 30)
        seconds = time.monotonic() - started
        self.step(f'{label}: the diff shows', True, f'{seconds:.2f} s to lines')
        return seconds

    def journey(self):
        self.launch()
        self.key('secondary-n')
        self.wait_for('composer-field')
        self.click('composer-field')
        self.type(EDIT)
        self.key('secondary-enter')
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and not self.named('view-changes-'):
            time.sleep(0.2)
        buttons = self.named('view-changes-')
        self.step('the turn shows its changed files as a button', len(buttons) == 1, str(buttons))
        self.shot('turn-with-files')

        self.key('secondary-shift-t')
        time.sleep(0.5)
        self.key('end')
        time.sleep(0.5)
        self.open_timed(lambda: self.key('secondary-shift-d'), 'keyboard, Cmd+Shift+D on the reply')
        self.shot('turn-diff-keyboard')
        self.key('escape')
        self.wait_for('diff-view', present=False)

        self.open_timed(lambda: self.click(buttons[0]), 'mouse, files button')
        self.shot('turn-diff')
        self.key('down')
        self.key('down')
        self.key('c')
        self.wait_for('diff-comment-input')
        self.type(FIRST_COMMENT)
        self.key('enter')
        self.wait_for('diff-comment-0')
        self.step('Down, C, typing and Enter leave a comment', True)
        self.key('n')
        for _ in range(3):
            self.key('shift-down')
        self.shot('second-file-selected')
        self.key('c')
        self.wait_for('diff-comment-input')
        self.type(SECOND_COMMENT)
        self.key('enter')
        self.wait_for('diff-comment-1')
        self.step('N and Shift+Down select lines in the next file', True)
        self.key('n')
        self.shot('large-file')
        self.key('p')
        self.key('p')
        self.shot('two-comments')
        self.key('secondary-enter')
        text = self.wait_message(FIRST_COMMENT, 60)
        self.step('Cmd+Enter sends both comments as one message',
                  SECOND_COMMENT in text and 'notes.md line 1:' in text
                  and 'src/lib.rs line 1:' in text, repr(text[:300]))
        self.wait_for('diff-comment-0', 60, present=False)
        self.shot('sent')

        self.key('escape')
        self.wait_for('diff-view', present=False)
        self.step('Escape returns to the conversation', True)

        self.key('secondary-shift-g')
        self.wait_for('changes-diff')
        self.open_timed(lambda: self.click('changes-diff'), 'mouse, Changes button')
        self.shot('uncommitted-diff')
        self.click('diff-next-file')
        time.sleep(1)
        self.click('diff-previous-file')
        self.wait_for('diff-row-2')
        time.sleep(1)
        self.click('diff-row-2')
        self.click('diff-comment')
        self.wait_for('diff-comment-input')
        self.type(MOUSE_COMMENT)
        self.click('diff-comment-save')
        self.wait_for('diff-comment-0')
        self.shot('mouse-comment')
        self.click('diff-send')
        text = self.wait_message(MOUSE_COMMENT, 60)
        self.step('the Send button sends the comment',
                  text.startswith('Comments on the uncommitted changes:'), repr(text[:200]))
        self.click('diff-back')
        self.wait_for('diff-view', present=False)
        self.shot('back')
        self.quit()


def main():
    if sys.platform != 'darwin':
        sys.exit('macos-diff runs on macOS. On Linux, use the verify skill.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    parser.add_argument('--background', action='store_true', help='keep the window behind other apps')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    binary = ROOT / 'target/debug/keystone-desktop'
    evidence = ROOT / '.audit' / time.strftime('macos-diff-%Y%m%d-%H%M%S')
    evidence.mkdir(parents=True)
    run = Journey(binary, evidence, args.background)
    error = None
    try:
        run.journey()
    except smoke.Failure as failure:
        error = str(failure)
    finally:
        run.finish(error)
    print(f'evidence: {evidence}')
    if error:
        sys.exit(f'macos-diff failed: {error}')
    print('macos-diff passed')


if __name__ == '__main__':
    main()
