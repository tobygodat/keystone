//! A read-only view of one file in Files. Lines are selected whole and sent to the
//! agent with their path and numbers. It never edits or saves the file.

use crate::Keystone;
use crate::excerpt::Excerpt;
use gpui::{
    App, AppContext, ClipboardItem, Context, DismissEvent, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, KeyDownEvent, ListAlignment, ListState, MouseButton, Pixels, Render,
    WeakEntity, Window, div, list, prelude::*, px,
};
use keystone_ui::prelude::*;
use std::io::Read;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

gpui::actions!(file_view, [Up, Down, SelectUp, SelectDown, SelectAll, Copy]);

const CONTEXT: &str = "FileView";

/// Bigger files open only in the editor.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Characters of a line drawn; the rest of a longer line is still sent.
const MAX_SHOWN: usize = 2000;

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("shift-up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("shift-down", SelectDown, Some(CONTEXT)),
        KeyBinding::new("secondary-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("secondary-c", Copy, Some(CONTEXT)),
    ]);
}

#[derive(Debug, PartialEq)]
enum Content {
    Loading,
    Lines(Vec<String>),
    Unreadable(&'static str),
}

/// The file's size and modification time when it was read.
type Stamp = Option<(u64, SystemTime)>;

/// Selected lines, 0-based: where the selection started and where it ends now.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Selected {
    anchor: usize,
    head: usize,
}

impl Selected {
    fn range(self) -> RangeInclusive<usize> {
        self.anchor.min(self.head)..=self.anchor.max(self.head)
    }
}

pub(crate) struct FileView {
    path: PathBuf,
    /// The path an excerpt names: relative to the Files root.
    name: String,
    content: Content,
    stamp: Stamp,
    selection: Option<Selected>,
    /// Whether a press in this view started the drag the pointer is making.
    dragging: bool,
    /// The read in flight; replacing it drops an older one.
    loading: Option<gpui::Task<()>>,
    list: ListState,
    focus: FocusHandle,
    gutter: Pixels,
    notice: Option<String>,
    keystone: WeakEntity<Keystone>,
}

impl EventEmitter<DismissEvent> for FileView {}

impl Focusable for FileView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FileView {
    pub(crate) fn new(
        path: PathBuf,
        name: String,
        keystone: WeakEntity<Keystone>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            path,
            name,
            content: Content::Loading,
            stamp: None,
            selection: None,
            dragging: false,
            loading: None,
            list: ListState::new(0, ListAlignment::Top, px(200.)),
            focus: cx.focus_handle(),
            gutter: px(0.),
            notice: None,
            keystone,
        };
        view.load(cx);
        view
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let read = cx.background_spawn(async move {
            let stamp = stamp(&path);
            (read_lines(&path), stamp)
        });
        self.loading = Some(cx.spawn(async move |this, cx| {
            let (content, stamp) = read.await;
            let _ = this.update(cx, |this, cx| {
                if let Content::Lines(lines) = &content {
                    this.list.reset(lines.len());
                }
                (this.content, this.stamp, this.selection) = (content, stamp, None);
                this.loading = None;
                cx.notify();
            });
        }));
    }

    /// Reads the file again when it changed since it was shown, so a selection never
    /// names lines that are no longer there. Says whether it did.
    fn reload_if_changed(&mut self, cx: &mut Context<Self>) -> bool {
        let changed = !self.fresh();
        if changed && self.loading.is_none() {
            self.selection = None;
            self.load(cx);
        }
        changed
    }

    /// Whether what is shown is what the file holds now, as far as its size and time tell.
    fn fresh(&self) -> bool {
        self.loading.is_none() && stamp(&self.path) == self.stamp
    }

    fn selected_text(&self) -> Option<(RangeInclusive<usize>, String)> {
        let (Content::Lines(lines), Some(selection)) = (&self.content, self.selection) else {
            return None;
        };
        let range = selection.range();
        Some((range.clone(), lines[range].join("\n")))
    }

    /// The selected lines with this file's path and their 1-based numbers, unless the
    /// file changed since it was shown.
    pub(crate) fn excerpt(&self) -> Option<Excerpt> {
        if !self.fresh() {
            return None;
        }
        let (range, text) = self.selected_text()?;
        Some(Excerpt::file(
            self.name.clone(),
            range.start() + 1..=range.end() + 1,
            &text,
        ))
    }

    fn line_count(&self) -> usize {
        match &self.content {
            Content::Lines(lines) => lines.len(),
            _ => 0,
        }
    }

    fn select(&mut self, line: usize, extend: bool, cx: &mut Context<Self>) {
        let line = line.min(self.line_count().saturating_sub(1));
        self.selection = Some(match self.selection {
            Some(selection) if extend => Selected {
                head: line,
                ..selection
            },
            _ => Selected {
                anchor: line,
                head: line,
            },
        });
        cx.notify();
    }

    fn step(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        if self.line_count() == 0 || self.reload_if_changed(cx) {
            return;
        }
        let line = match self.selection {
            None => self.list.logical_scroll_top().item_ix,
            Some(Selected { head, .. }) if down => head + 1,
            Some(Selected { head, .. }) => head.saturating_sub(1),
        };
        self.select(line, extend, cx);
        self.list
            .scroll_to_reveal_item(line.min(self.line_count() - 1));
    }

    fn press(&mut self, line: usize, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.dragging = !self.reload_if_changed(cx);
        if self.dragging {
            self.select(line, extend, cx);
        }
    }

    /// A right click keeps a selection it lands in and otherwise selects its line.
    fn open_menu(
        &mut self,
        line: usize,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        if self.reload_if_changed(cx) {
            return;
        }
        if !self.selection.is_some_and(|s| s.range().contains(&line)) {
            self.select(line, false, cx);
        }
        let excerpt = self.excerpt();
        let _ = self.keystone.update(cx, |keystone, cx| {
            let taken =
                excerpt.and_then(|excerpt| Some((keystone.selected(cx)?, Rc::new(excerpt))));
            keystone.open_selection_menu(taken, position, cx)
        });
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.reload_if_changed(cx) {
            return;
        }
        let Some(excerpt) = self.excerpt() else {
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

    fn row(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Content::Lines(lines) = &self.content else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors;
        let selected = self.selection.is_some_and(|s| s.range().contains(&index));
        h_flex()
            .id(("file-line", index))
            .automation_id(format!("file-line-{index}"))
            .items_start()
            .w_full()
            .min_h(TextSize::Sm.line_height())
            .px(px(8.))
            .when(selected, |d| d.bg(colors.selection))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.press(index, event.modifiers.shift, window, cx)
                }),
            )
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                    this.dragging &= event.pressed_button == Some(MouseButton::Left);
                    if this.dragging && this.selection.is_some_and(|s| s.head != index) {
                        this.select(index, true, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.open_menu(index, event.position, window, cx)
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(self.gutter)
                    .pr(px(12.))
                    .whitespace_nowrap()
                    .text_right()
                    .text_color(colors.text_faint)
                    .child((index + 1).to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(colors.code_text)
                    .child(shown(&lines[index])),
            )
            .into_any_element()
    }
}

impl Render for FileView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (colors, mono) = (theme.colors, theme.fonts.mono.clone());
        let digits = self.line_count().max(1).to_string().len() as f32;
        let font = window.text_system().resolve_font(&gpui::font(mono.clone()));
        let digit = window
            .text_system()
            .advance(font, TextSize::Sm.px(), '0')
            .map_or(TextSize::Sm.px(), |size| size.width);
        self.gutter = (digit * digits).ceil() + px(13.);
        let label = self
            .keystone
            .upgrade()
            .and_then(|keystone| keystone.read(cx).send_label(cx));
        let body = match &self.content {
            Content::Loading => div().flex_1().into_any_element(),
            Content::Unreadable(why) => div()
                .flex_1()
                .px(px(14.))
                .py_2()
                .automation_id("file-view-unreadable")
                .child(Label::new(*why).size(TextSize::Ui).color(Color::Muted))
                .into_any_element(),
            Content::Lines(_) => {
                let view = cx.weak_entity();
                div()
                    .flex_1()
                    .min_h_0()
                    .py_1()
                    .font_family(mono)
                    .text_size(TextSize::Sm.px())
                    .line_height(TextSize::Sm.line_height())
                    .automation_id("file-view-lines")
                    .child(
                        list(self.list.clone(), move |index, _, cx| {
                            view.update(cx, |view, cx| view.row(index, cx))
                                .unwrap_or_else(|_| div().into_any_element())
                        })
                        .size_full(),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            // Bubbling after the window's capture, so Escape closes an open menu first.
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.emit(DismissEvent);
                    cx.stop_propagation();
                }
            }))
            .on_action(
                cx.listener(|this, _: &crate::excerpt::SendSelection, window, cx| {
                    this.send(window, cx)
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = false),
            )
            .on_action(cx.listener(|this, _: &Up, _, cx| this.step(false, false, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.step(true, false, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step(false, true, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step(true, true, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                if this.reload_if_changed(cx) {
                    return;
                }
                if let Some(last) = this.line_count().checked_sub(1) {
                    this.selection = Some(Selected {
                        anchor: 0,
                        head: last,
                    });
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &Copy, _, cx| {
                if let Some((_, text)) = this.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }))
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
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .automation_id("file-view-name")
                            .child(Label::new(self.name.clone()).size(TextSize::Ui).truncate()),
                    ),
            )
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
                            .disabled(self.selection.is_none())
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
    }
}

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// The file's lines, or why they cannot be shown. Only regular files are read, and no
/// more than [`MAX_BYTES`] of them, so a pipe or a device never blocks or fills memory.
fn read_lines(path: &Path) -> Content {
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => return Content::Unreadable("Not a text file."),
        Ok(_) => {}
        Err(_) => return Content::Unreadable("Couldn't read this file."),
    }
    let Ok(file) = std::fs::File::open(path) else {
        return Content::Unreadable("Couldn't read this file.");
    };
    let mut bytes = Vec::new();
    if file.take(MAX_BYTES + 1).read_to_end(&mut bytes).is_err() {
        return Content::Unreadable("Couldn't read this file.");
    }
    if bytes.len() as u64 > MAX_BYTES {
        return Content::Unreadable("Too large to show.");
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Content::Unreadable("Not a text file.");
    };
    if text.contains('\0') {
        return Content::Unreadable("Not a text file.");
    }
    Content::Lines(text.lines().map(str::to_owned).collect())
}

/// A line as drawn: tabs as four spaces, and at most [`MAX_SHOWN`] characters.
fn shown(line: &str) -> String {
    let mut shown: String = line.chars().take(MAX_SHOWN).collect();
    if shown.len() < line.len() {
        shown.push('…');
    }
    shown.replace('\t', "    ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(content: Content) -> Vec<String> {
        match content {
            Content::Lines(lines) => lines,
            other => panic!("not lines: {other:?}"),
        }
    }

    #[test]
    fn text_files_read_as_lines_without_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\r\n\ttwo\r\n\nfour").unwrap();
        assert_eq!(lines(read_lines(&file)), ["one", "\ttwo", "", "four"]);
        std::fs::write(&file, "").unwrap();
        assert_eq!(lines(read_lines(&file)), Vec::<String>::new());
    }

    #[test]
    fn binary_large_missing_and_special_files_are_not_shown() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"a\0b").unwrap();
        assert_eq!(read_lines(&file), Content::Unreadable("Not a text file."));
        std::fs::write(&file, [0xff, 0xfe, b'a']).unwrap();
        assert_eq!(read_lines(&file), Content::Unreadable("Not a text file."));
        std::fs::write(&file, vec![b'a'; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(read_lines(&file), Content::Unreadable("Too large to show."));
        assert_eq!(
            read_lines(&dir.path().join("missing")),
            Content::Unreadable("Couldn't read this file.")
        );
        assert_eq!(
            read_lines(dir.path()),
            Content::Unreadable("Not a text file.")
        );
        let fifo = dir.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || done.send(read_lines(&fifo)).unwrap());
        assert_eq!(
            wait.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(Content::Unreadable("Not a text file.")),
            "a pipe is refused without waiting for a writer"
        );
    }

    #[test]
    fn long_lines_are_drawn_short_and_tabs_as_spaces() {
        assert_eq!(shown("\tx"), "    x");
        let long = "é".repeat(MAX_SHOWN + 5);
        let drawn = shown(&long);
        assert_eq!(drawn.chars().count(), MAX_SHOWN + 1);
        assert!(drawn.ends_with('…'));
    }
}
