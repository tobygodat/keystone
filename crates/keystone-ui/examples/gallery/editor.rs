//! The editor page: a code editor holding a sample Rust file, with a status line for the
//! cursor and whether the text differs from the sample. Nothing is saved.

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window, div, prelude::*,
};
use keystone_ui::prelude::*;

const SAMPLE: &str = r#"//! Sample text for the gallery, not a real file.

use std::collections::HashMap;

/// Counts how often each word appears.
pub fn word_counts(text: &str) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word).or_insert(0) += 1;
    }
    counts
}

#[derive(Debug, Clone, PartialEq)]
pub struct Greeting {
    pub name: String,
    pub language: &'static str,
}

impl Greeting {
    pub fn render(&self) -> String {
        match self.language {
            "ja" => format!("こんにちは、{}さん 👋", self.name),
            "fr" => format!("Bonjour, {} !", self.name),
            _ => format!("Hello, {}!", self.name),
        }
    }
}

// Tabs draw four columns wide:	like this.
// A long line scrolls sideways instead of wrapping, as in any code editor, so the shape of the code stays exactly as it was written.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_repeated_words() {
        let counts = word_counts("a b a");
        assert_eq!(counts["a"], 2);
        assert_eq!(counts["b"], 1);
    }
}
"#;

pub struct EditorPage {
    editor: Entity<CodeEditor>,
    sample_revision: u64,
}

impl EditorPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = CodeEditor::new(cx).with_id("gallery-editor");
            editor.set_text(SAMPLE, cx);
            editor
        });
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        let sample_revision = editor.read(cx).revision();
        Self {
            editor,
            sample_revision,
        }
    }
}

impl Focusable for EditorPage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Render for EditorPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.editor.read(cx);
        let cursor = editor.selection().end;
        let status = format!(
            "Sample Rust file, not saved · line {}, column {} · {} lines · {}",
            cursor.line + 1,
            cursor.column + 1,
            editor.line_count(),
            if editor.revision() == self.sample_revision {
                "unchanged"
            } else {
                "edited"
            },
        );
        v_flex()
            .flex_1()
            .min_h_0()
            .px_6()
            .pb_6()
            .gap_3()
            .child(
                div()
                    .automation_id("gallery-editor-status")
                    .child(Label::new(status).color(Color::Muted)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .py_1()
                    .surface(Elevation::Raised, cx)
                    .child(self.editor.clone()),
            )
    }
}
