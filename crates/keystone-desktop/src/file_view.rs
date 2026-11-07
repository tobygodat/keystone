//! The Files editor: one file in a [`CodeEditor`], saved only over what Keystone last
//! read or wrote, with outside changes reloaded or shown as a conflict.

use crate::Keystone;
use crate::excerpt::Excerpt;
use crate::file_disk::{self, Disk, Saved, Version};
use gpui::{
    App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, MouseButton, Render, Subscription, Task, WeakEntity, Window, div,
    prelude::*, px,
};
use keystone_ui::code_editor::{self, CodeEditor};
use keystone_ui::highlight::Language;
use keystone_ui::prelude::*;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

gpui::actions!(file_view, [Save]);

/// The Save command's id. Its keys come from the keymap, so the user can rebind it.
pub(crate) const SAVE_FILE: &str = "save-file";

const CONTEXT: &str = "FileView";

/// How often an open file is checked for changes made outside Keystone.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

enum Content {
    Loading { _read: Task<()> },
    Unreadable(&'static str),
    Open(Box<Open>),
}

struct Open {
    editor: Entity<CodeEditor>,
    /// What the file held when Keystone last read or wrote it.
    version: Version,
    /// The editor revision that matches `version`; edited means `editor.revision() != saved`.
    saved: u64,
    outside: Option<Outside>,
    saving: Option<Saving>,
    _changed: Subscription,
    _watch: Task<()>,
}

/// A save still writing.
struct Saving {
    _task: Task<()>,
    /// Back was pressed meanwhile: close once the save lands, if it leaves nothing unsaved.
    close: bool,
}

/// What the file on disk holds when it no longer matches `version`.
enum Outside {
    /// `None` when the new contents can no longer be read as text.
    Changed(Option<(String, Version)>),
    Deleted,
}

impl Outside {
    fn from_disk(disk: Disk) -> Option<Self> {
        match disk {
            Disk::Unchanged(_) => None,
            Disk::Changed(file_disk::Read::Text(text, version)) => {
                Some(Self::Changed(Some((text, version))))
            }
            Disk::Changed(file_disk::Read::Unreadable(_)) => Some(Self::Changed(None)),
            Disk::Deleted => Some(Self::Deleted),
        }
    }
}

impl Open {
    fn edited(&self, cx: &App) -> bool {
        self.editor.read(cx).revision() != self.saved
    }

    /// Whether quitting or closing the window now could lose what the user sees: edits
    /// not yet saved, or a save still writing, which exiting would cut off.
    fn unsettled(&self, cx: &App) -> bool {
        self.edited(cx) || self.saving.is_some()
    }

    /// Shows `text` as what the file now holds.
    fn take(&mut self, text: &str, version: Version, cx: &mut App) {
        self.editor.update(cx, |editor, cx| editor.reload(text, cx));
        self.saved = self.editor.read(cx).revision();
        self.version = version;
        self.outside = None;
    }
}

pub(crate) struct FileView {
    path: PathBuf,
    /// The path an excerpt names: relative to the Files root.
    name: String,
    content: Content,
    /// Open while asking whether to discard unsaved edits.
    discard: Option<DialogFocus>,
    focus: FocusHandle,
    notice: Option<String>,
    keystone: WeakEntity<Keystone>,
}

impl EventEmitter<DismissEvent> for FileView {}

impl Focusable for FileView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.content {
            Content::Open(open) => open.editor.focus_handle(cx),
            _ => self.focus.clone(),
        }
    }
}

impl FileView {
    pub(crate) fn new(
        path: PathBuf,
        name: String,
        keystone: WeakEntity<Keystone>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let read = cx.background_spawn({
            let path = path.clone();
            async move { file_disk::read(&path) }
        });
        let loading = cx.spawn_in(window, async move |this, cx| {
            let read = read.await;
            let _ = this.update_in(cx, |this, window, cx| this.opened(read, window, cx));
        });
        Self {
            path,
            name,
            content: Content::Loading { _read: loading },
            discard: None,
            focus: cx.focus_handle(),
            notice: None,
            keystone,
        }
    }

    fn opened(&mut self, read: file_disk::Read, window: &mut Window, cx: &mut Context<Self>) {
        let (text, version) = match read {
            file_disk::Read::Text(text, version) => (text, version),
            file_disk::Read::Unreadable(why) => {
                self.content = Content::Unreadable(why);
                return cx.notify();
            }
        };
        let language = Language::for_path(&self.path);
        let editor = cx.new(|cx| {
            let mut editor = CodeEditor::new(cx).with_id("file-editor");
            editor.set_text(&text, cx);
            editor.set_language(language, cx);
            editor
        });
        let saved = editor.read(cx).revision();
        let changed = cx.subscribe(&editor, |_, _, _: &code_editor::Changed, cx| cx.notify());
        if self.focus.is_focused(window) {
            window.focus(&editor.focus_handle(cx), cx);
        }
        self.content = Content::Open(Box::new(Open {
            editor,
            version,
            saved,
            outside: None,
            saving: None,
            _changed: changed,
            _watch: self.watch(cx),
        }));
        cx.notify();
    }

    fn open(&self) -> Option<&Open> {
        match &self.content {
            Content::Open(open) => Some(open),
            _ => None,
        }
    }

    fn open_mut(&mut self) -> Option<&mut Open> {
        match &mut self.content {
            Content::Open(open) => Some(open),
            _ => None,
        }
    }

    /// Whether focus is anywhere in the view: the editor, or its header after a click.
    pub(crate) fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }

    /// The name of the file while it holds edits that aren't saved.
    pub(crate) fn unsaved(&self, cx: &App) -> Option<&str> {
        self.open()
            .filter(|open| open.unsettled(cx))
            .map(|_| self.name.as_str())
    }

    /// Checks the file for an outside change every [`WATCH_INTERVAL`]. The size and
    /// time decide when to read it again, and each stamp is read once.
    fn watch(&self, cx: &mut Context<Self>) -> Task<()> {
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let mut checked = None;
            loop {
                cx.background_executor().timer(WATCH_INTERVAL).await;
                let stamp = file_disk::stamp(&path);
                let Ok(Some(known)) = this.read_with(cx, |this, _| {
                    this.open()
                        .filter(|open| open.saving.is_none())
                        .map(|open| open.version.clone())
                }) else {
                    if this.upgrade().is_none() {
                        return;
                    }
                    continue;
                };
                if stamp == known.stamp || checked.as_ref() == Some(&stamp) {
                    continue;
                }
                checked = Some(stamp);
                let disk = cx
                    .background_spawn({
                        let (path, known) = (path.clone(), known.clone());
                        async move { file_disk::check(&path, &known) }
                    })
                    .await;
                let applied = this.update(cx, |this, cx| this.outside_changed(&known, disk, cx));
                if applied.is_err() {
                    return;
                }
            }
        })
    }

    /// Applies a check made against `known`. An unedited file takes the new text at
    /// once, so an agent's edit just shows up; an edited one keeps its text and asks.
    fn outside_changed(&mut self, known: &Version, disk: Disk, cx: &mut Context<Self>) {
        let Some(open) = self.open_mut() else {
            return;
        };
        // A save or reload since the check began makes it stale.
        if open.version != *known || open.saving.is_some() {
            return;
        }
        match disk {
            Disk::Unchanged(version) => {
                open.version = version;
                open.outside = None;
            }
            Disk::Changed(file_disk::Read::Text(text, version)) if !open.edited(cx) => {
                open.take(&text, version, cx)
            }
            disk => open.outside = Outside::from_disk(disk),
        }
        cx.notify();
    }

    fn can_save(&self, cx: &App) -> bool {
        self.open()
            .is_some_and(|open| open.saving.is_none() && open.edited(cx))
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_save(cx) {
            self.write(false, window, cx);
        }
    }

    /// Writes the editor's text, over whatever the file holds when `overwrite`, and
    /// otherwise only while it still holds `version`.
    fn write(&mut self, overwrite: bool, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let Some(open) = self.open_mut() else {
            return;
        };
        if open.saving.is_some() {
            return;
        }
        let editor = open.editor.read(cx);
        let (revision, text) = (editor.revision(), editor.text());
        let known = (!overwrite).then(|| open.version.clone());
        let saved =
            cx.background_spawn(async move { file_disk::save(&path, &text, known.as_ref()) });
        open.saving = Some(Saving {
            _task: cx.spawn_in(window, async move |this, cx| {
                let saved = saved.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.written(revision, saved, window, cx)
                });
            }),
            close: false,
        });
        cx.notify();
    }

    fn written(
        &mut self,
        revision: u64,
        saved: Saved,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Content::Open(open) = &mut self.content else {
            return;
        };
        let close = open.saving.take().is_some_and(|saving| saving.close);
        match saved {
            Saved::Written(version) => {
                open.version = version;
                open.saved = revision;
                open.outside = None;
                self.notice = None;
                let path = self.path.clone();
                let _ = self
                    .keystone
                    .update(cx, |keystone, cx| keystone.file_saved(&path, cx));
            }
            Saved::Conflict(disk) => open.outside = Outside::from_disk(disk),
            Saved::Failed(why) => self.notice = Some(why),
        }
        // Back was pressed during the save. Edits typed since still need the question
        // Back asks; a conflict or a failed write stays open to show why.
        if close && open.outside.is_none() && self.notice.is_none() {
            if open.edited(cx) {
                self.discard = Some(DialogFocus::new(window, cx));
            } else {
                cx.emit(DismissEvent);
            }
        }
        cx.notify();
    }

    /// Replaces the editor's text with what the file now holds.
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open_mut() else {
            return;
        };
        if let Some(Outside::Changed(Some((text, version)))) = &open.outside {
            let (text, version) = (text.clone(), version.clone());
            open.take(&text, version, cx);
        }
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn focus_editor(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus_handle(cx), cx);
    }

    /// Closes the view, asking first when that would lose edits. During a save it waits
    /// for the save instead, which may still find a conflict the user has to settle.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.open_mut() {
            Some(Open {
                saving: Some(saving),
                ..
            }) => saving.close = true,
            Some(open) if open.edited(cx) => {
                self.discard = Some(DialogFocus::new(window, cx));
                cx.notify();
            }
            _ => cx.emit(DismissEvent),
        }
    }

    /// The whole lines the selection covers, with their 1-based numbers, while they are
    /// the lines the file holds on disk.
    pub(crate) fn excerpt(&self, cx: &App) -> Option<Excerpt> {
        let open = self.open()?;
        if open.edited(cx) || open.outside.is_some() {
            return None;
        }
        if file_disk::stamp(&self.path) != open.version.stamp {
            return None;
        }
        let editor = open.editor.read(cx);
        let selection = editor.selection();
        if selection.is_empty() {
            return None;
        }
        let first = selection.start.line;
        let last = if selection.end.column == 0 && selection.end.line > first {
            selection.end.line - 1
        } else {
            selection.end.line
        };
        let text = editor.text();
        let lines: Vec<&str> = text.lines().skip(first).take(last - first + 1).collect();
        Some(Excerpt::file(
            self.name.clone(),
            first + 1..=last + 1,
            &lines.join("\n"),
        ))
    }

    fn open_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_editor(window, cx);
        let excerpt = self.excerpt(cx);
        let _ = self.keystone.update(cx, |keystone, cx| {
            let taken =
                excerpt.and_then(|excerpt| Some((keystone.selected(cx)?, Rc::new(excerpt))));
            keystone.open_selection_menu(taken, position, cx)
        });
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(excerpt) = self.excerpt(cx) else {
            return;
        };
        let _ = self.keystone.update(cx, |keystone, cx| {
            if let Some(session) = keystone.selected(cx) {
                keystone.send_excerpt(&(session, Rc::new(excerpt)), window, cx)
            }
        });
    }

    fn open_in_editor(&mut self, cx: &mut Context<Self>) {
        self.notice = crate::files::open_in_editor(&self.path).err();
        cx.notify();
    }

    fn conflict_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let open = self.open()?;
        let outside = open.outside.as_ref()?;
        let message = match outside {
            Outside::Changed(_) => "This file changed on disk.",
            Outside::Deleted => "This file was deleted on disk.",
        };
        let saving = open.saving.is_some();
        Some(
            h_flex()
                .flex_none()
                .gap_1()
                .px(px(14.))
                .py(px(4.))
                .border_b_1()
                .border_color(cx.theme().colors.border)
                .automation_id("file-editor-conflict")
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Label::new(message).size(TextSize::Ui).color(Color::Warning)),
                )
                .when(matches!(outside, Outside::Changed(Some(_))), |bar| {
                    bar.child(
                        Button::new("file-editor-reload", "Reload")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .disabled(saving)
                            .debug_selector(|| "file-editor-reload".into())
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    )
                })
                .child(
                    Button::new("file-editor-overwrite", "Overwrite")
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .disabled(saving)
                        .debug_selector(|| "file-editor-overwrite".into())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.write(true, window, cx);
                            this.focus_editor(window, cx);
                        })),
                ),
        )
    }

    fn discard_dialog(&self, cx: &mut Context<Self>) -> Option<ConfirmDialog> {
        let focus = self.discard.as_ref()?;
        let view = cx.weak_entity();
        let cancel = view.clone();
        Some(
            ConfirmDialog::new(
                "file-editor-discard",
                focus,
                "Discard changes?",
                format!("Your edits to {} aren't saved.", self.name),
            )
            .confirm_label("Discard")
            .on_cancel(move |window, cx| {
                let _ = cancel.update(cx, |this, cx| {
                    this.discard = None;
                    this.focus_editor(window, cx);
                    cx.notify();
                });
            })
            .on_confirm(move |_, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.discard = None;
                    cx.emit(DismissEvent);
                });
            }),
        )
    }
}

/// Save's tooltip, with the key the user bound it to.
fn save_tooltip(cx: &App) -> Tooltip {
    let keys = crate::commands::hint(SAVE_FILE, cx);
    if keys.is_empty() {
        Tooltip::text("Save")
    } else {
        Tooltip::with_key("Save", keys)
    }
}

impl Render for FileView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let label = self
            .keystone
            .upgrade()
            .and_then(|keystone| keystone.read(cx).send_label(cx));
        let edited = self.open().is_some_and(|open| open.edited(cx));
        let can_send = self.open().is_some_and(|open| {
            !edited && open.outside.is_none() && !open.editor.read(cx).selection().is_empty()
        });
        let body = match &self.content {
            Content::Loading { .. } => div().flex_1().into_any_element(),
            Content::Unreadable(why) => div()
                .flex_1()
                .px(px(14.))
                .py_2()
                .automation_id("file-view-unreadable")
                .child(Label::new(*why).size(TextSize::Ui).color(Color::Muted))
                .into_any_element(),
            Content::Open(open) => div()
                .flex_1()
                .min_h_0()
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                        this.open_menu(event.position, window, cx)
                    }),
                )
                .child(open.editor.clone())
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            // Bubbling after the window's capture, so Escape closes an open menu first.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.back(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(window, cx)))
            .on_action(
                cx.listener(|this, _: &crate::excerpt::SendSelection, window, cx| {
                    this.send(window, cx)
                }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .px(px(8.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        IconButton::new("file-view-back", IconName::Left)
                            .size(ButtonSize::Compact)
                            .debug_selector(|| "file-view-back".into())
                            .aria_label("Back")
                            .tooltip(Tooltip::with_key("Back", "Esc"))
                            .on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .automation_id("file-view-name")
                            .child(Label::new(self.name.clone()).size(TextSize::Ui).truncate()),
                    )
                    .when(edited, |header| {
                        header.child(
                            div()
                                .flex_none()
                                .px_1()
                                .automation_id("file-editor-edited")
                                .child(Label::new("Edited").size(TextSize::Ui).color(Color::Muted)),
                        )
                    })
                    .when(self.open().is_some(), |header| {
                        header.child(
                            Button::new("file-editor-save", "Save")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .disabled(!self.can_save(cx))
                                .debug_selector(|| "file-editor-save".into())
                                .tooltip(save_tooltip(cx))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save(window, cx);
                                    this.focus_editor(window, cx);
                                })),
                        )
                    }),
            )
            .children(self.conflict_bar(cx))
            .children(self.notice.as_ref().map(|notice| {
                div()
                    .flex_none()
                    .px(px(14.))
                    .py_1()
                    .automation_id("file-view-notice")
                    .child(
                        Label::new(notice.clone())
                            .size(TextSize::Ui)
                            .color(Color::Danger),
                    )
            }))
            .child(body)
            .child(
                h_flex()
                    .flex_none()
                    .justify_end()
                    .gap_1()
                    .px(px(10.))
                    .py(px(6.))
                    .children(label.map(|label| {
                        Button::new("file-view-send", label)
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .disabled(!can_send)
                            .debug_selector(|| "file-view-send".into())
                            .tooltip(Tooltip::text(crate::commands::hint(
                                crate::excerpt::SEND_SELECTION,
                                cx,
                            )))
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                    }))
                    .child(
                        Button::new("file-view-open", "Open in editor")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .debug_selector(|| "file-view-open".into())
                            .on_click(cx.listener(|this, _, _, cx| this.open_in_editor(cx))),
                    ),
            )
            .children(self.discard_dialog(cx))
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "file_view_tests.rs"]
mod ui_tests;
