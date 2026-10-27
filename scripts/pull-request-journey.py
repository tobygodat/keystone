#!/usr/bin/env python3
"""The Pull request surface's native journey against scripts/fake-gh.py, in a private headless Sway.

    python3 scripts/pull-request-journey.py REPO OUT DRIVER [WIDTH]

REPO is this checkout with target/debug/keystone-desktop, keystone-fake-claude and keystone-fake-codex
built; OUT gets the screenshots, journey-WIDTH.json (every check and what each screenshot proves),
calls-WIDTH.log and, at 1920, journey-1920.mp4 stitched from screenshots taken through the merge. DRIVER is
the computer-use driver scripts/native-driver.py runs. WIDTH 720 drives the narrow window's fitted tabs.
The full run waits out the 5-minute hidden interval once.
"""
import json, queue, shutil, subprocess, sys, threading, time, os
from pathlib import Path

ROOT = Path(sys.argv[1]).resolve()
OUT = Path(sys.argv[2]).resolve()
DRIVER = sys.argv[3]
WIDTH = int(sys.argv[4]) if len(sys.argv) > 4 else 1920
OUT.mkdir(parents=True, exist_ok=True)
SCRATCH = Path(f'/tmp/keystone-pull-request-journey-{WIDTH}')
shutil.rmtree(SCRATCH, ignore_errors=True)
PROJECT = SCRATCH / 'demo'
(PROJECT / 'src').mkdir(parents=True)
(PROJECT / 'src/lib.rs').write_text('pub fn upload() {}\npub fn retry() {}\n')
def git(*a, cwd=PROJECT):
    subprocess.run(['git', *a], cwd=cwd, check=True, capture_output=True)
git('init', '-q', '-b', 'retry-uploads')
for k, v in [('user.name', 'Test'), ('user.email', 't@example.com'), ('commit.gpgsign', 'false'), ('core.hooksPath', '.git/hooks')]:
    git('config', k, v)
git('add', '-A'); git('commit', '-qm', 'first')
GH = SCRATCH / 'gh'
bindir = subprocess.run([sys.executable, str(ROOT / 'scripts/fake-gh.py'), 'install', str(GH), str(PROJECT)],
                        check=True, capture_output=True, text=True).stdout.strip()
(SCRATCH / 'gh-config').mkdir()
homes = {n: SCRATCH / 'homes' / n for n in ('claude', 'codex')}
for h in homes.values(): h.mkdir(parents=True)
claude = SCRATCH / 'claude'
claude.write_text(f'#!/bin/sh\nexport KEYSTONE_FAKE_CLAUDE_STORE=1\nexec "{ROOT}/target/debug/keystone-fake-claude" "$@"\n')
claude.chmod(0o755)
editor_log = SCRATCH / 'editor.log'
editor = SCRATCH / 'editor'
editor.write_text(f'#!/bin/sh\necho "$@" >> {editor_log}\n')
editor.chmod(0o755)

server = subprocess.Popen([sys.executable, str(ROOT / 'scripts/native-driver.py'), '--binary', str(ROOT / 'target/debug/keystone-desktop'),
                           '--driver', DRIVER], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, cwd=ROOT)
lines = queue.Queue()
threading.Thread(target=lambda: ([lines.put(l) for l in server.stdout], lines.put(None)), daemon=True).start()
seq = 0
def rpc(method, params):
    global seq
    seq += 1
    server.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': seq, 'method': method, 'params': params}) + '\n'); server.stdin.flush()
    while True:
        line = lines.get(timeout=180)
        if line is None: raise RuntimeError('driver closed')
        m = json.loads(line)
        if m.get('id') == seq: return m
def tool(name, **args):
    r = rpc('tools/call', {'name': name, 'arguments': args})['result']
    t = r['content'][0]['text']
    if r.get('isError'): raise RuntimeError(f'{name}: {t}')
    return json.loads(t)
def ids():
    try: return [e['id'] for e in tool('elements')['elements']]
    except RuntimeError as e:
        if 'NotReady' in str(e): return []
        raise
FRAMES = OUT / f'frames-{WIDTH}'
shutil.rmtree(FRAMES, ignore_errors=True); FRAMES.mkdir()
frames = []
filming = False
def frame():
    if filming:
        dst = FRAMES / f'{len(frames):05d}.png'
        shutil.copy(tool('screenshot')['path'], dst); frames.append((dst, time.monotonic()))
def film(seconds):
    end = time.monotonic() + seconds
    while time.monotonic() < end: frame()
def until(test, what, seconds=15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        r = test(ids())
        if r: return r
        frame()
        time.sleep(0.25)
    raise RuntimeError(f'timed out: {what}')
def wait(name, seconds=15, present=True):
    return until(lambda i: (name in i) == present, f'{name} {"shows" if present else "goes"}', seconds)
def click(name):
    wait(name); tool('click', id=name); time.sleep(0.5); film(1.2)
def key(*k):
    tool('input', actions=[{'action': 'keypress', 'keys': list(k)}]); time.sleep(0.6)
def stage(s): (GH / 'stage').write_text(s + '\n')
def calls(): return [json.loads(l) for l in (GH / 'calls.log').read_text().splitlines()] if (GH / 'calls.log').exists() else []
log = []
def shot(name, proves):
    x, y = WIDTH - 1, 1079
    tool('input', actions=[{'action': 'move', 'x': x - 1, 'y': y - 1}, {'action': 'move', 'x': x, 'y': y}])
    time.sleep(0.7)
    shutil.copy(tool('screenshot')['path'], OUT / f'{name}.png')
    log.append({'shot': name, 'proves': proves}); print('shot', name, flush=True)
def check(cond, what):
    log.append({'check': what, 'ok': bool(cond)}); print('OK ' if cond else 'FAIL', what, flush=True)
    if not cond: raise RuntimeError(what)

env = {'KEYSTONE_CLAUDE_PATH': str(claude), 'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
       'KEYSTONE_PROJECT_DIR': str(PROJECT), 'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '5',
       'CLAUDE_CONFIG_DIR': str(homes['claude']), 'CODEX_HOME': str(homes['codex']),
       'PATH': f'{bindir}:/usr/bin:/bin', 'GH_CONFIG_DIR': str(SCRATCH / 'gh-config'), 'VISUAL': str(editor)}
try:
    rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'pull-request-journey', 'version': '1'}})
    started = tool('start', width=WIDTH, height=1080, env=env)
    log.append({'start': {k: started.get(k) for k in ('evidence', 'database', 'bus')}})
    doc = tool('doctor'); log.append({'doctor': doc})
    wait('welcome-claude', 40)
    click('welcome-claude'); wait('welcome-claude', 20, present=False) if WIDTH == 720 else wait('session-1')
    if WIDTH == 720:
        tabs = lambda: {i for i in ids() if i.startswith('fitted-tab-') and not i.endswith('-dot')}
        key('ctrl', 'alt', 'p')
        wait('pull-request-title', 20)
        until(lambda i: len(tabs()) >= 2, 'fitted tabs')
        pr_tab = conversation_tab = None
        for t in sorted(tabs()):
            tool('click', id=t); time.sleep(0.8)
            if 'pull-request-title' in ids(): pr_tab = t
            else: conversation_tab = t
        log.append({'fitted_tabs': sorted(tabs()), 'pr_tab': pr_tab, 'conversation_tab': conversation_tab})
        click(pr_tab); wait('pull-request-title', 10)
        shot('720-running', 'At 720 px the Pull request surface is a fitted tab; the first report shows the running check.')
        click(conversation_tab)
        wait('pull-request-title', 10, present=False)
        stage('failing')
        wait(pr_tab + '-dot', 330)
        check('attention-section' not in ids(), 'the failure arriving with the tab hidden adds no Needs action section')
        shot('720-dot-on-tab', 'Narrow window: the check failed while the conversation tab was in front, so the fitted Pull request tab shows a dot.')
        click(pr_tab)
        wait(pr_tab + '-dot', 10, present=False)
        check(True, 'showing the fitted tab clears its dot')
        stage('mergeable'); click('pull-request-refresh')
        wait('pull-request-merge', 20)
        shot('720-mergeable', 'At 720 px the header, checks and the review comment fit, with Merge.')
        click('pull-request-merge'); wait('merge-dialog-confirm', 15)
        shot('720-confirm', 'The Merge confirmation at 720 px.')
        click('merge-dialog-cancel')
        check(not any('PUT' in c for c in calls()), 'Cancel at 720 sends no merge')
        raise SystemExit
    key('ctrl', 'alt', 'p')
    wait('pull-request-title', 20)
    until(lambda i: 'pull-request-check-0' in i, 'checks')
    ws = tool('workspace')
    s1 = next(s for s in ws['sessions'] if s['id'] == 1) if isinstance(ws.get('sessions'), list) else None
    log.append({'workspace_session_1': s1})
    check(any(c[:3] == ['pr', 'view', '--json'] for c in calls()), 'gh found the branch pull request by branch')
    shot('01-running', 'Found by branch: #12 Open, Review required, retry-uploads → main, test running with its elapsed time, lint passed in 41s.')
    check('attention-section' not in ids(), 'no Needs action section')

    stage('failing'); click('pull-request-refresh')
    until(lambda i: True, 'x', 1); time.sleep(2)
    shot('02-failing', 'Refresh: test failed after 3m 12s. The failure arrived while the tab was in view, so no dot.')
    check('session-pull-request-dot-1' not in ids(), 'no dot for a failure seen in view')

    key('ctrl', 'alt', 'g'); wait('worktree-dialog', 10); key('Return')
    wait('session-2', 20)
    until(lambda i: 'pull-request-empty' in i, 'other session reads No pull request', 20)
    shot('03-other-session', 'A worktree session on another branch: "No pull request." The first session stays in the sidebar.')
    stage('review')
    n = len(calls()); t0 = time.monotonic()
    wait('session-pull-request-dot-1', 330)
    waited = time.monotonic() - t0
    check(waited > 240, f'the hidden pull request was asked again only after the 5-minute interval ({waited:.0f}s)')
    check('attention-section' not in ids(), 'the review arriving out of view adds no Needs action section')
    shot('04-dot-on-row', 'A review arrived while another session was selected: a dot on the first session\'s row, and no Needs action section.')

    filming = True
    click('session-1')
    wait('session-pull-request-dot-1', 10, present=False)
    wait('pull-request-comment-0', 10)
    shot('05-review-seen', 'Selecting the session shows the tab: the dot clears; Changes requested; ana\'s review and the src/lib.rs:2 comment.')
    click('pull-request-comment-0'); time.sleep(1)
    check(editor_log.exists() and str(PROJECT / 'src/lib.rs') in editor_log.read_text(), 'the comment opens src/lib.rs in $VISUAL')

    stage('blocked'); click('pull-request-refresh')
    until(lambda i: True, 'x', 1); time.sleep(2)
    check('pull-request-merge' not in ids(), 'no Merge while the base branch rules block it')
    shot('06a-blocked', 'Checks passed and Approved, but GitHub reports the merge state as BLOCKED: no Merge button.')
    stage('mergeable'); click('pull-request-refresh'); wait('pull-request-merge', 20)
    shot('06-mergeable', 'Checks passed, Approved, merge state CLEAN: Merge shows.')
    click('pull-request-merge'); wait('merge-dialog-confirm', 15)
    stage('blocked'); click('merge-dialog-confirm')
    wait('pull-request-failed', 15)
    check(not any('PUT' in c for c in calls()), 'a merge the rules block by the time of confirming is never sent')
    shot('06b-refused', 'The rules blocked the merge between opening the confirmation and confirming: Keystone asked again, sent no merge, and says so.')
    stage('mergeable'); click('pull-request-refresh')
    wait('pull-request-failed', 15, present=False); wait('pull-request-merge', 20)
    click('pull-request-merge'); wait('merge-dialog-confirm', 15)
    shot('07-confirm', 'Merge asks first: squash is the default; merge commit is offered because the repo allows it; rebase is not.')
    click('merge-dialog-cancel'); wait('merge-dialog-confirm', 5, present=False)
    check(not any('PUT' in c for c in calls()), 'Cancel sends nothing')
    click('pull-request-merge'); wait('merge-dialog-confirm', 15)
    click('merge-dialog-confirm')
    until(lambda i: 'pull-request-merge' not in i, 'merged', 20); time.sleep(2)
    put = [c for c in calls() if 'PUT' in c]
    check(put == [['api', '--hostname', 'github.com', '--method', 'PUT', 'repos/keystone-fixture/demo/pulls/12/merge', '-f', 'merge_method=squash', '-f', 'sha=3f2c9e1']], f'one merge with the right argv: {put}')
    check(not any('--auto' in c or c[:2] == ['pr', 'merge'] for c in calls()), 'no auto-merge and no gh pr merge')
    film(2); filming = False
    shot('08-merged', 'Confirmed: one squash merge for the head the user saw, then refreshed to Merged; Merge is gone.')
    n = len(calls()); time.sleep(70)
    check(len(calls()) == n, f'a merged pull request stops polling (calls stayed at {n} for 70 s with the tab visible)')

    key('ctrl', 'q')
    until(lambda i: True, 'x', 1)
    end = time.monotonic() + 20
    while time.monotonic() < end and tool('call', tool='windows_list').get('windows'):
        time.sleep(0.5)
    tool('launch'); time.sleep(1); log.append({'doctor_relaunch': tool('doctor')})
    wait('pull-request-title', 40)
    check('pull-request-merge' not in ids(), 'relaunch restores the merged state')
    shot('09-restored', 'Quit and relaunch: the surface and the merged report are restored.')
    check(len(calls()) == n, 'relaunch does not ask gh about a merged pull request')
    opened = editor_log.read_text().count('src/lib.rs')
    click('pull-request-comment-0'); time.sleep(1)
    check(editor_log.read_text().count('src/lib.rs') == opened + 1, 'a restored comment still opens its file with gh not asked')

    stage('exit4'); click('pull-request-refresh')
    wait('pull-request-hint-gh-signed-out', 15)
    shot('10-signed-out', 'gh with no login: the exact command to run, as Changes shows it.')
finally:
    try:
        stop = tool('stop'); log.append({'stop': stop}); print('stop errors', stop.get('errors'), 'scratch', stop.get('scratch_retained'))
    except Exception as e:
        print('stop failed', e)
    server.stdin.close(); server.wait(30)
    shutil.copy(GH / 'calls.log', OUT / f'calls-{WIDTH}.log') if (GH / 'calls.log').exists() else None
    (OUT / f'journey-{WIDTH}.json').write_text(json.dumps(log, indent=1, default=str))
    if len(frames) > 1:
        listing = FRAMES / 'list.txt'
        with open(listing, 'w') as f:
            for (path, at), (_, after) in zip(frames, frames[1:] + [(None, frames[-1][1] + 1.0)]):
                f.write(f"file '{path}'\nduration {max(after - at, 0.05):.3f}\n")
            f.write(f"file '{frames[-1][0]}'\n")
        subprocess.run(['ffmpeg', '-y', '-loglevel', 'error', '-f', 'concat', '-safe', '0', '-i', str(listing),
                        '-vf', 'fps=15,scale=1920:-2,format=yuv420p', '-c:v', 'libx264', '-preset', 'veryfast', '-crf', '23',
                        '-movflags', '+faststart', str(OUT / f'journey-{WIDTH}.mp4')], check=False)
        print('video', OUT / f'journey-{WIDTH}.mp4', len(frames), 'frames', flush=True)
