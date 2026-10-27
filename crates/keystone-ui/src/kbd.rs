use crate::color::Color;
use crate::label::Label;
use crate::tokens::TextSize;
use gpui::{App, IntoElement, RenderOnce, SharedString, Window};

/// A keyboard shortcut hint, such as `Ctrl+N`, written by [`shortcut_text`].
#[derive(IntoElement)]
pub struct Kbd {
    keys: SharedString,
}

impl Kbd {
    pub fn new(keys: impl Into<SharedString>) -> Self {
        Self { keys: keys.into() }
    }
}

impl RenderOnce for Kbd {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        Label::new(self.keys).size(TextSize::Sm).color(Color::Faint)
    }
}

/// `keys`, GPUI keystrokes such as `secondary-shift-n`, as the platform writes them:
/// `Ctrl+Shift+N` on Linux, `⇧⌘N` on macOS. Chords separated by spaces stay separated.
pub fn shortcut_text(keys: &str) -> String {
    keys.split_whitespace()
        .map(stroke_text)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(not(target_os = "macos"))]
fn stroke_text(stroke: &str) -> String {
    stroke
        .split('-')
        .map(|part| match part {
            "secondary" | "ctrl" => "Ctrl".to_owned(),
            _ => {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Modifiers come in Apple's order, Control, Option, Shift, Command, then the key.
#[cfg(target_os = "macos")]
fn stroke_text(stroke: &str) -> String {
    let (modifiers, key) = stroke.rsplit_once('-').unwrap_or(("", stroke));
    let modifiers: Vec<&str> = modifiers.split('-').collect();
    let mut text = String::new();
    for (names, symbol) in [
        (&["ctrl"][..], "⌃"),
        (&["alt"], "⌥"),
        (&["shift"], "⇧"),
        (&["cmd", "secondary"], "⌘"),
    ] {
        if modifiers.iter().any(|m| names.contains(m)) {
            text.push_str(symbol);
        }
    }
    let symbol = match key {
        "enter" => "↩",
        "backspace" => "⌫",
        "delete" => "⌦",
        "tab" => "⇥",
        "escape" => "⎋",
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        _ => {
            let mut chars = key.chars();
            if let Some(first) = chars.next() {
                text.extend(first.to_uppercase().chain(chars));
            }
            return text;
        }
    };
    text.push_str(symbol);
    text
}

#[cfg(test)]
mod tests {
    use super::shortcut_text;

    #[test]
    fn names_each_key_in_the_platforms_notation() {
        let cases: &[(&str, &str, &str)] = &[
            ("secondary-shift-n", "Ctrl+Shift+N", "⇧⌘N"),
            ("secondary-b", "Ctrl+B", "⌘B"),
            ("ctrl-tab", "Ctrl+Tab", "⌃⇥"),
            ("f2", "F2", "F2"),
            ("alt-enter", "Alt+Enter", "⌥↩"),
            ("secondary-alt-o", "Ctrl+Alt+O", "⌥⌘O"),
            ("secondary-,", "Ctrl+,", "⌘,"),
            ("secondary-\\", "Ctrl+\\", "⌘\\"),
            ("secondary-k secondary-s", "Ctrl+K Ctrl+S", "⌘K ⌘S"),
        ];
        for (keys, linux, macos) in cases {
            let expected = if cfg!(target_os = "macos") {
                macos
            } else {
                linux
            };
            assert_eq!(shortcut_text(keys), *expected, "{keys}");
        }
    }
}
