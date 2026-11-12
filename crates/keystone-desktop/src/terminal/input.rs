//! What a keystroke sends to the shell, as xterm encodes it.

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;
use std::borrow::Cow;

/// The bytes `keystroke` sends to a program running in `mode`, or `None` when the
/// terminal does not take it, as for ⌘ shortcuts on macOS.
pub(crate) fn bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Cow<'static, [u8]>> {
    let m = &keystroke.modifiers;
    if m.platform || m.function {
        return None;
    }
    // xterm's modifier parameter: 1 plus shift 1, alt 2, ctrl 4.
    let param = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let cursor = |end: char| -> Cow<'static, [u8]> {
        match (param, app_cursor) {
            (1, true) => vec![0x1b, b'O', end as u8],
            (1, false) => vec![0x1b, b'[', end as u8],
            _ => format!("\x1b[1;{param}{end}").into_bytes(),
        }
        .into()
    };
    let tilde = |code: u8| -> Cow<'static, [u8]> {
        match param {
            1 => format!("\x1b[{code}~"),
            _ => format!("\x1b[{code};{param}~"),
        }
        .into_bytes()
        .into()
    };
    let function = |end: char| -> Cow<'static, [u8]> {
        match param {
            1 => vec![0x1b, b'O', end as u8],
            _ => format!("\x1b[1;{param}{end}").into_bytes(),
        }
        .into()
    };
    let alt = |bytes: &[u8]| -> Cow<'static, [u8]> {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if m.alt {
            out.push(0x1b);
        }
        out.extend_from_slice(bytes);
        out.into()
    };
    let named = match keystroke.key.as_str() {
        "enter" => Some(alt(b"\r")),
        "backspace" if m.control => Some(alt(b"\x08")),
        "backspace" => Some(alt(b"\x7f")),
        "tab" if m.shift => Some(Cow::Borrowed(&b"\x1b[Z"[..])),
        "tab" => Some(alt(b"\t")),
        "escape" => Some(alt(b"\x1b")),
        "space" if m.control => Some(alt(b"\0")),
        "space" => Some(alt(b" ")),
        "up" => Some(cursor('A')),
        "down" => Some(cursor('B')),
        "right" => Some(cursor('C')),
        "left" => Some(cursor('D')),
        "home" => Some(cursor('H')),
        "end" => Some(cursor('F')),
        "insert" => Some(tilde(2)),
        "delete" => Some(tilde(3)),
        "pageup" => Some(tilde(5)),
        "pagedown" => Some(tilde(6)),
        "f1" => Some(function('P')),
        "f2" => Some(function('Q')),
        "f3" => Some(function('R')),
        "f4" => Some(function('S')),
        "f5" => Some(tilde(15)),
        "f6" => Some(tilde(17)),
        "f7" => Some(tilde(18)),
        "f8" => Some(tilde(19)),
        "f9" => Some(tilde(20)),
        "f10" => Some(tilde(21)),
        "f11" => Some(tilde(23)),
        "f12" => Some(tilde(24)),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let mut chars = keystroke.key.chars();
    let (Some(key), None) = (chars.next(), chars.next()) else {
        return keystroke.key_char.as_ref().map(|text| alt(text.as_bytes()));
    };
    if m.control {
        let control = match key.to_ascii_lowercase() {
            c @ 'a'..='z' => c as u8 & 0x1f,
            '@' | '2' => 0,
            '[' | '3' => 0x1b,
            '\\' | '4' => 0x1c,
            ']' | '5' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '-' | '/' | '7' => 0x1f,
            '?' | '8' => 0x7f,
            _ => return None,
        };
        return Some(alt(&[control]));
    }
    // Alt sends ESC before the unmodified key, so readline's word motions work.
    if m.alt {
        let key = if m.shift {
            key.to_ascii_uppercase()
        } else {
            key
        };
        return Some(alt(key.to_string().as_bytes()));
    }
    keystroke
        .key_char
        .as_ref()
        .map(|text| Cow::Owned(text.as_bytes().to_vec()))
}

/// `text` as pasted: line breaks become Enter, and inside bracketed paste the program
/// gets it between markers it cannot be tricked into ending early.
pub(crate) fn paste(text: &str, mode: TermMode) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let text = text.replace('\x1b', "");
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(keys: &str, mode: TermMode) -> Option<String> {
        let mut keystroke = Keystroke::parse(keys).unwrap();
        if keystroke.key_char.is_none() && keystroke.key.chars().count() == 1 {
            let m = &keystroke.modifiers;
            if !m.control && !m.platform && !m.alt {
                keystroke.key_char = Some(if m.shift {
                    keystroke.key.to_uppercase()
                } else {
                    keystroke.key.clone()
                });
            }
        }
        bytes(&keystroke, mode).map(|bytes| String::from_utf8(bytes.into_owned()).unwrap())
    }

    #[test]
    fn keys_encode_as_xterm_does() {
        let normal = TermMode::empty();
        let table = [
            ("a", "a"),
            ("shift-a", "A"),
            ("enter", "\r"),
            ("backspace", "\x7f"),
            ("ctrl-backspace", "\x08"),
            ("tab", "\t"),
            ("shift-tab", "\x1b[Z"),
            ("escape", "\x1b"),
            ("space", " "),
            ("ctrl-space", "\0"),
            ("ctrl-c", "\x03"),
            ("ctrl-l", "\x0c"),
            ("ctrl-shift-d", "\x04"),
            ("ctrl-[", "\x1b"),
            ("ctrl-\\", "\x1c"),
            ("ctrl-]", "\x1d"),
            ("ctrl-/", "\x1f"),
            ("alt-b", "\x1bb"),
            ("alt-shift-b", "\x1bB"),
            ("alt-enter", "\x1b\r"),
            ("ctrl-alt-w", "\x1b\x17"),
            ("up", "\x1b[A"),
            ("left", "\x1b[D"),
            ("shift-up", "\x1b[1;2A"),
            ("ctrl-right", "\x1b[1;5C"),
            ("alt-left", "\x1b[1;3D"),
            ("home", "\x1b[H"),
            ("end", "\x1b[F"),
            ("pageup", "\x1b[5~"),
            ("pagedown", "\x1b[6~"),
            ("insert", "\x1b[2~"),
            ("delete", "\x1b[3~"),
            ("shift-delete", "\x1b[3;2~"),
            ("f1", "\x1bOP"),
            ("f4", "\x1bOS"),
            ("shift-f1", "\x1b[1;2P"),
            ("f5", "\x1b[15~"),
            ("f12", "\x1b[24~"),
            ("ctrl-f5", "\x1b[15;5~"),
        ];
        for (keys, expected) in table {
            assert_eq!(sent(keys, normal).as_deref(), Some(expected), "{keys}");
        }
    }

    #[test]
    fn application_cursor_mode_changes_arrows_home_and_end() {
        let app = TermMode::APP_CURSOR;
        assert_eq!(sent("up", app).as_deref(), Some("\x1bOA"));
        assert_eq!(sent("home", app).as_deref(), Some("\x1bOH"));
        assert_eq!(sent("end", app).as_deref(), Some("\x1bOF"));
        assert_eq!(sent("shift-up", app).as_deref(), Some("\x1b[1;2A"));
    }

    #[test]
    fn command_shortcuts_stay_with_the_app() {
        assert_eq!(sent("cmd-c", TermMode::empty()), None);
        assert_eq!(sent("cmd-j", TermMode::empty()), None);
    }

    #[test]
    fn typed_text_without_a_single_key_passes_through() {
        let keystroke = Keystroke {
            key: "dead".into(),
            key_char: Some("é".into()),
            modifiers: Default::default(),
        };
        assert_eq!(
            bytes(&keystroke, TermMode::empty()).as_deref(),
            Some("é".as_bytes())
        );
    }

    #[test]
    fn paste_turns_newlines_into_enter_and_brackets_when_asked() {
        assert_eq!(paste("a\nb\r\nc", TermMode::empty()), b"a\rb\rc");
        assert_eq!(
            paste("ls\x1b[201~\n", TermMode::BRACKETED_PASTE),
            b"\x1b[200~ls[201~\r\x1b[201~"
        );
    }
}
