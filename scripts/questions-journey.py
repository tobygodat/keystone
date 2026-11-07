#!/usr/bin/env python3
"""Native journey for agent questions on macOS (TOB-296), with the scripted providers.

Drives the real debug app through the automation inbox, as scripts/macos-smoke.py
does, and reuses its harness. The fake Claude asks two questions with
AskUserQuestion when a prompt says "ask me"; the fake Codex asks with
request_user_input on "ask me" and sends an MCP form on "fill a form".

The journey: answer Claude's questions with the mouse, answer them again with the
keyboard alone (typed "Other" text included), decline Codex's MCP form, answer it
with typed text and a number, check the sidebar says the session waits for an answer,
keep a half-answered question across a window close, and quit with a question
pending to see what a relaunch shows.

Run from the workspace root: python3 scripts/questions-journey.py
Evidence goes to .audit/questions-journey-<time>/: numbered PNGs and report.json.
"""

import argparse
import importlib.util
import json
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('macos_smoke', ROOT / 'scripts' / 'macos-smoke.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)
Failure = smoke.Failure


class Journey(smoke.Run):
    def env(self):
        env = super().env()
        del env['KEYSTONE_FAKE_CLAUDE_REPLY']
        return env

    def log_text(self):
        return '\n'.join(row[0] for row in self.read('SELECT payload FROM log_entries'))

    def wait_log(self, text, seconds=30):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if text in self.log_text():
                return True
            time.sleep(0.2)
        return False

    def focused(self):
        return [name for name, e in self.elements().items() if e.get('focused')]

    def send(self, text):
        self.click('composer-field')
        self.type(text)
        self.key('secondary-enter')

    def journey(self):
        self.launch()
        self.key('secondary-n')
        self.wait_for('composer-field')
        self.send('ask me about colors')
        self.wait_for('question', 30)
        self.step('Claude asks: the question card shows', True)
        self.shot('claude-question')

        self.click('question-0-1')
        self.click('question-1-0')
        self.click('question-1-2')
        self.shot('claude-answered-by-mouse')
        self.click('question-submit')
        self.wait_for('question', present=False)
        self.step('Claude gets the mouse answers', self.wait_log('Answers: \\"Which color do you prefer?\\"=\\"Blue\\", \\"Which fruits do you like?\\"=\\"Apple, Cherry\\"'))
        self.wait_for('turn-status', 30, present=False)
        self.shot('claude-reply-after-mouse')

        self.send('ask me again')
        self.wait_for('question', 30)
        # From the composer, Shift+Tab walks back through the card: Decline (Submit is
        # off until the answers are complete), the Fruit field, then its options.
        for _ in range(3):
            self.key('shift-tab')
        self.key('space')
        self.key('shift-tab')
        self.key('shift-tab')
        self.key('space')
        self.key('shift-tab')
        self.type('Teal')
        self.shot('claude-answered-by-keyboard')
        for _ in range(5):
            self.key('tab')
        self.key('enter')
        self.wait_for('question', present=False)
        self.step('Claude gets the keyboard answers, typed Other text included',
                  self.wait_log('\\"Which color do you prefer?\\"=\\"Teal\\", \\"Which fruits do you like?\\"=\\"Apple, Cherry\\"'))
        self.wait_for('turn-status', 30, present=False)
        self.shot('claude-reply-after-keyboard')

        self.key('secondary-shift-n')
        self.wait_for('composer-field')
        self.send('fill a form please')
        self.wait_for('question', 30)
        self.shot('codex-form')
        self.click('question-decline')
        self.wait_for('question', present=False)
        self.step('declining the MCP form tells Codex, and the turn goes on',
                  self.wait_log('The questions were declined. Scripted Codex reply'))
        self.wait_for('turn-status', 30, present=False)

        self.send('fill a form again')
        self.wait_for('question', 30)
        self.click('question-0-text')
        self.type('two')
        self.click('question-1-text')
        self.type('extra crisp')
        self.click('question-2-0')
        self.click('question-3-1')
        self.shot('codex-form-not-a-number')
        self.click('question-0-text')
        self.key('secondary-a')
        self.type('2')
        self.shot('codex-form-answered')
        self.click('question-submit')
        self.wait_for('question', present=False)
        self.step('the MCP form answer reaches Codex in its types',
                  self.wait_log('{\\"action\\":\\"accept\\",\\"content\\":{\\"count\\":2,\\"note\\":\\"extra crisp\\",\\"rush\\":true,\\"size\\":\\"large\\"}}'))
        self.wait_for('turn-status', 30, present=False)
        self.shot('codex-reply-after-form')

        self.send('ask me something')
        self.wait_for('question', 30)
        self.click('question-0-1')
        self.click('question-1-text')
        self.type('kept.txt')
        session = self.sessions()[-1]['id']
        self.key('secondary-alt-n')
        self.wait_for(f'session-{session}')
        self.click(f'session-{session}')
        self.wait_for('question')
        self.shot('second-window-has-the-answers')
        self.key('secondary-shift-w')
        # The window left behind draws its next frame for the capture. The session
        # moved to the closed window when it was selected there, so open it again.
        self.shot('window-left-behind')
        self.wait_for(f'session-{session}')
        self.click(f'session-{session}')
        self.wait_for('question')
        self.shot('after-closing-a-window')
        self.click('question-submit')
        self.wait_for('question', present=False)
        self.step('a closed window loses none of the answers',
                  self.wait_log('{\\"answers\\":{\\"color\\":{\\"answers\\":[\\"Blue\\"]},\\"name\\":{\\"answers\\":[\\"kept.txt\\"]}}}'))
        self.wait_for('turn-status', 30, present=False)

        self.send('ask me before quitting')
        self.wait_for('question', 30)
        self.shot('pending-before-quit')
        self.quit()
        self.launch()
        time.sleep(1)
        self.step('after a relaunch the question is gone, since nothing waits for it',
                  'question' not in self.elements())
        self.shot('relaunched')
        self.quit()


def main():
    if sys.platform != 'darwin':
        sys.exit('questions-journey runs on macOS. On Linux, use the verify skill.')
    parser = argparse.ArgumentParser()
    parser.add_argument('--no-build', action='store_true', help='use the existing debug build')
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-agent', '--bins'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '--locked', '-p', 'keystone-desktop', '--features', 'capture'],
                       cwd=ROOT, check=True)
    binary = ROOT / 'target/debug/keystone-desktop'
    evidence = ROOT / '.audit' / time.strftime('questions-journey-%Y%m%d-%H%M%S')
    evidence.mkdir(parents=True)
    run = Journey(binary, evidence)
    error = None
    try:
        run.journey()
    except Failure as failure:
        error = str(failure)
    finally:
        run.finish(error)
    print(f'evidence: {evidence}')
    if error:
        sys.exit(f'questions-journey failed: {error}')
    print('questions-journey passed')


if __name__ == '__main__':
    main()
