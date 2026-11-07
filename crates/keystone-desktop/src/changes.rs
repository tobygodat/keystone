//! The Changes surface: the session folder's repository as a list of changed files,
//! with staging, commit and push through the user's own git, and pull requests
//! through the user's own `gh`. There is no diff view.

use crate::app_state::AppState;
use crate::arrangement_editor::FILES_WIDTH;
use crate::diff_view::ViewChanges;
use crate::{Keystone, SendMessage};
use futures::channel::oneshot;
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    FontWeight, IntoElement, KeyBinding, Render, Task, WeakEntity, Window, div, prelude::*, px,
};
use keystone_agent::git::{self, Entry, PushError, Status};
use keystone_agent::github;
use keystone_agent::worker::Worker;
use keystone_core::{Axis, Component, Edit, Place};
use keystone_ui::prelude::*;
use keystone_ui::text_input::Changed;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

gpui::actions!(
    changes,
    [
        ToggleChanges,
        ToggleStaged,
        CopyChangePath,
        CopyPullRequestLink
    ]
);

const LIST_CONTEXT: &str = "ChangesList";
const ROW_HEIGHT: f32 = 26.;
/// How long typing must pause before the commit message is saved, as for drafts.
const DRAFT_DELAY: Duration = Duration::from_millis(250);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("space", ToggleStaged, Some(LIST_CONTEXT)),
        KeyBinding::new("secondary-shift-c", CopyChangePath, Some(LIST_CONTEXT)),
        KeyBinding::new("secondary-shift-d", ViewChanges, Some(LIST_CONTEXT)),
    ]);
}

/// One Changes panel per folder, so switching sessions keeps each folder's message
/// box and list position.
#[derive(Default)]
pub(crate) struct Changes {
    panels: RefCell<HashMap<PathBuf, Entity<ChangesPanel>>>,
    /// The folder the surface last drew, so drawing another one refreshes it.
    shown: RefCell<Option<PathBuf>>,
}

impl Changes {
    /// `folder`'s panel, made on first use. Its tab title follows its file count.
    fn panel(
        &self,
        folder: &Path,
        state: &Entity<AppState>,
        cx: &mut Context<Keystone>,
    ) -> Entity<ChangesPanel> {
        if let Some(panel) = self.panels.borrow().get(folder) {
            return panel.clone();
        }
        let keystone = cx.weak_entity();
        let panel = cx.new(|cx| ChangesPanel::new(folder.to_owned(), state, keystone, cx));
        cx.subscribe(&panel, |_, _, _: &CountChanged, cx| cx.notify())
            .detach();
        self.panels
            .borrow_mut()
            .insert(folder.to_owned(), panel.clone());
        panel
    }

    /// Re-reads the status of every panel whose repository holds `folder`, or whose
    /// folder is inside it. Separate worktrees have separate top folders, so a change
    /// in one never refreshes another's panel unless one lies inside the other.
    fn refresh(&self, folder: &Path, cx: &mut App) {
        let panels: Vec<_> = self.panels.borrow().values().cloned().collect();
        for panel in panels {
            let covers = {
                let panel = panel.read(cx);
                let top = panel.status().map_or(&panel.folder, |status| &status.root);
                folder.starts_with(top) || panel.folder.starts_with(folder)
            };
            if covers {
                panel.update(cx, |panel, cx| panel.refresh(cx));
            }
        }
    }

    /// Shows `folder`'s saved commit message in this window's panel for it, after
    /// another window changed it, unless this panel has unsaved typing of its own.
    pub(crate) fn sync_draft(&self, folder: &str, text: &str, cx: &mut App) {
        let panel = self.panels.borrow().get(Path::new(folder)).cloned();
        if let Some(panel) = panel {
            panel.update(cx, |panel, cx| {
                if !panel.unsaved && panel.message.read(cx).value() != text {
                    panel
                        .message
                        .update(cx, |input, cx| input.set_value(text.to_owned(), cx));
                    panel.unsaved = false;
                    panel.draft_task = None;
                }
            });
        }
    }

    /// The tab title for `folder`: "Changes", with the number of changed files once read.
    pub(crate) fn label(&self, folder: Option<&Path>, cx: &App) -> String {
        let count = folder
            .and_then(|folder| self.panels.borrow().get(folder).cloned())
            .and_then(|panel| Some(panel.read(cx).status()?.entries.len()))
            .unwrap_or(0);
        match count {
            0 => "Changes".into(),
            count => format!("Changes · {count}"),
        }
    }

    /// Saves every commit message not saved yet, before the window closes or Keystone
    /// quits. A message whose save failed is tried again here.
    pub(crate) fn save_drafts(&self, cx: &mut App) -> Result<(), String> {
        let panels: Vec<_> = self.panels.borrow().values().cloned().collect();
        for panel in panels {
            panel.update(cx, |panel, cx| {
                panel.draft_task = None;
                if panel.unsaved {
                    panel.save_draft(cx)?;
                }
                Ok::<_, String>(())
            })?;
        }
        Ok(())
    }
}

/// Whether the file at `path` still has a line git writes to mark a conflict.
fn has_markers(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| {
        bytes
            .split(|byte| *byte == b'\n')
            .any(|line| line.starts_with(b"<<<<<<< ") || line.starts_with(b">>>>>>> "))
    })
}

/// The number of changed files moved, so the tab title is stale.
pub(crate) struct CountChanged;

impl gpui::EventEmitter<CountChanged> for ChangesPanel {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Conflicted,
    Staged,
    Unstaged,
    Untracked,
}

impl Group {
    const ALL: [Group; 4] = [
        Group::Conflicted,
        Group::Staged,
        Group::Unstaged,
        Group::Untracked,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Conflicted => "Conflicts",
            Self::Staged => "Staged",
            Self::Unstaged => "Changes",
            Self::Untracked => "Untracked",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Conflicted => "conflicted",
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::Untracked => "untracked",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct File {
    group: Group,
    /// The one-letter state shown before the name: git's, `?` untracked, `!` conflicted.
    code: char,
    /// The path relative to the repository's top folder.
    shown: String,
    /// What staging or unstaging this row touches: its path, and a rename's source.
    paths: Vec<PathBuf>,
    absolute: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Header { group: Group, count: usize },
    File(File),
}

/// The list's rows: each non-empty group's header, then its files. A file with both
/// staged and unstaged changes is listed in both groups.
fn rows(status: &Status) -> Vec<Row> {
    let mut files: Vec<File> = Vec::new();
    let mut push = |group: Group, code: char, path: &Path, paths: Vec<PathBuf>| {
        files.push(File {
            group,
            code,
            shown: path.to_string_lossy().into_owned(),
            paths,
            absolute: status.root.join(path),
        })
    };
    for entry in &status.entries {
        match entry {
            Entry::Conflicted { path, .. } => {
                push(Group::Conflicted, '!', path, vec![path.clone()])
            }
            Entry::Untracked { path } => push(Group::Untracked, '?', path, vec![path.clone()]),
            Entry::Tracked {
                path,
                original,
                staged,
                unstaged,
            } => {
                if *staged != '.' {
                    // A rename's source is staged as deleted, so unstaging it restores
                    // both. A copy's source is its own row and keeps its own changes.
                    let source = original.as_ref().filter(|_| *staged == 'R');
                    let paths = std::iter::once(path).chain(source).cloned().collect();
                    push(Group::Staged, *staged, path, paths);
                }
                if *unstaged != '.' {
                    push(Group::Unstaged, *unstaged, path, vec![path.clone()]);
                }
            }
        }
    }
    Group::ALL
        .into_iter()
        .flat_map(|group| {
            let members: Vec<_> = files.iter().filter(|f| f.group == group).cloned().collect();
            let header = (!members.is_empty()).then_some(Row::Header {
                group,
                count: members.len(),
            });
            header.into_iter().chain(members.into_iter().map(Row::File))
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Notice {
    /// A command failed; the output is git's and its hooks', or gh's, as they wrote it.
    Failed {
        title: &'static str,
        output: String,
    },
    /// Something to do in a terminal first, with the command to run there. `id`
    /// names the case for tests and native verification.
    Hint {
        id: &'static str,
        title: String,
        command: &'static str,
        output: Option<String>,
    },
    Info(&'static str),
    /// The branch already had this pull request, which is saved on the session.
    AlreadyOpen(u64),
}

impl Notice {
    /// What to tell the user when gh can't make a pull request.
    fn github(error: github::Error) -> Self {
        match error {
            github::Error::Missing => Notice::Hint {
                id: "changes-hint-gh-missing",
                title: github::INSTALL_HINT.into(),
                command: github::SIGN_IN,
                output: None,
            },
            github::Error::SignedOut { output } => Notice::Hint {
                id: "changes-hint-gh-signed-out",
                title: github::SIGN_IN_HINT.into(),
                command: github::SIGN_IN,
                output: Some(output),
            },
            github::Error::Exists(pull_request) => Notice::AlreadyOpen(pull_request.number),
            github::Error::Failed { output } => Notice::Failed {
                title: "Could not create the pull request.",
                output,
            },
        }
    }
}

/// The form `gh pr create` is filled from.
struct PullRequestForm {
    /// The session the form was opened for, which the pull request is saved on.
    session: Option<u64>,
    /// The branch the form was opened for. gh works on the checked-out branch, so
    /// creating is refused once another is checked out.
    branch: String,
    title: Entity<TextInput>,
    body: Entity<TextInput>,
    base: Entity<TextInput>,
    draft: bool,
    /// What gh is doing for the form.
    busy: Option<&'static str>,
}

pub(crate) struct ChangesPanel {
    folder: PathBuf,
    /// Runs this folder's git commands one at a time, in order, so a refresh queued
    /// after a stage sees it.
    worker: Option<Worker>,
    state: WeakEntity<AppState>,
    /// The window whose conversation pane shows this folder's diff.
    keystone: WeakEntity<Keystone>,
    /// `None` until the first status arrives.
    status: Option<Result<Status, git::Error>>,
    rows: Rc<Vec<Row>>,
    list: Entity<VirtualListState>,
    message: Entity<TextInput>,
    focus: FocusHandle,
    /// A status read in flight, and whether another was asked for since it began.
    reading: Option<bool>,
    /// The message a commit in flight took, so a success clears only that text.
    committing: Option<String>,
    pushing: bool,
    /// The session this panel was last drawn for, which a new pull request form is
    /// opened for.
    session: Option<u64>,
    form: Option<PullRequestForm>,
    notice: Option<Notice>,
    /// The message changed since it was last saved.
    unsaved: bool,
    draft_task: Option<Task<()>>,
    _message: gpui::Subscription,
}

impl Drop for ChangesPanel {
    /// A commit hook may run for a long time, so closing the window does not wait.
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.detach();
        }
    }
}

impl ChangesPanel {
    fn new(
        folder: PathBuf,
        state: &Entity<AppState>,
        keystone: WeakEntity<Keystone>,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = folder
            .to_str()
            .map(|key| state.read(cx).commit_draft(key))
            .unwrap_or_default();
        let message = cx.new(|cx| {
            let mut input = TextInput::new("Commit message", InputMode::Multiline, cx)
                .text_style(TextSize::Ui, Color::Strong);
            input.set_value(draft, cx);
            input
        });
        let _message = cx.subscribe(&message, |this, _, _: &Changed, cx| {
            this.unsaved = true;
            this.draft_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(DRAFT_DELAY).await;
                let _ = this.update(cx, |this, cx| {
                    this.draft_task = None;
                    let _ = this.save_draft(cx);
                });
            }));
            cx.notify();
        });
        let mut panel = Self {
            worker: Some(Worker::new("keystone-git")),
            state: state.downgrade(),
            keystone,
            status: None,
            rows: Rc::default(),
            list: cx.new(|cx| VirtualListState::new(0, cx)),
            message,
            focus: cx.focus_handle(),
            reading: None,
            committing: None,
            pushing: false,
            session: None,
            form: None,
            notice: None,
            unsaved: false,
            draft_task: None,
            _message,
            folder,
        };
        panel.refresh(cx);
        panel
    }

    /// Runs `job` on this folder's git thread and hands its result to `done`.
    fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(worker) = &self.worker else {
            return;
        };
        let (sender, receiver) = oneshot::channel();
        worker.run(move || {
            let _ = sender.send(job());
        });
        cx.spawn(async move |this, cx| {
            if let Ok(result) = receiver.await {
                let _ = this.update(cx, |this, cx| done(this, result, cx));
            }
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(again) = &mut self.reading {
            *again = true;
            return;
        }
        self.reading = Some(false);
        let folder = self.folder.clone();
        self.run(
            move || git::status(&folder),
            |this, status, cx| {
                let again = this.reading.take() == Some(true);
                let count = |status: Option<&Status>| status.map(|s| s.entries.len());
                if count(this.status()) != count(status.as_ref().ok()) {
                    cx.emit(CountChanged);
                }
                let rows = status.as_ref().map(rows).unwrap_or_default();
                if *this.rows != rows {
                    let highlight = this
                        .list
                        .read(cx)
                        .highlighted()
                        .and_then(|old| follow(&this.rows, old, &rows));
                    this.list.update(cx, |list, _| {
                        list.set_count(rows.len());
                        if let Some(index) = highlight {
                            list.move_to(index);
                        }
                    });
                    this.rows = Rc::new(rows);
                }
                this.status = Some(status);
                cx.notify();
                if again {
                    this.refresh(cx);
                }
            },
            cx,
        );
    }

    fn status(&self) -> Option<&Status> {
        self.status.as_ref()?.as_ref().ok()
    }

    fn files(&self, group: Group) -> impl Iterator<Item = &File> {
        self.rows.iter().filter_map(move |row| match row {
            Row::File(file) if file.group == group => Some(file),
            _ => None,
        })
    }

    /// Unstages `paths` from the staged group, or stages them from any other, then
    /// refreshes.
    fn toggle(&mut self, group: Group, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.committing.is_some() {
            return;
        }
        let Some(root) = self.status().map(|status| status.root.clone()) else {
            return;
        };
        let unstage = group == Group::Staged;
        let title = if unstage {
            "Couldn't unstage"
        } else {
            "Couldn't stage"
        };
        self.run(
            move || {
                if unstage {
                    return git::unstage(&root, &paths).map_err(|error| error.to_string());
                }
                // Staging a conflicted file marks it resolved, which Changes can't undo.
                if group == Group::Conflicted
                    && let Some(path) = paths.iter().find(|path| has_markers(&root.join(path)))
                {
                    return Err(format!("{} still has conflict markers.", path.display()));
                }
                git::stage(&root, &paths).map_err(|error| error.to_string())
            },
            move |this, result, cx| {
                match result {
                    Err(output) => this.notice = Some(Notice::Failed { title, output }),
                    Ok(())
                        if matches!(
                            this.notice,
                            Some(Notice::Failed {
                                title: "Couldn't stage" | "Couldn't unstage",
                                ..
                            })
                        ) =>
                    {
                        this.notice = None
                    }
                    Ok(()) => return,
                }
                cx.notify();
            },
            cx,
        );
        self.refresh(cx);
    }

    fn toggle_group(&mut self, group: Group, cx: &mut Context<Self>) {
        let paths = self
            .files(group)
            .flat_map(|file| file.paths.iter().cloned())
            .collect();
        self.toggle(group, paths, cx);
    }

    fn highlighted_file(&self, cx: &App) -> Option<File> {
        match self.rows.get(self.list.read(cx).highlighted()?)? {
            Row::File(file) => Some(file.clone()),
            Row::Header { .. } => None,
        }
    }

    fn open(&mut self, file: &File, cx: &mut Context<Self>) {
        self.notice = if file.absolute.symlink_metadata().is_err() {
            Some(Notice::Info("That file no longer exists."))
        } else {
            crate::files::open_in_editor(&file.absolute)
                .err()
                .map(|output| Notice::Failed {
                    title: "Couldn't open the file",
                    output,
                })
        };
        cx.notify();
    }

    /// Shows the uncommitted changes in place of the conversation, from `file`.
    /// Deferred until this panel's update ends: bringing the conversation forward from
    /// a tab beside this panel reads the panel's focus.
    fn view_diff(&self, file: Option<String>, window: &mut Window, cx: &mut App) {
        let root = self
            .status()
            .map_or_else(|| self.folder.clone(), |status| status.root.clone());
        let keystone = self.keystone.clone();
        window.defer(cx, move |window, cx| {
            let _ = keystone.update(cx, |keystone, cx| {
                keystone.open_working_diff(root, file, window, cx)
            });
        });
    }

    fn copy_path(file: &File, cx: &mut App) {
        let path = file.absolute.to_string_lossy().into_owned();
        cx.write_to_clipboard(ClipboardItem::new_string(path));
    }

    /// Saves the message box as this folder's draft. A failure stays unsaved, so
    /// closing the window or quitting tries again.
    fn save_draft(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let (Some(key), Some(state)) = (self.folder.to_str(), self.state.upgrade()) else {
            self.unsaved = false;
            return Ok(());
        };
        let text = self.message.read(cx).value().to_owned();
        match state.update(cx, |state, cx| state.set_commit_draft(key, text, cx)) {
            Ok(()) => {
                self.unsaved = false;
                Ok(())
            }
            Err(error) => {
                self.notice = Some(Notice::Failed {
                    title: "Commit message not saved",
                    output: error.clone(),
                });
                cx.notify();
                Err(error)
            }
        }
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        if self.committing.is_some() {
            return;
        }
        let Some(root) = self.status().map(|status| status.root.clone()) else {
            return;
        };
        let message = self.message.read(cx).value().to_owned();
        if self.files(Group::Staged).next().is_none() || message.trim().is_empty() {
            return;
        }
        self.notice = None;
        cx.notify();
        self.committing = Some(message.clone());
        self.run(
            move || git::commit(&root, &message),
            |this, result, cx| {
                let message = this.committing.take().unwrap_or_default();
                match result {
                    Ok(()) => {
                        // Text typed while the hooks ran was not committed, so it stays.
                        if this.message.read(cx).value() == message {
                            this.message
                                .update(cx, |input, cx| input.set_value(String::new(), cx));
                            this.draft_task = None;
                            // Unsaved until the store has it, so a failure is retried
                            // at close and quit.
                            this.unsaved = true;
                            let _ = this.save_draft(cx);
                        }
                    }
                    Err(error) => {
                        this.notice = Some(Notice::Failed {
                            title: "Commit failed.",
                            output: error.to_string(),
                        })
                    }
                }
                cx.notify();
            },
            cx,
        );
        self.refresh(cx);
    }

    /// Pushes the branch shown, giving it an upstream when it needs one. Never forces.
    fn push(&mut self, cx: &mut Context<Self>) {
        if self.pushing || self.committing.is_some() {
            return;
        }
        let Some((root, Some(branch))) = self
            .status()
            .map(|status| (status.root.clone(), status.branch.head.clone()))
        else {
            return;
        };
        self.pushing = true;
        self.notice = None;
        cx.notify();
        self.run(
            move || git::push(&root, &branch),
            |this, result, cx| {
                this.pushing = false;
                this.notice = match result {
                    Ok(_) => None,
                    Err(PushError::NoRemote) => Some(Notice::Hint {
                        id: "changes-hint-no-remote",
                        title: "No remote. Add one:".into(),
                        command: "git remote add origin <url>",
                        output: None,
                    }),
                    Err(PushError::UnclearRemote(remotes)) => Some(Notice::Hint {
                        id: "changes-hint-unclear-remote",
                        title: format!("Several remotes ({}). Pick one:", remotes.join(", ")),
                        command: "git config remote.pushDefault <remote>",
                        output: None,
                    }),
                    Err(PushError::Failed(error)) => {
                        let output = error.to_string();
                        let rejected =
                            output.contains("[rejected]") || output.contains("[remote rejected]");
                        Some(Notice::Failed {
                            title: if rejected {
                                "Push rejected."
                            } else {
                                "Push failed."
                            },
                            output,
                        })
                    }
                };
                cx.notify();
            },
            cx,
        );
        self.refresh(cx);
    }

    /// Opens the pull request form, titled after the session, for a published branch
    /// with nothing left to push. Whether gh is signed in shows when it creates.
    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.form.is_some() || self.session.is_none() {
            return;
        }
        let Some(status) = self.status() else {
            return;
        };
        let Some(branch) = status.branch.head.clone() else {
            return;
        };
        if !status.branch.published {
            return;
        }
        if status.branch.unpushed > 0 {
            self.notice = Some(Notice::Info("Push your commits first."));
            return cx.notify();
        }
        if let Err(error) = github::locate() {
            self.notice = Some(Notice::github(error));
            return cx.notify();
        }
        let root = status.root.clone();
        let title = self
            .session
            .zip(self.state.upgrade())
            .and_then(|(id, state)| Some(state.read(cx).session(id)?.title().to_owned()))
            .unwrap_or_default();
        let input = |placeholder: &str, mode, value: String, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut input =
                    TextInput::new(placeholder, mode, cx).text_style(TextSize::Ui, Color::Strong);
                input.set_value(value, cx);
                input
            })
        };
        let form = PullRequestForm {
            session: self.session,
            branch,
            title: input("Title", InputMode::SingleLine, title, cx),
            body: input("Description", InputMode::Multiline, String::new(), cx),
            base: input("Base branch", InputMode::SingleLine, String::new(), cx),
            draft: false,
            busy: None,
        };
        form.title.focus_handle(cx).focus(window, cx);
        let base = form.base.downgrade();
        self.form = Some(form);
        self.notice = None;
        cx.notify();
        self.run(
            move || git::default_branch(&root),
            move |_, default, cx| {
                if let (Some(default), Some(base)) = (default, base.upgrade())
                    && base.read(cx).value().is_empty()
                {
                    base.update(cx, |input, cx| input.set_value(default, cx));
                }
            },
            cx,
        );
    }

    /// `gh pr create` from the form. The new pull request, or the one gh says the
    /// branch already has, is saved on the session the form was opened for, even
    /// when this window has closed by the time gh answers.
    fn create_pull_request(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.status().map(|status| status.root.clone()) else {
            return;
        };
        let Some(form) = &mut self.form else {
            return;
        };
        if form.busy.is_some() {
            return;
        }
        let request = github::Request {
            title: form.title.read(cx).value().trim().to_owned(),
            body: form.body.read(cx).value().to_owned(),
            base: form.base.read(cx).value().trim().to_owned(),
            draft: form.draft,
        };
        if request.title.is_empty() {
            self.notice = Some(Notice::Info("Add a title."));
            return cx.notify();
        }
        form.busy = Some("Creating…");
        let (session, branch) = (form.session, form.branch.clone());
        self.notice = None;
        cx.notify();
        let Some(worker) = &self.worker else {
            return;
        };
        let (sender, receiver) = oneshot::channel();
        worker.run(move || {
            let result = match git::current_branch(&root) {
                Ok(Some(current)) if current == branch => github::create(&root, &request),
                Ok(current) => Err(github::Error::Failed {
                    output: format!(
                        "{} is checked out. Check out {branch} to create this pull request.",
                        current.as_deref().unwrap_or("No branch")
                    ),
                }),
                Err(error) => Err(github::Error::Failed {
                    output: error.to_string(),
                }),
            };
            let _ = sender.send(result);
        });
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let Ok(result) = receiver.await else {
                return;
            };
            let pull_request = match &result {
                Ok(pull_request) | Err(github::Error::Exists(pull_request)) => {
                    Some(pull_request.clone())
                }
                Err(_) => None,
            };
            let saved = match (&pull_request, session) {
                (Some(pull_request), Some(id)) => state
                    .update(cx, |state, cx| {
                        state.set_pull_request(id, pull_request.clone(), cx)
                    })
                    .unwrap_or_else(|_| Err("Keystone is closing.".into())),
                _ => Ok(()),
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(form) = &mut this.form {
                    form.busy = None;
                }
                if pull_request.is_some() {
                    this.form = None;
                }
                this.notice = match (result, saved) {
                    (Ok(_), Ok(())) => None,
                    (Ok(pull_request), Err(output)) => Some(Notice::Failed {
                        title: "Created, but not saved on the session.",
                        output: format!("{}\n{output}", pull_request.url),
                    }),
                    (Err(github::Error::Exists(pull_request)), Err(output)) => {
                        Some(Notice::Failed {
                            title: "Already open, but not saved on the session.",
                            output: format!("{}\n{output}", pull_request.url),
                        })
                    }
                    (Err(error), _) => Some(Notice::github(error)),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Closes the form, unless gh is still working for it.
    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        if self.form.as_ref().is_some_and(|form| form.busy.is_none()) {
            self.form = None;
            cx.notify();
        }
    }

    /// The upstream and its ahead and behind counts, with Push and Create pull request.
    fn sync_row(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let branch = &self.status()?.branch;
        let published = branch.published;
        let state = match (&branch.head, &branch.upstream) {
            (Some(_), Some(upstream)) if published => upstream.clone(),
            (Some(_), _) if !published => "Not pushed".to_owned(),
            _ => String::new(),
        };
        let can_push = branch.head.is_some() && !self.pushing && self.committing.is_none();
        Some(
            h_flex()
                .flex_shrink_0()
                .h(px(30.))
                .pl(px(12.))
                .pr_1()
                .gap_1()
                .child(
                    div()
                        .id("changes-upstream")
                        .automation_id("changes-upstream")
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(state)
                                .size(TextSize::Sm)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .children(
                    branch
                        .upstream
                        .as_ref()
                        .filter(|_| (branch.ahead, branch.behind) != (0, 0))
                        .map(|_| {
                            div()
                                .id("changes-sync")
                                .automation_id("changes-sync")
                                .child(
                                    Label::new(format!("↑{} ↓{}", branch.ahead, branch.behind))
                                        .size(TextSize::Sm)
                                        .color(Color::Muted),
                                )
                        }),
                )
                .child(
                    Button::new(
                        "changes-push",
                        if self.pushing { "Pushing…" } else { "Push" },
                    )
                    .size(ButtonSize::Compact)
                    .disabled(!can_push)
                    .debug_selector(|| "changes-push".into())
                    .on_click(cx.listener(|this, _, _, cx| this.push(cx))),
                )
                .child(
                    IconButton::new("changes-create-pr", IconName::PullRequest)
                        .size(ButtonSize::Compact)
                        .disabled(!published || self.session.is_none() || self.form.is_some())
                        .debug_selector(|| "changes-create-pr".into())
                        .aria_label("Create pull request")
                        .tooltip(Tooltip::text(if self.session.is_none() {
                            "Select a session"
                        } else if published {
                            "Create pull request"
                        } else if branch.head.is_none() {
                            "No branch"
                        } else {
                            "Push first"
                        }))
                        .on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))),
                ),
        )
    }

    fn field(
        id: &'static str,
        input: &Entity<TextInput>,
        multiline: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors;
        let focus = input.clone();
        div()
            .id(id)
            .automation_id(id)
            .w_full()
            .when(multiline, |d| d.min_h(px(64.)).max_h(px(160.)))
            .px(px(8.))
            .py(px(6.))
            .rounded(px(6.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.background)
            .cursor(gpui::CursorStyle::IBeam)
            .on_click(move |_, window, cx| focus.focus_handle(cx).focus(window, cx))
            .child(input.clone())
    }

    fn form(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let form = self.form.as_ref()?;
        let panel = cx.weak_entity();
        Some(
            v_flex()
                .flex_shrink_0()
                .px(px(12.))
                .pb_2()
                .gap_2()
                .automation_id("changes-pr-form")
                .child(Self::field("changes-pr-title", &form.title, false, cx))
                .child(Self::field("changes-pr-body", &form.body, true, cx))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new("Into").size(TextSize::Sm).color(Color::Muted))
                        .child(div().flex_1().min_w_0().child(Self::field(
                            "changes-pr-base",
                            &form.base,
                            false,
                            cx,
                        ))),
                )
                .child(
                    Switch::new("changes-pr-draft", form.draft)
                        .label("Draft")
                        .on_change(move |on, _, cx| {
                            if let Some(panel) = panel.upgrade() {
                                panel.update(cx, |panel, cx| {
                                    if let Some(form) = &mut panel.form {
                                        form.draft = *on;
                                        cx.notify();
                                    }
                                });
                            }
                        }),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("changes-pr-create", form.busy.unwrap_or("Create"))
                                .style(ButtonStyle::Filled)
                                .disabled(form.busy.is_some())
                                .debug_selector(|| "changes-pr-create".into())
                                .tooltip(Tooltip::text(keystone_ui::shortcut_text(
                                    "secondary-enter",
                                )))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.create_pull_request(cx)),
                                ),
                        )
                        .child(
                            Button::new("changes-pr-cancel", "Cancel")
                                .disabled(form.busy.is_some())
                                .debug_selector(|| "changes-pr-cancel".into())
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
                        ),
                ),
        )
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = self.status().map(|status| &status.branch);
        let name = match branch {
            Some(branch) => match (&branch.head, &branch.oid) {
                (Some(head), _) => head.clone(),
                (None, Some(oid)) => format!("Detached at {}", &oid[..oid.len().min(7)]),
                (None, None) => "Detached".into(),
            },
            None => self
                .folder
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        };
        h_flex()
            .flex_shrink_0()
            .h(px(34.))
            .pl(px(12.))
            .pr_1()
            .gap_2()
            .child(
                Icon::new(IconName::Branch)
                    .size(px(16.))
                    .color(Color::Muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .automation_id("changes-branch")
                    .child(
                        Label::new(name)
                            .size(TextSize::Ui)
                            .weight(FontWeight::MEDIUM)
                            .truncate(),
                    ),
            )
            .child(
                IconButton::new("changes-diff", IconName::Diff)
                    .size(ButtonSize::Compact)
                    .debug_selector(|| "changes-diff".into())
                    .aria_label("View uncommitted changes")
                    .tooltip(Tooltip::with_key(
                        "View uncommitted changes",
                        keystone_ui::shortcut_text("secondary-alt-u"),
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.view_diff(None, window, cx))),
            )
            .child(
                IconButton::new("changes-refresh", IconName::Refresh)
                    .size(ButtonSize::Compact)
                    .debug_selector(|| "changes-refresh".into())
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn commit_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let staged = self.files(Group::Staged).count();
        let ready = staged > 0 && !self.message.read(cx).value().trim().is_empty();
        let label = if self.committing.is_some() {
            "Committing…"
        } else {
            "Commit"
        };
        let message = self.message.clone();
        v_flex()
            .flex_shrink_0()
            .px(px(12.))
            .pb_2()
            .gap_2()
            .child(
                div()
                    .id("changes-message")
                    .automation_id("changes-message")
                    .min_h(px(64.))
                    .max_h(px(160.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.background)
                    .cursor(gpui::CursorStyle::IBeam)
                    .on_click(move |_, window, cx| message.focus_handle(cx).focus(window, cx))
                    .child(self.message.clone()),
            )
            .child(
                Button::new("changes-commit", label)
                    .style(ButtonStyle::Filled)
                    .disabled(!ready || self.committing.is_some())
                    .debug_selector(|| "changes-commit".into())
                    .tooltip(Tooltip::text(keystone_ui::shortcut_text("secondary-enter")))
                    .on_click(cx.listener(|this, _, _, cx| this.commit(cx))),
            )
    }

    fn notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let colors = cx.theme().colors;
        let notice = self.notice.as_ref()?;
        let (title, color, output) = match notice {
            Notice::Info(text) => ((*text).into(), Color::Muted, None),
            Notice::AlreadyOpen(number) => {
                (format!("Already open: #{number}."), Color::Muted, None)
            }
            Notice::Failed { title, output } => {
                ((*title).into(), Color::Danger, Some(output.clone()))
            }
            Notice::Hint { title, output, .. } => (title.clone(), Color::Warning, output.clone()),
        };
        let (command, id) = match notice {
            Notice::Hint { command, id, .. } => (Some(*command), *id),
            _ => (None, "changes-notice-title"),
        };
        Some(
            v_flex()
                .flex_shrink_0()
                .mx(px(12.))
                .mb_2()
                .gap_1()
                .automation_id("changes-notice")
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .automation_id(id)
                                .child(Label::new(title).size(TextSize::Ui).color(color)),
                        )
                        .child(
                            IconButton::new("changes-notice-dismiss", IconName::Close)
                                .size(ButtonSize::Compact)
                                .debug_selector(|| "changes-notice-dismiss".into())
                                .tooltip(Tooltip::text("Dismiss"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.notice = None;
                                    cx.notify();
                                })),
                        ),
                )
                .children(command.map(|command| {
                    h_flex()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px_2()
                                .py_1()
                                .rounded(px(6.))
                                .bg(colors.background)
                                .border_1()
                                .border_color(colors.border)
                                .font_mono(cx)
                                .text_size(TextSize::Sm.px())
                                .text_color(colors.text)
                                .automation_id("changes-hint-command")
                                .child(command),
                        )
                        .child(
                            IconButton::new("changes-hint-copy", IconName::Copy)
                                .size(ButtonSize::Compact)
                                .debug_selector(|| "changes-hint-copy".into())
                                .tooltip(Tooltip::text("Copy command"))
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(command.into()))
                                }),
                        )
                }))
                .children(output.filter(|o| !o.trim().is_empty()).map(|output| {
                    div()
                        .id("changes-output")
                        .automation_id("changes-output")
                        .max_h(px(180.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(px(6.))
                        .bg(colors.background)
                        .border_1()
                        .border_color(colors.border)
                        .font_mono(cx)
                        .text_size(TextSize::Sm.px())
                        .text_color(colors.text)
                        .child(output)
                }))
                .into_any_element(),
        )
    }

    fn list(&self, cx: &mut Context<Self>) -> AnyElement {
        let empty = |text: String, id: &'static str| {
            div()
                .flex_1()
                .px(px(12.))
                .py_2()
                .automation_id(id)
                .child(Label::new(text).size(TextSize::Ui).color(Color::Muted))
                .into_any_element()
        };
        match &self.status {
            None => return div().flex_1().into_any_element(),
            Some(Err(error)) => {
                return empty(
                    format!("Could not read changes. {error}"),
                    "changes-unavailable",
                );
            }
            Some(Ok(status)) if self.rows.is_empty() => {
                let text = if status.untracked_hidden {
                    "No changes. Untracked files are hidden by git config."
                } else {
                    "No changes."
                };
                return empty(text.into(), "changes-empty");
            }
            Some(Ok(_)) => {}
        }
        let (rows, panel) = (self.rows.clone(), cx.weak_entity());
        let confirm = {
            let (rows, panel) = (rows.clone(), panel.clone());
            move |index: &usize, _: &mut Window, cx: &mut App| {
                if let (Some(Row::File(file)), Some(panel)) = (rows.get(*index), panel.upgrade()) {
                    panel.update(cx, |panel, cx| panel.open(file, cx));
                }
            }
        };
        let busy = self.committing.is_some();
        div()
            .flex_1()
            .min_h_0()
            .key_context(LIST_CONTEXT)
            .on_action(cx.listener(|this, _: &ToggleStaged, _, cx| {
                if let Some(file) = this.highlighted_file(cx) {
                    this.toggle(file.group, file.paths, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CopyChangePath, _, cx| {
                if let Some(file) = this.highlighted_file(cx) {
                    Self::copy_path(&file, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ViewChanges, window, cx| {
                let file = this.highlighted_file(cx).map(|file| file.shown);
                this.view_diff(file, window, cx);
            }))
            .child(
                VirtualList::new(
                    "changes-list",
                    &self.list,
                    move |index, highlighted, _, cx| {
                        row(&rows[index], index, highlighted, busy, &panel, cx)
                    },
                )
                .on_confirm(confirm)
                .size_full()
                .px(px(12.)),
            )
            .into_any_element()
    }
}

/// Where the highlight goes when `old` rows become `new`: the same file in the same
/// group, else the file now at the old one's place in its group (the next one), else
/// that group's last file, else the same index.
fn follow(old: &[Row], index: usize, new: &[Row]) -> Option<usize> {
    let Some(Row::File(file)) = old.get(index) else {
        return (!new.is_empty()).then(|| index.min(new.len() - 1));
    };
    let same =
        |row: &Row| matches!(row, Row::File(f) if f.group == file.group && f.shown == file.shown);
    if let Some(found) = new.iter().position(same) {
        return Some(found);
    }
    let ordinal = old[..index]
        .iter()
        .filter(|row| matches!(row, Row::File(f) if f.group == file.group))
        .count();
    let group: Vec<usize> = new
        .iter()
        .enumerate()
        .filter(|(_, row)| matches!(row, Row::File(f) if f.group == file.group))
        .map(|(at, _)| at)
        .collect();
    group
        .get(ordinal)
        .or(group.last())
        .copied()
        .or_else(|| (!new.is_empty()).then(|| index.min(new.len() - 1)))
}

fn code_color(code: char) -> Color {
    match code {
        'M' | 'T' => Color::Warning,
        'A' | '?' => Color::Success,
        'D' | '!' => Color::Danger,
        _ => Color::Muted,
    }
}

fn row(
    row: &Row,
    index: usize,
    highlighted: bool,
    busy: bool,
    panel: &WeakEntity<ChangesPanel>,
    cx: &mut App,
) -> AnyElement {
    let colors = cx.theme().colors;
    let base = h_flex()
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_1()
        .gap_2()
        .rounded(radius::CONTROL)
        .when(highlighted, |d| d.bg(colors.element_selected));
    let panel = panel.clone();
    match row {
        Row::Header { group, count } => {
            let group = *group;
            let action = match group {
                Group::Conflicted => None,
                Group::Staged => Some((IconName::Minus, "Unstage all")),
                Group::Unstaged | Group::Untracked => Some((IconName::Plus, "Stage all")),
            };
            base.child(
                div().flex_1().min_w_0().child(
                    Label::new(format!("{} · {count}", group.label()).to_uppercase())
                        .size(TextSize::Xs)
                        .weight(FontWeight::MEDIUM)
                        .color(Color::Faint),
                ),
            )
            .children(action.filter(|_| !busy).map(|(icon, tip)| {
                IconButton::new(("changes-all", index), icon)
                    .size(ButtonSize::Compact)
                    .debug_selector(move || format!("changes-all-{}", group.id()))
                    .tooltip(Tooltip::text(tip))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |panel, cx| panel.toggle_group(group, cx));
                        }
                    })
            }))
            .into_any_element()
        }
        Row::File(file) => {
            let trimmed = file.shown.trim_end_matches('/');
            let (folder, name) = match trimmed.rsplit_once('/') {
                Some((folder, _)) => (folder, &file.shown[folder.len() + 1..]),
                None => ("", file.shown.as_str()),
            };
            let (icon, tip) = match file.group {
                Group::Staged => (IconName::Minus, "Unstage"),
                Group::Conflicted => (IconName::Plus, "Mark resolved"),
                Group::Unstaged | Group::Untracked => (IconName::Plus, "Stage"),
            };
            let (copy, stage, diff) = (file.clone(), file.clone(), file.shown.clone());
            let viewer = panel.clone();
            let selector = format!("changes-stage-{}-{}", file.group.id(), file.shown);
            base.child(
                div().w(px(12.)).flex_shrink_0().child(
                    Label::new(file.code.to_string())
                        .size(TextSize::Xs)
                        .mono()
                        .color(code_color(file.code)),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w_full()
                            .child(Label::new(name.to_owned()).size(TextSize::Ui).truncate()),
                    )
                    .when(!folder.is_empty(), |d| {
                        d.child(
                            div().flex_1().min_w_0().child(
                                Label::new(folder.to_owned())
                                    .size(TextSize::Sm)
                                    .color(Color::Faint)
                                    .truncate(),
                            ),
                        )
                    }),
            )
            .when(highlighted, |d| {
                d.child(
                    IconButton::new(("changes-diff-file", index), IconName::Diff)
                        .size(ButtonSize::Compact)
                        .debug_selector(|| "changes-diff-file".into())
                        .aria_label("View diff")
                        .tooltip(Tooltip::with_key(
                            "View diff",
                            keystone_ui::shortcut_text("secondary-shift-d"),
                        ))
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            if let Some(panel) = viewer.upgrade() {
                                panel.update(cx, |panel, cx| {
                                    panel.view_diff(Some(diff.clone()), window, cx)
                                });
                            }
                        }),
                )
                .child(
                    IconButton::new(("changes-copy", index), IconName::Copy)
                        .size(ButtonSize::Compact)
                        .debug_selector(|| "changes-copy".into())
                        .tooltip(Tooltip::text("Copy path"))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            ChangesPanel::copy_path(&copy, cx);
                        }),
                )
            })
            .when(!busy, |d| {
                d.child(
                    IconButton::new(("changes-stage", index), icon)
                        .size(ButtonSize::Compact)
                        .debug_selector(move || selector)
                        .tooltip(Tooltip::text(tip))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            if let Some(panel) = panel.upgrade() {
                                let File { group, paths, .. } = stage.clone();
                                panel.update(cx, |panel, cx| panel.toggle(group, paths, cx));
                            }
                        }),
                )
            })
            .into_any_element()
        }
    }
}

impl Focusable for ChangesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ChangesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .key_context("Changes")
            .on_action(cx.listener(|this, _: &SendMessage, _, cx| {
                if this.form.is_some() {
                    this.create_pull_request(cx)
                } else {
                    this.commit(cx)
                }
            }))
            .child(self.header(cx))
            .children(self.sync_row(cx))
            .when_some(self.form(cx), |d, form| d.child(form))
            .when(self.form.is_none(), |d| d.child(self.commit_box(cx)))
            .children(self.notice(cx))
            .child(self.list(cx))
    }
}

impl Keystone {
    /// Copies the selected session's pull request link, if it has one.
    pub(crate) fn copy_pull_request_link(&self, cx: &mut Context<Self>) {
        let url = self
            .selected(cx)
            .and_then(|session| self.state.read(cx).session(session)?.pull_request())
            .map(|pull_request| pull_request.url.clone());
        if let Some(url) = url {
            cx.write_to_clipboard(ClipboardItem::new_string(url));
        }
    }

    /// The selected session's pull request as `#N` in the top bar, opening it in the
    /// browser.
    pub(crate) fn pull_request_link(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let session = self.selected(cx)?;
        let pull_request = self
            .state
            .read(cx)
            .session(session)?
            .pull_request()?
            .clone();
        let hover = cx.theme().colors.element_hover;
        let url = pull_request.url.clone();
        Some(
            self.own_press(
                h_flex()
                    .id("session-pull-request")
                    .automation_id("session-pull-request")
                    .flex_shrink_0()
                    .px(px(6.))
                    .py(px(2.))
                    .gap_1()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(px(16.))
                            .color(Color::Muted),
                    )
                    .child(Label::new(format!("#{}", pull_request.number)).color(Color::Muted))
                    .tooltip(Tooltip::text(pull_request.url).builder())
                    .on_click(move |_, _, cx| cx.open_url(&url)),
                cx,
            )
            .into_any_element(),
        )
    }

    pub(crate) fn changes_panel(&self, cx: &mut Context<Self>) -> Option<Entity<ChangesPanel>> {
        let folder = self.files_root(cx).ok()?;
        Some(self.changes.panel(&folder, &self.state, cx))
    }

    /// Refreshes the shown folder's changes and puts focus in its message box.
    pub(crate) fn focus_changes(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .shown_composition(cx)
            .views()
            .any(|(_, component)| component == Component::Changes)
        {
            return;
        }
        if let Some(panel) = self.changes_panel(cx) {
            panel.update(cx, |panel, cx| {
                panel.refresh(cx);
                panel.message.focus_handle(cx).focus(window, cx);
            });
        }
    }

    /// Re-reads the open Changes view's folder, for when the window comes back into
    /// focus.
    pub(crate) fn refresh_changes(&self, cx: &mut App) {
        if !self
            .shown_composition(cx)
            .views()
            .any(|(_, component)| component == Component::Changes)
        {
            return;
        }
        let panel = self
            .files_root(cx)
            .ok()
            .and_then(|folder| self.changes.panels.borrow().get(&folder).cloned());
        if let Some(panel) = panel {
            panel.update(cx, |panel, cx| panel.refresh(cx));
        }
    }

    /// Re-reads the changes of each session folder an agent may just have written to.
    pub(crate) fn agent_changed_files(&self, sessions: &[u64], cx: &mut App) {
        for &session in sessions {
            if let Some(crate::presentation::ConversationFolder::Recorded(directory)) =
                self.conversation_folder(session, cx)
            {
                self.changes.refresh(directory.path(), cx);
            }
        }
    }

    /// Closes the Changes view when it has focus, brings an open one forward and
    /// focuses it, or opens one: as a tab with Files when Files is open, else to the
    /// right of the conversation, as Files opens.
    pub(crate) fn toggle_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.shown_composition(cx);
        if let Some((changes, _)) = shown.views().find(|(_, c)| *c == Component::Changes) {
            if self.view_contains_focus(changes, window, cx) {
                return self.edit_surfaces(Edit::Close(changes), window, cx);
            }
            self.reveal_view(changes, window, cx);
            return self.focus_changes(window, cx);
        }
        let at = match shown.views().find(|(_, c)| *c == Component::Files) {
            Some((files, _)) => Place::TabWith(files),
            None => Place::Beside {
                target: self.side_surface_target(cx),
                axis: Axis::Row,
                after: true,
            },
        };
        self.open_surface(Component::Changes, at, FILES_WIDTH, window, cx);
        if let Some((changes, _)) = self
            .shown_composition(cx)
            .views()
            .find(|(_, c)| *c == Component::Changes)
        {
            self.reveal_view(changes, window, cx);
            self.focus_changes(window, cx);
        }
    }

    pub(crate) fn changes_list(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.files_root(cx) {
            Ok(folder) => {
                let known = self.changes.panels.borrow().contains_key(&folder);
                let panel = self.changes.panel(&folder, &self.state, cx);
                let session = self.selected(cx);
                panel.update(cx, |panel, cx| {
                    if panel
                        .form
                        .as_ref()
                        .is_some_and(|form| form.session != session && form.busy.is_none())
                    {
                        panel.form = None;
                        cx.notify();
                    }
                    panel.session = session;
                });
                if self.changes.shown.borrow().as_ref() != Some(&folder) {
                    *self.changes.shown.borrow_mut() = Some(folder);
                    if known {
                        panel.update(cx, |panel, cx| panel.refresh(cx));
                    }
                }
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(panel)
                    .into_any_element()
            }
            Err(error) => div()
                .flex_1()
                .px(px(14.))
                .py_2()
                .automation_id("changes-unavailable")
                .child(
                    Label::new(format!("No project folder. {error}"))
                        .size(TextSize::Ui)
                        .color(Color::Muted),
                )
                .into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(path: &str, staged: char, unstaged: char) -> Entry {
        Entry::Tracked {
            path: path.into(),
            original: None,
            staged,
            unstaged,
        }
    }

    #[test]
    fn rows_group_conflicts_staged_changes_and_untracked_with_counts() {
        let status = Status {
            root: "/repo".into(),
            entries: vec![
                tracked("both.rs", 'M', 'M'),
                Entry::Tracked {
                    path: "src/new.rs".into(),
                    original: Some("src/old.rs".into()),
                    staged: 'R',
                    unstaged: '.',
                },
                tracked("gone.rs", '.', 'D'),
                Entry::Conflicted {
                    path: "fight.rs".into(),
                    state: "UU".into(),
                },
                Entry::Untracked {
                    path: "notes/".into(),
                },
            ],
            ..Status::default()
        };
        let shown: Vec<String> = rows(&status)
            .iter()
            .map(|row| match row {
                Row::Header { group, count } => format!("{} · {count}", group.label()),
                Row::File(file) => format!(
                    "{} {} {:?} {}",
                    file.code,
                    file.shown,
                    file.paths,
                    file.absolute.display()
                ),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "Conflicts · 1",
                "! fight.rs [\"fight.rs\"] /repo/fight.rs",
                "Staged · 2",
                "M both.rs [\"both.rs\"] /repo/both.rs",
                "R src/new.rs [\"src/new.rs\", \"src/old.rs\"] /repo/src/new.rs",
                "Changes · 2",
                "M both.rs [\"both.rs\"] /repo/both.rs",
                "D gone.rs [\"gone.rs\"] /repo/gone.rs",
                "Untracked · 1",
                "? notes/ [\"notes/\"] /repo/notes/",
            ]
        );
        assert!(rows(&Status::default()).is_empty());
    }

    #[test]
    fn unstaging_a_copy_leaves_its_source_alone() {
        let status = Status {
            root: "/repo".into(),
            entries: vec![
                tracked("a.rs", 'M', '.'),
                Entry::Tracked {
                    path: "b.rs".into(),
                    original: Some("a.rs".into()),
                    staged: 'C',
                    unstaged: '.',
                },
            ],
            ..Status::default()
        };
        let paths: Vec<_> = rows(&status)
            .into_iter()
            .filter_map(|row| match row {
                Row::File(file) => Some(file.paths),
                Row::Header { .. } => None,
            })
            .collect();
        assert_eq!(paths, [vec![PathBuf::from("a.rs")], vec!["b.rs".into()]]);
    }

    #[test]
    fn the_highlight_follows_its_file_or_takes_the_next_one_in_its_group() {
        let status = |entries| Status {
            root: "/repo".into(),
            entries,
            ..Status::default()
        };
        let before = rows(&status(vec![
            tracked("a", '.', 'M'),
            tracked("b", '.', 'M'),
            tracked("c", '.', 'M'),
        ]));
        let after = rows(&status(vec![
            tracked("a", '.', 'M'),
            tracked("b", 'M', '.'),
            tracked("c", '.', 'M'),
        ]));
        // Changes · 3, a, b, c  ->  Staged · 1, b, Changes · 2, a, c
        assert_eq!(
            follow(&before, 2, &after),
            Some(4),
            "b was staged: c is next"
        );
        assert_eq!(follow(&before, 1, &after), Some(3), "a stays highlighted");
        assert_eq!(follow(&before, 3, &after), Some(4), "c stays highlighted");
        let none = rows(&status(Vec::new()));
        assert_eq!(follow(&before, 2, &none), None);
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod ui_tests {
    use super::*;
    use keystone_agent::{Event, Item, ItemKind, ItemStatus};
    use keystone_core::{Provider, PullRequest, Store, WorkingDirectory};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::Instant;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// A repository with `kept.txt`, `edited.txt` and `removed.txt` committed, then
    /// `edited.txt` changed, `removed.txt` deleted and `added.txt` new.
    fn fixture(repo: &Path) {
        git(repo, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", ".git/hooks"),
        ] {
            git(repo, &["config", key, value]);
        }
        for file in ["kept.txt", "edited.txt", "removed.txt"] {
            fs::write(repo.join(file), "one\n").unwrap();
        }
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "first"]);
        fs::write(repo.join("edited.txt"), "two\n").unwrap();
        fs::remove_file(repo.join("removed.txt")).unwrap();
        fs::write(repo.join("added.txt"), "new\n").unwrap();
    }

    fn open_keystone<'a>(
        project: &Path,
        state: &Path,
        cx: &'a mut gpui::TestAppContext,
    ) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
        let mut store = Store::open(state.join("workspace.json")).unwrap();
        if store.workspace().sessions().is_empty() {
            let directory = WorkingDirectory::new(project).unwrap();
            store.create(Provider::Claude, &directory, 1).unwrap();
            let shown = keystone_core::ProjectId::new(project).unwrap();
            store
                .open_project(keystone_core::WindowId::FIRST, &shown, None, Some(1), 1)
                .unwrap();
        }
        cx.update(keystone_ui::init);
        cx.update(crate::bind_keys);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update_in(cx, |this, window, cx| this.focus.focus(window, cx));
        cx.run_until_parked();
        (view, cx)
    }

    fn panel(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Entity<ChangesPanel> {
        view.read_with(cx, |view, cx| {
            view.changes.panels.borrow()[&view.files_root(cx).unwrap()].clone()
        })
    }

    /// The list as shown: headers and `code path` rows.
    fn shown(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        panel(view, cx).read_with(cx, |panel, _| {
            panel
                .rows
                .iter()
                .map(|row| match row {
                    Row::Header { group, count } => format!("{} · {count}", group.label()),
                    Row::File(file) => format!("{} {}", file.code, file.shown),
                })
                .collect()
        })
    }

    /// Runs the app until git's thread has answered and `done` holds.
    fn until(
        cx: &mut gpui::VisualTestContext,
        what: &str,
        mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool,
    ) {
        let start = Instant::now();
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out: {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn settled(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> bool {
        panel(view, cx).read_with(cx, |panel, _| {
            panel.reading.is_none() && panel.committing.is_none() && panel.status.is_some()
        })
    }

    fn click(cx: &mut gpui::VisualTestContext, selector: &str) {
        let bounds = cx
            .debug_bounds(selector.to_owned().leak())
            .unwrap_or_else(|| panic!("{selector} not rendered"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    fn notice(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Option<Notice> {
        panel(view, cx).read_with(cx, |panel, _| panel.notice.clone())
    }

    #[gpui::test]
    fn changes_lists_stages_and_commits_with_the_users_git(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        fixture(repo);
        let (view, cx) = open_keystone(repo, state.path(), cx);

        cx.simulate_keystrokes("secondary-shift-g");
        until(cx, "first status", |cx| settled(&view, cx));
        assert!(
            cx.debug_bounds("changes-tab-0").is_some(),
            "a lone Changes tab"
        );
        assert_eq!(
            shown(&view, cx),
            [
                "Changes · 2",
                "M edited.txt",
                "D removed.txt",
                "Untracked · 1",
                "? added.txt"
            ]
        );
        let label = view.read_with(cx, |view, cx| {
            view.changes.label(view.files_root(cx).ok().as_deref(), cx)
        });
        assert_eq!(label, "Changes · 3");
        assert!(
            cx.debug_bounds("changes-tab-0")
                .is_some_and(|_| cx.debug_bounds("changes-branch").is_some())
        );

        click(cx, "changes-stage-unstaged-edited.txt");
        until(cx, "staged", |cx| settled(&view, cx));
        assert_eq!(
            git(repo, &["diff", "--cached", "--name-only"]),
            "edited.txt\n"
        );
        assert_eq!(
            shown(&view, cx),
            [
                "Staged · 1",
                "M edited.txt",
                "Changes · 1",
                "D removed.txt",
                "Untracked · 1",
                "? added.txt"
            ]
        );

        click(cx, "changes-message");
        cx.simulate_input("verify commit");
        cx.run_until_parked();
        let draft = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| {
                let key = view.files_root(cx).unwrap();
                view.state.read(cx).commit_draft(key.to_str().unwrap())
            })
        };
        assert_eq!(draft(&view, cx), "", "saving waits for typing to pause");
        cx.executor().advance_clock(DRAFT_DELAY);
        cx.run_until_parked();
        assert_eq!(draft(&view, cx), "verify commit");

        cx.simulate_keystrokes("secondary-enter");
        until(cx, "committed", |cx| settled(&view, cx));
        assert_eq!(git(repo, &["log", "-1", "--format=%s"]), "verify commit\n");
        assert_eq!(
            git(repo, &["show", "--name-only", "--format=", "HEAD"]),
            "edited.txt\n",
            "only the staged file is committed"
        );
        assert_eq!(
            notice(&view, cx),
            None,
            "the cleared box and list show the commit"
        );
        assert_eq!(
            shown(&view, cx),
            [
                "Changes · 1",
                "D removed.txt",
                "Untracked · 1",
                "? added.txt"
            ]
        );
        assert_eq!(
            panel(&view, cx).read_with(cx, |panel, cx| panel.message.read(cx).value().to_owned()),
            ""
        );
        assert_eq!(draft(&view, cx), "", "a commit clears the saved message");
        let sent = view.read_with(cx, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| !c.rows.is_empty())
        });
        assert!(
            !sent,
            "Ctrl+Enter in the message box sends nothing to the agent"
        );

        click(cx, "changes-all-untracked");
        until(cx, "staged all untracked", |cx| settled(&view, cx));
        click(cx, "changes-all-unstaged");
        until(cx, "staged all changes", |cx| settled(&view, cx));
        assert_eq!(
            shown(&view, cx),
            ["Staged · 2", "A added.txt", "D removed.txt"]
        );
        click(cx, "changes-all-staged");
        until(cx, "unstaged all", |cx| settled(&view, cx));
        assert_eq!(
            shown(&view, cx),
            [
                "Changes · 1",
                "D removed.txt",
                "Untracked · 1",
                "? added.txt"
            ]
        );
    }

    #[gpui::test]
    fn a_failing_hook_shows_its_output_and_keeps_the_message(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        fixture(repo);
        let hook = repo.join(".git/hooks/pre-commit");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(
            &hook,
            "#!/bin/sh\necho 'pre-commit: lint failed in edited.txt' >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let (view, cx) = open_keystone(repo, state.path(), cx);

        cx.simulate_keystrokes("secondary-shift-g");
        until(cx, "first status", |cx| settled(&view, cx));
        cx.simulate_keystrokes("secondary-enter");
        assert_eq!(
            notice(&view, cx),
            None,
            "nothing staged: Ctrl+Enter does nothing"
        );
        click(cx, "changes-stage-unstaged-edited.txt");
        until(cx, "staged", |cx| settled(&view, cx));
        cx.simulate_keystrokes("secondary-enter");
        assert_eq!(
            notice(&view, cx),
            None,
            "no message: Ctrl+Enter does nothing"
        );
        assert_eq!(git(repo, &["log", "-1", "--format=%s"]), "first\n");
        click(cx, "changes-message");
        cx.simulate_input("verify commit");
        cx.simulate_keystrokes("secondary-enter");
        until(cx, "commit failed", |cx| settled(&view, cx));
        assert_eq!(
            notice(&view, cx),
            Some(Notice::Failed {
                title: "Commit failed.",
                output: "pre-commit: lint failed in edited.txt".into(),
            })
        );
        assert!(cx.debug_bounds("changes-output").is_some());
        assert_eq!(git(repo, &["log", "-1", "--format=%s"]), "first\n");
        assert_eq!(
            panel(&view, cx).read_with(cx, |panel, cx| panel.message.read(cx).value().to_owned()),
            "verify commit"
        );
        click(cx, "changes-notice-dismiss");
        assert!(cx.debug_bounds("changes-output").is_none());
    }

    #[gpui::test]
    fn clearing_the_message_after_a_commit_is_retried_when_its_save_fails(
        cx: &mut gpui::TestAppContext,
    ) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        fixture(repo);
        {
            let (view, cx) = open_keystone(repo, state.path(), cx);
            cx.simulate_keystrokes("secondary-shift-g");
            until(cx, "first status", |cx| settled(&view, cx));
            click(cx, "changes-stage-unstaged-edited.txt");
            until(cx, "staged", |cx| settled(&view, cx));
            click(cx, "changes-message");
            cx.simulate_input("saved then committed");
            view.update(cx, |view, cx| view.changes.save_drafts(cx))
                .unwrap();

            fs::set_permissions(state.path(), fs::Permissions::from_mode(0o555)).unwrap();
            cx.simulate_keystrokes("secondary-enter");
            until(cx, "committed", |cx| settled(&view, cx));
            assert_eq!(
                git(repo, &["log", "-1", "--format=%s"]),
                "saved then committed\n"
            );
            let app = view.read_with(cx, |view, _| view.state.clone());
            crate::tests::settle(&app, cx);
            assert!(
                view.read_with(cx, |view, cx| view.status_line(cx))
                    .is_some_and(|(error, _)| error.starts_with("Not saved."))
            );
            assert!(app.read_with(cx, |app, _| app.store().flush()).is_err());

            fs::set_permissions(state.path(), fs::Permissions::from_mode(0o755)).unwrap();
            view.update(cx, |view, cx| view.changes.save_drafts(cx))
                .unwrap();
            crate::tests::settle(&app, cx);
        }
        let reopened = Store::open(state.path().join("workspace.json")).unwrap();
        let key = WorkingDirectory::new(repo).unwrap();
        assert_eq!(reopened.workspace().commit_draft(key.as_str()), "");
    }

    #[gpui::test]
    fn the_message_survives_a_restart_and_agent_file_changes_refresh(
        cx: &mut gpui::TestAppContext,
    ) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        fixture(repo);
        {
            let (view, cx) = open_keystone(repo, state.path(), cx);
            cx.simulate_keystrokes("secondary-shift-g");
            until(cx, "first status", |cx| settled(&view, cx));
            cx.simulate_input("half written");
            view.update(cx, |view, cx| view.changes.save_drafts(cx))
                .unwrap();

            fs::write(repo.join("agent.txt"), "from the agent\n").unwrap();
            let item = Item {
                id: "edit-1".into(),
                kind: ItemKind::FileChange {
                    paths: vec!["agent.txt".into()],
                },
                text: String::new(),
                status: ItemStatus::Completed,
            };
            view.update(cx, |view, cx| {
                view.receive(Vec::new(), vec![(1, Event::ItemCompleted(item))], cx)
            });
            until(cx, "agent refresh", |cx| {
                settled(&view, cx) && shown(&view, cx).contains(&"? agent.txt".to_owned())
            });

            fs::create_dir(repo.join("sub")).unwrap();
            fs::write(
                repo.join("sub/deeper.txt"),
                "from an agent in a subfolder\n",
            )
            .unwrap();
            let sub = WorkingDirectory::new(&repo.join("sub")).unwrap();
            view.update(cx, |view, cx| view.changes.refresh(sub.path(), cx));
            until(
                cx,
                "a change in a subfolder refreshes the repository's panel",
                |cx| {
                    settled(&view, cx) && shown(&view, cx).contains(&"? sub/deeper.txt".to_owned())
                },
            );
        }
        let reopened = Store::open(state.path().join("workspace.json")).unwrap();
        let key = WorkingDirectory::new(repo).unwrap();
        assert_eq!(
            reopened.workspace().commit_draft(key.as_str()),
            "half written"
        );
    }

    #[gpui::test]
    fn the_surface_menu_and_palette_open_changes_and_focus_closes_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fixture(project.path());
        let (view, cx) = open_keystone(project.path(), state.path(), cx);
        cx.simulate_keystrokes("secondary-shift-e");
        cx.run_until_parked();
        click(cx, "surface-add");
        click(cx, "surface-add-changes");
        until(cx, "first status", |cx| settled(&view, cx));
        assert!(
            cx.debug_bounds("changes-message").is_some(),
            "Changes opens as a tab beside Files"
        );
        assert!(
            cx.debug_bounds("files-tab-0").is_none(),
            "Files is no longer alone"
        );
        click(cx, "surface-add");
        assert!(
            cx.debug_bounds("surface-add-changes").is_none(),
            "only one Changes view"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        cx.simulate_keystrokes("secondary-shift-g");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("changes-message").is_some(),
            "unfocused, the shortcut focuses Changes"
        );
        cx.simulate_keystrokes("secondary-shift-g");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("changes-message").is_none(),
            "with its message box focused, the shortcut closes Changes"
        );
        cx.simulate_keystrokes("secondary-k");
        cx.run_until_parked();
        cx.simulate_input("toggle changes");
        cx.simulate_keystrokes("enter");
        until(cx, "reopened", |cx| settled(&view, cx));
        assert!(
            cx.debug_bounds("changes-message").is_some(),
            "the palette opens it"
        );
    }

    #[gpui::test]
    fn the_keyboard_stages_and_copies_and_text_typed_during_a_commit_stays(
        cx: &mut gpui::TestAppContext,
    ) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        fixture(repo);
        let hook = repo.join(".git/hooks/pre-commit");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(&hook, "#!/bin/sh\nsleep 0.5\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let (view, cx) = open_keystone(repo, state.path(), cx);
        cx.simulate_keystrokes("secondary-shift-g");
        until(cx, "first status", |cx| settled(&view, cx));

        let list = panel(&view, cx).read_with(cx, |panel, cx| panel.list.read(cx).focus_handle(cx));
        cx.update(|window, cx| window.focus(&list, cx));
        cx.simulate_keystrokes("down");
        assert_eq!(
            panel(&view, cx).read_with(cx, |panel, cx| panel.highlighted_file(cx).map(|f| f.shown)),
            Some("edited.txt".into())
        );
        cx.simulate_keystrokes("secondary-shift-c");
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        let edited = WorkingDirectory::new(repo)
            .unwrap()
            .path()
            .join("edited.txt");
        assert_eq!(copied.as_deref(), edited.to_str());

        cx.simulate_keystrokes("space");
        until(cx, "staged", |cx| settled(&view, cx));
        assert_eq!(
            git(repo, &["diff", "--cached", "--name-only"]),
            "edited.txt\n"
        );
        assert_eq!(
            panel(&view, cx).read_with(cx, |panel, cx| panel.highlighted_file(cx).map(|f| f.shown)),
            Some("removed.txt".into()),
            "the highlight moves on to the next change, not onto the staged file"
        );

        click(cx, "changes-message");
        cx.simulate_input("verify commit");
        cx.simulate_keystrokes("secondary-enter");
        cx.run_until_parked();
        cx.simulate_input(" and more");
        until(cx, "committed", |cx| settled(&view, cx));
        assert_eq!(git(repo, &["log", "-1", "--format=%s"]), "verify commit\n");
        assert_eq!(
            panel(&view, cx).read_with(cx, |panel, cx| panel.message.read(cx).value().to_owned()),
            "verify commit and more",
            "text typed while the hook ran is not cleared"
        );
    }

    #[gpui::test]
    fn windows_share_one_folders_commit_message(cx: &mut gpui::TestAppContext) {
        let (project, state_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        fixture(repo);
        let (first, cx) = open_keystone(repo, state_dir.path(), cx);
        let app_state = first.read_with(cx, |view, _| view.state.clone());
        cx.simulate_keystrokes("secondary-shift-g");
        until(cx, "first status", |cx| settled(&first, cx));
        click(cx, "changes-message");
        cx.simulate_input("shared");
        cx.executor().advance_clock(DRAFT_DELAY);
        cx.run_until_parked();

        let id = cx.update(|_, cx| {
            app_state.update(cx, |state, _| {
                let project = keystone_core::ProjectId::new(repo).unwrap();
                state
                    .open_window(Some(&project), Some(1), None)
                    .unwrap()
                    .value
            })
        });
        let handle = cx.update(|_, cx| {
            cx.open_window(Default::default(), |window, cx| {
                cx.new(|cx| Keystone::in_window(app_state.clone(), id, false, false, window, cx))
            })
            .unwrap()
        });
        let second = cx.update(|_, cx| handle.entity(cx).unwrap());
        let message = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| {
                let folder = view.files_root(cx).unwrap();
                let panel = view.changes.panels.borrow().get(&folder).cloned();
                panel.map(|panel| panel.read(cx).message.read(cx).value().to_owned())
            })
        };
        let mut other = gpui::VisualTestContext::from_window(handle.into(), cx);
        other.update(|window, cx| {
            second.update(cx, |view, cx| {
                view.focus.focus(window, cx);
                view.toggle_changes(window, cx)
            })
        });
        until(&mut other, "second window status", |cx| {
            message(&second, cx).is_some() && settled(&second, cx)
        });
        assert_eq!(
            message(&second, &mut other).as_deref(),
            Some("shared"),
            "a new panel starts from the saved message"
        );

        cx.simulate_input(" message");
        cx.executor().advance_clock(DRAFT_DELAY);
        cx.run_until_parked();
        assert_eq!(
            message(&second, &mut other).as_deref(),
            Some("shared message"),
            "a saved edit in one window shows in the other"
        );

        click(cx, "changes-stage-unstaged-edited.txt");
        until(cx, "staged", |cx| settled(&first, cx));
        click(cx, "changes-message");
        cx.simulate_keystrokes("secondary-enter");
        until(cx, "committed", |cx| settled(&first, cx));
        assert_eq!(git(repo, &["log", "-1", "--format=%s"]), "shared message\n");
        other.run_until_parked();
        assert_eq!(
            message(&second, &mut other).as_deref(),
            Some(""),
            "a commit clears the message in every window"
        );
    }

    #[gpui::test]
    fn a_conflict_still_marked_is_not_staged_as_resolved(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        fixture(repo);
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "second"]);
        git(repo, &["checkout", "-q", "-b", "other"]);
        fs::write(repo.join("kept.txt"), "theirs\n").unwrap();
        git(repo, &["commit", "-q", "-am", "theirs"]);
        git(repo, &["checkout", "-q", "main"]);
        fs::write(repo.join("kept.txt"), "ours\n").unwrap();
        git(repo, &["commit", "-q", "-am", "ours"]);
        let merge = Command::new("git")
            .current_dir(repo)
            .args(["merge", "-q", "other"])
            .output()
            .unwrap();
        assert!(!merge.status.success());
        let (view, cx) = open_keystone(repo, state.path(), cx);
        cx.simulate_keystrokes("secondary-shift-g");
        until(cx, "first status", |cx| settled(&view, cx));
        assert_eq!(shown(&view, cx), ["Conflicts · 1", "! kept.txt"]);

        click(cx, "changes-stage-conflicted-kept.txt");
        until(cx, "refused", |cx| {
            settled(&view, cx) && notice(&view, cx).is_some()
        });
        assert_eq!(
            notice(&view, cx),
            Some(Notice::Failed {
                title: "Couldn't stage",
                output: "kept.txt still has conflict markers.".into(),
            })
        );
        assert_eq!(shown(&view, cx), ["Conflicts · 1", "! kept.txt"]);

        fs::write(repo.join("kept.txt"), "both\n").unwrap();
        click(cx, "changes-stage-conflicted-kept.txt");
        until(cx, "resolved", |cx| {
            settled(&view, cx) && shown(&view, cx) == ["Staged · 1", "M kept.txt"]
        });
        assert_eq!(notice(&view, cx), None, "the stale failure is gone");
    }

    /// Puts a stand-in `gh` first on `PATH`, once for the whole process. It runs the
    /// repository's own `.git/fake-gh` when there is one, so each test scripts its
    /// own, and otherwise acts signed out, as gh does with no login. It never reaches
    /// GitHub or the user's gh login.
    fn fake_gh() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join(format!("keystone-fake-gh-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let gh = dir.join("gh");
            fs::write(
                &gh,
                "#!/bin/sh\n\
                 [ -x \"$PWD/.git/fake-gh\" ] && exec \"$PWD/.git/fake-gh\" \"$@\"\n\
                 echo 'To get started with GitHub CLI, please run:  gh auth login' >&2\n\
                 exit 4\n",
            )
            .unwrap();
            fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
            let path = std::env::var_os("PATH").unwrap_or_default();
            let path =
                std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&path)))
                    .unwrap();
            // SAFETY: as for the scripted providers; every test only adds to PATH.
            unsafe { std::env::set_var("PATH", path) };
        });
    }

    /// This repository's gh: it records each call's arguments, one per line, then
    /// runs `script`. It replaces the old script by renaming over it, as a gh call
    /// still running it would make writing to it fail.
    fn script_gh(repo: &Path, script: &str) {
        let path = repo.join(".git/fake-gh.new");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> .git/fake-gh-argv\n[ \"$1\" = pr ] && {{ cat; echo; }} >> .git/fake-gh-argv\necho --- >> .git/fake-gh-argv\n{script}"
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&path, repo.join(".git/fake-gh")).unwrap();
    }

    /// `fixture` with its first commit on a bare `origin`, whose HEAD is `main`, and
    /// `feature` checked out with one commit more.
    fn with_remote(repo: &Path) -> tempfile::TempDir {
        fixture(repo);
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        git(
            repo,
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        git(repo, &["push", "-q", "origin", "main"]);
        git(repo, &["remote", "set-head", "origin", "main"]);
        git(repo, &["switch", "-q", "-c", "feature"]);
        git(repo, &["commit", "-q", "-am", "feature work"]);
        remote
    }

    fn settled_all(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> bool {
        settled(view, cx)
            && panel(view, cx).read_with(cx, |panel, _| {
                !panel.pushing && panel.form.as_ref().is_none_or(|form| form.busy.is_none())
            })
    }

    fn upstream(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Option<String> {
        panel(view, cx).read_with(cx, |panel, _| panel.status()?.branch.upstream.clone())
    }

    fn form_value(
        view: &Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
        field: fn(&PullRequestForm) -> &Entity<TextInput>,
    ) -> Option<String> {
        panel(view, cx).read_with(cx, |panel, cx| {
            Some(field(panel.form.as_ref()?).read(cx).value().to_owned())
        })
    }

    fn saved_pull_request(
        view: &Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
    ) -> Option<PullRequest> {
        view.read_with(cx, |view, cx| {
            view.state.read(cx).session(1)?.pull_request().cloned()
        })
    }

    #[gpui::test]
    fn push_sets_the_upstream_and_a_new_pull_request_is_saved_on_the_session(
        cx: &mut gpui::TestAppContext,
    ) {
        fake_gh();
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        let remote = with_remote(repo);
        let (view, cx) = open_keystone(repo, state.path(), cx);
        cx.simulate_keystrokes("secondary-shift-g");
        until(cx, "first status", |cx| settled_all(&view, cx));
        assert_eq!(upstream(&view, cx), None);
        click(cx, "changes-create-pr");
        assert!(
            cx.debug_bounds("changes-pr-form").is_none(),
            "no pull request before the branch is pushed"
        );

        click(cx, "changes-push");
        until(cx, "pushed", |cx| settled_all(&view, cx));
        assert_eq!(
            git(remote.path(), &["rev-parse", "feature"]),
            git(repo, &["rev-parse", "HEAD"])
        );
        assert_eq!(upstream(&view, cx).as_deref(), Some("origin/feature"));
        assert_eq!(notice(&view, cx), None, "the upstream shows the push");

        script_gh(
            repo,
            "[ \"$1\" = auth ] && exit 0\n\
             echo 'Warning: 2 uncommitted changes' >&2\n\
             echo https://github.com/owner/repo/pull/12\n",
        );
        click(cx, "changes-create-pr");
        until(cx, "gh checked and the base filled", |cx| {
            settled_all(&view, cx)
                && form_value(&view, cx, |form| &form.base).is_some_and(|base| !base.is_empty())
        });
        assert_eq!(
            form_value(&view, cx, |form| &form.title).as_deref(),
            Some("New Claude session"),
            "the title starts as the session's"
        );
        assert_eq!(
            form_value(&view, cx, |form| &form.base).as_deref(),
            Some("main"),
            "the base starts as the remote's default branch"
        );
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("Push from Changes");
        click(cx, "changes-pr-body");
        cx.simulate_input("Adds a push button.");
        click(cx, "changes-pr-draft");
        cx.simulate_keystrokes("secondary-enter");
        until(cx, "created", |cx| {
            settled_all(&view, cx)
                && panel(&view, cx).read_with(cx, |panel, _| panel.form.is_none())
        });
        let argv = fs::read_to_string(repo.join(".git/fake-gh-argv")).unwrap();
        let create = "pr\ncreate\n--title\nPush from Changes\n--body-file\n-\n--base\nmain\n--draft\nAdds a push button.\n---\n";
        assert!(argv.starts_with(create), "{argv}");
        assert!(
            argv[create.len()..]
                .split_terminator("---\n")
                .all(|call| {
                    // A report reads the pull request, then its line comments.
                    call.starts_with("pr\nview\nhttps://github.com/owner/repo/pull/12\n")
                        || call.starts_with("api\n--hostname\ngithub.com\n--paginate\nrepos/owner/repo/pulls/12/comments")
                }),
            "then gh is only asked about the new pull request: {argv}"
        );
        let created = PullRequest {
            number: 12,
            url: "https://github.com/owner/repo/pull/12".into(),
        };
        assert_eq!(notice(&view, cx), None, "the top bar shows it");
        assert_eq!(saved_pull_request(&view, cx), Some(created.clone()));
        assert!(
            cx.debug_bounds("session-pull-request").is_some(),
            "the top bar shows the session's pull request"
        );
        view.update_in(cx, |this, window, cx| this.focus.focus(window, cx));
        cx.simulate_keystrokes("secondary-alt-c");
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())),
            Some(created.url),
            "Copy pull request link copies the session's pull request"
        );
        assert_eq!(
            git(repo, &["log", "-1", "--format=%s"]),
            "feature work\n",
            "Ctrl+Enter in the form commits nothing"
        );

        script_gh(
            repo,
            "[ \"$1\" = auth ] && exit 0\n\
             echo 'a pull request for branch \"feature\" into branch \"main\" already exists:' >&2\n\
             echo https://github.com/owner/repo/pull/7 >&2\n\
             exit 1\n",
        );
        click(cx, "changes-create-pr");
        until(cx, "gh checked again", |cx| settled_all(&view, cx));
        click(cx, "changes-pr-create");
        until(cx, "found the existing one", |cx| {
            settled_all(&view, cx)
                && panel(&view, cx).read_with(cx, |panel, _| panel.form.is_none())
        });
        let existing = PullRequest {
            number: 7,
            url: "https://github.com/owner/repo/pull/7".into(),
        };
        assert_eq!(notice(&view, cx), Some(Notice::AlreadyOpen(7)));
        assert_eq!(saved_pull_request(&view, cx), Some(existing));
    }

    #[gpui::test]
    fn a_missing_remote_a_rejected_push_and_a_signed_out_gh_say_what_to_do(
        cx: &mut gpui::TestAppContext,
    ) {
        fake_gh();
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        fixture(repo);
        let (view, cx) = open_keystone(repo, state.path(), cx);
        cx.simulate_keystrokes("secondary-shift-g");
        until(cx, "first status", |cx| settled_all(&view, cx));
        click(cx, "changes-push");
        until(cx, "no remote", |cx| settled_all(&view, cx));
        assert!(matches!(
            notice(&view, cx),
            Some(Notice::Hint {
                command: "git remote add origin <url>",
                ..
            })
        ));
        assert!(cx.debug_bounds("changes-hint-command").is_some());

        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        git(
            repo,
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        click(cx, "changes-push");
        until(cx, "pushed", |cx| settled_all(&view, cx));
        assert_eq!(upstream(&view, cx).as_deref(), Some("origin/main"));

        let other = tempfile::tempdir().unwrap();
        git(
            other.path(),
            &["clone", "-q", remote.path().to_str().unwrap(), "."],
        );
        for (key, value) in [
            ("user.name", "Other"),
            ("user.email", "other@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", ".git/hooks"),
        ] {
            git(other.path(), &["config", key, value]);
        }
        git(
            other.path(),
            &["commit", "-q", "--allow-empty", "-m", "theirs"],
        );
        git(other.path(), &["push", "-q"]);
        let theirs = git(remote.path(), &["rev-parse", "main"]);
        git(repo, &["commit", "-q", "-am", "ours"]);
        click(cx, "changes-refresh");
        until(cx, "ahead", |cx| settled_all(&view, cx));
        click(cx, "changes-push");
        until(cx, "rejected", |cx| settled_all(&view, cx));
        assert!(
            matches!(
                notice(&view, cx),
                Some(Notice::Failed { title: "Push rejected.", output })
                    if output.contains("[rejected]")
            ),
            "{:?}",
            notice(&view, cx)
        );
        assert!(cx.debug_bounds("changes-output").is_some());
        assert_eq!(git(remote.path(), &["rev-parse", "main"]), theirs);

        click(cx, "changes-create-pr");
        assert_eq!(
            notice(&view, cx),
            Some(Notice::Info("Push your commits first.")),
            "gh would refuse a branch with unpushed commits"
        );
        git(repo, &["pull", "-q", "--rebase"]);
        click(cx, "changes-push");
        until(cx, "pushed after the rebase", |cx| settled_all(&view, cx));
        assert_eq!(
            git(remote.path(), &["rev-parse", "main"]),
            git(repo, &["rev-parse", "HEAD"])
        );

        click(cx, "changes-create-pr");
        until(cx, "form open", |cx| settled_all(&view, cx));
        click(cx, "changes-pr-create");
        until(cx, "gh answered", |cx| settled_all(&view, cx));
        assert!(
            matches!(
                notice(&view, cx),
                Some(Notice::Hint { id: "changes-hint-gh-signed-out", command: "gh auth login", output: Some(output), .. })
                    if output.contains("gh auth login")
            ),
            "{:?}",
            notice(&view, cx)
        );
        assert_eq!(
            form_value(&view, cx, |form| &form.title).as_deref(),
            Some("New Claude session"),
            "a signed-out gh keeps the form and its text"
        );
        assert_eq!(saved_pull_request(&view, cx), None);

        git(repo, &["switch", "-q", "-c", "other"]);
        script_gh(repo, "echo https://github.com/owner/repo/pull/99\n");
        click(cx, "changes-pr-create");
        until(cx, "refused", |cx| settled_all(&view, cx));
        assert!(
            matches!(
                notice(&view, cx),
                Some(Notice::Failed { output, .. })
                    if output.contains("other is checked out. Check out main")
            ),
            "{:?}",
            notice(&view, cx)
        );
        assert!(
            !repo.join(".git/fake-gh-argv").exists(),
            "gh never ran for another branch"
        );
        assert_eq!(saved_pull_request(&view, cx), None);
    }

    #[gpui::test]
    fn a_pull_request_is_saved_on_the_session_its_form_was_opened_for_even_once_closed(
        cx: &mut gpui::TestAppContext,
    ) {
        fake_gh();
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let repo = project.path();
        let _remote = with_remote(repo);
        git(repo, &["push", "-q", "-u", "origin", "feature"]);
        {
            let mut store = Store::open(state.path().join("workspace.json")).unwrap();
            let directory = WorkingDirectory::new(repo).unwrap();
            store.create(Provider::Claude, &directory, 1).unwrap();
            store.create(Provider::Codex, &directory, 2).unwrap();
            store.rename(1, "First session").unwrap();
            let shown = keystone_core::ProjectId::new(repo).unwrap();
            store
                .open_project(keystone_core::WindowId::FIRST, &shown, None, Some(1), 1)
                .unwrap();
        }
        script_gh(
            repo,
            "sleep 0.3\necho https://github.com/owner/repo/pull/3\n",
        );
        let (view, cx) = open_keystone(repo, state.path(), cx);
        view.update(cx, |view, cx| view.select(1, cx));
        cx.simulate_keystrokes("secondary-shift-g");
        until(cx, "first status", |cx| settled_all(&view, cx));
        click(cx, "changes-create-pr");
        until(cx, "gh checked", |cx| settled_all(&view, cx));
        assert_eq!(
            form_value(&view, cx, |form| &form.title).as_deref(),
            Some("First session")
        );

        view.update(cx, |view, cx| view.select(2, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("changes-pr-form").is_none(),
            "an idle form closes when another session is selected"
        );
        view.update(cx, |view, cx| view.select(1, cx));
        cx.run_until_parked();
        click(cx, "changes-create-pr");
        until(cx, "form open again", |cx| settled_all(&view, cx));
        click(cx, "changes-pr-create");
        view.update(cx, |view, cx| view.select(2, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, cx| view.selected(cx)), Some(2));
        assert!(
            cx.debug_bounds("changes-pr-form").is_some(),
            "a form gh is working for stays"
        );
        view.update(cx, |view, _| view.changes.panels.borrow_mut().clear());
        cx.run_until_parked();
        let saved = |cx: &mut gpui::VisualTestContext, id: u64| {
            view.read_with(cx, |view, cx| {
                view.state.read(cx).session(id)?.pull_request().cloned()
            })
        };
        until(cx, "saved after the panel is gone", |cx| {
            saved(cx, 1).is_some()
        });
        assert_eq!(saved(cx, 1).map(|pr| pr.number), Some(3));
        assert_eq!(
            saved(cx, 2),
            None,
            "not on the session selected since the form opened"
        );
    }

    #[gpui::test]
    fn a_folder_outside_git_says_so(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (view, cx) = open_keystone(project.path(), state.path(), cx);
        cx.simulate_keystrokes("secondary-shift-g");
        until(cx, "first status", |cx| settled(&view, cx));
        assert!(cx.debug_bounds("changes-unavailable").is_some());
    }
}
