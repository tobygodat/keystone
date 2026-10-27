#!/usr/bin/env python3
"""The repair actions' native journey against scripts/fake-gh.py, in a private headless Sway.

    python3 scripts/repair-journey.py REPO OUT DRIVER

REPO is this checkout with target/debug/keystone-desktop, keystone-fake-claude and keystone-fake-codex
built; OUT gets the screenshots, journey.json (every check and what each screenshot proves), calls.log
and the driver's live recording of the whole run. DRIVER is the computer-use driver
scripts/native-driver.py runs. The project has a bare remote that must end as it began.
"""
import json, queue, shutil, subprocess, sys, threading, time
from pathlib import Path

ROOT = Path(sys.argv[1]).resolve()
OUT = Path(sys.argv[2]).resolve()
DRIVER = sys.argv[3]
OUT.mkdir(parents=True, exist_ok=True)
SCRATCH = Path('/tmp/keystone-repair-journey')
shutil.rmtree(SCRATCH, ignore_errors=True)
PROJECT = SCRATCH / 'demo'
REMOTE = SCRATCH / 'remote.git'
(PROJECT / 'src').mkdir(parents=True)
(PROJECT / 'src/lib.rs').write_text('pub fn upload() {}\npub fn retry() {}\n')
def git(*a, cwd=PROJECT):
    return subprocess.run(['git', *a], cwd=cwd, check=True, capture_output=True, text=True).stdout
git('init', '-q', '-b', 'retry-uploads')
for k, v in [('user.name', 'Test'), ('user.email', 't@example.com'), ('commit.gpgsign', 'false'), ('core.hooksPath', '.git/hooks')]:
    git('config', k, v)
git('add', '-A'); git('commit', '-qm', 'first')
git('init', '-q', '--bare', str(REMOTE), cwd=SCRATCH)
git('remote', 'add', 'origin', str(REMOTE)); git('push', '-q', '-u', 'origin', 'retry-uploads')
def remote_refs(): return git('for-each-ref', '--format=%(refname) %(objectname)', cwd=REMOTE)
REFS_BEFORE = remote_refs()
# The scripted Claude appends each message it reads to this file when it exists.
RECEIPT = PROJECT / '.fake-claude-messages'
RECEIPT.write_text('')
(PROJECT / '.git/info/exclude').write_text('.fake-claude-messages\n')
GH = SCRATCH / 'gh'
bindir = subprocess.run([sys.executable, str(ROOT / 'scripts/fake-gh.py'), 'install', str(GH), str(PROJECT)],
                        check=True, capture_output=True, text=True).stdout.strip()
(GH / 'stage').write_text('failing\n')
(SCRATCH / 'gh-config').mkdir()
homes = {n: SCRATCH / 'homes' / n for n in ('claude', 'codex')}
for h in homes.values(): h.mkdir(parents=True)

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
def until(test, what, seconds=15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        r = test()
        if r: return r
        time.sleep(0.25)
    raise RuntimeError(f'timed out: {what}')
def wait(name, seconds=15, present=True):
    return until(lambda: (name in ids()) == present, f'{name} {"shows" if present else "goes"}', seconds)
def click(name):
    wait(name); tool('click', id=name); time.sleep(0.8)
def key(*k):
    tool('input', actions=[{'action': 'keypress', 'keys': list(k)}]); time.sleep(0.6)
def stage(s): (GH / 'stage').write_text(s + '\n')
def calls(): return [json.loads(l) for l in (GH / 'calls.log').read_text().splitlines()] if (GH / 'calls.log').exists() else []
def draft(session):
    return next(s for s in tool('workspace')['workspace']['sessions'] if s['id'] == session).get('draft', '')
def received(): return [json.loads(l) for l in RECEIPT.read_text().splitlines()]
log = []
def shot(name, proves):
    tool('input', actions=[{'action': 'move', 'x': 1918, 'y': 1078}, {'action': 'move', 'x': 1919, 'y': 1079}])
    time.sleep(0.7)
    shutil.copy(tool('screenshot')['path'], OUT / f'{name}.png')
    log.append({'shot': name, 'proves': proves}); print('shot', name, flush=True)
def check(cond, what):
    log.append({'check': what, 'ok': bool(cond)}); print('OK ' if cond else 'FAIL', what, flush=True)
    if not cond: raise RuntimeError(what)

env = {'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
       'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
       'KEYSTONE_PROJECT_DIR': str(PROJECT), 'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '5',
       'CLAUDE_CONFIG_DIR': str(homes['claude']), 'CODEX_HOME': str(homes['codex']),
       'PATH': f'{bindir}:/usr/bin:/bin', 'GH_CONFIG_DIR': str(SCRATCH / 'gh-config')}
recording = None
try:
    rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'repair-journey', 'version': '1'}})
    started = tool('start', width=1920, height=1080, env=env)
    log.append({'start': {k: started.get(k) for k in ('evidence', 'database', 'bus')}})
    log.append({'doctor': tool('doctor')})
    wait('welcome-claude', 40)
    recording = tool('record_start'); log.append({'record_start': recording})
    click('welcome-claude'); wait('session-1')
    key('ctrl', 'alt', 'p')
    wait('pull-request-repair-0', 20)
    check('pull-request-repair-1' not in ids(), 'only the failed check offers a repair')
    shot('01-repair-action', 'A failed check (CI / test, Failure) offers "Ask Claude to repair"; the passed lint check offers nothing.')

    click('pull-request-repair-0')
    until(lambda: 'LAST-LINE' in draft(1), 'the excerpt reaches the draft')
    text = draft(1)
    log.append({'draft_head': text[:300], 'draft_tail': text[-300:], 'draft_bytes': len(text.encode())})
    check(text.startswith('Fix the failing check `CI / test` on https://github.com/keystone-fixture/demo/pull/12.\n\n```\n'),
          'the draft names the check and the pull request')
    check('FIRST-LINE' in text and 'LAST-LINE' in text and 'MIDDLE-LINE' not in text and 'bytes omitted' in text,
          "the excerpt keeps the log's first and last 32 KiB and drops the middle")
    check(len(text.encode()) < 2 * 32 * 1024 + 400, f'the excerpt is capped ({len(text.encode())} bytes from a 76 KiB log)')
    run_view = [c for c in calls() if c[:2] == ['run', 'view']]
    check(run_view == [['run', 'view', '--job', '9311', '--log-failed', '--repo', 'github.com/keystone-fixture/demo']],
          f'one gh run view for the failed job: {run_view}')
    check(received() == [], 'nothing was sent before Send')
    shot('02-repair-prefilled', 'The composer holds the check name, the pull request and the capped log; nothing is sent until Send.')

    click('send')
    until(lambda: any('LAST-LINE' in m for m in received()), 'the scripted Claude receives the repair', 30)
    message = next(m for m in received() if 'LAST-LINE' in m)
    check(message == text, "the scripted Claude's receipt is exactly the prefilled text, excerpt included")

    stage('review'); click('pull-request-refresh')
    wait('pull-request-send-0', 20)
    shot('03-send-action', 'A review comment (src/lib.rs:2, ana) offers "Send to Claude".')
    n = len(calls())
    click('pull-request-send-0')
    expected = "Address ana's review comment on src/lib.rs:2 in https://github.com/keystone-fixture/demo/pull/12:\n\n> Cap this at three tries."
    until(lambda: draft(1) == expected, 'the comment reaches the draft')
    check(len(calls()) == n, 'Send to asks gh for nothing')
    shot('04-comment-prefilled', 'The composer holds the file, line, author and the comment, quoted, ready to send.')
    sent = len(received())
    click('send')
    until(lambda: len(received()) > sent, 'the scripted Claude receives the comment', 30)
    check(received()[-1] == expected, 'the receipt is the comment prompt')

    click('new-session'); click('new-codex'); wait('session-2', 20)
    if 'pull-request-title' not in ids(): key('ctrl', 'alt', 'p')
    wait('pull-request-repair-0', 20); wait('pull-request-send-0', 20)
    shot('05-codex-actions', 'A Codex session on the same branch gets the same actions: "Ask Codex to repair" and "Send to Codex".')
    click('pull-request-send-0')
    until(lambda: draft(2) == expected, 'Send to Codex prefills the Codex draft')
    click('pull-request-repair-0')
    until(lambda: 'LAST-LINE' in draft(2), 'Ask Codex to repair prefills the Codex draft')
    check(draft(2).startswith(expected + '\nFix the failing check `CI / test`'), 'the repair follows the comment on a line of its own')

    check(remote_refs() == REFS_BEFORE, 'the bare remote is unchanged')
    check(not any('PUT' in c or c[:2] == ['pr', 'merge'] for c in calls()), 'gh was asked to merge nothing')
    time.sleep(1)
    recording = tool('record_stop'); log.append({'record_stop': recording})
finally:
    try:
        stop = tool('stop'); log.append({'stop': stop}); print('stop errors', stop.get('errors'), 'scratch', stop.get('scratch_retained'))
    except Exception as e:
        print('stop failed', e)
    server.stdin.close(); server.wait(30)
    if (GH / 'calls.log').exists(): shutil.copy(GH / 'calls.log', OUT / 'calls.log')
    if isinstance(recording, dict) and recording.get('path') and Path(recording['path']).exists():
        shutil.copy(recording['path'], OUT / 'repair-journey.mp4')
    (OUT / 'journey.json').write_text(json.dumps(log, indent=1, default=str))
