#!/usr/bin/env python3
"""Native journey for choosing model, effort and speed after the first Send (TOB-297).

macOS only, like scripts/macos-smoke.py, whose harness it reuses: the real debug app,
the scripted Claude, a throwaway home, workspace and project, input through the
automation inbox and screenshots from GPUI's renderer. The app runs in the background
(KEYSTONE_AUTOMATION_BACKGROUND) unless --foreground is given, so it never takes the
pointer or the front window.

The journey: Cmd+N, look at the menu before the first Send, send, then change the
effort with the mouse, fast mode and the next effort level with their shortcuts, and the
model with the mouse and with the palette's Next model. After each change the next
reply must name what the scripted Claude actually ran, and the transcript must show a
note. Then a draft, Cmd+Q, relaunch, and everything must come back.

Run from the workspace root: python3 scripts/model-choice-journey.py
Evidence goes to .audit/model-choice-<time>/: numbered PNGs and report.json.
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
Failure = smoke.Failure

DRAFT = 'Draft kept with the model choice'


class Journey(smoke.Run):
    background = True

    def env(self):
        env = super().env()
        # The scripted reply names the model, effort and speed the process ran with.
        env.pop('KEYSTONE_FAKE_CLAUDE_REPLY')
        if self.background:
            env['KEYSTONE_AUTOMATION_BACKGROUND'] = '1'
        return env

    def entries(self):
        return [row[0] for row in self.read('SELECT payload FROM log_entries')]

    def reply_to(self, text, seconds=30):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            for payload in self.entries():
                if f'You wrote: {text}' in payload and 'agent_message' in payload:
                    return json.loads(payload)['item']['text']
            time.sleep(0.2)
        raise Failure(f'no reply to {text!r} in {seconds} s')

    def send(self, text):
        self.click('composer-field')
        self.type(text)
        self.key('secondary-enter')
        reply = self.reply_to(text)
        self.wait_for('turn-status', 30, present=False)
        return reply

    def notes(self):
        return [json.loads(p)['text'] for p in self.entries() if '"note"' in p and 'Next turns' in p]

    def choice(self):
        session = self.sessions()[0]
        return session.get('model'), session.get('effort'), session.get('speed')

    def wait_for(self, name, seconds=20, present=True):
        try:
            super().wait_for(name, seconds, present)
        except Failure:
            (self.evidence / 'elements-at-failure.json').write_text(
                json.dumps(self.elements(), indent=1, sort_keys=True) + '\n')
            try:
                self.command(f'capture {self.evidence / "at-failure.png"}', 30)
            except Failure:
                pass
            raise

    def command(self, text, seconds=10):
        # macos-smoke reads the answer as soon as its file exists, which can be before
        # the app has written it; wait for the text.
        self.sequence += 1
        path = self.inbox / f'{self.sequence:05}.cmd'
        staging = path.with_suffix('.tmp')
        staging.write_text(text)
        staging.rename(path)
        done = path.with_suffix('.done')
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            answer = done.read_text() if done.exists() else ''
            if answer:
                if answer != 'ok':
                    raise Failure(f'{text!r}: {answer}')
                return
            time.sleep(0.05)
        raise Failure(f'{text!r} got no answer in {seconds} s')

    def open_menu(self):
        # The button's label changes as the provider reports its model, so a click can
        # land on a frame that is already gone; try again before calling it a failure.
        for _ in range(3):
            self.click('model-selector')
            try:
                smoke.Run.wait_for(self, 'model-column', 5)
                return
            except Failure:
                pass
        self.wait_for('model-column', 1)

    def journey(self):
        self.launch()
        self.key('secondary-n')
        self.wait_for('composer-field')
        self.open_menu()
        self.wait_for('model-fake-opus', 60)
        self.wait_for('effort-default')
        self.step('before the first Send the menu offers Default effort', True)
        self.shot('menu-before-send')
        self.key('escape')

        reply = self.send('first')
        self.step('the first turn runs the provider default', '**Model:** `fake-opus`' in reply
                  and '**Effort:**' not in reply, reply)

        self.open_menu()
        self.wait_for('effort-high', 60)
        elements = self.elements()
        self.step('after the first Send the provider column is gone',
                  'provider-claude' not in elements)
        self.shot('menu-after-send')
        self.click('effort-high')
        self.wait_for('effort-high', present=False)
        self.step('clicking High saves the effort', self.choice()[1] == 'high', repr(self.choice()))

        self.key('secondary-alt-f')
        time.sleep(0.5)
        self.step('Cmd+Alt+F turns fast mode on', self.choice()[2] == 'fast', repr(self.choice()))
        self.key('secondary-alt-e')
        time.sleep(0.5)
        self.step('Cmd+Alt+E moves to the next effort', self.choice()[1] == 'max', repr(self.choice()))

        reply = self.send('second')
        self.step('the next turn runs at max effort with fast mode',
                  '**Effort:** `max`' in reply and '**Fast:** `on`' in reply, reply)
        self.step('each change is noted in the transcript', len(self.notes()) == 3, repr(self.notes()))
        self.shot('notes-and-reply')

        self.open_menu()
        self.wait_for('model-fake-haiku', 60)
        self.click('model-fake-haiku')
        self.wait_for('model-fake-haiku', present=False)
        self.step('clicking Fake Haiku saves the model', self.choice()[0] == 'fake-haiku',
                  repr(self.choice()))
        reply = self.send('third')
        self.step('the next turn runs on Fake Haiku', '**Model:** `fake-haiku`' in reply, reply)

        self.key('secondary-k')
        self.wait_for('palette')
        self.type('Next model')
        time.sleep(0.5)
        self.shot('palette-next-model')
        self.key('enter')
        self.wait_for('palette', present=False)
        time.sleep(0.5)
        model = self.choice()[0]
        self.step('the palette\'s Next model moves on from Fake Haiku',
                  model not in (None, 'fake-haiku'), repr(model))
        reply = self.send('fourth')
        self.step('the next turn runs on that model', f'**Model:** `{model}`' in reply, reply)
        self.shot('after-next-model')

        self.click('composer-field')
        self.type(DRAFT)
        time.sleep(1)
        before = (self.choice(), self.notes())
        self.quit()
        self.step('the draft is saved', self.sessions()[0].get('draft') == DRAFT)

        self.launch()
        self.wait_for('model-selector')
        # The restored transcript opens at its end, and only rows in view are named.
        deadline = time.monotonic() + 30
        while not any(name.startswith('transcript-row-') for name in self.elements()):
            if time.monotonic() > deadline:
                raise Failure('the restored transcript never showed')
            time.sleep(0.2)
        after = (self.choice(), self.notes())
        self.step('the choice and the notes survive the relaunch', after == before, repr(after))
        self.step('the draft survives the relaunch', self.sessions()[0].get('draft') == DRAFT)
        self.shot('restored')
        self.open_menu()
        self.wait_for('choice-column', 60)
        self.shot('menu-restored')
        self.key('escape')
        self.quit()


def main():
    if sys.platform != 'darwin':
        sys.exit('model-choice-journey runs on macOS. On Linux, use the verify skill.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    parser.add_argument('--foreground', action='store_true',
                        help='let the app take focus; a fully covered window draws no frames')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    binary = ROOT / 'target/debug/keystone-desktop'
    evidence = ROOT / '.audit' / time.strftime('model-choice-%Y%m%d-%H%M%S')
    evidence.mkdir(parents=True)
    run = Journey(binary, evidence)
    run.background = not args.foreground
    error = None
    try:
        run.journey()
    except Failure as failure:
        error = str(failure)
    finally:
        run.finish(error)
    print(f'evidence: {evidence}')
    if error:
        sys.exit(f'model-choice-journey failed: {error}')
    print('model-choice-journey passed')


if __name__ == '__main__':
    main()
