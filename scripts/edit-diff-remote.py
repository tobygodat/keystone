#!/usr/bin/env python3
"""An edit row on a session on another machine, in the real app (TOB-310).

The "machine" is this one behind a stand-in `ssh`, as the verify skill's machines
feature describes: the stand-in runs the app's remote command with a clean environment,
a host home whose `.profile` puts a scratch copy of `keystone-host` on `PATH`, and
`keystone-fake-claude` as the host's Claude. The journey links the machine from the
palette, starts a Claude session there in a folder, replays the recorded edits, opens
an edit row, opens the full edit, and copies one with Ctrl+Shift+C, reading it back
with `wl-paste`. Every path it shows or copies must be relative to the remote folder.

Run from the workspace root after `cargo build -p keystone-desktop -p keystone-agent --bins --locked`:
    python3 scripts/edit-diff-remote.py [--driver PATH]
Evidence goes to .audit/edit-diff-remote-<time>/.
"""

import argparse
import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SSH = '''#!/bin/sh
# Skips ssh's options up to "--", then runs the remote command as a login shell on a
# clean host environment.
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--" ]; then shift; break; fi
    shift
done
shift
exec env -i HOME={home} KEYSTONE_HOST_DIR={hostdir} KEYSTONE_CLAUDE_PATH={claude} \\
    KEYSTONE_CODEX_PATH={codex} KEYSTONE_FAKE_CLAUDE_DELAY_MS=2 PATH=/usr/bin:/bin sh -c "$*"
'''


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--driver', default=None)
    args = parser.parse_args()
    journey = load('edit_diff_journey', ROOT / 'scripts/edit-diff-journey.py')
    driver = args.driver or str(journey.load_driver().default_driver())
    j = journey.Journey(driver, str(ROOT / 'target/debug/keystone-desktop'))
    out = ROOT / '.audit' / f'edit-diff-remote-{time.strftime("%Y%m%dT%H%M%S")}'
    out.mkdir(parents=True)
    bin_dir, home = out / 'bin', out / 'host-home'
    for folder in (bin_dir, home, home / 'bin'):
        folder.mkdir()
    folder = out / 'remote-folder'
    folder.mkdir()
    # The host's socket path must fit SUN_LEN, so its folder is short.
    hostdir = Path(tempfile.mkdtemp(prefix='kh', dir='/tmp'))
    shutil.copy(ROOT / 'target/debug/keystone-host', home / 'bin/keystone-host')
    (home / '.profile').write_text(f'export PATH={home}/bin:$PATH\n')
    ssh = bin_dir / 'ssh'
    ssh.write_text(SSH.format(home=home, hostdir=hostdir,
                              claude=ROOT / 'target/debug/keystone-fake-claude',
                              codex=ROOT / 'target/debug/keystone-fake-codex'))
    ssh.chmod(0o755)
    env = {
        'PATH': f'{bin_dir}:{os.environ["PATH"]}',
        'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
        'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
        'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '2',
        'KEYSTONE_PROJECT_DIR': str(j.project),
        'CLAUDE_CONFIG_DIR': str(j.scratch / 'claude'),
    }
    error = None
    try:
        j.call('start', width=1100, height=720, env=env)
        j.wait_for('welcome-claude', seconds=60)

        def palette(command):
            j.keys('ctrl', 'k')
            j.wait_for('palette-input')
            j.type(command)
            j.settle()
            j.keys('Return')

        palette('Link a machine')
        j.wait_for('machine-destination')
        j.click('machine-destination')
        j.type('desk')
        j.click('machine-link')
        j.wait('the machine to connect', lambda: any(
            e['id'].startswith('machine-state-') for e in j.elements()), 30)
        j.settle()
        j.shot('machine-linked')
        j.keys('Escape')
        palette('New Claude session on a machine')
        j.settle()
        j.keys('Return')
        j.settle()
        j.type(str(folder))
        j.settle()
        j.keys('Return')
        j.wait('a session on the machine', lambda: len(j.sessions()) == 1, 20)
        j.wait_for('composer-field')
        j.send('replay edits')
        rows = j.edit_rows(5, seconds=60)
        j.wait_for('send', seconds=30)
        j.shot('remote-edits-collapsed')
        drawn = j.by_mouse(rows[0], 'remote')
        j.step('the remote edit opens to its own diff', drawn == 6, f'{drawn} lines')
        j.shot('remote-edit-open')
        j.close_by_mouse(rows[0], 'remote')
        text = j.copied_paths_are_relative(rows[0], 'remote')
        (out / 'copied-remote.diff').write_text(text)
        j.step('the copy names no path of the remote folder', str(folder) not in text, text.splitlines()[0])
        j.select(rows[0])
        j.keys('ctrl', 'shift', 'd')
        j.wait_for('diff-lines')
        j.settle()
        j.shot('remote-full-edit')
        j.step('the full edit opens from a session on a machine', True)
        j.keys('Escape')
        j.quit()
    except Exception as failure:
        error = f'{type(failure).__name__}: {failure}'
        print(f'FAIL {error}', flush=True)
        try:
            j.shot('failure')
        except Exception:
            pass
    finally:
        j.call('stop')
        for shot in j.shots:
            shutil.copy(shot, out)
        shutil.rmtree(hostdir, ignore_errors=True)
        print(f'evidence: {out}')
    if error:
        sys.exit(f'edit-diff-remote failed: {error}')
    print('edit-diff-remote passed')


if __name__ == '__main__':
    main()
