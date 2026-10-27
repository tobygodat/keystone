#!/usr/bin/env python3
"""Right-click menus under rename, settings and a preview, and the send-selection follow-ups (TOB-184),
natively with the scripted Claude in a private headless Sway.

    python3 scripts/menu-overlay-journey.py REPO OUT DRIVER [BINARY] [--before]

REPO is this checkout with keystone-fake-claude and keystone-fake-codex built; BINARY defaults to its
target/debug/keystone-desktop. OUT gets the screenshots, journey.json (every check and what each
screenshot proves) and a recording. --before runs only the rename, settings and palette steps and records what
it sees without failing, for a build from before the change.
"""
import json, os, queue, re, shutil, struct, subprocess, sys, threading, time, zlib
from pathlib import Path

args = [a for a in sys.argv[1:] if a != '--before']
BEFORE = '--before' in sys.argv
ROOT = Path(args[0]).resolve()
OUT = Path(args[1]).resolve()
DRIVER = args[2]
BINARY = Path(args[3]).resolve() if len(args) > 3 else ROOT / 'target/debug/keystone-desktop'
OUT.mkdir(parents=True, exist_ok=True)
SCRATCH = Path(f'/tmp/keystone-menu-journey-{os.getpid()}')
shutil.rmtree(SCRATCH, ignore_errors=True)
PROJECT = SCRATCH / 'demo'
(PROJECT / 'src').mkdir(parents=True)
(PROJECT / 'src/lib.rs').write_text('pub fn one() {}\npub fn two() {}\npub fn three() {}\n')
def png(path, w, h, rgb):
    chunk = lambda t, d: struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d))
    raw = b''.join(b'\0' + bytes(rgb) * w for _ in range(h))
    path.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0))
                     + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))
png(PROJECT / 'shot.png', 640, 360, (40, 90, 160))
RECEIPT = PROJECT / '.fake-claude-messages'
RECEIPT.write_text('')
homes = {n: SCRATCH / 'homes' / n for n in ('claude', 'codex')}
for h in homes.values(): h.mkdir(parents=True)
claude = SCRATCH / 'claude'
claude.write_text('#!/usr/bin/python3\nimport os, sys\n'
                  "os.environ['KEYSTONE_FAKE_CLAUDE_REPLY'] = 'Here it is.\\n\\n![](./shot.png)'\n"
                  f'binary = {json.dumps(str(ROOT / "target/debug/keystone-fake-claude"))}\n'
                  'os.execv(binary, [binary, *sys.argv[1:]])\n')
claude.chmod(0o755)

server = subprocess.Popen([sys.executable, str(ROOT / 'scripts/native-driver.py'), '--binary', str(BINARY), '--driver', DRIVER],
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, cwd=ROOT)
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
def tool(name, **kw):
    r = rpc('tools/call', {'name': name, 'arguments': kw})['result']
    t = r['content'][0]['text']
    if r.get('isError'): raise RuntimeError(f'{name}: {t}')
    return json.loads(t)
def elements():
    try: return tool('elements')['elements']
    except RuntimeError as e:
        if 'NotReady' in str(e): return []
        raise
def ids(): return [e['id'] for e in elements()]
def until(test, what, seconds=15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        r = test()
        if r: return r
        time.sleep(0.25)
    raise RuntimeError(f'timed out: {what}')
def wait(name, seconds=15, present=True):
    return until(lambda: (name in ids()) == present, f'{name} {"shows" if present else "goes"}', seconds)
def click_at(at, button='left'):
    tool('input', actions=[{'action': 'click', 'x': at['x'], 'y': at['y'], 'button': button}]); time.sleep(0.8)
def click(name, button='left'):
    wait(name)
    click_at(next(e for e in elements() if e['id'] == name)['center'], button)
def key(*k):
    tool('input', actions=[{'action': 'keypress', 'keys': list(k)}]); time.sleep(0.8)
log, failed = [], []
def shot(name, proves):
    tool('input', actions=[{'action': 'move', 'x': 1918, 'y': 1078}, {'action': 'move', 'x': 1919, 'y': 1079}])
    time.sleep(0.7)
    shutil.copy(tool('screenshot')['path'], OUT / f'{name}.png')
    log.append({'shot': name, 'proves': proves}); print('shot', name, flush=True)
def check(cond, what):
    log.append({'check': what, 'ok': bool(cond)}); print('OK ' if cond else 'FAIL', what, flush=True)
    if not cond:
        failed.append(what)
        if not BEFORE: raise RuntimeError(what)
MENU = 'menu-open-in-new-window'
def session_menu():
    while BEFORE and {'rename-dialog', 'settings-page', 'palette', MENU} & set(ids()):
        key('Escape')
    click('session-1', button='right'); wait(MENU)

env = {'KEYSTONE_CLAUDE_PATH': str(claude), 'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
       'KEYSTONE_PROJECT_DIR': str(PROJECT), 'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '5',
       'CLAUDE_CONFIG_DIR': str(homes['claude']), 'CODEX_HOME': str(homes['codex'])}
recording = None
try:
    rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'menu-overlay-journey', 'version': '1'}})
    started = tool('start', width=1920, height=1080, env=env)
    log.append({'start': {k: started.get(k) for k in ('evidence', 'database', 'bus')}})
    log.append({'doctor': tool('doctor')})
    wait('welcome-claude', 40)
    recording = tool('record_start'); log.append({'record_start': recording})
    click('welcome-claude'); wait('session-1')

    session_menu()
    shot('01-session-menu', "A right click on the session opens its menu.")
    key('F2'); wait('rename-dialog')
    check(MENU not in ids(), 'F2 opens Rename and the menu is gone')
    shot('02-rename-after-menu', 'F2 with the menu open: the rename dialog, and no menu over it.')
    key('Escape')
    check('rename-dialog' not in ids() and MENU not in ids(), 'one Escape closes the rename dialog')

    session_menu()
    key('ctrl', 'comma'); wait('settings-page')
    check(MENU not in ids(), 'Ctrl+, opens Settings and the menu is gone')
    shot('03-settings-after-menu', 'Ctrl+, with the menu open: Settings, and no menu over it.')
    key('Escape')
    check('settings-page' not in ids() and MENU not in ids(), 'one Escape closes Settings')

    session_menu()
    key('ctrl', 'k'); wait('palette')
    check(MENU not in ids(), 'Ctrl+K opens the palette and the menu is gone')
    shot('04-palette-after-menu', 'Ctrl+K with the menu open: the palette, and no menu over it.')
    key('Escape')
    check('palette' not in ids() and MENU not in ids(), 'one Escape closes the palette')
    if BEFORE: raise SystemExit

    click('composer-field'); tool('input', actions=[{'action': 'type', 'text': 'show me'}]); click('send')
    thumb = until(lambda: next((i for i in ids() if re.fullmatch(r'thumbnail-\d+-0', i)), None), 'the reply shows its thumbnail', 30)
    session_menu()
    click(thumb); wait('preview-overlay')
    check(MENU not in ids(), 'a thumbnail click opens the preview and the menu is gone')
    shot('05-preview-after-menu', 'A thumbnail clicked with the menu open: the preview, and no menu.')
    key('Escape')
    check('preview-overlay' not in ids() and MENU not in ids(), 'one Escape closes the preview')

    key('ctrl', 'shift', 'e'); wait('files-tree-row-1')
    click('files-tree-row-1'); wait('files-tree-row-2'); key('Down'); key('space'); wait('file-line-1')
    click('file-line-1')
    reply = next(e for e in elements() if re.fullmatch(r'reply-\d+-selection-surface', e['id']))['visible']
    click_at({'x': reply['x'] + 4, 'y': reply['y'] + 9}, button='right')
    check('menu-send-selection' not in ids(), "a right click on a reply never offers the Files view's selected line")
    shot('06-reply-no-menu', "Line 2 selected in Files, then a right click on the reply: no menu offers the Files line.")
    y = reply['y'] + 9
    tool('input', actions=[{'action': 'drag', 'path': [{'x': reply['x'] + 1, 'y': y}, {'x': reply['x'] + 60, 'y': y}]}]); time.sleep(0.8)
    click_at({'x': reply['x'] + 4, 'y': y}, button='right')
    check('menu-send-selection' in ids(), "the same right click offers the reply's own selected text")
    key('Escape')
    click('file-line-1'); click('file-line-1', button='right'); wait('menu-send-selection')
    shot('07-files-menu', "The Files view's own right click offers Send to Claude Code.")
    key('ctrl', 'n')
    until(lambda: 'session-2' in ids(), 'Ctrl+N shows a new session')
    check('menu-send-selection' not in ids(), 'Ctrl+N shows a new session with the send menu closed')
    shot('08-switched', 'Ctrl+N with the send menu open: the new session shows, and the send menu is gone.')
    check(len(RECEIPT.read_text().splitlines()) == 1, 'only the typed message was ever sent')
    time.sleep(1)
    recording = tool('record_stop'); log.append({'record_stop': recording})
except SystemExit:
    recording = tool('record_stop'); log.append({'record_stop': recording})
except Exception:
    log.append({'failed_with_elements': ids()})
    shot('zz-failure', 'The screen when the journey failed.')
    raise
finally:
    try:
        stop = tool('stop'); log.append({'stop': stop}); print('stop errors', stop.get('errors'), 'scratch', stop.get('scratch_retained'))
    except Exception as e:
        print('stop failed', e)
    server.stdin.close(); server.wait(30)
    if isinstance(recording, dict) and recording.get('path') and Path(recording['path']).exists():
        shutil.copy(recording['path'], OUT / 'menu-overlay-journey.mp4')
    log.append({'failed': failed})
    (OUT / 'journey.json').write_text(json.dumps(log, indent=1, default=str))
    shutil.rmtree(SCRATCH, ignore_errors=True)
