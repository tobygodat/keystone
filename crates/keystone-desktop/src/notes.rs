//! Notes: one note per project in a Notes surface of the layout, kept in Keystone's
//! database rather than in the project's folder. Add to draft puts the selection, or
//! else the whole note, in the composer; it never sends.

use crate::Keystone;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, Focusable, IntoElement, Subscription, Task,
    Window, div, prelude::*, px,
};
use keystone_core::{Component, ProjectId};
use keystone_ui::prelude::*;
use keystone_ui::text_input::Changed;
use std::collections::HashMap;
use std::num::NonZeroU16;

gpui::actions!(notes, [ToggleNotes]);

pub(crate) const NOTES_WIDTH: keystone_core::Size =
    keystone_core::Size::Px(NonZeroU16::new(320).unwrap());

/// This window's editor for each project's note, so switching projects keeps each
/// note's caret, and typing not yet saved still saves to its own project.
#[derive(Default)]
pub(crate) struct Notes {
    editors: HashMap<ProjectId, NoteEditor>,
}

struct NoteEditor {
    input: Entity<TextInput>,
    save_task: Option<Task<()>>,
    _changed: Subscription,
}

impl Keystone {
    fn note_editor(&mut self, project: &ProjectId, cx: &mut Context<Self>) -> Entity<TextInput> {
        if let Some(editor) = self.notes.editors.get(project) {
            return editor.input.clone();
        }
        let this = cx.entity();
        let typed = crate::open_windows(cx)
            .into_iter()
            .filter(|other| other != &this)
            .find_map(|other| {
                other
                    .read(cx)
                    .notes
                    .editors
                    .get(project)
                    .map(|editor| editor.input.read(cx).value().to_owned())
            });
        let text = typed.unwrap_or_else(|| {
            self.state
                .read(cx)
                .store()
                .workspace()
                .note(project)
                .to_owned()
        });
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", InputMode::Multiline, cx)
                .text_style(TextSize::Body, Color::Strong)
                .fill_height();
            input.set_value(text, cx);
            input
        });
        let changed = project.clone();
        let subscription = cx.subscribe(&input, move |this, _, _: &Changed, cx| {
            this.note_changed(&changed, cx)
        });
        self.notes.editors.insert(
            project.clone(),
            NoteEditor {
                input: input.clone(),
                save_task: None,
                _changed: subscription,
            },
        );
        input
    }

    /// Shows the typed note in every other window's editor for the project, and saves
    /// it once typing pauses, as the composer saves its draft.
    fn note_changed(&mut self, project: &ProjectId, cx: &mut Context<Self>) {
        let Some(editor) = self.notes.editors.get_mut(project) else {
            return;
        };
        let text = editor.input.read(cx).value().to_owned();
        let saving = project.clone();
        editor.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |this, cx| this.save_note(&saving, cx));
        }));
        let this = cx.entity();
        for other in crate::open_windows(cx) {
            if other == this {
                continue;
            }
            other.update(cx, |other, cx| {
                if let Some(editor) = other.notes.editors.get_mut(project)
                    && editor.input.read(cx).value() != text
                {
                    editor.save_task = None;
                    editor
                        .input
                        .update(cx, |input, cx| input.set_value(text.clone(), cx));
                }
            });
        }
    }

    fn save_note(&mut self, project: &ProjectId, cx: &mut Context<Self>) -> bool {
        let Some(editor) = self.notes.editors.get_mut(project) else {
            return true;
        };
        editor.save_task = None;
        let text = editor.input.read(cx).value().to_owned();
        match self
            .state
            .update(cx, |state, cx| state.set_note(project, text, cx))
        {
            Ok(applied) => {
                self.error = crate::durability_warning(applied.durability);
                cx.notify();
                self.error.is_none()
            }
            Err(error) => {
                self.error = Some(format!("Note not saved. {error}"));
                cx.notify();
                false
            }
        }
    }

    /// The notes this window's editors hold that differ from the saved ones, for a
    /// final save. Their pending saves stay, so a refused Quit or close still saves them.
    pub(crate) fn unsaved_notes(&self, cx: &App) -> Vec<(ProjectId, String)> {
        let workspace = self.state.read(cx).store().workspace();
        self.notes
            .editors
            .iter()
            .map(|(project, editor)| (project, editor.input.read(cx).value()))
            .filter(|(project, text)| *text != workspace.note(project))
            .map(|(project, text)| (project.clone(), text.to_owned()))
            .collect()
    }

    /// Saves every note this window has unsaved. Returns false, with the error shown,
    /// when one cannot be saved; the others are still saved.
    pub(crate) fn save_notes(&mut self, cx: &mut Context<Self>) -> bool {
        let mut failed = None;
        for (project, _) in self.unsaved_notes(cx) {
            if !self.save_note(&project, cx) && failed.is_none() {
                failed = self.error.clone();
            }
        }
        if failed.is_some() {
            self.error = failed;
        }
        self.error.is_none()
    }

    pub(crate) fn focus_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .shown_composition(cx)
            .views()
            .any(|(_, component)| component == Component::Notes)
        {
            return;
        }
        if let Ok(project) = self.presentation.project.clone() {
            let input = self.note_editor(&project, cx);
            window.focus(&input.focus_handle(cx), cx);
        }
    }

    pub(crate) fn notes_contain_focus(&self, window: &Window, cx: &App) -> bool {
        self.presentation
            .project
            .as_ref()
            .ok()
            .and_then(|project| self.notes.editors.get(project))
            .is_some_and(|editor| editor.input.focus_handle(cx).contains_focused(window, cx))
    }

    /// Puts the note's selection, or else the whole note, in the composer at its
    /// cursor, on a line of its own after any text there, and focuses the composer.
    /// Nothing is sent.
    fn send_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self
            .presentation
            .project
            .as_ref()
            .ok()
            .and_then(|project| self.notes.editors.get(project))
            .map(|editor| editor.input.read(cx))
        else {
            return;
        };
        let text = match &input.value()[input.selected_range()] {
            "" => input.value(),
            selection => selection,
        }
        .to_owned();
        if text.trim().is_empty() || self.selected(cx).is_none() {
            return;
        }
        self.insert_into_composer(&text, window, cx);
    }

    pub(crate) fn note_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let project = match self.presentation.project.clone() {
            Ok(project) => project,
            Err(error) => {
                return div()
                    .flex_1()
                    .px(px(14.))
                    .py_2()
                    .automation_id("notes-empty")
                    .child(Label::new(error).size(TextSize::Ui).color(Color::Muted))
                    .into_any_element();
            }
        };
        let input = self.note_editor(&project, cx);
        let sendable = self.selected(cx).is_some();
        v_flex()
            // Ctrl+Enter, which sends the composer's message, puts the note in it here.
            .on_action(
                self.scoped_listener(cx, |this, _: &crate::SendMessage, window, cx| {
                    this.send_note(window, cx)
                }),
            )
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                div()
                    .automation_id("notes-editor")
                    .flex_1()
                    .min_h_0()
                    .px(px(14.))
                    .py(px(10.))
                    .child(input),
            )
            .child(
                h_flex()
                    .flex_none()
                    .justify_end()
                    .px(px(10.))
                    .py(px(6.))
                    .child(
                        Button::new("notes-send", "Add to draft")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .disabled(!sendable)
                            .debug_selector(|| "notes-send".into())
                            .tooltip(Tooltip::text(if sendable {
                                "Ctrl+Enter"
                            } else {
                                "Select a session"
                            }))
                            .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                                this.send_note(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "notes_tests.rs"]
mod tests;
