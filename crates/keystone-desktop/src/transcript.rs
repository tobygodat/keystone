//! How one transcript row looks. The transcript list in `main.rs` lays rows out and
//! virtualizes them; this module only decides a row's content.

use crate::conversation::Row;
use crate::view_state::CopyTarget;
use gpui::{
    AnyElement, App, ClipboardItem, ElementId, FocusHandle, SharedString, Window, div, prelude::*,
    px,
};
use keystone_agent::diff::{Change, FileDiff, Hunk, Line, Patch};
use keystone_agent::recovery::{Limit, Outcome, RecoveryReport, TurnEvidence};
use keystone_agent::{Item, ItemKind, ItemStatus, TurnStatus};
use keystone_ui::markdown::Document;
use keystone_ui::prelude::*;
use keystone_ui::{Mark, MarkSlot, marked_text};
use std::sync::Arc;
use std::time::Duration;

/// What a click on a row's control does.
pub type ClickHandler = Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>;

/// Each open edit row's cached preview, by session and row, with its item's ID.
pub type EditPreviews =
    std::collections::HashMap<(u64, usize), (String, gpui::Entity<EditPreview>)>;

pub struct ActivityControl {
    pub expanded: bool,
    pub selector: String,
    pub body_selector: String,
    pub on_toggle: Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>,
    /// An edit row's Open full edit: the whole edit in the diff view.
    pub on_open_edit: ClickHandler,
    /// An open edit row's preview.
    pub preview: Option<gpui::Entity<EditPreview>>,
}

#[derive(Default)]
pub struct RowControls {
    pub action: Option<(CopyTarget, FocusHandle)>,
    pub selected: bool,
    pub activity: Option<ActivityControl>,
    pub selection_start: Option<Box<dyn Fn(&mut Window, &mut App) -> Option<FocusHandle>>>,
    /// What a right click on a reply does: open the menu for its selection.
    pub selection_menu: Option<Box<dyn Fn(&gpui::Point<gpui::Pixels>, &mut Window, &mut App)>>,
    /// What a widget row's inputs and Buttons do.
    pub widget: Option<crate::widget::WidgetControl>,
    /// For a launched session's delivered result, that session's title.
    pub result: Option<SharedString>,
    /// A message row's Fork from here control.
    pub fork: Option<ForkControl>,
    /// The Restore to here control on the last reply of a turn with a checkpoint.
    pub restore: Option<RestoreControl>,
    /// A finished reply's Second opinion control: its selector and what it does.
    pub second_opinion: Option<(
        String,
        Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>,
    )>,
    /// How the row's images and videos show and open.
    pub previews: Option<crate::preview::Previews>,
    /// For an edit's row, the folder its paths are shown from.
    pub edit_folder: Option<EditFolder>,
    /// Find's matches in the row, by the row's texts as [`crate::find::row_texts`]
    /// lists them, and where to record the current one as it is drawn.
    pub marks: Vec<Mark>,
    pub mark_slot: Option<MarkSlot>,
}

pub enum RestoreControl {
    /// Restore to here, with how many files the turn changed, when known, and what
    /// showing those changes does.
    Offered {
        files: Option<usize>,
        on_click: Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>,
        view_changes: Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>,
    },
    /// The session has no checkpoints; the tooltip says why, and a click does nothing.
    Unavailable(&'static str),
}

/// A turn's checkpoint: Restore to here, and how many files the turn changed.
fn restore_button(index: usize, control: RestoreControl, quiet: bool, cx: &App) -> AnyElement {
    let (label, files, on_click, view_changes) = match control {
        RestoreControl::Offered {
            files,
            on_click,
            view_changes,
        } => ("Restore to here", files, Some(on_click), Some(view_changes)),
        RestoreControl::Unavailable(why) => (why, None, None, None),
    };
    let files = files.filter(|files| *files > 0).map(|files| match files {
        1 => "1 file".to_owned(),
        files => format!("{files} files"),
    });
    let unavailable = on_click.is_none();
    h_flex()
        .gap_1()
        .child(
            div()
                .when(quiet && files.is_none() && !unavailable, |d| {
                    d.opacity(0.).group_hover("agent-reply", |s| s.opacity(1.))
                })
                .child(
                    IconButton::new(("restore", index), IconName::Restore)
                        .size(ButtonSize::Compact)
                        .aria_label(label)
                        .tooltip(Tooltip::text(label))
                        .debug_selector(move || format!("restore-{index}"))
                        .when_some(on_click, |button, on_click| {
                            button.on_click(move |event, window, cx| on_click(event, window, cx))
                        }),
                ),
        )
        .when_some(files.zip(view_changes), |d, (files, view_changes)| {
            d.child(
                div()
                    .automation_id(format!("checkpoint-files-{index}"))
                    .child(
                        Button::new(("checkpoint-files", index), files)
                            .style(ButtonStyle::Ghost)
                            .size(ButtonSize::Compact)
                            .icon(IconName::Diff)
                            .aria_label("View this turn's changes")
                            .tooltip(Tooltip::with_key(
                                "View this turn's changes",
                                crate::commands::hint("view-changes", cx),
                            ))
                            .debug_selector(move || format!("view-changes-{index}"))
                            .on_click(move |event, window, cx| view_changes(event, window, cx)),
                    ),
            )
        })
        .into_any_element()
}

pub struct ForkControl {
    pub label: &'static str,
    pub on_click: Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>,
}

fn fork_button(
    index: usize,
    control: ForkControl,
    action: &Option<(CopyTarget, FocusHandle)>,
) -> AnyElement {
    IconButton::new(("fork", index), IconName::Branch)
        .size(ButtonSize::Compact)
        .aria_label(control.label)
        .tooltip(Tooltip::text(control.label))
        .debug_selector(move || format!("fork-{index}"))
        .when_some(
            action
                .as_ref()
                .filter(|(target, _)| *target == CopyTarget::Fork),
            |button, (_, focus)| button.track_focus(focus).reveal_on_focus(),
        )
        .on_click(move |event, window, cx| (control.on_click)(event, window, cx))
        .into_any_element()
}

pub fn activity_name(item: &Item) -> Option<String> {
    match &item.kind {
        ItemKind::Reasoning => Some("reasoning".into()),
        ItemKind::Command { .. } => Some("command output".into()),
        ItemKind::FileChange { patch: Some(_), .. } => Some("edit".into()),
        ItemKind::Tool { name } => Some(format!("tool {name}")),
        ItemKind::Other { name }
            if !item.id.starts_with(keystone_agent::log::LOSS_MARKER_PREFIX) =>
        {
            Some(format!("activity {name}"))
        }
        _ => None,
    }
}

/// Whether a row's disclosure starts open. An edit's diff starts closed, so a turn of
/// edits reads as one line each.
pub fn opens_expanded(item: &Item) -> bool {
    !matches!(item.kind, ItemKind::FileChange { .. })
}

/// Diff lines an open edit row draws. The rest of a longer edit opens in the diff view,
/// which lays out only the lines in view, so a row costs the same whatever the edit's size.
pub const PREVIEW_LINES: usize = 40;

/// One row of an edit's preview.
enum PreviewRow<'a> {
    Name(&'a FileDiff),
    Hunk(&'a Hunk),
    Line(&'a Line),
}

/// The rows an edit's preview draws: each file's name when the edit changed more than
/// one, then its hunks, up to [`PREVIEW_LINES`] lines.
fn preview_rows(patch: &Patch) -> Vec<PreviewRow<'_>> {
    let named = patch.files.len() > 1;
    let mut rows = Vec::new();
    let mut room = PREVIEW_LINES;
    for file in &patch.files {
        if room == 0 {
            break;
        }
        if named {
            rows.push(PreviewRow::Name(file));
        }
        for hunk in &file.hunks {
            if room == 0 {
                break;
            }
            rows.push(PreviewRow::Hunk(hunk));
            let lines = &hunk.lines[..hunk.lines.len().min(room)];
            room -= lines.len();
            rows.extend(lines.iter().map(PreviewRow::Line));
        }
    }
    rows
}

/// The texts an edit's preview draws that find searches, in drawing order: each file's
/// name when it names them, and each line. Hunk headers are not searched.
pub fn preview_texts(patch: &Patch, folder: &EditFolder) -> Vec<String> {
    preview_rows(patch)
        .iter()
        .filter_map(|row| match row {
            PreviewRow::Name(file) => Some(file_name(file, folder)),
            PreviewRow::Hunk(_) => None,
            PreviewRow::Line(line) => Some(crate::diff_view::shown(&line.text)),
        })
        .collect()
}

/// A file's name over its part of an edit's preview.
fn file_name(file: &FileDiff, folder: &EditFolder) -> String {
    let change = match file.change {
        Change::Added => " · added",
        Change::Deleted => " · deleted",
        Change::Renamed | Change::Modified => "",
    };
    format!("{}{change}", folder.shown(&file.path))
}

/// An edit row's label: the paths it changed, and how it ended when it did not apply.
pub fn edit_label(folder: &EditFolder, paths: &[String], status: ItemStatus) -> String {
    let shown: Vec<_> = paths.iter().map(|path| folder.shown(path)).collect();
    format!("Changed {}{}", shown.join(", "), outcome_note(status))
}

/// A preview row's height. Rows never wrap, so the preview's height is known before it
/// is drawn, which a cached view needs.
fn preview_row_height(row: &PreviewRow) -> gpui::Pixels {
    let line = TextSize::Sm.line_height();
    match row {
        PreviewRow::Hunk(_) => line + px(4.),
        PreviewRow::Line(drawn) if drawn.no_newline => line * 2.,
        PreviewRow::Name(_) | PreviewRow::Line(_) => line,
    }
}

/// The start of one edit's diff, drawn with the diff view's own rows. It is its own
/// view, so outside verification it is cached: a keystroke elsewhere, which redraws the
/// transcript, does not lay it out again. While find marks it, the row draws it itself.
pub struct EditPreview {
    index: usize,
    patch: Patch,
    folder: EditFolder,
}

impl EditPreview {
    pub fn new(index: usize, patch: Patch, folder: EditFolder) -> Self {
        Self {
            index,
            patch,
            folder,
        }
    }

    pub fn height(patch: &Patch) -> gpui::Pixels {
        preview_rows(patch).iter().map(preview_row_height).sum()
    }
}

impl Render for EditPreview {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        preview_element(self.index, &self.patch, &self.folder, &[], None, cx)
    }
}

/// The preview's rows as one element of [`EditPreview::height`], with find's `marks`,
/// whose texts count from 1 as [`preview_texts`] lists them after the row's label.
fn preview_element(
    index: usize,
    patch: &Patch,
    folder: &EditFolder,
    marks: &[Mark],
    slot: Option<&MarkSlot>,
    cx: &App,
) -> gpui::Div {
    {
        let colors = cx.theme().colors;
        let rows = preview_rows(patch);
        let biggest = rows
            .iter()
            .filter_map(|row| match row {
                PreviewRow::Line(line) => line.old.max(line.new),
                _ => None,
            })
            .max();
        let gutter = crate::diff_view::gutter(biggest.unwrap_or(1), cx);
        let mut drawn = 0;
        let mut text = 0;
        let mut marked = |shown: String| {
            text += 1;
            marked_text(shown, text, marks, slot, cx)
        };
        let mut children = Vec::with_capacity(rows.len());
        for row in &rows {
            let content = match row {
                PreviewRow::Name(file) => div()
                    .px(px(8.))
                    .text_color(colors.text_muted)
                    .child(marked(file_name(file, folder))),
                PreviewRow::Hunk(hunk) => crate::diff_view::hunk_row(&hunk.header, cx),
                PreviewRow::Line(line) => {
                    drawn += 1;
                    let shown = marked(crate::diff_view::shown(&line.text));
                    crate::diff_view::line_row_with(line, shown, gutter, false, cx)
                        .automation_id(format!("edit-{index}-line-{}", drawn - 1))
                }
            };
            children.push(
                div()
                    .h(preview_row_height(row))
                    .overflow_hidden()
                    .child(content),
            );
        }
        v_flex()
            .w_full()
            .font_family(cx.theme().fonts.mono.clone())
            .text_size(TextSize::Sm.px())
            .line_height(TextSize::Sm.line_height())
            .whitespace_nowrap()
            .children(children)
    }
}

/// An open edit: its preview, then how many lines are left and a way to open all of it,
/// or where a cut edit's kept lines end, or that it has no lines.
struct EditBody<'a> {
    index: usize,
    patch: &'a Patch,
    preview: Option<gpui::Entity<EditPreview>>,
    /// Find's marks on the row, and where to record the current one.
    marks: (&'a [Mark], Option<&'a MarkSlot>),
    folder: &'a EditFolder,
    selector: String,
    open_edit: ClickHandler,
    action: &'a Option<(CopyTarget, FocusHandle)>,
}

fn edit_body(body: EditBody, cx: &App) -> AnyElement {
    let EditBody {
        index,
        patch,
        preview,
        marks: (marks, slot),
        folder,
        selector,
        open_edit,
        action,
    } = body;
    let colors = cx.theme().colors;
    let total: usize = patch
        .files
        .iter()
        .flat_map(|file| &file.hunks)
        .map(|hunk| hunk.lines.len())
        .sum();
    let more = total.saturating_sub(PREVIEW_LINES);
    let height = EditPreview::height(patch);
    // A reused cached view reports no element geometry, so verification draws it
    // uncached, as the sidebar and the terminal do.
    let marked = marks.iter().any(|mark| mark.text > 0);
    let preview = match preview {
        Some(preview) if !marked && !keystone_ui::automation::enabled() => {
            gpui::AnyView::from(preview)
                .cached(gpui::StyleRefinement::default().w_full().h(height))
                .into_any_element()
        }
        Some(preview) if !marked => div().w_full().h(height).child(preview).into_any_element(),
        _ => preview_element(index, patch, folder, marks, slot, cx).into_any_element(),
    };
    let note = |id: String, text: String| {
        div()
            .automation_id(id)
            .px(px(8.))
            .child(Label::new(text).size(TextSize::Sm).color(Color::Muted))
    };
    let empty = match patch.files.iter().all(|file| file.change == Change::Added) {
        true => "The file is empty.",
        false => "No lines changed.",
    };
    v_flex()
        .automation_id(selector)
        .py_1()
        .rounded(radius::PANEL)
        .overflow_hidden()
        .bg(colors.surface_raised)
        .child(preview)
        .when(total == 0, |d| {
            d.child(note(format!("edit-empty-{index}"), empty.into()))
        })
        .when(patch.truncated && more == 0, |d| {
            d.child(
                note(
                    format!("edit-cut-{index}"),
                    "This edit is too long to keep whole. The rest isn't shown.".into(),
                )
                .pt_1(),
            )
        })
        .when(more > 0, |d| {
            let label = match more {
                1 => "1 more line".to_owned(),
                more => format!("{more} more lines"),
            };
            d.child(
                h_flex()
                    .pt_1()
                    .pr_1()
                    .gap_2()
                    .child(note(format!("edit-more-{index}"), label))
                    .child(
                        Button::new(("edit-open", index), "Open full edit")
                            .style(ButtonStyle::Ghost)
                            .size(ButtonSize::Compact)
                            .icon(IconName::Diff)
                            .tooltip(Tooltip::with_key(
                                "Open the whole edit",
                                crate::commands::hint("view-changes", cx),
                            ))
                            .debug_selector(move || format!("edit-open-{index}"))
                            .when_some(
                                action
                                    .as_ref()
                                    .filter(|(target, _)| *target == CopyTarget::OpenEdit),
                                |button, (_, focus)| button.track_focus(focus).reveal_on_focus(),
                            )
                            .on_click(move |event, window, cx| open_edit(event, window, cx)),
                    ),
            )
        })
        .into_any_element()
}

impl crate::Keystone {
    /// The preview of the edit at row `index` of `session`, when it was made for this
    /// item as the row opened.
    pub(crate) fn edit_preview(
        &self,
        session: u64,
        index: usize,
        item: &Item,
    ) -> Option<gpui::Entity<EditPreview>> {
        let previews = self.edit_previews.borrow();
        let (id, preview) = previews.get(&(session, index))?;
        (*id == item.id).then(|| preview.clone())
    }

    /// Makes the preview of the edit at row `index` as it opens, or forgets it as it
    /// closes.
    pub(crate) fn edit_row_toggled(
        &mut self,
        session: u64,
        index: usize,
        open: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        if !open {
            self.edit_previews.borrow_mut().remove(&(session, index));
            return;
        }
        let edit = match self.state.read(cx).conversations().get(&session) {
            Some(conversation) => match conversation.rows.get(index) {
                Some(crate::conversation::Row::Item(Item {
                    id,
                    kind:
                        ItemKind::FileChange {
                            patch: Some(patch), ..
                        },
                    ..
                })) => Some((id.clone(), patch.clone())),
                _ => None,
            },
            None => None,
        };
        let Some((id, patch)) = edit else {
            return;
        };
        let folder = self.edit_folder(session, cx);
        let preview = cx.new(|_| EditPreview::new(index, patch, folder));
        self.edit_previews
            .borrow_mut()
            .insert((session, index), (id, preview));
    }
}

/// The folder a session's edits are shown from, here or on another machine, as text.
/// It only shortens paths for display and copying: nothing reads a file through it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditFolder(Option<String>);

impl EditFolder {
    pub fn of(context: keystone_core::SessionContext<'_>) -> Self {
        use keystone_core::SessionContext;
        Self(match context {
            SessionContext::Recorded { directory, .. } => {
                Some(directory.path().to_string_lossy().into_owned())
            }
            SessionContext::Remote { folder, .. } => Some(folder.wire().to_owned()),
            SessionContext::LegacyMissing | SessionContext::Unsupported => None,
        })
    }

    /// An edit's path as the user should see it: relative to the session's folder when
    /// inside it. Every surface that shows or copies an edit's paths goes through here.
    pub fn shown<'a>(&self, path: &'a str) -> std::borrow::Cow<'a, str> {
        match &self.0 {
            Some(folder) => keystone_agent::diff::shown_path(path, folder),
            None => path.into(),
        }
    }

    /// `patch` with each path as [`EditFolder::shown`] gives it.
    pub fn patch(&self, patch: &Patch) -> Patch {
        match &self.0 {
            Some(folder) => patch.shown_from(folder),
            None => patch.clone(),
        }
    }
}

impl crate::Keystone {
    /// The folder `session`'s edits are shown from.
    pub(crate) fn edit_folder(&self, session: u64, cx: &App) -> EditFolder {
        self.state
            .read(cx)
            .session(session)
            .map_or_else(EditFolder::default, |session| {
                EditFolder::of(session.context())
            })
    }
}

fn activity_toggle(
    index: usize,
    name: String,
    action: &Option<(CopyTarget, FocusHandle)>,
    control: ActivityControl,
    compact: bool,
) -> AnyElement {
    let label = format!(
        "{} {name}",
        if control.expanded {
            "Collapse"
        } else {
            "Expand"
        }
    );
    if compact {
        // A new id per state, so a tooltip opened before a click closes with it.
        let id = if control.expanded {
            "activity-toggle-open"
        } else {
            "activity-toggle"
        };
        return IconButton::new(
            (id, index),
            if control.expanded {
                IconName::Chevron
            } else {
                IconName::Right
            },
        )
        .size(ButtonSize::Compact)
        .reserve_focus_ring()
        .aria_label(label.clone())
        .aria_expanded(control.expanded)
        .tooltip(Tooltip::text(label))
        .debug_selector(move || control.selector)
        .when_some(
            action
                .as_ref()
                .filter(|(target, _)| *target == CopyTarget::ActivityToggle),
            |button, (_, focus)| button.track_focus(focus).reveal_on_focus(),
        )
        .on_click(move |event, window, cx| (control.on_toggle)(event, window, cx))
        .into_any_element();
    }
    h_flex()
        .items_start()
        .child(
            Button::new(("activity-toggle", index), label.clone())
                .size(ButtonSize::Compact)
                .reserve_focus_ring()
                .aria_label(label)
                .aria_expanded(control.expanded)
                .debug_selector(move || control.selector)
                .when_some(
                    action
                        .as_ref()
                        .filter(|(target, _)| *target == CopyTarget::ActivityToggle),
                    |button, (_, focus)| button.track_focus(focus).reveal_on_focus(),
                )
                .on_click(move |event, window, cx| (control.on_toggle)(event, window, cx)),
        )
        .into_any_element()
}

/// The content of a row that [`crate::conversation::Conversation::shows`] accepts. `index` is the
/// row's place in the conversation, and `reply` the parsed Markdown of a reply row.
pub fn row(
    row: &Row,
    index: usize,
    reply: Option<Arc<Document>>,
    controls: RowControls,
    cx: &App,
) -> AnyElement {
    let RowControls {
        action,
        selected,
        activity,
        selection_start,
        selection_menu,
        widget,
        result,
        fork,
        restore,
        second_opinion,
        previews,
        edit_folder,
        marks,
        mark_slot,
    } = controls;
    let marked = |text: String, n: usize| marked_text(text, n, &marks, mark_slot.as_ref(), cx);
    let colors = cx.theme().colors;
    let (code_size, code_line) = cx.theme().transcript_code_text();
    match row {
        Row::Item(item) => match &item.kind {
            ItemKind::UserMessage
                if let Some((title, read)) =
                    result.zip(crate::app_state::delivery::parse(&item.text)) =>
            {
                result_row(
                    index,
                    &title,
                    &read,
                    reply,
                    &item.text,
                    &action,
                    (marks.clone(), mark_slot.clone()),
                    cx,
                )
            }
            ItemKind::UserMessage => h_flex()
                .group("user-message")
                .justify_end()
                .items_start()
                .gap_1()
                .child(
                    h_flex()
                        .when(!selected, |d| d.invisible())
                        .group_hover("user-message", |s| s.visible())
                        .child(
                            CopyButton::new(
                                ("copy-message", index),
                                "Copy message",
                                item.text.clone(),
                            )
                            .keyboard(
                                action
                                    .as_ref()
                                    .filter(|(n, _)| *n == CopyTarget::Row)
                                    .map(|(_, f)| f),
                            ),
                        )
                        .when_some(fork, |d, fork| d.child(fork_button(index, fork, &action))),
                )
                .child(
                    div()
                        .max_w(px(620.))
                        .px_3()
                        .py_2()
                        .rounded(radius::ROW)
                        .bg(colors.element_filled)
                        .text_color(colors.text_strong)
                        .child(marked(item.text.clone(), 0)),
                )
                .into_any_element(),
            ItemKind::AgentMessage => {
                let document = reply.unwrap_or_else(|| Arc::new(Document::parse(&item.text)));
                let row_copy = item.status != ItemStatus::InProgress;
                let markdown =
                    Markdown::new(SharedString::from(format!("reply-{index}")), document)
                        .managed_focus()
                        .marks(marks.clone(), mark_slot.clone())
                        .when_some(selection_start, |markdown, start| {
                            markdown.on_selection_start(start)
                        })
                        .when_some(previews.as_ref(), |markdown, previews| {
                            markdown.images(previews.markdown(index, row_copy))
                        });
                let markdown = match &action {
                    Some((CopyTarget::Code(n), focus)) => markdown.copy_focus(*n, focus),
                    Some((CopyTarget::DiagramToggle(n), focus)) => {
                        markdown.diagram_toggle_focus(*n, focus)
                    }
                    Some((CopyTarget::TextSelection, focus)) => markdown.selection_focus(focus),
                    _ => markdown,
                };
                v_flex()
                    .group("agent-reply")
                    .gap_1()
                    .when_some(selection_menu, |d, open| {
                        d.on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
                            open(&event.position, window, cx)
                        })
                    })
                    .child(markdown)
                    .when(item.status == ItemStatus::Interrupted, |d| {
                        d.child(
                            h_flex().child(
                                badge("Interrupted", Color::Muted, cx)
                                    .automation_id(format!("interrupted-{index}")),
                            ),
                        )
                    })
                    .child(
                        h_flex()
                            .when(item.status == ItemStatus::InProgress, |d| d.invisible())
                            .when_some(restore.filter(|_| row_copy), |d, restore| {
                                d.child(restore_button(index, restore, !selected, cx))
                            })
                            .child(
                                h_flex()
                                    .when(!selected, |d| {
                                        d.opacity(0.).group_hover("agent-reply", |s| s.opacity(1.))
                                    })
                                    .child(
                                        CopyButton::new(
                                            ("copy-reply", index),
                                            "Copy reply",
                                            item.text.clone(),
                                        )
                                        .keyboard(
                                            action
                                                .as_ref()
                                                .filter(|(n, _)| *n == CopyTarget::Row && row_copy)
                                                .map(|(_, f)| f),
                                        ),
                                    )
                                    .when_some(fork.filter(|_| row_copy), |d, fork| {
                                        d.child(fork_button(index, fork, &action))
                                    })
                                    .when_some(
                                        second_opinion.filter(|_| row_copy),
                                        |d, (selector, ask)| {
                                            d.child(
                                                IconButton::new(
                                                    ("second-opinion", index),
                                                    IconName::Chat,
                                                )
                                                .size(ButtonSize::Compact)
                                                .aria_label(
                                                    "Ask for a second opinion on this reply",
                                                )
                                                .tooltip(Tooltip::with_key(
                                                    "Second opinion…",
                                                    crate::commands::hint("second-opinion", cx),
                                                ))
                                                .debug_selector(move || selector)
                                                .on_click(move |event, window, cx| {
                                                    ask(event, window, cx)
                                                }),
                                            )
                                        },
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            ItemKind::Reasoning => {
                let control = activity.expect("reasoning disclosure");
                let expanded = control.expanded;
                let body_selector = control.body_selector.clone();
                v_flex()
                    .gap_1()
                    .child(activity_toggle(
                        index,
                        "reasoning".into(),
                        &action,
                        control,
                        false,
                    ))
                    .when(expanded, |d| {
                        d.child(
                            div()
                                .automation_id(body_selector)
                                .text_size(TextSize::Ui.px())
                                .text_color(colors.text_muted)
                                .child(marked(item.text.clone(), 0)),
                        )
                    })
                    .into_any_element()
            }
            ItemKind::Command { command, exit_code } => {
                let (status, color) = match (item.status, exit_code) {
                    (ItemStatus::InProgress, _) => ("Running".to_owned(), Color::Muted),
                    (ItemStatus::Declined, _) => ("Not run".to_owned(), Color::Muted),
                    (ItemStatus::Interrupted, _) => ("Interrupted".to_owned(), Color::Muted),
                    (ItemStatus::Failed, None) => ("Stopped".to_owned(), Color::Muted),
                    (_, Some(0) | None) => (String::new(), Color::Muted),
                    (_, Some(code)) => (format!("Exit {code}"), Color::Danger),
                };
                let output = item.text.trim_end();
                let control = activity.expect("command disclosure");
                let expanded = control.expanded;
                let body_selector = control.body_selector.clone();
                let copy = if output.is_empty() {
                    CopyButton::new(("copy-command", index), "Copy command", command.clone())
                } else {
                    CopyButton::new(("copy-command", index), "Copy output", output.to_owned())
                };
                v_flex()
                    .bg(colors.surface_raised)
                    .rounded(radius::PANEL)
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .min_h(px(36.))
                            .pl_3()
                            .pr_1()
                            .py_1()
                            .gap_2()
                            .font_mono(cx)
                            .text_size(code_size)
                            .line_height(code_line)
                            .text_color(colors.text_muted)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(marked(format!("$ {command}"), 0)),
                            )
                            .child(Label::new(status).size(TextSize::Sm).color(color))
                            .child(
                                copy.keyboard(
                                    action
                                        .as_ref()
                                        .filter(|(n, _)| *n == CopyTarget::Row)
                                        .map(|(_, f)| f),
                                ),
                            )
                            .child(activity_toggle(
                                index,
                                "command output".into(),
                                &action,
                                control,
                                true,
                            )),
                    )
                    .when(expanded && !output.is_empty(), |d| {
                        d.child(
                            div()
                                .automation_id(body_selector)
                                .px_3()
                                .pb_2p5()
                                .font_mono(cx)
                                .text_size(code_size)
                                .line_height(code_line)
                                .text_color(colors.text)
                                .child(marked(output.to_owned(), 1)),
                        )
                    })
                    .into_any_element()
            }
            ItemKind::FileChange { paths, patch } => {
                let folder = edit_folder.unwrap_or_default();
                let label = marked(edit_label(&folder, paths, item.status), 0);
                let previews = previews
                    .as_ref()
                    .and_then(|previews| previews.changed(index, item.status, paths));
                let Some((patch, control)) = patch.as_ref().zip(activity) else {
                    return v_flex()
                        .gap_2()
                        .text_size(TextSize::Ui.px())
                        .text_color(colors.text_muted)
                        .child(label)
                        .children(previews)
                        .into_any_element();
                };
                let (added, removed) = patch
                    .files
                    .iter()
                    .map(FileDiff::counts)
                    .fold((0, 0), |(a, r), (added, removed)| (a + added, r + removed));
                let expanded = control.expanded;
                let body_selector = control.body_selector.clone();
                let mut control = control;
                let open_edit =
                    std::mem::replace(&mut control.on_open_edit, Box::new(|_, _, _| {}));
                let preview = control.preview.take();
                v_flex()
                    .gap_2()
                    .text_size(TextSize::Ui.px())
                    .text_color(colors.text_muted)
                    .child(
                        h_flex()
                            .gap_2()
                            .pl_1p5()
                            .child(div().min_w_0().child(label))
                            .when(added > 0, |d| {
                                d.child(
                                    Label::new(format!("+{added}"))
                                        .size(TextSize::Sm)
                                        .color(Color::Success),
                                )
                            })
                            .when(removed > 0, |d| {
                                d.child(
                                    Label::new(format!("−{removed}"))
                                        .size(TextSize::Sm)
                                        .color(Color::Danger),
                                )
                            })
                            .child(activity_toggle(
                                index,
                                "edit".into(),
                                &action,
                                control,
                                true,
                            )),
                    )
                    .when(expanded, |d| {
                        d.child(edit_body(
                            EditBody {
                                index,
                                patch,
                                preview,
                                marks: (&marks, mark_slot.as_ref()),
                                folder: &folder,
                                selector: body_selector,
                                open_edit,
                                action: &action,
                            },
                            cx,
                        ))
                    })
                    .children(previews)
                    .into_any_element()
            }
            ItemKind::Other { .. }
                if item.id.starts_with(keystone_agent::log::LOSS_MARKER_PREFIX) =>
            {
                report(format!("log-loss-notice-{index}"), loss_summary(item), cx)
            }
            ItemKind::Tool { .. } | ItemKind::Other { .. } => {
                let control = activity.expect("tool disclosure");
                let expanded = control.expanded;
                let body_selector = control.body_selector.clone();
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(activity_toggle(
                                index,
                                activity_name(item).unwrap(),
                                &action,
                                control,
                                false,
                            ))
                            .when_some(
                                match item.status {
                                    ItemStatus::InProgress => Some("Running"),
                                    ItemStatus::Completed => None,
                                    ItemStatus::Failed => Some("Failed"),
                                    ItemStatus::Declined => Some("Declined"),
                                    ItemStatus::Interrupted => Some("Interrupted"),
                                },
                                |d, status| {
                                    d.child(
                                        div()
                                            .automation_id(format!("activity-status-{index}"))
                                            .child(
                                                Label::new(status)
                                                    .size(TextSize::Sm)
                                                    .color(Color::Muted),
                                            ),
                                    )
                                },
                            ),
                    )
                    .when(expanded, |d| {
                        d.child(
                            div()
                                .automation_id(body_selector)
                                .text_size(TextSize::Ui.px())
                                .text_color(colors.text_muted)
                                .when(!item.text.is_empty(), |d| {
                                    d.child(div().child(marked(item.text.clone(), 0)))
                                }),
                        )
                    })
                    .into_any_element()
            }
        },
        Row::TurnEnded {
            status,
            error,
            limit,
        } => {
            let (text, color) = match (status, limit) {
                (TurnStatus::Interrupted, _) => ("Stopped", Color::Muted),
                (_, Some(_)) => ("Usage limit reached", Color::Warning),
                _ => ("Failed", Color::Danger),
            };
            // Under the usage limit card, the provider's own words, which the card's
            // headline does not repeat.
            let text = match (limit, error) {
                (Some(_), Some(error)) => error.clone(),
                (_, Some(error)) => format!("{text}: {error}"),
                (_, None) => text.to_owned(),
            };
            Label::new(text)
                .size(TextSize::Ui)
                .color(color)
                .into_any_element()
        }
        Row::Recovery(recovery) => report(
            format!("recovery-report-{index}"),
            recovery_summary(recovery),
            cx,
        ),
        Row::Surface(surface) => {
            crate::widget::SurfaceView::new(surface.clone(), widget.expect("widget controls"))
                .into_any_element()
        }
        Row::Attachments(attachments) => crate::attachments::sent(
            index,
            attachments,
            previews.map(|previews| previews.open),
            cx,
        ),
        Row::Decision { action, .. } => Label::new(format!("Denied: {action}"))
            .size(TextSize::Ui)
            .color(Color::Muted)
            .into_any_element(),
        Row::Note(text) => div()
            .automation_id(format!("note-{index}"))
            .child(
                Label::new(text.clone())
                    .size(TextSize::Ui)
                    .color(Color::Muted),
            )
            .into_any_element(),
        Row::OlderNotImported => div()
            .automation_id(format!("older-not-imported-{index}"))
            .child(
                Label::new("Older messages weren't imported.")
                    .size(TextSize::Ui)
                    .color(Color::Muted),
            )
            .into_any_element(),
    }
}

/// A launched session's result that started this turn: who it came from, how its turn
/// ended, and its reply. It came from an agent, so it sits with the agent's rows rather
/// than in a user bubble.
#[allow(clippy::too_many_arguments)]
fn result_row(
    index: usize,
    title: &str,
    read: &crate::app_state::delivery::ResultMessage<'_>,
    reply: Option<Arc<Document>>,
    text: &str,
    action: &Option<(CopyTarget, FocusHandle)>,
    (marks, mark_slot): (Vec<Mark>, Option<MarkSlot>),
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors;
    let (status, detail) = match read.outcome {
        "It finished." => (None, None),
        "It was stopped." => (Some(("Stopped", Color::Muted)), None),
        outcome => (
            Some(("Failed", Color::Danger)),
            Some(
                outcome
                    .strip_prefix("It failed: ")
                    .unwrap_or(outcome)
                    .to_owned(),
            ),
        ),
    };
    v_flex()
        .automation_id(format!("result-{index}"))
        .bg(colors.surface_raised)
        .rounded(radius::PANEL)
        .overflow_hidden()
        .child(
            h_flex()
                .min_h(px(38.))
                .pl_3()
                .pr_1()
                .py_1()
                .gap_2()
                .text_size(TextSize::Ui.px())
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            Label::new("Result from")
                                .size(TextSize::Ui)
                                .color(Color::Muted),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(colors.text_strong)
                                .child(title.to_owned()),
                        ),
                )
                .when_some(status, |d, (status, color)| {
                    d.child(badge(status, color, cx))
                })
                .child(
                    CopyButton::new(("copy-result", index), "Copy result", text.to_owned())
                        .keyboard(
                            action
                                .as_ref()
                                .filter(|(n, _)| *n == CopyTarget::Row)
                                .map(|(_, f)| f),
                        ),
                ),
        )
        .child(
            v_flex()
                .px_3()
                .pb_2p5()
                .gap_1()
                .when_some(detail, |d, detail| {
                    d.child(Label::new(detail).size(TextSize::Ui).color(Color::Danger))
                })
                .child(
                    Markdown::new(
                        SharedString::from(format!("result-reply-{index}")),
                        reply.unwrap_or_else(|| Arc::new(Document::parse(read.reply))),
                    )
                    .marks(marks, mark_slot),
                ),
        )
        .into_any_element()
}

/// A report row: badges, then the headline, then a line per detail.
fn report(id: String, summary: RecoverySummary, cx: &App) -> AnyElement {
    v_flex()
        .automation_id(id)
        .gap_1()
        .child(
            h_flex()
                .items_start()
                .gap_2()
                .children(
                    summary
                        .badges
                        .iter()
                        .map(|(text, color)| badge(text, *color, cx)),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(summary.headline)
                            .size(TextSize::Ui)
                            .color(Color::Muted),
                    ),
                ),
        )
        .children(summary.details.into_iter().map(|line| {
            Label::new(line)
                .size(TextSize::Sm)
                .color(Color::Muted)
                .into_any_element()
        }))
        .into_any_element()
}

/// A small outlined label beside a row.
fn badge(text: &str, color: Color, cx: &App) -> gpui::Div {
    div()
        .flex_none()
        .px_1p5()
        .rounded(radius::CONTROL)
        .border_1()
        .border_color(cx.theme().colors.border)
        .child(Label::new(text.to_owned()).size(TextSize::Sm).color(color))
}

/// Said after a tool row whose outcome is not the plain one.
fn outcome_note(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::Interrupted => " · no recorded result",
        ItemStatus::Failed => " · failed",
        ItemStatus::Declined => " · not run",
        _ => "",
    }
}

/// How a recovery report reads: badges, one headline, and a line per limitation.
pub struct RecoverySummary {
    pub badges: Vec<(&'static str, Color)>,
    pub headline: String,
    pub details: Vec<String>,
}

fn plural(count: u64, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// How a log gap marker reads, as a partial recovery does: what is shown here is not
/// everything that happened.
pub fn loss_summary(marker: &Item) -> RecoverySummary {
    // A recovery has since added what the provider recorded there.
    if keystone_agent::log::lost_count(marker).is_none() {
        return RecoverySummary {
            badges: Vec::new(),
            headline: marker.text.clone(),
            details: Vec::new(),
        };
    }
    RecoverySummary {
        badges: vec![("Partial", Color::Warning)],
        headline: marker.text.clone(),
        details: Vec::new(),
    }
}

pub fn recovery_summary(report: &RecoveryReport) -> RecoverySummary {
    let mut badges = Vec::new();
    match report.outcome {
        Outcome::Recovered => {}
        Outcome::Partial => badges.push(("Partial", Color::Warning)),
        Outcome::Conflict => badges.push(("Conflict", Color::Warning)),
    }
    let mut recovered = Vec::new();
    if report.messages > 0 {
        recovered.push(plural(report.messages.into(), "message", "messages"));
    }
    if report.tools > 0 {
        recovered.push(plural(report.tools.into(), "tool call", "tool calls"));
    }
    let mut parts = Vec::new();
    if !recovered.is_empty() {
        parts.push(format!("Recovered {}", recovered.join(" and ")));
    }
    if report.extended > 0 {
        parts.push(format!(
            "completed {}",
            plural(report.extended.into(), "partial row", "partial rows")
        ));
    }
    let found = if parts.is_empty() {
        "Nothing more was recovered.".to_owned()
    } else {
        let mut text = parts.join("; ");
        text.push('.');
        let mut chars = text.chars();
        chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect())
            .unwrap_or(text)
    };
    let headline = match report.turn {
        TurnEvidence::Recorded(TurnStatus::Completed) => {
            format!("Connection interrupted; the turn completed. {found}")
        }
        TurnEvidence::Recorded(TurnStatus::Interrupted) => {
            format!("Connection interrupted; the turn was interrupted. {found}")
        }
        TurnEvidence::Recorded(TurnStatus::Failed) => {
            format!("Connection interrupted; the turn failed. {found}")
        }
        TurnEvidence::Unknown => format!("Interrupted; the turn's outcome is unknown. {found}"),
        TurnEvidence::Unaffected => found,
    };
    let details = report
        .limits
        .iter()
        .map(|limit| match limit {
            Limit::TornTail => "The last saved record was cut off and left out.".into(),
            Limit::Malformed { count } => format!(
                "{} could not be read.",
                plural(*count, "saved record", "saved records")
            ),
            Limit::Unsupported { count } => format!(
                "{} of unsupported kinds were skipped.",
                plural(*count, "record", "records")
            ),
            Limit::Unstable => "The provider's saved conversation changed while it was read.".into(),
            Limit::Truncated => "Only part of a long history was checked.".into(),
            Limit::OlderNotRead => {
                "Only the newest part of the provider's history was read; it does not reach what is shown here.".into()
            }
            Limit::Compacted => {
                "The provider compacted this conversation, so older parts are summaries.".into()
            }
            Limit::ItemsNotLoaded => {
                "The provider returned summaries instead of some items.".into()
            }
            Limit::UnknownOutcome { count } => format!(
                "{} no recorded result. Any approval that was waiting has expired.",
                if *count == 1 {
                    "1 tool call has".to_owned()
                } else {
                    format!("{count} tool calls have")
                }
            ),
            Limit::PromptNotRecorded => {
                "Your last message is not in the provider's record, so it may not have been delivered."
                    .into()
            }
            Limit::EarlierHistoryDiffers { count } => format!(
                "{} earlier in the provider's record {} not match this transcript and {} left out.",
                plural(*count, "item", "items"),
                if *count == 1 { "does" } else { "do" },
                if *count == 1 { "was" } else { "were" },
            ),
            Limit::Branched => {
                "The provider's record has more than one branch, so nothing was matched.".into()
            }
            Limit::IdentityMismatch => {
                "The provider's record names a different folder, so it was not used.".into()
            }
            Limit::ReusedIds => "The provider reuses item IDs, so nothing was matched.".into(),
            Limit::OrderDiffers => {
                "Matching items appear in a different order, so nothing was matched.".into()
            }
            Limit::NoCommonPoint => {
                "No part of the provider's record matches this transcript, so nothing was added."
                    .into()
            }
            Limit::ConflictingText { count } => format!(
                "{} the provider's copy; Keystone's copy was kept.",
                if *count == 1 {
                    "1 saved message differs from".to_owned()
                } else {
                    format!("{count} saved messages differ from")
                }
            ),
        })
        .collect();
    RecoverySummary {
        badges,
        headline,
        details,
    }
}

/// Copies `text`, then shows a check for a moment.
#[derive(IntoElement)]
struct CopyButton {
    id: ElementId,
    label: &'static str,
    text: String,
    focus: Option<FocusHandle>,
}

impl CopyButton {
    fn new(id: impl Into<ElementId>, label: &'static str, text: String) -> Self {
        Self {
            id: id.into(),
            label,
            text,
            focus: None,
        }
    }

    fn keyboard(mut self, focus: Option<&FocusHandle>) -> Self {
        self.focus = focus.cloned();
        self
    }
}

impl RenderOnce for CopyButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let copied = window.use_keyed_state((self.id.clone(), "copied"), cx, |_, _| false);
        let is_copied = *copied.read(cx);
        let selector = match &self.id {
            ElementId::NamedInteger(name, index) => format!("{name}-{index}"),
            id => id.to_string(),
        };
        let text = self.text;
        IconButton::new(
            self.id,
            if is_copied {
                IconName::Check
            } else {
                IconName::Copy
            },
        )
        .size(ButtonSize::Compact)
        .when_some(self.focus, |button, focus| {
            button.track_focus(&focus).reveal_on_focus()
        })
        .debug_selector(move || selector)
        .tooltip(Tooltip::text(if is_copied { "Copied" } else { self.label }))
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            copied.update(cx, |copied, cx| {
                *copied = true;
                cx.notify();
                cx.spawn(async move |copied, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(1500))
                        .await;
                    let _ = copied.update(cx, |copied, cx| {
                        *copied = false;
                        cx.notify();
                    });
                })
                .detach();
            });
        })
    }
}
