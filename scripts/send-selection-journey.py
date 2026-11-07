#!/usr/bin/env python3
"""Send to the agent's native journey with the scripted Claude, in a private headless Sway.

    python3 scripts/send-selection-journey.py REPO OUT DRIVER

REPO is this checkout with target/debug/keystone-desktop, keystone-fake-claude and keystone-fake-codex
built; OUT gets the screenshots, journey.json (every check and what each screenshot proves) and the
driver's live recording of the whole run. DRIVER is the computer-use driver scripts/native-driver.py runs.
Lines picked in a Files view go in by the menu, a reply's text by the palette, and a 135 KiB file by the
shortcut; each draft must reach the scripted Claude unchanged after Send, and nothing before it.
"""
import json, os, queue, re, shutil, subprocess, sys, threading, time
from pathlib import Path

ROOT = Path(sys.argv[1]).resolve()
OUT = Path(sys.argv[2]).resolve()
DRIVER = sys.argv[3]
OUT.mkdir(parents=True, exist_ok=True)
SCRATCH = Path(f'/tmp/keystone-selection-journey-{os.getpid()}')
shutil.rmtree(SCRATCH, ignore_errors=True)
PROJECT = SCRATCH / 'demo'
(PROJECT / 'src').mkdir(parents=True)
UPLOAD = [
    'use std::time::Duration;', '', 'pub struct Upload {', '    pub tries: u32,', '    pub wait: Duration,', '}', '',
    'impl Upload {', '    pub fn new() -> Self {', '        Self { tries: 0, wait: Duration::from_millis(250) }', '    }', '',
    '    /// Retries a failed upload, doubling the wait each time.', '    pub fn retry(&mut self) -> Option<Duration> {',
    '        if self.tries == 5 {', '            return None;', '        }', '        self.tries += 1;',
    '        self.wait *= 2;', '        Some(self.wait)', '    }', '}',
]
(PROJECT / 'src/upload.rs').write_text('\n'.join(UPLOAD) + '\n')
BIG = ['FIRST-LINE'] + [f'{n:05} middle of the log, MIDDLE-LINE padding to fifty bytes' for n in range(2, 2300)] + ['LAST-LINE']
(PROJECT / 'big.log').write_text('\n'.join(BIG) + '\n')
assert (PROJECT / 'big.log').stat().st_size > 100 * 1024
RECEIPT = PROJECT / '.fake-claude-messages'
RECEIPT.write_text('')
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
def elements():
    try: return tool('elements')['elements']
    except RuntimeError as e:
        if 'NotReady' in str(e): return []
        raise
def ids(): return [e['id'] for e in elements()]
def center(name):
    e = next(e for e in elements() if e['id'] == name)
    return e['center']
def until(test, what, seconds=15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        r = test()
        if r: return r
        time.sleep(0.25)
    raise RuntimeError(f'timed out: {what}')
def wait(name, seconds=15, present=True):
    return until(lambda: (name in ids()) == present, f'{name} {"shows" if present else "goes"}', seconds)
def click(name, button='left', keys=()):
    wait(name)
    at = center(name)
    tool('input', actions=[{'action': 'click', 'x': at['x'], 'y': at['y'], 'button': button, 'keys': list(keys)}])
    time.sleep(0.8)
def key(*k):
    tool('input', actions=[{'action': 'keypress', 'keys': list(k)}]); time.sleep(0.6)
def typed(text):
    tool('input', actions=[{'action': 'type', 'text': text}]); time.sleep(0.6)
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
def fenced(path, lines, text):
    ticks = max([len(r) for r in re.findall('`+', text)] + [2]) + 1
    span = f'line {lines[0]}' if lines[0] == lines[1] else f'lines {lines[0]}-{lines[1]}'
    return f'{path} {span}:\n{"`" * ticks}\n{text}\n{"`" * ticks}'
def send_and_receive(expected, what):
    sent = len(received())
    click('send')
    until(lambda: len(received()) > sent, f'the scripted Claude receives {what}', 30)
    check(received()[-1] == expected, f"the scripted Claude's receipt is exactly the prefilled {what}")
    until(lambda: draft(1) == '', 'the composer clears after Send')
def replies(): return sorted((int(m.group(1)) for i in ids() if (m := re.fullmatch(r'reply-(\d+)-selection-surface', i))))
def theme(index):
    key('ctrl', 'comma'); click('settings-section-appearance')
    click(f'settings-theme-option-{index}'); key('Escape'); wait('session-1')

env = {'KEYSTONE_CLAUDE_PATH': str(ROOT / 'target/debug/keystone-fake-claude'),
       'KEYSTONE_CODEX_PATH': str(ROOT / 'target/debug/keystone-fake-codex'),
       'KEYSTONE_PROJECT_DIR': str(PROJECT), 'KEYSTONE_FAKE_CLAUDE_DELAY_MS': '5',
       'CLAUDE_CONFIG_DIR': str(homes['claude']), 'CODEX_HOME': str(homes['codex'])}
recording = None
try:
    rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'send-selection-journey', 'version': '1'}})
    started = tool('start', width=1920, height=1080, env=env)
    log.append({'start': {k: started.get(k) for k in ('evidence', 'database', 'bus')}})
    log.append({'doctor': tool('doctor')})
    wait('welcome-claude', 40)
    recording = tool('record_start'); log.append({'record_start': recording})
    click('welcome-claude'); wait('session-1')

    # Files: src/upload.rs lines 13-20, by the context menu.
    key('ctrl', 'shift', 'e'); wait('files-tree-row-1')
    click('files-tree-row-1'); wait('files-tree-row-2')
    key('Down'); click('view-file'); wait('file-line-19')
    check('file-view-send' in ids() and 'file-view-open' in ids(), 'the view offers Send and Open in editor')
    rows = {e['id']: e['bounds']['h'] for e in elements() if e['id'] in ('file-line-0', 'file-line-10')}
    check(rows['file-line-0'] == rows['file-line-10'], f'line 11 is one row high like line 1, its number unwrapped: {rows}')
    click('file-line-12'); click('file-line-19', keys=['shift'])
    shot('01-files-lines-selected', 'Light theme: src/upload.rs read-only in Files, lines 13-20 selected by click and Shift+click.')
    click('file-line-15', button='right'); wait('menu-send-selection')
    shot('02-files-menu', 'A right click on the selection offers "Send to Claude Code".')
    click('menu-send-selection')
    expected = fenced('src/upload.rs', (13, 20), '\n'.join(UPLOAD[12:20]))
    until(lambda: draft(1) == expected, 'the lines reach the draft')
    check(received() == [], 'nothing was sent before Send')
    shot('03-files-prefilled', 'The composer holds the path, the line range and the lines in a fence; nothing is sent until Send.')
    send_and_receive(expected, 'path, lines and quote')

    # A reply: its first line, by the palette.
    until(lambda: replies(), 'a reply shows', 30)
    surface = replies()[-1]
    name = f'reply-{surface}-selection-surface'
    box = next(e for e in elements() if e['id'] == name)['visible']
    y = box['y'] + 9
    tool('input', actions=[{'action': 'drag', 'path': [{'x': box['x'] + 1, 'y': y}, {'x': box['x'] + 200, 'y': y}, {'x': box['x'] + box['w'] - 2, 'y': y}]}])
    time.sleep(0.8)
    shot('04-reply-selected', 'Text selected in the scripted reply.')
    key('ctrl', 'k'); wait('palette'); typed('Send selection'); time.sleep(0.4)
    shot('05-palette', 'The palette lists "Send selection to agent" with its Ctrl+Shift+A shortcut.')
    key('Return')
    until(lambda: draft(1).startswith('> '), 'the quote reaches the draft')
    quote = draft(1)
    log.append({'reply_quote': quote})
    check(len(quote) > 4 and '\n' not in quote, 'one quoted line of the reply')
    check(len(received()) == 1, 'nothing more was sent before Send')
    shot('06-reply-prefilled', 'The composer holds the reply text as a quote.')
    send_and_receive(quote, 'quote')

    # big.log, 135 KiB: Space, Ctrl+A and the shortcut.
    click('file-view-back'); wait('files-tree-row-3')
    click('files-tree-row-1'); until(lambda: 'files-tree-row-3' not in ids(), 'src collapses')
    key('Down'); key('space'); wait('file-line-0')
    key('ctrl', 'a'); key('ctrl', 'shift', 'a')
    until(lambda: 'LAST-LINE' in draft(1), 'the big selection reaches the draft')
    text = draft(1)
    log.append({'big_head': text[:200], 'big_tail': text[-200:], 'big_bytes': len(text.encode())})
    check(text.startswith(f'big.log lines 1-{len(BIG)}:\n```\nFIRST-LINE\n'), 'the draft names big.log and every line')
    check('bytes omitted' in text and 'LAST-LINE' in text, 'the excerpt keeps both ends and marks the cut')
    check(len(text.encode()) < 2 * 32 * 1024 + 200, f'capped at 32 KiB each end ({len(text.encode())} bytes from {(PROJECT / "big.log").stat().st_size})')
    shot('07-big-capped', f'A {(PROJECT / "big.log").stat().st_size // 1024} KiB selection in the composer, cut to its first and last 32 KiB.')
    send_and_receive(text, 'capped excerpt')

    # Dark theme (the private desktop starts light): the same surfaces.
    theme(2)
    if 'file-view-back' not in ids(): key('ctrl', 'shift', 'e')
    click('file-view-back'); wait('files-tree-row-2')
    click('files-tree-row-1'); wait('files-tree-row-3'); key('Down'); key('space'); wait('file-line-19')
    click('file-line-12'); click('file-line-19', keys=['shift']); click('file-line-15', button='right'); wait('menu-send-selection')
    shot('08-dark-files-menu', 'Dark theme: the read-only view, the selected lines and the menu.')
    key('Escape')
    until(lambda: 'menu-send-selection' not in ids(), 'Escape closes the menu')
    check('file-line-19' in ids(), 'Escape on the menu keeps the file shown')
    running = 'stop' in ids()
    sent = len(received())
    click('file-view-send')
    until(lambda: draft(1) == expected, 'the button prefills the same lines')
    check(len(received()) == sent, f'the button sends nothing (turn running: {running})')
    shot('09-dark-prefilled', 'Dark theme: the composer after the view\'s Send to Claude Code button' + (', while a turn still runs.' if running else '.'))
    time.sleep(1)
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
        shutil.copy(recording['path'], OUT / 'send-selection-journey.mp4')
    (OUT / 'journey.json').write_text(json.dumps(log, indent=1, default=str))
    shutil.rmtree(SCRATCH, ignore_errors=True)
