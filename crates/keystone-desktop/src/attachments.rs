//! Attachments in the composer. An image pasted, dropped, chosen with the attach button
//! or typed as a path is copied into Keystone's data folder and shown as a chip under
//! the draft; it is saved with the draft and sent with it in the provider's own image
//! form. Any other file is attached by mentioning its path in the draft, so the agent
//! reads it with its own tools. See [`keystone_core::attachment`] for the size limit
//! and how long copies are kept.

use crate::local_files::LocalPath;
use crate::{Keystone, mentions, screenshot};
use gpui::{
    Anchor, AnyElement, App, ClipboardEntry, Context, ElementId, Entity, ExternalPaths, Focusable,
    SharedString, Window, div, prelude::*, px,
};
use keystone_core::Attachment;
use keystone_ui::prelude::*;
use std::path::{Path, PathBuf};

gpui::actions!(keystone, [AttachFiles]);

/// The composer's attachment state, kept while the app runs.
pub(crate) struct Attach {
    /// The typed-path field, shown where no file picker is available.
    pub(crate) path: Entity<TextInput>,
    pub(crate) path_open: bool,
    /// Why the last attachment was refused, for the session it was attached to.
    pub(crate) notice: Option<(u64, String)>,
    /// Attachments still being copied, by session. That session's Send is blocked until
    /// they land, so a message never goes without an image attached just before it.
    pub(crate) importing: std::collections::HashMap<u64, usize>,
    /// The attach button's menu is open.
    pub(crate) menu: bool,
    /// A screenshot is being picked, so another capture waits for it.
    pub(crate) capturing: bool,
    /// Run in place of the platform's screenshot tool; see [`screenshot::test_command`].
    pub(crate) screenshot_command: Option<Vec<String>>,
}

impl Attach {
    pub(crate) fn new(cx: &mut Context<Keystone>) -> Self {
        Self {
            path: cx.new(|cx| TextInput::new("Path to a file", InputMode::SingleLine, cx)),
            path_open: false,
            notice: None,
            importing: Default::default(),
            menu: false,
            capturing: false,
            screenshot_command: screenshot::test_command(),
        }
    }
}

/// What attaching one file came to.
enum Attached {
    Image(Attachment),
    /// Not an image: mentioned by its path.
    Mention(PathBuf),
    Refused(String),
}

/// How `path` is mentioned in a message ([`mentions::token`]): relative to `root`
/// when it is inside it, else whole.
fn mention(path: &Path, root: Option<&Path>) -> Option<String> {
    mentions::token(
        root.and_then(|root| path.strip_prefix(root).ok())
            .filter(|relative| !relative.as_os_str().is_empty())
            .unwrap_or(path)
            .to_str()?,
    )
}

impl Keystone {
    /// Whether `session` has attachments still being copied.
    pub(crate) fn importing(&self, session: u64) -> bool {
        self.attach.importing.contains_key(&session)
    }

    fn start_import(&mut self, session: u64, cx: &mut Context<Self>) {
        *self.attach.importing.entry(session).or_default() += 1;
        cx.notify();
    }

    fn end_import(&mut self, session: u64, cx: &mut Context<Self>) {
        if let Some(count) = self.attach.importing.get_mut(&session) {
            *count -= 1;
            if *count == 0 {
                self.attach.importing.remove(&session);
            }
        }
        cx.notify();
    }

    /// The images attached to the selected session's draft, as saved.
    pub(crate) fn draft_attachments(&self, cx: &App) -> Vec<Attachment> {
        self.selected_session(cx)
            .map(|session| session.draft_attachments().to_vec())
            .unwrap_or_default()
    }

    fn attachments_dir(&self, cx: &App) -> PathBuf {
        self.state.read(cx).attachments.dir().to_owned()
    }

    /// The machine `session` works on, when it is another: an image picked, pasted or
    /// captured here goes to it by its bytes, and any other file is refused, since a
    /// mention would name a path there.
    fn elsewhere(&self, session: u64, cx: &App) -> Option<String> {
        match self.session_place(session, cx) {
            Some(crate::presentation::SessionPlace::Remote(machine)) => Some(machine),
            Some(
                crate::presentation::SessionPlace::Local(_)
                | crate::presentation::SessionPlace::Unusable,
            )
            | None => None,
        }
    }

    fn show_attach_notice(&mut self, session: u64, notice: Option<String>, cx: &mut Context<Self>) {
        self.attach.notice = notice.map(|notice| (session, notice));
        cx.notify();
    }

    /// Saves `session`'s draft images. A failed save leaves them as they were and says
    /// why.
    fn save_attachments(
        &mut self,
        session: u64,
        attachments: Vec<Attachment>,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.state.update(cx, |state, cx| {
            state.set_draft_attachments(session, attachments, cx)
        }) {
            Ok(_) => true,
            Err(error) => {
                self.show_attach_notice(
                    session,
                    Some(format!("Attachment not saved. {error}")),
                    cx,
                );
                false
            }
        }
    }

    /// Adds images to `session`'s draft after those it holds, once each.
    fn add_attachments(&mut self, session: u64, added: Vec<Attachment>, cx: &mut Context<Self>) {
        let Some(mut attachments) = self
            .state
            .read(cx)
            .session(session)
            .map(|s| s.draft_attachments().to_vec())
        else {
            return;
        };
        let before = attachments.len();
        for attachment in added {
            if !attachments.iter().any(|a| a.sha256 == attachment.sha256) {
                attachments.push(attachment);
            }
        }
        if attachments.len() != before {
            self.save_attachments(session, attachments, cx);
        }
        self.refresh(cx);
    }

    pub(crate) fn remove_attachment(
        &mut self,
        session: u64,
        sha256: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut attachments) = self
            .state
            .read(cx)
            .session(session)
            .map(|s| s.draft_attachments().to_vec())
        else {
            return;
        };
        attachments.retain(|a| a.sha256 != sha256);
        if self.save_attachments(session, attachments, cx) {
            self.show_attach_notice(session, None, cx);
        }
        self.focus_composer(window, cx);
        self.refresh(cx);
    }

    /// Attaches files to the selected session's draft: images are copied off the UI
    /// thread and added as chips, other files are mentioned at the caret, and anything
    /// refused says why.
    pub(crate) fn attach_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected(cx) else {
            return;
        };
        // A session on another machine has no files here, so only a file the user
        // picked here by its full path is read, and only an image goes.
        let elsewhere = self.elsewhere(session, cx);
        let files = self.local_files(session, cx);
        if files.is_none() && elsewhere.is_none() {
            return;
        }
        let attachments = self.state.read(cx).attachments.clone();
        self.start_import(session, cx);
        let task = cx.background_spawn(async move {
            paths
                .into_iter()
                .map(|path| {
                    let path = match (&files, &elsewhere) {
                        (Some(files), _) => files.picked(path),
                        (None, Some(machine)) => match LocalPath::picked_here(path) {
                            Ok(path) => path,
                            Err(path) => {
                                return Attached::Refused(format!(
                                    "{} isn't a full path on this machine, so it can't go to a session on {machine}.",
                                    path.display()
                                ));
                            }
                        },
                        (None, None) => unreachable!("checked above"),
                    };
                    match (attachments.import_file(path.as_path()), &elsewhere) {
                        (Ok(Some(image)), _) => Attached::Image(image),
                        (Ok(None), None) => Attached::Mention(path.as_path().to_owned()),
                        (Ok(None), Some(machine)) => Attached::Refused(format!(
                            "{} isn't an image. Only images go to sessions on {machine}; mention files there by their path on that machine.",
                            path.as_path().display()
                        )),
                        (Err(reason), _) => Attached::Refused(reason),
                    }
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let attached = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_attach(session, attached, window, cx)
            });
        })
        .detach();
    }

    fn finish_attach(
        &mut self,
        session: u64,
        attached: Vec<Attached>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.end_import(session, cx);
        let files = self.local_files(session, cx);
        let root = files.as_ref().and_then(|files| files.root());
        let (mut images, mut mentions, mut refused) = (Vec::new(), Vec::new(), Vec::new());
        for attached in attached {
            match attached {
                Attached::Image(image) => images.push(image),
                Attached::Mention(path) => match mention(&path, root) {
                    Some(mention) => mentions.push(mention),
                    None => refused.push(format!(
                        "{} can't be mentioned: quote or line break in its path.",
                        path.display()
                    )),
                },
                Attached::Refused(reason) => refused.push(reason),
            }
        }
        self.show_attach_notice(
            session,
            (!refused.is_empty()).then(|| refused.join("\n")),
            cx,
        );
        self.add_attachments(session, images, cx);
        if mentions.is_empty() {
            return;
        }
        if self.selected(cx) == Some(session) {
            for mention in &mentions {
                self.insert_mention(mention, window, cx);
            }
        } else if let Some(draft) = self
            .state
            .read(cx)
            .session(session)
            .map(|s| s.draft().to_owned())
        {
            let text = format!("{} ", mentions.join(" "));
            let joined = if draft.is_empty() || draft.ends_with(char::is_whitespace) {
                format!("{draft}{text}")
            } else {
                format!("{draft} {text}")
            };
            if let Err(error) = self
                .state
                .update(cx, |state, cx| state.set_draft(session, joined, cx))
            {
                self.show_attach_notice(
                    session,
                    Some(format!(
                        "{} not added to the draft. {error}",
                        mentions.join(" ")
                    )),
                    cx,
                );
            }
        }
        self.refresh(cx);
    }

    /// Ctrl+V in the composer: an image on the clipboard is attached instead of pasted.
    /// Returns false when there is none, so the text is pasted as usual.
    pub(crate) fn paste_image(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.selected(cx) else {
            return false;
        };
        let Some(image) = cx.read_from_clipboard().and_then(|item| {
            item.entries().iter().find_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(image.clone()),
                _ => None,
            })
        }) else {
            return false;
        };
        let attachments = self.state.read(cx).attachments.clone();
        let name = format!(
            "Image.{}",
            image.format.mime_type().trim_start_matches("image/")
        );
        self.start_import(session, cx);
        let task = cx.background_spawn(async move {
            match attachments.import_bytes(&image.bytes, name) {
                Ok(image) => Attached::Image(image),
                Err(reason) => Attached::Refused(reason),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let attached = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_attach(session, vec![attached], window, cx)
            });
        })
        .detach();
        true
    }

    /// "Capture screenshot": the user picks a region with the platform's tool and it is
    /// attached to the draft like a pasted image. Cancelling leaves the draft as it was;
    /// with no tool the notice says what to install.
    pub(crate) fn capture_screenshot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.attach.menu = false;
        let Some(session) = self.selected(cx) else {
            return;
        };
        if self.attach.capturing {
            return;
        }
        self.attach.capturing = true;
        let attachments = self.state.read(cx).attachments.clone();
        let command = self.attach.screenshot_command.clone();
        self.start_import(session, cx);
        let task = cx.background_spawn(async move { screenshot::take(&attachments, command) });
        cx.spawn_in(window, async move |this, cx| {
            let taken = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.attach.capturing = false;
                match taken {
                    None => this.end_import(session, cx),
                    Some(taken) => this.finish_attach(
                        session,
                        vec![taken.map_or_else(Attached::Refused, Attached::Image)],
                        window,
                        cx,
                    ),
                }
            });
        })
        .detach();
    }

    /// The attach button and Ctrl+Shift+U: the platform's file picker, or where it has
    /// none, a field for a typed path.
    pub(crate) fn choose_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.attach.menu = false;
        let Some(session) = self.selected(cx) else {
            return;
        };
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let picked = picked.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.selected(cx) != Some(session) {
                    return;
                }
                match picked {
                    Ok(Ok(Some(paths))) => this.attach_paths(paths, window, cx),
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => {
                        eprintln!("No file picker: {error}");
                        this.open_attach_path(window, cx);
                    }
                    Err(_) => this.open_attach_path(window, cx),
                }
            });
        })
        .detach();
    }

    /// Shows the typed-path field.
    pub(crate) fn open_attach_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected(cx).is_none() {
            return;
        }
        self.attach.path_open = true;
        self.attach
            .path
            .update(cx, |input, cx| input.set_value(String::new(), cx));
        self.attach.path.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(crate) fn close_attach_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.attach.path_open = false;
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// Enter in the typed-path field: `~/` is the home folder, and a relative path is
    /// read from the session's folder.
    fn confirm_attach_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.attach.path.read(cx).value().trim().to_owned();
        if typed.is_empty() {
            return;
        }
        let path = match typed.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(rest))
                .unwrap_or_else(|| PathBuf::from(&typed)),
            None => PathBuf::from(&typed),
        };
        self.attach.path_open = false;
        if let Some(session) = self.selected(cx) {
            self.show_attach_notice(session, None, cx);
        }
        self.attach_paths(vec![path], window, cx);
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// The paperclip and its menu: attach files, or capture a screenshot.
    pub(crate) fn attach_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        PopoverMenu::new(
            "attach-menu",
            IconButton::new("attach", IconName::Paperclip)
                .aria_label("Attach")
                .selected(self.attach.menu)
                .debug_selector(|| "attach".into())
                .when(!self.attach.menu, |button| {
                    button.tooltip(Tooltip::text("Attach"))
                }),
        )
        .anchor(Anchor::BottomRight)
        .open(self.attach.menu)
        .on_open_change(self.scoped_listener(cx, |this, open: &bool, _, cx| {
            this.attach.menu = *open;
            cx.notify();
        }))
        .child(
            MenuItem::new("attach-files", "Attach files…")
                .kbd(crate::commands::hint("attach-files", cx))
                .debug_selector(|| "attach-files".into())
                .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                    this.choose_attachments(window, cx)
                })),
        )
        .child(
            MenuItem::new("capture-screenshot", "Capture screenshot")
                .kbd(crate::commands::hint("capture-screenshot", cx))
                .debug_selector(|| "capture-screenshot".into())
                .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                    this.capture_screenshot(window, cx)
                })),
        )
    }

    /// The chips, the typed-path field and any refusal, under the draft.
    pub(crate) fn attachment_strip(
        &self,
        session: u64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let attachments = self.draft_attachments(cx);
        let notice = self
            .attach
            .notice
            .as_ref()
            .filter(|(at, _)| *at == session)
            .map(|(_, notice)| notice.clone());
        if attachments.is_empty() && notice.is_none() && !self.attach.path_open {
            return None;
        }
        let dir = self.attachments_dir(cx);
        let colors = cx.theme().colors;
        let chips = h_flex()
            .flex_wrap()
            .gap(px(6.))
            .children(attachments.iter().enumerate().map(|(index, attachment)| {
                let sha256 = attachment.sha256.clone();
                let remove = Button::new(("attachment-remove", index), "")
                    .style(ButtonStyle::Ghost)
                    .icon(IconName::Close)
                    .aria_label(format!("Remove {}", attachment.name))
                    .tooltip(Tooltip::text("Remove"))
                    .debug_selector(move || format!("attachment-remove-{index}"))
                    .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                        this.remove_attachment(session, &sha256, window, cx)
                    }));
                chip(("attachment", index), attachment, &dir, cx).child(remove)
            }));
        let path = self.attach.path_open.then(|| {
            let submit = cx.weak_entity();
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .px(px(10.))
                        .py(px(4.))
                        .rounded(radius::CONTROL)
                        .border_1()
                        .border_color(colors.border_focused)
                        .bg(colors.background)
                        .automation_id("attach-path")
                        .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                            let key = event.keystroke.key.as_str();
                            if key != "enter" && key != "escape" {
                                return;
                            }
                            cx.stop_propagation();
                            let _ = submit.update(cx, |this, cx| {
                                if key == "enter" {
                                    this.confirm_attach_path(window, cx)
                                } else {
                                    this.close_attach_path(window, cx)
                                }
                            });
                        })
                        .child(self.attach.path.clone()),
                )
                .child(
                    Button::new("attach-path-confirm", "Attach")
                        .style(ButtonStyle::Subtle)
                        .debug_selector(|| "attach-path-confirm".into())
                        .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                            this.confirm_attach_path(window, cx)
                        })),
                )
        });
        Some(
            v_flex()
                .gap(px(8.))
                .when(!attachments.is_empty(), |d| d.child(chips))
                .children(path)
                .children(notice.map(|notice| {
                    div()
                        .automation_id("attachment-notice")
                        .child(Label::new(notice).size(TextSize::Ui).color(Color::Danger))
                }))
                .into_any_element(),
        )
    }

    /// The composer card's file drop target.
    pub(crate) fn on_file_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attach_paths(paths.paths().to_vec(), window, cx);
    }
}

/// An image as a chip: a thumbnail of its copy in `dir` and its name. A copy no longer
/// kept shows a file icon instead.
pub(crate) fn chip(
    id: impl Into<ElementId>,
    attachment: &Attachment,
    dir: &Path,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors;
    let id = id.into();
    let selector: SharedString = format!("{id}").into();
    h_flex()
        .id(id)
        .automation_id(selector)
        .flex_none()
        .gap(px(6.))
        .p(px(3.))
        .pr(px(6.))
        .rounded(radius::CONTROL)
        .border_1()
        .border_color(colors.border)
        .bg(colors.surface_raised)
        .child(crate::preview::ChipImage::new(
            crate::local_files::LocalPath::kept(attachment, dir),
        ))
        .child(
            div().max_w(px(160.)).min_w_0().child(
                Label::new(attachment.name.clone())
                    .size(TextSize::Ui)
                    .color(Color::Default)
                    .truncate(),
            ),
        )
        .tooltip(Tooltip::text(attachment.name.clone()).builder())
}

/// Where the app's attached images are copied, for rows drawn without the app state.
pub(crate) struct AttachmentsDir(pub(crate) PathBuf);
impl gpui::Global for AttachmentsDir {}

/// The images sent with a message, under it in the transcript, as thumbnails that
/// `open` previews. A copy no longer kept is a chip.
pub(crate) fn sent(
    index: usize,
    attachments: &[Attachment],
    open: Option<crate::preview::OpenPreview>,
    cx: &App,
) -> AnyElement {
    let dir = cx
        .try_global::<AttachmentsDir>()
        .map(|dir| dir.0.clone())
        .unwrap_or_default();
    h_flex()
        .automation_id(SharedString::from(format!("sent-attachments-{index}")))
        .debug_selector(move || format!("sent-attachments-{index}"))
        .justify_end()
        .flex_wrap()
        .gap(px(6.))
        .children(attachments.iter().enumerate().map(|(n, attachment)| {
            let id = SharedString::from(format!("sent-attachment-{index}-{n}"));
            let chip = chip(id.clone(), attachment, &dir, cx).into_any_element();
            match &open {
                Some(open) => crate::preview::Thumbnail::new(
                    id,
                    crate::local_files::LocalPath::kept(attachment, &dir),
                    index,
                    open.clone(),
                )
                .fallback(chip)
                .into_any_element(),
                None => chip,
            }
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::mention;
    use std::path::Path;

    #[test]
    fn files_are_mentioned_relative_to_the_session_folder_and_quoted_with_spaces() {
        let root = Path::new("/work/app");
        assert_eq!(
            mention(Path::new("/work/app/src/a.rs"), Some(root)).as_deref(),
            Some("@src/a.rs")
        );
        assert_eq!(
            mention(Path::new("/work/app/my notes/a b.txt"), Some(root)).as_deref(),
            Some("@\"my notes/a b.txt\"")
        );
        assert_eq!(
            mention(Path::new("/tmp/log.txt"), Some(root)).as_deref(),
            Some("@/tmp/log.txt")
        );
        assert_eq!(
            mention(Path::new("/tmp/log.txt"), None).as_deref(),
            Some("@/tmp/log.txt")
        );
        assert_eq!(mention(Path::new("/tmp/a\"b"), None), None);
        assert_eq!(mention(Path::new("/tmp/a\nb"), None), None);
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "attachments_tests.rs"]
mod ui_tests;
