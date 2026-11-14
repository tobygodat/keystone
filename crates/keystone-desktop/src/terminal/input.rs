//! What a keystroke sends to the shell, as xterm encodes it. Text, the characters a
//! keyboard layout or an input method types, comes through the pane's input handler
//! instead; see `docs/design/terminal.md`.

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;
use std::borrow::Cow;

/// The bytes `keystroke` sends to a program running in `mode` as a key, or `None` when
/// it sends none as a key: ⌘ shortcuts on macOS, which stay with the app, and text,
/// which the platform then hands to the pane's input handler. With `alt_is_meta`, Alt
/// (Option on macOS) with a character key sends Esc and the key; without it the
/// character the layout types with Option is text.
pub(crate) fn bytes(
    keystroke: &Keystroke,
    mode: TermMode,
    alt_is_meta: bool,
) -> Option<Cow<'static, [u8]>> {
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
        // A key named by its keysym, such as the keypad's `add` or a composed dead
        // key's `eacute`. Alone it types text through the input handler. With Ctrl or
        // Meta the platform hands the input handler nothing, so its character goes
        // here, after ESC for Meta: Alt+KP_Add sends ESC +.
        if !(m.control || m.alt && alt_is_meta) {
            return None;
        }
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
    // As Meta, Alt sends ESC before the unmodified key, so readline's word motions
    // work.
    if m.alt && alt_is_meta {
        let key = if m.shift {
            key.to_ascii_uppercase()
        } else {
            key
        };
        return Some(alt(key.to_string().as_bytes()));
    }
    None
}

/// Whether Option types the character the keyboard layout gives it unless the user
/// chose Option as Meta, as on macOS. Elsewhere Alt is always Meta.
pub(crate) fn option_types_text() -> bool {
    #[cfg(test)]
    if let Some(mac) = MAC_KEYBOARD.get() {
        return mac;
    }
    cfg!(target_os = "macos")
}

#[cfg(test)]
thread_local! {
    /// Makes this thread's panes treat Option as macOS does, or as Linux does.
    pub(crate) static MAC_KEYBOARD: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
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
        sent_as(keys, mode, true)
    }

    fn sent_as(keys: &str, mode: TermMode, alt_is_meta: bool) -> Option<String> {
        let keystroke = Keystroke::parse(keys).unwrap().with_simulated_ime();
        bytes(&keystroke, mode, alt_is_meta)
            .map(|bytes| String::from_utf8(bytes.into_owned()).unwrap())
    }

    #[test]
    fn keys_encode_as_xterm_does() {
        let normal = TermMode::empty();
        let table = [
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

    /// Characters arrive as text through the input handler, so input methods and
    /// dead keys can compose them; as keys they send nothing.
    #[test]
    fn text_is_left_to_the_input_handler() {
        for keys in ["a", "shift-a", "1", "shift-2", "/"] {
            assert_eq!(sent(keys, TermMode::empty()), None, "{keys}");
        }
        let composed = Keystroke {
            key: "eacute".into(),
            key_char: Some("é".into()),
            modifiers: Default::default(),
        };
        assert_eq!(bytes(&composed, TermMode::empty(), true), None);
    }

    /// Keypad operators are named by keysym (`add` for KP_Add). With Alt as Meta they
    /// send ESC and their character; alone they type it as text.
    #[test]
    fn alt_with_a_keypad_operator_sends_esc_and_its_character() {
        let key = |key: &str, text: &str, alt: bool| Keystroke {
            key: key.into(),
            key_char: Some(text.into()),
            modifiers: gpui::Modifiers {
                alt,
                ..Default::default()
            },
        };
        let send = |keystroke: &Keystroke, meta: bool| {
            bytes(keystroke, TermMode::empty(), meta)
                .map(|bytes| String::from_utf8(bytes.into_owned()).unwrap())
        };
        for (name, text) in [
            ("add", "+"),
            ("subtract", "-"),
            ("multiply", "*"),
            ("divide", "/"),
            ("decimal", "."),
            ("separator", ","),
            ("equal", "="),
        ] {
            assert_eq!(
                send(&key(name, text, true), true).as_deref(),
                Some(format!("\x1b{text}").as_str()),
                "Alt+KP_{name}"
            );
            assert_eq!(
                send(&key(name, text, false), true),
                None,
                "KP_{name} types text"
            );
            assert_eq!(
                send(&key(name, text, true), false),
                None,
                "Option+KP_{name} types text on macOS"
            );
        }
    }

    /// Option without Meta leaves its character to the layout, as on a German Mac
    /// where Option+L types @. Named keys keep xterm's Alt encoding either way.
    #[test]
    fn option_types_text_unless_it_is_meta() {
        let normal = TermMode::empty();
        assert_eq!(sent_as("alt-l", normal, false), None);
        assert_eq!(sent_as("alt-shift-7", normal, false), None);
        assert_eq!(sent_as("alt-l", normal, true).as_deref(), Some("\x1bl"));
        assert_eq!(
            sent_as("alt-left", normal, false).as_deref(),
            Some("\x1b[1;3D")
        );
        assert_eq!(
            sent_as("alt-backspace", normal, false).as_deref(),
            Some("\x1b\x7f")
        );
        assert_eq!(
            sent_as("ctrl-alt-w", normal, false).as_deref(),
            Some("\x1b\x17")
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
