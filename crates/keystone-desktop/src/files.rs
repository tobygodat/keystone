use crate::Keystone;
use crate::arrangement_editor::FILES_WIDTH;
use crate::excerpt::Excerpt;
use crate::file_view::FileView;
use crate::presentation::ConversationFolder;
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, DismissEvent, Entity, Focusable,
    IntoElement, KeyBinding, Render, Subscription, WeakEntity, Window, div, prelude::*, px,
};
use keystone_core::Component;
use keystone_ui::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

gpui::actions!(files, [CopyPath, MentionInComposer, ToggleFiles, ViewFile]);

const CONTEXT: &str = "FileTree";

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-shift-c", CopyPath, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-m", MentionInComposer, Some(CONTEXT)),
        KeyBinding::new("space", ViewFile, Some(CONTEXT)),
    ]);
}

/// One file browser per project folder, so switching sessions or closing and reopening
/// the Files view keeps each folder's expanded state.
#[derive(Default)]
pub(crate) struct Files {
    browsers: RefCell<HashMap<PathBuf, Entity<FileBrowser>>>,
}

impl Files {
    pub(crate) fn browser(&self, root: &Path, cx: &mut Context<Keystone>) -> Entity<FileBrowser> {
        let keystone = cx.weak_entity();
        self.browsers
            .borrow_mut()
            .entry(root.to_owned())
            .or_insert_with(|| cx.new(|cx| FileBrowser::new(root.to_owned(), keystone, cx)))
            .clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Notice {
    SpawnFailed { command: String, error: String },
    ReadFailed { path: PathBuf, error: String },
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed { command, error } => write!(f, "Couldn't run {command}: {error}"),
            Self::ReadFailed { path, error } => {
                write!(f, "Couldn't read {}: {error}", path.display())
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    name: String,
    path: PathBuf,
    kind: NodeKind,
}

/// A walk of `dir` that leaves out hidden files and anything a `.gitignore` of the
/// enclosing repository excludes, as the Files view and the composer's `@` picker show.
pub(crate) fn walker(dir: &Path) -> ignore::WalkBuilder {
    ignore::WalkBuilder::new(dir)
}

/// The entries directly inside `dir` that [`walker`] keeps, folders first and then by
/// name ignoring case.
fn list_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut failure = None;
    for result in walker(dir).max_depth(Some(1)).build() {
        match result {
            Ok(entry) if entry.depth() == 0 => {}
            Ok(entry) => {
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir())
                    || (entry.path_is_symlink() && entry.path().is_dir());
                entries.push(Entry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    kind: if is_dir {
                        NodeKind::Folder
                    } else {
                        NodeKind::File
                    },
                    path: entry.into_path(),
                });
            }
            Err(error) => failure = Some(error),
        }
    }
    if let (true, Some(error)) = (entries.is_empty(), failure) {
        return Err(io::Error::other(error));
    }
    entries.sort_by_cached_key(|e| (e.kind == NodeKind::File, e.name.to_lowercase()));
    Ok(entries)
}

/// The program that opens a file, with its leading arguments. The file's path is
/// always appended as its own argument.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EditorCommand {
    program: String,
    args: Vec<String>,
}

const TERMINAL_EDITORS: &[&str] = &[
    "vi", "vim", "nvim", "nano", "pico", "helix", "hx", "micro", "kak", "ed", "ne", "joe", "mg",
    "mcedit",
];

/// The program name `word` runs, without its directory or a suffix such as the
/// `.basic` of `vim.basic`.
fn program_name(word: &str) -> &str {
    let name = word.rsplit('/').next().unwrap_or(word);
    name.split('.').next().unwrap_or(name)
}

/// The words after a leading `env` and its options and `NAME=value` assignments,
/// which is the command `env` runs.
fn skip_env(words: &[String]) -> &[String] {
    if words.first().is_none_or(|word| program_name(word) != "env") {
        return words;
    }
    let mut rest = &words[1..];
    while let Some((word, tail)) = rest.split_first() {
        rest = match word.as_str() {
            "--" => return tail,
            "-u" | "--unset" | "-C" | "--chdir" => tail.get(1..).unwrap_or_default(),
            _ if word.starts_with('-') || word.contains('=') => tail,
            _ => break,
        };
    }
    rest
}

/// Whether `words` runs an editor that needs a terminal, which a detached spawn
/// does not have, so it would hang or exit at once.
fn needs_terminal(words: &[String]) -> bool {
    let Some((program, args)) = skip_env(words).split_first() else {
        return false;
    };
    let has = |flags: &[&str]| args.iter().any(|arg| flags.contains(&arg.as_str()));
    match program_name(program) {
        name if TERMINAL_EDITORS.contains(&name) => true,
        "gvim" => has(&["-v"]),
        // Emacs opens a window of its own unless told to stay in the terminal.
        "emacs" | "emacsclient" => has(&["-nw", "-t", "--tty", "--no-window-system"]),
        _ => false,
    }
}

/// Whether `words` names a program that can open a file without a terminal.
fn opens_detached(words: &[String]) -> bool {
    skip_env(words)
        .first()
        .is_some_and(|program| !program.is_empty())
        && !needs_terminal(words)
}

/// The program that opens a file with its default app: `open` on macOS, `xdg-open`
/// elsewhere.
fn system_opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// `$VISUAL`, then `$EDITOR`, then the [`system_opener`]. A value that does not parse as shell
/// words, names no program, or names a terminal editor is skipped.
fn editor_command(env: impl Fn(&str) -> Option<String>) -> EditorCommand {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|name| shlex::split(&env(name)?))
        .find(|words| opens_detached(words))
        .map_or_else(
            || EditorCommand {
                program: system_opener().into(),
                args: Vec::new(),
            },
            |mut words| EditorCommand {
                program: words.remove(0),
                args: words,
            },
        )
}

impl EditorCommand {
    fn command(&self, path: &Path) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args).arg(path);
        command
    }

    /// Starts the editor in a session of its own with no terminal, so quitting
    /// Keystone or a hangup to its process group leaves the editor running.
    fn spawn(&self, path: &Path) -> Result<(), Notice> {
        let mut child = self.start(path)?;
        std::thread::spawn(move || child.wait());
        Ok(())
    }

    fn start(&self, path: &Path) -> Result<std::process::Child, Notice> {
        let mut command = self.command(path);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().map_err(|error| Notice::SpawnFailed {
            command: self.to_string(),
            error: error.to_string(),
        })
    }
}

impl fmt::Display for EditorCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words = std::iter::once(&self.program).chain(&self.args);
        match shlex::try_join(words.map(String::as_str)) {
            Ok(joined) => f.write_str(&joined),
            Err(_) => f.write_str(&self.program),
        }
    }
}

/// Opens `path` in the user's editor, as a Files row does.
pub(crate) fn open_in_editor(path: &Path) -> Result<(), String> {
    editor_command(|name| std::env::var(name).ok())
        .spawn(path)
        .map_err(|notice| notice.to_string())
}

/// Opens `path` with its default app through the [`system_opener`], and waits for the
/// opener to exit. Some desktops' `xdg-open` runs the app itself, so that can be when the
/// app quits: call it on a thread of its own. Returns whether it succeeded.
pub(crate) fn open_with_system(path: &Path) -> bool {
    EditorCommand {
        program: system_opener().into(),
        args: Vec::new(),
    }
    .start(path)
    .ok()
    .and_then(|mut child| child.wait().ok())
    .is_some_and(|status| status.success())
}

/// A lazily read tree of one folder. Node 0 is the folder itself, and `paths` holds
/// each node's absolute path by node id.
pub(crate) struct FileBrowser {
    tree: Entity<TreeState>,
    paths: Vec<PathBuf>,
    /// Folders with a listing in flight, and whether they expanded again since it began.
    loading: HashMap<NodeId, bool>,
    notice: Option<Notice>,
    /// The window whose composer "Mention in composer" writes to.
    keystone: WeakEntity<Keystone>,
    /// The file shown read-only in the tree's place.
    view: Option<(Entity<FileView>, Subscription)>,
}

impl FileBrowser {
    fn new(root: PathBuf, keystone: WeakEntity<Keystone>, cx: &mut Context<Self>) -> Self {
        let name = root.file_name().map_or_else(
            || root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let model = TreeModel::new([(name, NodeKind::Folder)]);
        let tree = cx.new(|cx| TreeState::new(model, cx).reload_on_expand());
        let mut browser = Self {
            tree,
            paths: vec![root],
            loading: HashMap::new(),
            notice: None,
            keystone,
            view: None,
        };
        if let Some(root) = browser.tree.update(cx, |tree, cx| tree.expand(0, cx)) {
            browser.load(root, cx);
        }
        browser
    }

    fn load(&mut self, folder: NodeId, cx: &mut Context<Self>) {
        if let Some(again) = self.loading.get_mut(&folder) {
            *again = true;
            return;
        }
        self.loading.insert(folder, false);
        let dir = self.paths[folder as usize].clone();
        let listing = cx
            .background_executor()
            .spawn(async move { list_dir(&dir) });
        cx.spawn(async move |this, cx| {
            let listing = listing.await;
            let _ = this.update(cx, |this, cx| this.fill(folder, listing, cx));
        })
        .detach();
    }

    fn fill(&mut self, folder: NodeId, listing: io::Result<Vec<Entry>>, cx: &mut Context<Self>) {
        let again = self.loading.remove(&folder) == Some(true);
        let entries = listing.unwrap_or_else(|error| {
            self.notice = Some(Notice::ReadFailed {
                path: self.paths[folder as usize].clone(),
                error: error.to_string(),
            });
            Vec::new()
        });
        if !self.unchanged(folder, &entries, cx) {
            let range = self.tree.update(cx, |tree, cx| {
                tree.set_children(folder, entries.iter().map(|e| (e.name.clone(), e.kind)), cx)
            });
            debug_assert_eq!(range.start as usize, self.paths.len());
            self.paths.extend(entries.into_iter().map(|e| e.path));
            cx.notify();
        }
        if again {
            self.load(folder, cx);
        }
    }

    /// Whether `folder` already holds exactly `entries`. Keeping its children then keeps
    /// their node ids and the expanded state of folders inside it.
    fn unchanged(&self, folder: NodeId, entries: &[Entry], cx: &App) -> bool {
        let model = self.tree.read(cx).model();
        model.node(folder).children().is_some_and(|children| {
            children.len() == entries.len()
                && children.zip(entries).all(|(id, entry)| {
                    let node = model.node(id);
                    node.kind() == entry.kind && *node.name() == entry.name
                })
        })
    }

    fn open(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let command = editor_command(|name| std::env::var(name).ok());
        self.notice = command.spawn(&self.paths[node as usize]).err();
        cx.notify();
    }

    /// How the composer would mention `node`: only a file of the selected session's
    /// recorded folder whose path has a token.
    fn mention_token(&self, node: NodeId, cx: &App) -> Option<String> {
        let root = self.keystone.upgrade()?.read(cx).mention_root(cx)?;
        let relative = self.paths[node as usize]
            .strip_prefix(root)
            .ok()?
            .to_str()?;
        crate::mentions::token(relative)
    }

    /// Puts an `@` mention of the file `node` in the composer.
    fn mention(&self, node: NodeId, window: &mut Window, cx: &mut App) {
        let path = self.paths[node as usize].clone();
        let _ = self
            .keystone
            .update(cx, |keystone, cx| keystone.mention_file(&path, window, cx));
    }

    /// Shows the file `node` read-only in the tree's place, until Back or Escape.
    fn view(&mut self, node: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.paths[node as usize].clone();
        let name = path
            .strip_prefix(&self.paths[0])
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let keystone = self.keystone.clone();
        let view = cx.new(|cx| FileView::new(path, name, keystone, cx));
        let back = cx.subscribe_in(&view, window, |this, _, _: &DismissEvent, window, cx| {
            this.view = None;
            window.focus(&this.tree.read(cx).focus_handle(cx), cx);
            cx.notify();
        });
        window.focus(&view.focus_handle(cx), cx);
        self.view = Some((view, back));
        cx.notify();
    }

    fn copy_path(&self, node: NodeId, cx: &mut App) {
        let path = self.paths[node as usize].to_string_lossy().into_owned();
        cx.write_to_clipboard(ClipboardItem::new_string(path));
    }
}

impl Focusable for FileBrowser {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        match &self.view {
            Some((view, _)) => view.focus_handle(cx),
            None => self.tree.read(cx).focus_handle(cx),
        }
    }
}

impl Render for FileBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((view, _)) = &self.view {
            return view.clone().into_any_element();
        }
        let browser = cx.weak_entity();
        v_flex()
            .size_full()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &CopyPath, _, cx| {
                if let Some(node) = this.tree.read(cx).highlighted_node(cx) {
                    this.copy_path(node, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ViewFile, window, cx| {
                let tree = this.tree.read(cx);
                if let Some(node) = tree.highlighted_node(cx)
                    && tree.model().node(node).kind() == NodeKind::File
                {
                    this.view(node, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &MentionInComposer, window, cx| {
                let tree = this.tree.read(cx);
                if let Some(node) = tree.highlighted_node(cx)
                    && tree.model().node(node).kind() == NodeKind::File
                {
                    this.mention(node, window, cx);
                }
            }))
            .children(self.notice.as_ref().map(|notice| {
                div()
                    .flex_shrink_0()
                    .px(px(14.))
                    .py_1()
                    .automation_id("files-notice")
                    .child(
                        Label::new(notice.to_string())
                            .size(TextSize::Ui)
                            .color(Color::Danger),
                    )
            }))
            .child(
                Tree::new(
                    "files-tree",
                    &self.tree,
                    move |entry, highlighted, _, cx| {
                        let (id, browser) = (entry.id, browser.clone());
                        let icon = match entry.kind {
                            NodeKind::Folder => IconName::Folder,
                            NodeKind::File => IconName::File,
                        };
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(Icon::new(icon).size(px(16.)).color(Color::Muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Label::new(entry.name.clone()).truncate()),
                            )
                            .when(highlighted && entry.kind == NodeKind::File, |d| {
                                let browser = browser.clone();
                                d.child(
                                    IconButton::new(("view-file", id as usize), IconName::Eye)
                                        .size(ButtonSize::Compact)
                                        .debug_selector(|| "view-file".into())
                                        .aria_label("View")
                                        .tooltip(Tooltip::with_key("View", "Space"))
                                        .on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            if let Some(browser) = browser.upgrade() {
                                                browser.update(cx, |browser, cx| {
                                                    browser.view(id, window, cx)
                                                });
                                            }
                                        }),
                                )
                            })
                            .when(
                                highlighted
                                    && entry.kind == NodeKind::File
                                    && browser.upgrade().is_some_and(|b| {
                                        b.read(cx).mention_token(id, cx).is_some()
                                    }),
                                |d| {
                                    let browser = browser.clone();
                                    d.child(
                                        IconButton::new(
                                            ("mention-file", id as usize),
                                            IconName::At,
                                        )
                                        .size(ButtonSize::Compact)
                                        .debug_selector(|| "mention-file".into())
                                        .aria_label("Mention in composer")
                                        .tooltip(Tooltip::with_key(
                                            "Mention in composer",
                                            keystone_ui::shortcut_text("secondary-shift-m"),
                                        ))
                                        .on_click(
                                            move |_, window, cx| {
                                                cx.stop_propagation();
                                                if let Some(browser) = browser.upgrade() {
                                                    browser.update(cx, |browser, cx| {
                                                        browser.mention(id, window, cx)
                                                    });
                                                }
                                            },
                                        ),
                                    )
                                },
                            )
                            .when(highlighted, |d| {
                                d.child(
                                    IconButton::new(("copy-path", id as usize), IconName::Copy)
                                        .size(ButtonSize::Compact)
                                        .debug_selector(|| "copy-path".into())
                                        .tooltip(Tooltip::text("Copy path"))
                                        .on_click(move |_, _, cx| {
                                            cx.stop_propagation();
                                            if let Some(browser) = browser.upgrade() {
                                                browser.update(cx, |browser, cx| {
                                                    browser.copy_path(id, cx)
                                                });
                                            }
                                        }),
                                )
                            })
                            .into_any_element()
                    },
                )
                .on_open(cx.listener(|this, id: &NodeId, _, cx| this.open(*id, cx)))
                .on_expand(cx.listener(|this, id: &NodeId, _, cx| this.load(*id, cx)))
                .flex_1()
                .min_h_0()
                .px_1(),
            )
            .into_any_element()
    }
}

impl Keystone {
    /// The selected session's recorded folder, or else the current project.
    pub(crate) fn files_root(&self, cx: &App) -> Result<PathBuf, String> {
        let recorded = self
            .selected(cx)
            .and_then(|session| self.conversation_folder(session, cx));
        match (recorded, &self.presentation.project) {
            (Some(ConversationFolder::Recorded(directory)), _) => Ok(directory.path().to_owned()),
            (_, Ok(project)) => Ok(project.path().to_owned()),
            (_, Err(error)) => Err(error.clone()),
        }
    }

    /// The lines selected in this window's Files view, while it has focus.
    pub(crate) fn file_view_excerpt(&self, window: &Window, cx: &App) -> Option<Excerpt> {
        let root = self.files_root(cx).ok()?;
        let browser = self.files.browsers.borrow().get(&root)?.clone();
        let (view, _) = browser.read(cx).view.as_ref()?;
        let view = view.read(cx);
        view.focus_handle(cx)
            .contains_focused(window, cx)
            .then(|| view.excerpt())
            .flatten()
    }

    pub(crate) fn focus_files(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .shown_composition(cx)
            .views()
            .any(|(_, component)| component == Component::Files)
        {
            return;
        }
        if let Ok(root) = self.files_root(cx) {
            let browser = self.files.browser(&root, cx);
            window.focus(&browser.focus_handle(cx), cx);
        }
    }

    pub(crate) fn toggle_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_surface(Component::Files, FILES_WIDTH, window, cx);
    }

    /// The conversation a side surface such as Files opens beside: the one the user
    /// last worked in, else the first visible or declared one.
    pub(crate) fn side_surface_target(&self, cx: &App) -> keystone_core::ViewId {
        let shown = self.shown_composition(cx);
        let visible: Vec<_> = self
            .presentation
            .current
            .arrangement
            .visible
            .iter()
            .copied()
            .filter(|view| shown.component(*view) == Some(Component::Conversation))
            .collect();
        self.selected(cx)
            .and_then(|session| {
                self.presentation
                    .current
                    .views
                    .send_target(&visible, session)
            })
            .map(|key| key.view)
            .or_else(|| visible.first().copied())
            .or_else(|| {
                shown
                    .views()
                    .find(|(_, c)| *c == Component::Conversation)
                    .map(|(id, _)| id)
            })
            .expect("a composition has a conversation")
    }

    pub(crate) fn file_list(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.files_root(cx) {
            Ok(root) => div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(self.files.browser(&root, cx))
                .into_any_element(),
            Err(error) => div()
                .flex_1()
                .px(px(14.))
                .py_2()
                .automation_id("files-empty")
                .child(
                    Label::new(format!("No project folder to browse. {error}"))
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
    use std::fs;

    fn names(entries: &[Entry]) -> Vec<(&str, NodeKind)> {
        entries.iter().map(|e| (e.name.as_str(), e.kind)).collect()
    }

    #[test]
    fn lists_one_level_respecting_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
        for folder in ["build", "src/nested/deeper", "Docs"] {
            fs::create_dir_all(root.join(folder)).unwrap();
        }
        for file in [
            "build/out.bin",
            "src/nested/deeper/leaf.rs",
            "src/main.rs",
            "src/trace.log",
            "b.txt",
            "A.md",
        ] {
            fs::write(root.join(file), "").unwrap();
        }

        let top = list_dir(root).unwrap();
        assert_eq!(
            names(&top),
            [
                ("Docs", NodeKind::Folder),
                ("src", NodeKind::Folder),
                ("A.md", NodeKind::File),
                ("b.txt", NodeKind::File),
            ],
            "one level, folders first, ignored and hidden entries left out"
        );
        assert_eq!(top[1].path, root.join("src"));

        let src = list_dir(&root.join("src")).unwrap();
        assert_eq!(
            names(&src),
            [("nested", NodeKind::Folder), ("main.rs", NodeKind::File)],
            "the repository's .gitignore applies below its root"
        );

        assert!(list_dir(&root.join("missing")).is_err());
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    fn argv(command: &EditorCommand, path: &str) -> Vec<String> {
        let command = command.command(Path::new(path));
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn editor_command_prefers_visual_then_editor_then_xdg_open() {
        let path = "/repo/a file; rm -rf ~.rs";
        for (vars, expected) in [
            (
                &[("VISUAL", "code --wait"), ("EDITOR", "gedit")][..],
                &["code", "--wait", path][..],
            ),
            (&[("VISUAL", "vim"), ("EDITOR", "gedit")], &["gedit", path]),
            (
                &[("VISUAL", "/usr/bin/nvim"), ("EDITOR", "emacs -nw")],
                &[system_opener(), path],
            ),
            (&[("EDITOR", "emacs")], &["emacs", path]),
            (
                &[("VISUAL", "'/opt/My Editor/bin/edit' --new-window")],
                &["/opt/My Editor/bin/edit", "--new-window", path],
            ),
            (
                &[("VISUAL", "'unclosed"), ("EDITOR", "")],
                &[system_opener(), path],
            ),
            (&[], &[system_opener(), path]),
            (
                &[("VISUAL", "env nvim"), ("EDITOR", "gedit")],
                &["gedit", path],
            ),
            (
                &[("VISUAL", "env -u TERM FOO=1 nvim -p"), ("EDITOR", "gedit")],
                &["gedit", path],
            ),
            (
                &[("VISUAL", "env FOO=1 code --wait")],
                &["env", "FOO=1", "code", "--wait", path],
            ),
            (&[("VISUAL", "env FOO=1")], &[system_opener(), path]),
            (
                &[("VISUAL", "vim.tiny"), ("EDITOR", "gedit")],
                &["gedit", path],
            ),
            (
                &[("VISUAL", "/opt/nvim.appimage"), ("EDITOR", "gedit")],
                &["gedit", path],
            ),
            (
                &[("VISUAL", "gvim -v"), ("EDITOR", "gedit")],
                &["gedit", path],
            ),
            (&[("VISUAL", "gvim --nofork")], &["gvim", "--nofork", path]),
            (&[("VISUAL", "gvim")], &["gvim", path]),
            (&[("VISUAL", "''"), ("EDITOR", "gedit")], &["gedit", path]),
        ] {
            assert_eq!(argv(&editor_command(env(vars)), path), expected, "{vars:?}");
        }
    }

    #[test]
    fn spawn_failure_reports_error() {
        let command = editor_command(env(&[("VISUAL", "/nonexistent/editor --flag")]));
        let notice = command.spawn(Path::new("/tmp/file.txt")).unwrap_err();
        let message = notice.to_string();
        assert!(
            message.starts_with("Couldn't run /nonexistent/editor --flag: "),
            "{message}"
        );
        assert!(message.contains("No such file"), "{message}");
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod ui_tests {
    use super::*;
    use keystone_core::{Provider, Store, WorkingDirectory};
    use std::fs;
    use std::time::{Duration, Instant};

    fn open_keystone<'a>(
        project: &Path,
        state: &Path,
        cx: &'a mut gpui::TestAppContext,
    ) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
        let mut store = Store::open(state.join("workspace.json")).unwrap();
        let directory = WorkingDirectory::new(project).unwrap();
        store.create(Provider::Claude, &directory, 1).unwrap();
        let shown = keystone_core::ProjectId::new(project).unwrap();
        store
            .open_project(keystone_core::WindowId::FIRST, &shown, None, Some(1), 1)
            .unwrap();
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

    fn toggle_files(cx: &mut gpui::VisualTestContext) {
        cx.simulate_keystrokes("secondary-shift-e");
        cx.run_until_parked();
    }

    fn browser(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Entity<FileBrowser> {
        view.read_with(cx, |view, cx| {
            view.files.browsers.borrow()[&view.files_root(cx).unwrap()].clone()
        })
    }

    fn rows(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        let browser = browser(view, cx);
        browser.read_with(cx, |browser, cx| {
            let model = browser.tree.read(cx).model();
            model
                .visible()
                .iter()
                .map(|row| match *row {
                    keystone_ui::tree::Row::Node(id) => model.node(id).name().to_string(),
                    keystone_ui::tree::Row::Loading(_) => "Loading".into(),
                })
                .collect()
        })
    }
    /// Collapses or expands the folder named `name` the way a click does, without
    /// running the listing it starts.
    fn toggle(view: &Entity<Keystone>, name: &str, cx: &mut gpui::VisualTestContext) {
        let row = rows(view, cx).iter().position(|row| row == name).unwrap();
        browser(view, cx).update(cx, |browser, cx| {
            if let Some(folder) = browser.tree.update(cx, |tree, cx| tree.toggle(row, cx)) {
                browser.load(folder, cx);
            }
        });
    }

    #[gpui::test]
    fn closing_focused_files_keeps_shortcuts_working(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fs::write(project.path().join("README.md"), "").unwrap();
        let (_view, cx) = open_keystone(project.path(), state.path(), cx);
        toggle_files(cx);
        assert!(cx.debug_bounds("files-tree-row-1").is_some(), "Files opens");
        toggle_files(cx);
        assert!(
            cx.debug_bounds("files-tree-row-0").is_none(),
            "Files closes"
        );
        toggle_files(cx);
        assert!(
            cx.debug_bounds("files-tree-row-1").is_some(),
            "the shortcut still reaches the app after the focused tree closed"
        );
    }

    #[gpui::test]
    fn toggle_files_brings_a_hidden_files_tab_forward_before_closing_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fs::write(project.path().join("README.md"), "").unwrap();
        let (_view, cx) = open_keystone(project.path(), state.path(), cx);
        toggle_files(cx);
        let add = cx.debug_bounds("surface-add").expect("surface-add");
        cx.simulate_click(add.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let conversation = cx
            .debug_bounds("surface-add-conversation")
            .expect("surface-add-conversation");
        cx.simulate_click(conversation.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("files-tree-row-0").is_none(),
            "Files is behind the new tab"
        );

        toggle_files(cx);
        assert!(
            cx.debug_bounds("files-tree-row-1").is_some(),
            "the shortcut shows Files instead of closing it"
        );
        toggle_files(cx);
        assert!(
            cx.debug_bounds("files-tree-row-0").is_none(),
            "with the tree focused, the shortcut closes Files"
        );
    }

    #[gpui::test]
    fn toggle_files_in_a_narrow_window_shows_and_focuses_the_tree(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fs::write(project.path().join("README.md"), "").unwrap();
        let (_view, cx) = open_keystone(project.path(), state.path(), cx);
        cx.simulate_resize(gpui::size(px(400.), px(900.)));
        cx.run_until_parked();
        toggle_files(cx);
        assert!(
            cx.debug_bounds("files-tree-row-1").is_some(),
            "Files is the fitted tab in front"
        );
        toggle_files(cx);
        assert!(
            cx.debug_bounds("files-tree-row-0").is_none(),
            "with the tree focused, the shortcut closes Files"
        );
        assert!(cx.debug_bounds("fitted-tab-0").is_some());
    }

    #[gpui::test]
    fn files_toggle_browses_copies_and_keeps_expanded_folders(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = project.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("build/out.o"), "").unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        fs::write(root.join("README.md"), "").unwrap();
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        let (view, cx) = open_keystone(root, state.path(), cx);

        toggle_files(cx);
        assert!(
            cx.debug_bounds("session-1").is_some(),
            "the session list stays beside the Files view"
        );
        assert!(cx.debug_bounds("files-tab-0").is_some());
        assert_eq!(rows(&view, cx), [name.as_str(), "src", "README.md"]);

        cx.simulate_keystrokes("down right");
        assert_eq!(
            rows(&view, cx),
            [name.as_str(), "src", "main.rs", "README.md"]
        );
        cx.simulate_keystrokes("down secondary-shift-c");
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        let main_rs = WorkingDirectory::new(root)
            .unwrap()
            .path()
            .join("src/main.rs");
        assert_eq!(copied.as_deref(), Some(main_rs.to_str().unwrap()));

        fs::write(root.join("src/lib.rs"), "").unwrap();
        cx.simulate_keystrokes("up left right");
        assert_eq!(
            rows(&view, cx),
            [name.as_str(), "src", "lib.rs", "main.rs", "README.md"],
            "expanding again re-reads the folder"
        );

        toggle_files(cx);
        assert!(
            cx.debug_bounds("files-tree-row-1").is_none(),
            "Files closed"
        );
        toggle_files(cx);
        assert!(
            cx.debug_bounds("files-tree-row-4").is_some(),
            "the reopened tree is drawn"
        );
        assert_eq!(
            rows(&view, cx),
            [name.as_str(), "src", "lib.rs", "main.rs", "README.md"],
            "expanded folders survive the round trip"
        );
    }

    #[gpui::test]
    fn refreshing_an_unchanged_folder_keeps_nested_folders_expanded(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = project.path();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join("src/nested/deep.rs"), "").unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        let (view, cx) = open_keystone(root, state.path(), cx);
        let expanded = [name.as_str(), "src", "nested", "deep.rs", "main.rs"];

        toggle_files(cx);
        cx.simulate_keystrokes("down right");
        cx.simulate_keystrokes("down right");
        assert_eq!(rows(&view, cx), expanded);
        let paths = browser(&view, cx).read_with(cx, |browser, _| browser.paths.len());

        for _ in 0..3 {
            cx.simulate_keystrokes("up left right");
            assert_eq!(rows(&view, cx), expanded, "nested folder stays expanded");
            cx.simulate_keystrokes("down");
        }
        assert_eq!(
            browser(&view, cx).read_with(cx, |browser, _| browser.paths.len()),
            paths,
            "an unchanged listing adds no nodes"
        );
    }

    #[gpui::test]
    fn expanding_again_during_a_listing_reads_the_folder_once_more(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = project.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        let (view, cx) = open_keystone(root, state.path(), cx);
        toggle_files(cx);
        cx.simulate_keystrokes("down right");
        assert_eq!(rows(&view, cx), [name.as_str(), "src", "main.rs"]);

        toggle(&view, "src", cx);
        toggle(&view, "src", cx);
        while cx.dispatcher.tick(true) {}
        fs::write(root.join("src/lib.rs"), "").unwrap();
        toggle(&view, "src", cx);
        toggle(&view, "src", cx);
        cx.run_until_parked();
        assert_eq!(
            rows(&view, cx),
            [name.as_str(), "src", "lib.rs", "main.rs"],
            "a file added after the first listing read the folder still shows"
        );
    }

    #[gpui::test]
    #[ignore = "benchmark; builds a 50,000-file fixture"]
    fn perf_files_first_rows(cx: &mut gpui::TestAppContext) {
        let (project, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let fixture = project.path().join("fixture");
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/make-file-fixture.py");
        let status = Command::new("python3")
            .arg("-I")
            .arg(script)
            .arg(&fixture)
            .arg("50000")
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "fixture script failed");
        let (view, cx) = open_keystone(&fixture, state.path(), cx);
        cx.run_until_parked();

        // A frame is the queued main-thread work, such as filling in a listing, plus
        // the draw that shows it. The test executor runs background tasks on this
        // thread too, so they run first and outside the frame.
        let frame = |cx: &mut gpui::VisualTestContext| {
            while cx.dispatcher.tick(true) {}
            let start = Instant::now();
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            start.elapsed()
        };
        let start = Instant::now();
        view.update_in(cx, |this, window, cx| this.toggle_files(window, cx));
        let mut longest = start.elapsed();
        loop {
            longest = longest.max(frame(cx));
            let shown = rows(&view, cx);
            if shown.len() > 1
                && shown[1] != "Loading"
                && cx.debug_bounds("files-tree-row-1").is_some()
            {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "rows never drew");
        }
        let first_rows = start.elapsed();
        eprintln!(
            "PERF files_first_rows_ms={:.2} files_longest_frame_ms={:.2} rows=[{}]",
            first_rows.as_secs_f64() * 1000.,
            longest.as_secs_f64() * 1000.,
            rows(&view, cx).join(" "),
        );
        assert!(
            first_rows < Duration::from_millis(300),
            "first rows took {first_rows:?}, over 300 ms"
        );
        assert!(
            longest < Duration::from_millis(33),
            "longest frame took {longest:?}, over 33 ms"
        );
    }
}
