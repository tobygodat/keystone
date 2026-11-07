#!/usr/bin/env python3
"""The Files editor's journey on macOS, in the real debug app through the automation inbox.

Opens src/lib.rs from Files with the pencil button and saves it with the Save button,
then opens it with Space and saves it with Cmd+S. Takes an agent's outside edit without
asking, shows the conflict bar for an outside edit to an edited file and checks Save,
Reload and Overwrite, refuses Cmd+Q while edits are unsaved, and asks before Escape
discards them. Ends with the session and draft coming back after a relaunch. Each step
checks the file on disk and the named elements.

Run from the workspace root: python3 scripts/macos-file-editor.py [--no-build]
Evidence goes to .audit/macos-file-editor-<time>/: numbered PNGs and report.json.
"""

import argparse
import importlib.util
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('macos_smoke', ROOT / 'scripts/macos-smoke.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

LIB = 'pub fn one() {}\npub fn two() {}\n'


class Journey(smoke.Run):
    background = False

    def env(self):
        env = super().env()
        if self.background:
            # Keeps the app behind the windows of whoever is at the Mac.
            env['KEYSTONE_AUTOMATION_BACKGROUND'] = '1'
        return env

    def disk(self):
        return (self.project / 'src/lib.rs').read_text()

    def until(self, test, what, seconds=10):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if test():
                return self.step(what, True)
            time.sleep(0.1)
        self.step(what, False, 'timed out')

    def journey(self):
        (self.project / 'src').mkdir()
        (self.project / 'src/lib.rs').write_text(LIB)
        self.launch()
        self.key('secondary-n')
        self.wait_for('composer-field', 90)
        self.key('secondary-shift-e')
        self.wait_for('files-tree-row-1')
        self.key('down')
        self.key('right')
        self.wait_for('files-tree-row-2')
        self.key('right')
        self.wait_for('view-file')

        # By mouse: the pencil button opens it, a click puts the caret in it, Save writes it.
        self.click('view-file')
        self.wait_for('file-editor')
        self.step('the pencil button opens the editor', 'file-editor-save' in self.elements())
        self.shot('editor-open')
        self.click('file-editor')
        self.key('secondary-up')
        self.type('// mouse ')
        self.wait_for('file-editor-edited')
        self.step('typing marks it Edited and writes nothing', self.disk() == LIB)
        self.shot('edited')
        self.click('file-editor-save')
        self.wait_for('file-editor-edited', present=False)
        self.step('the Save button writes it', self.disk() == '// mouse ' + LIB, repr(self.disk()))
        self.click('file-view-back')
        self.wait_for('files-tree-row-2')
        self.step('Back closes an unedited file at once', 'file-editor' not in self.elements())

        # By keyboard: End highlights the file (the pointer left over the tree highlights
        # the row under it), Space opens it, Cmd+S writes it.
        self.key('end')
        self.key('space')
        self.wait_for('file-editor')
        self.step('Space opens the highlighted file in the editor', True)
        self.type('// keys ')
        self.wait_for('file-editor-edited')
        self.key('secondary-s')
        self.wait_for('file-editor-edited', present=False)
        self.step('Cmd+S writes it', self.disk() == '// keys // mouse ' + LIB, repr(self.disk()))
        self.shot('saved')

        agent = 'pub fn from_the_agent() {}\n'
        (self.project / 'src/lib.rs').write_text(agent)
        time.sleep(2.5)
        self.step('an outside edit to an unedited file asks nothing',
                  'file-editor-conflict' not in self.elements() and 'file-editor-edited' not in self.elements())
        self.shot('outside-edit-reloaded')
        self.key('secondary-up')
        self.type('// after ')
        self.key('secondary-s')
        self.until(lambda: self.disk() == '// after ' + agent, 'the editor held the outside edit when saved')

        self.type('mine ')
        self.wait_for('file-editor-edited')
        theirs = 'pub fn theirs() {}\n'
        (self.project / 'src/lib.rs').write_text(theirs)
        self.wait_for('file-editor-conflict', 5)
        self.key('secondary-s')
        time.sleep(1)
        self.step('Save writes nothing over the outside edit', self.disk() == theirs, repr(self.disk()))
        self.shot('conflict')
        self.click('file-editor-reload')
        self.wait_for('file-editor-conflict', present=False)
        self.step('Reload takes the disk text', 'file-editor-edited' not in self.elements())
        self.shot('reloaded')

        self.key('secondary-up')
        self.type('// mine ')
        self.wait_for('file-editor-edited')
        (self.project / 'src/lib.rs').write_text('pub fn theirs_again() {}\n')
        self.wait_for('file-editor-conflict', 5)
        self.click('file-editor-overwrite')
        self.until(lambda: self.disk() == '// mine ' + theirs, 'Overwrite writes the editor text over the outside edit')
        self.wait_for('file-editor-conflict', present=False)

        self.type('x')
        self.wait_for('file-editor-edited')
        self.key('secondary-q')
        self.wait_for('shutdown-error', 5)
        self.step('Cmd+Q is refused while edits are unsaved', self.app.poll() is None)
        self.shot('quit-refused')

        self.key('escape')
        self.wait_for('file-editor-discard')
        self.shot('discard-dialog')
        self.click('file-editor-discard-cancel')
        self.wait_for('file-editor-discard', present=False)
        self.step('Cancel keeps the edits', 'file-editor-edited' in self.elements())
        self.key('escape')
        self.wait_for('file-editor-discard')
        self.click('file-editor-discard-confirm')
        self.wait_for('files-tree-row-1')
        self.step('Discard closes the editor and leaves the file', self.disk() == '// mine ' + theirs)
        self.shot('tree')

        draft = 'Draft kept across restart'
        self.click('composer-field')
        self.type(draft)
        time.sleep(1)
        self.quit()
        self.launch()
        sessions = self.sessions()
        self.step('the session and its draft come back after relaunch',
                  len(sessions) == 1 and sessions[0].get('draft') == draft, repr(sessions))
        self.shot('restored')
        self.quit()


def main():
    if sys.platform != 'darwin':
        sys.exit('macos-file-editor runs on macOS. On Linux, use the verify skill.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    parser.add_argument('--background', action='store_true',
                        help='launch the app behind other windows; a fully covered window draws no frames')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    evidence = ROOT / '.audit' / time.strftime('macos-file-editor-%Y%m%d-%H%M%S')
    evidence.mkdir(parents=True)
    run = Journey(ROOT / 'target/debug/keystone-desktop', evidence)
    run.background = args.background
    error = None
    try:
        run.journey()
    except smoke.Failure as failure:
        error = str(failure)
    finally:
        run.finish(error)
    print(f'evidence: {evidence}')
    if error:
        sys.exit(f'macos-file-editor failed: {error}')
    print('macos-file-editor passed')


if __name__ == '__main__':
    main()
