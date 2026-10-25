//! Projects. Each window shows one project at a time and keeps its own: the folder
//! its new sessions start in, the sessions its sidebar lists, and its layout and tabs.
//! The project list at the top of the sidebar, or Ctrl+O, switches it. Taking a
//! project off the list never touches its folder or its sessions.

use crate::{Keystone, open_windows};
use gpui::{
    AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, InteractiveElement,
    IntoElement, ParentElement, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use keystone_core::{ProjectId, Session, SessionContext};
use keystone_ui::prelude::*;
use keystone_ui::{CommandPalette, CommandPaletteState, PaletteItem};
use std::path::{Path, PathBuf};

gpui::actions!(keystone, [SwitchProject, OpenProject]);

const LISTED: &str = "project:";
const TYPED: &str = "path:";
const OPEN_FOLDER: &str = "open-folder";

/// The open project list and the focus it returns to when it closes.
pub(crate) struct ProjectList {
    state: Entity<CommandPaletteState>,
    pub(crate) restore: Option<FocusHandle>,
}

/// Whether `session` is one of `project`'s: it works in the project's folder or in a
/// folder inside it. A session saved before sessions recorded their folder belongs to
/// every project, so it stays in reach until a folder is chosen for it.
pub(crate) fn holds(project: &ProjectId, session: &Session) -> bool {
    match session.context() {
        SessionContext::Recorded { directory, .. } => project.contains(directory.path()),
        SessionContext::LegacyMissing | SessionContext::Unsupported => true,
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `path` with the home folder written as `~`.
pub(crate) fn display_path(path: &Path) -> String {
    match home().and_then(|home| path.strip_prefix(home).ok().map(Path::to_owned)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// A typed folder path, with a leading `~` as the home folder. `None` unless it is
/// absolute.
fn typed_path(query: &str) -> Option<PathBuf> {
    let query = query.trim();
    let path = match query.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home()?.join(rest.trim_start_matches('/'))
        }
        Some(_) => return None,
        None => PathBuf::from(query),
    };
    path.is_absolute().then_some(path)
}

fn typed_item(query: &str) -> Option<PaletteItem> {
    let path = typed_path(query)?;
    let item = PaletteItem::new(format!("{TYPED}{query}"), query.trim());
    Some(if path.is_dir() {
        item
    } else {
        item.detail("No folder here yet")
    })
}

impl Keystone {
    /// The session the window shows on opening `project`: the one it has selected when
    /// that is the project's, else `returning`, the one it showed when it last left, else
    /// the project's most recently viewed session.
    pub(crate) fn selection_in(
        &self,
        project: &ProjectId,
        returning: Option<u64>,
        cx: &gpui::App,
    ) -> Option<u64> {
        let state = self.state.read(cx);
        let held = |id: &u64| state.session(*id).is_some_and(|s| holds(project, s));
        state
            .store()
            .workspace()
            .selected_in(self.window_id)
            .filter(held)
            .or(returning.filter(held))
            .or_else(|| {
                state
                    .store()
                    .workspace()
                    .sessions()
                    .iter()
                    .filter(|s| matches!(s.context(), SessionContext::Recorded { .. }))
                    .filter(|s| holds(project, s))
                    .max_by_key(|s| (s.viewed_at(), s.updated_at(), s.id()))
                    .map(Session::id)
            })
    }

    /// The project to open so this window shows `session`: none when its own project
    /// holds the session, else the known project holding it, else its folder.
    pub(crate) fn project_for(&self, session: u64, cx: &gpui::App) -> Option<ProjectId> {
        let session = self.state.read(cx).session(session)?;
        match &self.presentation.project {
            Ok(project) if !holds(project, session) => {}
            _ => return None,
        }
        let SessionContext::Recorded { directory, .. } = session.context() else {
            return None;
        };
        let (listed, folders) = self.known_projects(cx);
        listed
            .into_iter()
            .chain(folders)
            .find(|project| project.contains(directory.path()))
            .or_else(|| Some(ProjectId::of(directory)))
    }

    /// Saves the project a window opened on, so it is listed and reopens even when
    /// Keystone does not quit cleanly. A saved selection of another project's session
    /// gives way to one of this project's, or stays saved for when its own project
    /// opens when this one has none.
    pub(crate) fn save_opened_project(&mut self, cx: &mut Context<Self>) {
        let Ok(project) = self.presentation.project.clone() else {
            return;
        };
        let workspace = self.state.read(cx).store().workspace();
        let saved = workspace.selected_in(self.window_id);
        let selected = match saved {
            Some(_) if self.selected(cx).is_none() => {
                self.selection_in(&project, None, cx).or(saved)
            }
            _ => saved,
        };
        if workspace
            .preferences()
            .window_project(self.window_id)
            .as_ref()
            == Some(&project)
            && workspace.projects().iter().any(|p| p.id() == project)
            && saved == selected
        {
            return;
        }
        let window_id = self.window_id;
        let saved = self.state.update(cx, |state, _| {
            state.open_project(window_id, &project, None, selected, crate::now_ms())
        });
        if let Err(error) = saved {
            self.error = Some(format!("Window's project not saved. {error}"));
        }
    }

    /// Projects that another open window shows, which stay on the list while it does.
    fn shown_elsewhere(&self, cx: &Context<Self>) -> Vec<ProjectId> {
        let this = cx.entity();
        open_windows(cx)
            .into_iter()
            .filter(|other| other != &this)
            .filter_map(|other| other.read(cx).presentation.project.clone().ok())
            .collect()
    }

    /// This window's project, every other listed one, most recently opened first, and
    /// last the folders of sessions no project holds, listed or removed, such as
    /// sessions from before projects, most recently active first. A folder inside
    /// another of those is offered as that one.
    fn known_projects(&self, cx: &gpui::App) -> (Vec<ProjectId>, Vec<ProjectId>) {
        let current = self.presentation.project.as_ref().ok();
        let workspace = self.state.read(cx).store().workspace();
        let listed: Vec<ProjectId> = current
            .cloned()
            .into_iter()
            .chain(
                workspace
                    .projects()
                    .into_iter()
                    .map(|p| p.id())
                    .filter(|id| Some(id) != current),
            )
            .collect();
        let removed: Vec<ProjectId> = workspace.removed_projects().collect();
        let mut folders: Vec<(ProjectId, Option<u64>)> = Vec::new();
        for session in workspace.sessions() {
            let SessionContext::Recorded { directory, .. } = session.context() else {
                continue;
            };
            if listed
                .iter()
                .chain(&removed)
                .any(|p| p.contains(directory.path()))
            {
                continue;
            }
            let id = ProjectId::of(directory);
            match folders.iter_mut().find(|(folder, _)| *folder == id) {
                Some((_, latest)) => *latest = (*latest).max(session.updated_at()),
                None => folders.push((id, session.updated_at())),
            }
        }
        let outer: Vec<ProjectId> = folders.iter().map(|(id, _)| id.clone()).collect();
        let mut collapsed: Vec<(ProjectId, Option<u64>)> = Vec::new();
        for (id, latest) in folders {
            let top = outer
                .iter()
                .filter(|other| other.contains(id.path()))
                .min_by_key(|other| other.path().components().count())
                .cloned()
                .unwrap_or(id);
            match collapsed.iter_mut().find(|(folder, _)| *folder == top) {
                Some((_, newest)) => *newest = (*newest).max(latest),
                None => collapsed.push((top, latest)),
            }
        }
        let mut folders = collapsed;
        folders.sort_by_key(|(_, latest)| std::cmp::Reverse(*latest));
        (listed, folders.into_iter().map(|(id, _)| id).collect())
    }

    /// The known project at `path`, from [`Keystone::known_projects`].
    fn known_project(&self, path: &Path, cx: &gpui::App) -> Option<ProjectId> {
        let (listed, folders) = self.known_projects(cx);
        listed.into_iter().chain(folders).find(|p| p.path() == path)
    }

    /// The known projects, the window's own first, then Open folder.
    fn project_items(&self, cx: &Context<Self>) -> Vec<PaletteItem> {
        let current = self.presentation.project.as_ref().ok();
        let elsewhere = self.shown_elsewhere(cx);
        let (listed, folders) = self.known_projects(cx);
        let unlisted = folders.clone();
        let mut items: Vec<PaletteItem> = listed
            .into_iter()
            .chain(folders)
            .map(|id| {
                let mut detail = Vec::new();
                if Some(&id) == current {
                    detail.push("Current".to_owned());
                }
                if !id.path().is_dir() {
                    detail.push("Folder missing".to_owned());
                }
                detail.push(display_path(id.path()));
                let item = PaletteItem::new(
                    format!("{LISTED}{}", id.path().display()),
                    id.name().to_owned(),
                )
                .detail(detail.join(" · "));
                if Some(&id) == current || elsewhere.contains(&id) || unlisted.contains(&id) {
                    item
                } else {
                    item.removable()
                }
            })
            .collect();
        items.push(PaletteItem::new(OPEN_FOLDER, "Open folder…").kbd("Ctrl+Shift+O"));
        items
    }

    /// Opens the project list, with `query` typed when given, or closes it.
    pub(crate) fn open_projects(
        &mut self,
        query: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.projects.is_some() && query.is_none() {
            return self.close_projects(window, cx);
        }
        self.new_menu = false;
        self.filter_menu = false;
        self.approval_menu = false;
        self.model_menu = None;
        let restore = self
            .projects
            .take()
            .and_then(|list| list.restore)
            .or_else(|| self.palette.take().and_then(|palette| palette.restore))
            .or_else(|| self.prompt_list.take().and_then(|list| list.restore))
            .or_else(|| window.focused(cx));
        let items = self.project_items(cx);
        let state = cx.new(|cx| {
            let mut state =
                CommandPaletteState::with_placeholder(items, "Project or folder path", cx);
            state.set_query_item(typed_item, cx);
            if let Some(query) = query {
                state.set_query(query, cx);
            }
            state
        });
        state.focus_handle(cx).focus(window, cx);
        self.projects = Some(ProjectList { state, restore });
        self.refresh(cx);
    }

    fn close_projects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(list) = self.projects.take() else {
            return;
        };
        list.restore
            .unwrap_or_else(|| self.focus.clone())
            .focus(window, cx);
        self.refresh(cx);
    }

    fn confirm_project(&mut self, item: &PaletteItem, window: &mut Window, cx: &mut Context<Self>) {
        let id = item.id.as_ref();
        if id == OPEN_FOLDER {
            self.close_projects(window, cx);
            return self.choose_project(window, cx);
        }
        let project = if let Some(path) = id.strip_prefix(LISTED) {
            self.known_project(Path::new(path), cx)
                .ok_or_else(|| format!("{path} is no longer on the project list."))
        } else if let Some(typed) = id.strip_prefix(TYPED) {
            typed_path(typed)
                .ok_or_else(|| "Type a full path, starting with / or ~.".to_owned())
                .and_then(|path| ProjectId::new(&path).map_err(|e| e.to_string()))
        } else {
            return;
        };
        match project {
            Ok(project) => {
                self.close_projects(window, cx);
                self.open_project(project, cx);
                self.focus_project_transition(window, cx);
            }
            Err(error) => {
                self.error = Some(format!("Project not opened. {error}"));
                self.refresh(cx);
            }
        }
    }

    fn forget_project(&mut self, item: &PaletteItem, cx: &mut Context<Self>) {
        let Some(path) = item.id.strip_prefix(LISTED) else {
            return;
        };
        let Some(project) = self
            .state
            .read(cx)
            .store()
            .workspace()
            .projects()
            .into_iter()
            .map(|p| p.id())
            .find(|p| p.path() == Path::new(path))
        else {
            return;
        };
        if self.presentation.project.as_ref().ok() == Some(&project)
            || self.shown_elsewhere(cx).contains(&project)
        {
            return;
        }
        match self
            .state
            .update(cx, |state, _| state.forget_project(&project))
        {
            Ok(applied) => {
                self.error = crate::durability_warning(applied.durability);
                if let Some(list) = &self.projects {
                    list.state
                        .update(cx, |state, cx| state.remove_item(&item.id, cx));
                }
            }
            Err(error) => self.error = Some(format!("Project not removed. {error}")),
        }
        self.refresh(cx);
    }

    pub(crate) fn projects_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let list = self.projects.as_ref()?;
        let dismiss = self.scoped_listener(cx, |this, _: &(), window, cx| {
            this.close_projects(window, cx)
        });
        Some(
            CommandPalette::new("projects", &list.state)
                .empty_label("No matches. Type a path.")
                .on_confirm(
                    self.scoped_listener(cx, |this, item: &PaletteItem, window, cx| {
                        this.confirm_project(item, window, cx)
                    }),
                )
                .on_remove(self.scoped_listener(cx, |this, item: &PaletteItem, _, cx| {
                    this.forget_project(item, cx)
                }))
                .on_dismiss(move |window, cx| dismiss(&(), window, cx)),
        )
    }

    /// The current project at the top of the sidebar, which opens the project list.
    pub(crate) fn project_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let (name, tooltip, missing) = match &self.presentation.project {
            Ok(project) => (
                project.name().to_owned(),
                format!("Switch project · {}", display_path(project.path())),
                !project.path().is_dir(),
            ),
            Err(error) => (
                "No project".to_owned(),
                format!("Switch project · {error}"),
                true,
            ),
        };
        h_flex()
            .id("project-switcher")
            .automation_id("project-switcher")
            .flex_shrink_0()
            .mx_2()
            .mb_1()
            .h(cx.theme().density.control(px(30.)))
            .px(px(8.))
            .gap(px(6.))
            .rounded(px(6.))
            .cursor_pointer()
            .when(self.projects.is_some(), |d| d.bg(colors.element_hover))
            .hover(|d| d.bg(colors.element_hover))
            .when(self.projects.is_none(), |d| {
                d.tooltip(Tooltip::with_key(tooltip, "Ctrl+O").builder())
            })
            .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                this.open_projects(None, window, cx)
            }))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(name)
                        .size(TextSize::Ui)
                        .weight(FontWeight::MEDIUM)
                        .truncate(),
                ),
            )
            .children(missing.then(|| {
                Label::new("Folder missing")
                    .size(TextSize::Sm)
                    .color(Color::Warning)
            }))
            .child(
                Icon::new(IconName::Chevron)
                    .size(px(14.))
                    .color(Color::Muted),
            )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "projects_tests.rs"]
mod tests;
