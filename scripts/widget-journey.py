#!/usr/bin/env python3
"""The widget keyboard and Stopped native journey (TOB-90, TOB-86), in a private headless Sway.

    python3 scripts/widget-journey.py REPO OUT DRIVER THEME

REPO is this checkout with target/debug/keystone-desktop and keystone-fake-claude built; OUT gets
the screenshots, journey.json (every check and what each screenshot proves) and the driver's live
recording. THEME is light or dark. Every step that answers a widget uses the keyboard alone.
"""
import json, os, queue, re, shutil, subprocess, sys, threading, time
from pathlib import Path

ROOT = Path(sys.argv[1]).resolve()
OUT = Path(sys.argv[2]).resolve()
DRIVER = sys.argv[3]
THEME = sys.argv[4]
OUT.mkdir(parents=True, exist_ok=True)
SCRATCH = Path(f'/tmp/keystone-widget-journey-{THEME}-{os.getpid()}')
shutil.rmtree(SCRATCH, ignore_errors=True)
PROJECT = SCRATCH / 'demo'
PROJECT.mkdir(parents=True)
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
    tool('input', actions=[{'action': 'keypress', 'keys': list(k)}]); time.sleep(0.5)
def keys(*strokes):
    for s in strokes: key(*s.split('+'))
def typed(text):
    tool('input', actions=[{'action': 'type', 'text': text}]); time.sleep(0.5)
def widgets():
    return sorted({int(m.group(1)) for i in ids() for m in [re.fullmatch(r'widget-(\d+)', i)] if m})
log = []
def shot(name, proves):
    tool('input', actions=[{'action': 'move', 'x': 1918, 'y': 1078}, {'action': 'move', 'x': 1919, 'y': 1079}])
    time.sleep(0.7)
    shutil.copy(tool('screenshot')['path'], OUT / f'{name}.png')
    log.append({'shot': name, 'proves': proves}); print('shot', name, flush=True)
def check(cond, what):
    log.append({'check': what, 'ok': bool(cond)}); print('OK ' if cond else 'FAIL', what, flush=True)
    if not cond: raise RuntimeError(what)
def send(prompt):
    key('Escape')
    typed(prompt)
    key('ctrl', 'Return')
def new_widget(before, what, button='send'):
    return until(lambda: [w for w in widgets() if w not in before and f'widget-{w}-{button}-button' in ids()], what, 20)[0]

env = {'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
       'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
       'KEYSTONE_PROJECT_DIR': str(PROJECT), 'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '40',
       'CLAUDE_CONFIG_DIR': str(homes['claude']), 'CODEX_HOME': str(homes['codex'])}
recording = None
try:
    rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'widget-journey', 'version': '1'}})
    started = tool('start', width=1920, height=1080, env=env)
    log.append({'start': {k: started.get(k) for k in ('evidence', 'database', 'bus')}})
    log.append({'doctor': tool('doctor')})
    wait('welcome-claude', 40)
    key('ctrl', 'comma')
    click('settings-section-appearance')
    click('settings-theme')
    click(f'settings-theme-option-{["system", "light", "dark"].index(THEME)}')
    key('Escape')
    wait('welcome-claude', 5)
    recording = tool('record_start'); log.append({'record_start': recording})
    click('welcome-claude'); wait('composer-field', 10)

    send('show a widget checklist')
    first = new_widget([], 'the checklist shows')
    w = f'widget-{first}'
    check(f'{w}-docs-checked' in ids() and f'{w}-tests-unchecked' in ids(), 'the checklist starts with Docs ticked and Tests not')
    keys('ctrl+shift+t', 'End', 'Tab')
    shot(f'{THEME}-01-first-control', 'Keyboard only: Ctrl+Shift+T, End, Tab puts the focus ring on the first box, Docs updated.')
    keys('Tab', 'space')
    until(lambda: f'{w}-tests-checked' in ids(), 'Space ticks Tests pass')
    check(f'{w}-tests-checked' in ids(), 'Tab then Space ticked Tests pass')
    shot(f'{THEME}-02-ticked', 'Tab moved the ring to Tests pass and Space ticked it.')
    keys('Tab', 'Tab', 'Tab')
    shot(f'{THEME}-03-send-focused', 'Three more Tabs (Changelog, Skip, Send) put the ring on Send checklist.')
    key('Return')
    wait(f'{w}-status', 20)
    check(f'{w}-status' in ids(), 'Enter sent the checklist: the widget says what it sent')
    time.sleep(3)
    shot(f'{THEME}-04-sent', 'Enter pressed Send: the widget reads "Sent submit" and the agent replies with tests:true.')

    send('show a widget form')
    form = new_widget([first], 'the form shows', 'submit')
    f = f'widget-{form}'
    keys('ctrl+shift+t', 'End', 'Tab')
    typed('Sidebar flickers')
    shot(f'{THEME}-05-form-field', 'Keyboard only: Tab put the focus border on the Summary field and typing filled it.')
    keys('Tab', 'Tab', 'Tab', 'space', 'Tab', 'space')
    until(lambda: f'{f}-severity-high-checked' in ids(), 'Space picks High')
    keys('Tab', 'Tab', 'Tab', 'Right', 'Right')
    shot(f'{THEME}-06-form-slider', 'Space picked High and the Sidebar chip; Tab reached the Confidence slider (ring) and Right moved it from 5 to 7.')
    keys('Tab', 'space', 'Tab')
    until(lambda: f'{f}-notify-checked' in ids(), 'Space ticks Notify me')
    key('Return')
    wait(f'{f}-status', 20)
    check(f'{f}-status' in ids(), 'Enter filed the form')

    send('show a widget checklist')
    second = new_widget([first, form], 'a second checklist waits')
    check(f'widget-{second}-status' not in ids(), 'the second checklist waits for an answer')
    shot(f'{THEME}-07-waiting', 'A second checklist waits for the user while its turn runs.')
    click('stop')
    wait(f'widget-{second}-status', 20)
    check(f'widget-{second}-status' in ids(), 'after Stop the waiting checklist is marked Stopped')
    shot(f'{THEME}-08-stopped', 'Stop ended the turn and the checklist is Stopped: a press now sends a new message.')

    send('show a widget checklist withdraw')
    third = new_widget([first, form, second], 'a third checklist waits')
    wait(f'widget-{third}-status', 20)
    check('stop' in ids(), 'the agent withdrew the call and its turn still runs')
    shot(f'{THEME}-09-withdrawn', 'The agent withdrew its show_ui call: the checklist is Stopped while the turn keeps running.')
    click('stop')
    wait('stop', 20, present=False)
    log.append({'record_stop': tool('record_stop')})
    recording = None
    log.append({'doctor_end': tool('doctor')})
finally:
    try:
        if recording: log.append({'record_stop': tool('record_stop')})
    except Exception as e: log.append({'record_stop_error': str(e)})
    try: log.append({'stop': tool('stop')})
    except Exception as e: log.append({'stop_error': str(e)})
    (OUT / f'journey-{THEME}.json').write_text(json.dumps(log, indent=1))
    server.stdin.close(); server.wait(30)
    shutil.rmtree(SCRATCH, ignore_errors=True)
