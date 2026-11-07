//! Send to the agent: text selected in a Files view or a reply goes into the composer
//! of the session this window shows, with the file's path and lines. It never sends.

use crate::Keystone;
use gpui::{App, Context, Pixels, Point, Window};
use keystone_ui::prelude::*;
use std::ops::RangeInclusive;
use std::rc::Rc;

gpui::actions!(excerpt, [SendSelection]);

/// The palette row that sends the selection the palette was opened over.
pub(crate) const SEND_SELECTION: &str = "send-selection";

/// Selected text, cut to its first and last 32 KiB as tool output is, and the file and
/// 1-based lines it came from when it came from a file.
#[derive(Debug, PartialEq)]
pub(crate) struct Excerpt {
    file: Option<(String, RangeInclusive<usize>)>,
    text: String,
}

/// A selection taken for the session the window showed when it was taken.
pub(crate) type Taken = (u64, Rc<Excerpt>);

impl Excerpt {
    pub(crate) fn file(path: String, lines: RangeInclusive<usize>, text: &str) -> Self {
        Self {
            file: Some((path, lines)),
            text: keystone_agent::cap_output(text),
        }
    }

    pub(crate) fn reply(text: &str) -> Self {
        Self {
            file: None,
            text: keystone_agent::cap_output(text),
        }
    }

    /// The path and lines, then the text in a code fence; a reply's text is quoted.
    pub(crate) fn prompt(&self) -> String {
        let Some((path, lines)) = &self.file else {
            return self
                .text
                .lines()
                .map(|line| format!("> {line}").trim_end().to_owned())
                .collect::<Vec<_>>()
                .join("\n");
        };
        let lines = if lines.start() == lines.end() {
            format!("line {}", lines.start())
        } else {
            format!("lines {}-{}", lines.start(), lines.end())
        };
        let ticks = self
            .text
            .split(|c| c != '`')
            .map(str::len)
            .max()
            .unwrap_or(0);
        let fence = "`".repeat(ticks.max(2) + 1);
        format!("{path} {lines}:\n{fence}\n{}\n{fence}", self.text)
    }
}

impl Keystone {
    /// What the focused Files view or reply has selected, for the session this window shows.
    pub(crate) fn take_selection(&self, window: &Window, cx: &App) -> Option<Taken> {
        let session = self.selected(cx)?;
        let excerpt = self.file_view_excerpt(window, cx).or_else(|| {
            keystone_ui::markdown::focused_selection(window, cx)
                .filter(|text| !text.trim().is_empty())
                .map(|text| Excerpt::reply(&text))
        })?;
        Some((session, Rc::new(excerpt)))
    }

    /// "Send to Claude Code", for the session this window shows.
    pub(crate) fn send_label(&self, cx: &App) -> Option<String> {
        let provider = self.provider(self.selected(cx)?, cx)?;
        Some(format!("Send to {}", crate::provider_name(provider)))
    }

    /// Puts the excerpt in the composer on lines of its own and focuses it, while the
    /// window still shows the session it was taken for. Nothing is sent.
    pub(crate) fn send_excerpt(
        &mut self,
        (session, excerpt): &Taken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected(cx) == Some(*session) {
            self.insert_into_composer(&excerpt.prompt(), window, cx);
        }
    }

    pub(crate) fn send_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(taken) = self.take_selection(window, cx) {
            self.send_excerpt(&taken, window, cx);
        }
    }

    /// The menu a right click on a selection opens.
    pub(crate) fn open_selection_menu(
        &mut self,
        taken: Option<Taken>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(taken) = taken.filter(|_| self.send_label(cx).is_some()) {
            self.context_menu = Some(crate::windows::ContextMenu::selection(taken, position));
            cx.notify();
        }
    }

    /// The right click on reply `index` of `key`: its menu, for its own selection only.
    pub(crate) fn open_reply_menu(
        &mut self,
        key: crate::view_state::ViewKey,
        index: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keyboard = self.presentation.current.views.keyboard(key, cx);
        let own = keyboard.row == Some(index)
            && keyboard.action == Some(crate::view_state::CopyTarget::TextSelection)
            && keyboard.action_focus.is_focused(window);
        let taken = own.then(|| self.take_selection(window, cx)).flatten();
        self.open_selection_menu(taken, position, cx);
    }

    pub(crate) fn selection_menu_item(
        &self,
        taken: &Taken,
        cx: &mut Context<Self>,
    ) -> Option<MenuItem> {
        let taken = taken.clone();
        Some(
            MenuItem::new("send-selection", self.send_label(cx)?)
                .debug_selector(|| "menu-send-selection".into())
                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                    this.context_menu = None;
                    this.send_excerpt(&taken, window, cx);
                })),
        )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "excerpt_tests.rs"]
pub(crate) mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, lines: RangeInclusive<usize>, text: &str) -> Excerpt {
        Excerpt::file(path.into(), lines, text)
    }

    #[test]
    fn a_file_excerpt_names_its_path_and_lines_and_fences_the_text() {
        assert_eq!(
            file("src/lib.rs", 12..=14, "fn a() {\n    b();\n}").prompt(),
            "src/lib.rs lines 12-14:\n```\nfn a() {\n    b();\n}\n```"
        );
        assert_eq!(
            file("a.md", 3..=3, "x").prompt(),
            "a.md line 3:\n```\nx\n```"
        );
    }

    #[test]
    fn the_fence_is_longer_than_any_backticks_in_the_text() {
        assert_eq!(
            file("a.md", 1..=3, "```rust\nx\n````").prompt(),
            "a.md lines 1-3:\n`````\n```rust\nx\n````\n`````"
        );
    }

    #[test]
    fn a_reply_excerpt_is_quoted() {
        assert_eq!(
            Excerpt::reply("first\n\nsecond").prompt(),
            "> first\n>\n> second"
        );
    }

    #[test]
    fn a_long_selection_keeps_its_first_and_last_32_kib_from_when_it_is_taken() {
        let text = format!(
            "{}{}{}",
            "a".repeat(40 * 1024),
            "b".repeat(20 * 1024),
            "c".repeat(40 * 1024)
        );
        let prompt = file("big.txt", 1..=1, &text).prompt();
        let body = prompt
            .strip_prefix("big.txt line 1:\n```\n")
            .and_then(|body| body.strip_suffix("\n```"))
            .unwrap();
        assert_eq!(body, keystone_agent::cap_output(&text));
        assert!(body.starts_with(&"a".repeat(32 * 1024)));
        assert!(body.ends_with(&"c".repeat(32 * 1024)));
        assert!(!body.contains("bb"));
        assert!(body.contains(&format!("… {} bytes omitted …", text.len() - 64 * 1024)));
        assert!(file("big.txt", 1..=1, &text).text.len() < 64 * 1024 + 64);
    }
}
