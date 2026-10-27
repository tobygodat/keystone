#!/usr/bin/env python3
"""Opening, switching and closing session tabs at 50 tabs, then a session picked in Customize
after Apply, in a private headless Sway.

    python3 scripts/tabs-journey.py REPO OUT [DRIVER]

REPO is this checkout with target/debug/keystone-desktop, keystone-fake-claude and
keystone-fake-codex built; OUT gets the screenshots, journey.json (every check and what each
screenshot proves) and the driver's live recording from the 49th tab on. DRIVER is the
computer-use driver scripts/native-driver.py runs; it defaults to the one the driver finds.
"""
import json, queue, re, shutil, subprocess, sys, tempfile, threading, time
from pathlib import Path

ROOT = Path(sys.argv[1]).resolve()
OUT = Path(sys.argv[2]).resolve()
OUT.mkdir(parents=True, exist_ok=True)
SCRATCH = Path(tempfile.mkdtemp(prefix='keystone-tabs-journey-'))
PROJECT = SCRATCH / 'demo'
PROJECT.mkdir(parents=True)
homes = {n: SCRATCH / 'homes' / n for n in ('claude', 'codex')}
for h in homes.values():
    h.mkdir(parents=True)

command = [sys.executable, str(ROOT / 'scripts/native-driver.py'), '--binary', str(ROOT / 'target/debug/keystone-desktop')]
if len(sys.argv) > 3:
    command += ['--driver', sys.argv[3]]
server = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, cwd=ROOT)
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
def elements():
    try: return tool('elements')['elements']
    except RuntimeError as e:
        if 'NotReady' in str(e): return []
        raise
def ids(): return [e['id'] for e in elements()]
def visible():
    return {e['id'] for e in elements() if e.get('visible') and e['visible']['w'] > 0 and e['visible']['h'] > 0}
def until(test, what, seconds=15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        r = test()
        if r: return r
        time.sleep(0.25)
    raise RuntimeError(f'timed out: {what}')
def wait(name, seconds=15):
    return until(lambda: name in ids(), f'{name} shows', seconds)
def key(*k, pause=0.6):
    tool('input', actions=[{'action': 'keypress', 'keys': list(k)}]); time.sleep(pause)
def workspace(): return tool('workspace')['workspace']
def tabs():
    saved = workspace()['preferences'].get('project_tabs', {})
    return next(iter(saved.values()), [])
def selected(): return workspace().get('selected')
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
       'KEYSTONE_PROJECT_DIR': str(PROJECT),
       'CLAUDE_CONFIG_DIR': str(homes['claude']), 'CODEX_HOME': str(homes['codex'])}
recording = None
try:
    rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'tabs-journey', 'version': '1'}})
    started = tool('start', width=1920, height=1080, env=env)
    log.append({'start': {k: started.get(k) for k in ('evidence', 'database', 'bus')}})
    log.append({'doctor': tool('doctor')})
    wait('welcome-claude', 40)
    tool('click', id='welcome-claude'); wait('session-1')
    for n in range(2, 49):
        key('ctrl', 't', pause=0.15)
        until(lambda: len(tabs()) == n, f'{n} tabs saved')
    key('ctrl', 'backslash')
    until(lambda: len(tabs()) == 48, '48 tabs after the split')
    check(len({t['session'] for t in tabs()}) == 48, '48 tabs hold 48 different sessions')

    recording = tool('record_start'); log.append({'record_start': recording})
    time.sleep(1)
    for n in (49, 50):
        key('ctrl', 't', pause=0.8)
        until(lambda: len(tabs()) == n, f'Ctrl+T opens tab {n}')
        check(selected() == max(t['session'] for t in tabs()), f'tab {n} holds the new session, which is selected')
    shot('01-fifty-tabs', 'Ctrl+T opened the 50th tab beside the active one; the saved layout holds 50 tabs in two panes.')

    seen = [selected()]
    for _ in range(4):
        key('ctrl', 'tab', pause=0.8)
        seen.append(selected())
    check(len(set(seen)) == 5, f'Ctrl+Tab moved through five different sessions: {seen}')
    key('ctrl', 'shift', 'tab', pause=0.8)
    check(selected() == seen[-2], 'Ctrl+Shift+Tab goes back one tab')
    shot('02-switched', 'Ctrl+Tab and Ctrl+Shift+Tab move the active tab; the composer and top bar follow the selected session.')

    for n in (49, 48):
        before = selected()
        key('ctrl', 'w', pause=0.8)
        until(lambda: len(tabs()) == n, f'Ctrl+W leaves {n} tabs')
        held = {t['session'] for t in tabs()}
        check(before not in held and selected() in held, 'the closed tab released its session and the composer moved to a tab')
    shot('03-closed', 'Ctrl+W twice closed the active tab; the composer moved to a remaining tab each time; 48 tabs remain.')

    tool('click', id='customize-toggle'); wait('customize-apply')
    held = {t['session'] for t in tabs()}
    close = min((i for i in visible() if re.fullmatch(r'surface-tabs-\d+-tab-\d+-close', i)),
                key=lambda i: int(i.split('-')[2]))
    tool('click', id=close); time.sleep(0.6)
    tool('click', id='customize-apply')
    until(lambda: len(tabs()) == 47, 'Apply saves the layout without that tab')
    (picked,) = held - {t['session'] for t in tabs()}
    check(f'session-{picked}' in visible(), f'session {picked}, whose tab Apply closed, is in the sidebar')
    tool('click', id=f'session-{picked}'); time.sleep(0.6)
    tool('click', id='customize-close')
    until(lambda: picked in {t['session'] for t in tabs()}, 'the picked session gets a tab once Customize closes')
    check(selected() == picked, f'session {picked}, picked after Apply, stays selected')
    shot('04-picked-after-apply', f'Session {picked}, closed and then picked in the sidebar after Apply, shows in a tab once Customize closes.')
    time.sleep(1)
    recording = tool('record_stop'); log.append({'record_stop': recording})
finally:
    try:
        stop = tool('stop'); log.append({'stop': stop}); print('stop errors', stop.get('errors'), 'scratch', stop.get('scratch_retained'))
    except Exception as e:
        print('stop failed', e)
    server.stdin.close(); server.wait(30)
    if isinstance(recording, dict) and recording.get('path') and Path(recording['path']).exists():
        shutil.copy(recording['path'], OUT / 'tabs-journey.mp4')
    (OUT / 'journey.json').write_text(json.dumps(log, indent=1, default=str))
    shutil.rmtree(SCRATCH, ignore_errors=True)
