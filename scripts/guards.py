#!/usr/bin/env python3
"""Source rules that scripts/check.sh enforces, one per argument.

`python3 scripts/guards.py colors` fails on GPUI colors outside crates/keystone-ui/src/theme.rs.
`python3 scripts/guards.py shortcuts` fails on ctrl- key bindings other than ctrl-tab and ctrl-shift-tab.
`python3 scripts/guards.py hints` fails on Ctrl+… hints written out in docs/ui-strings.txt.

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
        if SHORTCUT.search(pcre_text(text)):
            yield b'%s:%d:%s' % (os.fsencode(path), number, text)


def hints():
    source = b''
    for _, text in lines(INVENTORY):
        if text.startswith(b'crates/'):
            source = text
        if HINT.search(text) and source not in HINT_EXEMPT:
            yield source + b': ' + text


RULES = {
    'colors': (colors, 'Colors belong in crates/keystone-ui/src/theme.rs. Use a theme token or Color.'),
    'shortcuts': (shortcuts, 'Spell shortcuts secondary-… (Cmd on macOS, Ctrl elsewhere). '
                             'Only ctrl-tab and ctrl-shift-tab stay ctrl-.'),
    'hints': (hints, 'Write shortcut hints with keystone_ui::shortcut_text("secondary-…"), '
                     'which shows Ctrl+… on Linux and ⌘ on macOS.'),
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
