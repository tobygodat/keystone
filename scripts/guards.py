#!/usr/bin/env python3
"""Source rules that scripts/check.sh enforces, one per argument.

`python3 scripts/guards.py colors` fails on GPUI colors outside crates/keystone-ui/src/theme.rs.
`python3 scripts/guards.py shortcuts` fails on ctrl- key bindings other than ctrl-tab and ctrl-shift-tab,
outside the terminal's key encoding.
`python3 scripts/guards.py hints` fails on Ctrl+… hints written out in docs/ui-strings.txt.
`python3 scripts/guards.py executables` fails on a script (`#!` text) written with fs::write, which a sibling
test's fork can hold open so that running it fails with "Text file busy".

It exits 0 when nothing matches, 1 after printing each match, and 2 when a file or folder cannot be read.
"""
import os
import re
import sys
import unicodedata
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCES = 'crates'
INVENTORY = 'docs/ui-strings.txt'
THEME = 'crates/keystone-ui/src/theme.rs'
COLOR = re.compile(r'\b(rgba?|hsla|black|white|transparent_black|transparent_white|red|blue|green|yellow'
                   r'|opaque_grey)\(|(?<!-> )(?<!for )\b(Hsla|Rgba)\s*\{')
SHORTCUT = re.compile(r'"(?:[^"\n]*[ -])?ctrl-(?!(?:shift-)?tab\b)')
HINT = re.compile(rb'Ctrl[+ ]')
WRITE = re.compile(rb'\bfs::write\(')
STRING = re.compile(rb'r(#*)".*?"\1|b?"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\'', re.S)
# The terminal's key encoding and its tests mean the Control key itself on every platform.
SHORTCUT_EXEMPT = {'crates/keystone-desktop/src/terminal/input.rs', 'crates/keystone-desktop/src/terminal_tests/input.rs'}
HINT_EXEMPT = {b'crates/keystone-agent/src/lib.rs', b'crates/keystone-ui/src/kbd.rs'}


def lines(path):
    text = (ROOT / path).read_bytes().split(b'\n')
    return enumerate(text[:-1] if text[-1] == b'' else text, 1)


def pcre_text(text):
    # PCRE2 includes nonspacing marks and connector punctuation in \w; Python re does not.
    return ''.join('é' if char != '_' and unicodedata.category(char) in ('Mn', 'Pc') else char
                   for char in text.decode('utf-8', errors='surrogateescape'))


def rust_lines():
    def fail(error):
        raise error

    paths = []
    for folder, dirs, names in os.walk(ROOT / SOURCES, onerror=fail):
        dirs.sort()
        paths += [Path(folder, name).relative_to(ROOT).as_posix() for name in sorted(names)
                  if name.endswith('.rs') and not Path(folder, name).is_symlink()]
    if not paths:
        raise FileNotFoundError(f'no Rust files under {SOURCES}')
    for path in paths:
        for number, text in lines(path):
            yield path, number, text


def colors():
    for path, number, text in rust_lines():
        if path != THEME and COLOR.search(pcre_text(text)):
            yield b'%s:%d:%s' % (os.fsencode(path), number, text)


def shortcuts():
    for path, number, text in rust_lines():
        if path not in SHORTCUT_EXEMPT and SHORTCUT.search(pcre_text(text)):
            yield b'%s:%d:%s' % (os.fsencode(path), number, text)


def hints():
    source = b''
    for _, text in lines(INVENTORY):
        if text.startswith(b'crates/'):
            source = text
        if HINT.search(text) and source not in HINT_EXEMPT:
            yield source + b': ' + text


def call_text(text, start):
    """The text of the call whose `(` ends at `start`, through its matching `)`."""
    depth, at = 1, start
    while depth and at < len(text):
        string = STRING.match(text, at)
        if string:
            at = string.end()
            continue
        depth += {ord('('): 1, ord(')'): -1}.get(text[at], 0)
        at += 1
    return text[start:at]


def executables():
    for path in sorted({path for path, _, _ in rust_lines()}):
        text = (ROOT / path).read_bytes()
        for call in WRITE.finditer(text):
            if b'#!' in call_text(text, call.end()):
                number = text.count(b'\n', 0, call.start()) + 1
                yield b'%s:%d:%s' % (os.fsencode(path), number, text.split(b'\n')[number - 1])


RULES = {
    'colors': (colors, 'Colors belong in crates/keystone-ui/src/theme.rs. Use a theme token or Color.'),
    'shortcuts': (shortcuts, 'Spell shortcuts secondary-… (Cmd on macOS, Ctrl elsewhere). '
                             'Only ctrl-tab and ctrl-shift-tab stay ctrl-.'),
    'hints': (hints, 'Write shortcut hints with keystone_ui::shortcut_text("secondary-…"), '
                     'which shows Ctrl+… on Linux and ⌘ on macOS.'),
    'executables': (executables, 'Write a script a test runs with keystone_core::test_support::write_executable, '
                                 'so no fork from another test can keep it busy.'),
}


def main(argv):
    if len(argv) != 2 or argv[1] not in RULES:
        print(f'Usage: python3 scripts/guards.py {"|".join(RULES)}', file=sys.stderr)
        return 2
    rule, message = RULES[argv[1]]
    try:
        found = list(rule())
    except OSError as error:
        print(f'guards.py {argv[1]}: {error}', file=sys.stderr)
        return 2
    for line in found:
        sys.stdout.buffer.write(line + b'\n')
    if found:
        sys.stdout.flush()
        print(message, file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
