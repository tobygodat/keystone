//! The desktop's windows. The first window keeps what Keystone saved before it had
//! several: each project's layout and tabs, and the workspace selection. Every other
//! window saves its own, so each window shows its own sessions while they all share
//! one workspace.

use crate::composition::{Component, Composition, SavedComposition, ViewId};
use crate::project::{ProjectId, ProjectLayouts, ProjectMap};
use crate::tabs::{SavedTabs, SessionTabs};
use crate::{Applied, Preferences, Store, StoreResult, Workspace, invalid};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct WindowId(pub u32);

impl WindowId {
    /// The window that keeps what Keystone saved before it had several.
    pub const FIRST: Self = Self(0);
}

/// Where a window was and how big it was, in logical pixels. A compositor that does
/// not tell clients where their windows are reports the origin as zero.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct Placement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub maximized: bool,
}

/// One window as saved. The first window's entry holds only its placement and
/// project.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct SavedWindow {
    id: WindowId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placement: Option<Placement>,
    /// The project the window shows, by directory. Each window shows any project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected: Option<u64>,
    #[serde(default, skip_serializing_if = "ProjectMap::is_empty")]
    layouts: ProjectLayouts,
    #[serde(default, skip_serializing_if = "ProjectMap::is_empty")]
    tabs: ProjectMap<SavedTabs>,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

impl SavedWindow {
    fn new(id: WindowId) -> Self {
        Self {
            id,
            placement: None,
            project: None,
            selected: None,
            layouts: ProjectLayouts::default(),
            tabs: ProjectMap::default(),
            unknown: Map::new(),
        }
    }
}

/// The windows open when Keystone last saved, in the order they opened, kept as is
/// when this version cannot read them.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum SavedWindows {
    Supported(Vec<SavedWindow>),
    Unsupported(Value),
}

impl Default for SavedWindows {
    fn default() -> Self {
        Self::Supported(Vec::new())
    }
}

impl SavedWindows {
    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Self::Supported(windows) if windows.is_empty())
    }

    fn get(&self, id: WindowId) -> Option<&SavedWindow> {
        match self {
            Self::Supported(windows) => windows.iter().find(|w| w.id == id),
            Self::Unsupported(_) => None,
        }
    }

    fn list_mut(&mut self) -> StoreResult<&mut Vec<SavedWindow>> {
        match self {
            Self::Supported(windows) => Ok(windows),
            Self::Unsupported(_) => {
                Err(invalid("saved windows are from a newer version of Keystone").into())
            }
        }
    }

    fn entry(&mut self, id: WindowId) -> StoreResult<&mut SavedWindow> {
        let windows = self.list_mut()?;
        match windows.iter().position(|w| w.id == id) {
            Some(index) => Ok(&mut windows[index]),
            None if id == WindowId::FIRST => {
                windows.insert(0, SavedWindow::new(id));
                Ok(&mut windows[0])
            }
            None => Err(invalid(format!("unknown window {}", id.0)).into()),
        }
    }
}

/// What one window shows of one project: its layout when it changes, which session each
/// conversation view holds, and its selection.
#[derive(Clone, Debug)]
pub struct WindowView {
    pub window: WindowId,
    pub project: ProjectId,
    pub layout: Option<Composition>,
    pub tabs: SessionTabs,
    pub selected: Option<u64>,
}

/// Where one window keeps its layouts, its tabs and its selection.
pub(crate) struct Shown<'a> {
    pub(crate) layouts: &'a mut ProjectLayouts,
    pub(crate) tabs: &'a mut ProjectMap<SavedTabs>,
    pub(crate) selected: &'a mut Option<u64>,
}

impl Shown<'_> {
    /// Drops `project`'s tabs for views its layout no longer has, so a view id used
    /// again never inherits a session.
    pub(crate) fn forget_closed_tabs(&mut self, project: &ProjectId, sidebar: &Composition) {
        let Some(SavedTabs::Supported(tabs)) = self.tabs.get(project) else {
            return;
        };
        let layout = self
            .layouts
            .get(project)
            .map_or_else(|| sidebar.clone(), SavedComposition::shown);
        let mut tabs = tabs.clone();
        tabs.retain(|view, _| layout.component(view) == Some(Component::Conversation));
        let _ = self.tabs.set(
            project,
            (!tabs.is_empty()).then_some(SavedTabs::Supported(tabs)),
        );
    }
}

impl Preferences {
    fn maps(&self, window: WindowId) -> Option<(&ProjectLayouts, &ProjectMap<SavedTabs>)> {
        if window == WindowId::FIRST {
            return Some((&self.project_layouts, &self.project_tabs));
        }
        let saved = self.windows.get(window)?;
        Some((&saved.layouts, &saved.tabs))
    }

    /// The layout `window` saved for `project`, readable or not.
    pub fn layout_in(&self, project: &ProjectId, window: WindowId) -> Option<&SavedComposition> {
        self.maps(window)?.0.get(project)
    }

    /// Every project's layout `window` saved.
    pub fn layouts_in(
        &self,
        window: WindowId,
    ) -> impl Iterator<Item = (ProjectId, &SavedComposition)> {
        self.maps(window)
            .into_iter()
            .flat_map(|(layouts, _)| layouts.iter())
    }

    /// The composition `window` draws for `project`. Until it saves its own, a window
    /// shows the sidebar layout every project shared before compositions existed.
    pub fn composition_in(&self, project: &ProjectId, window: WindowId) -> Composition {
        match self
            .maps(window)
            .and_then(|(layouts, _)| layouts.get(project))
        {
            Some(saved) => saved.shown(),
            None => Composition::from_sidebar(self.sidebar_layout.shown()),
        }
    }

    /// The session `view` of `project` holds in `window`, without copying the tabs.
    pub fn held_in(&self, project: &ProjectId, window: WindowId, view: ViewId) -> Option<u64> {
        match self.maps(window)?.1.get(project)? {
            SavedTabs::Supported(tabs) => tabs.get(view),
            SavedTabs::Unsupported(_) => None,
        }
    }

    /// Which session each of `project`'s conversation views holds in `window`.
    pub fn session_tabs_in(&self, project: &ProjectId, window: WindowId) -> SessionTabs {
        self.maps(window)
            .and_then(|(_, tabs)| tabs.get(project))
            .map(SavedTabs::shown)
            .unwrap_or_default()
    }

    /// The project `window` last showed, when it saved one.
    pub fn window_project(&self, window: WindowId) -> Option<ProjectId> {
        let saved = self.windows.get(window)?.project.as_deref()?;
        Some(ProjectId::from_saved(saved))
    }

    /// The windows to open, in the order they first opened, the first window always
    /// first. Windows this version cannot read are left closed.
    pub fn windows(&self) -> Vec<(WindowId, Option<Placement>)> {
        let mut windows = vec![(
            WindowId::FIRST,
            self.windows.get(WindowId::FIRST).and_then(|w| w.placement),
        )];
        if let SavedWindows::Supported(saved) = &self.windows {
            for window in saved {
                if windows.iter().all(|(id, _)| *id != window.id) {
                    windows.push((window.id, window.placement));
                }
            }
        }
        windows
    }
}

impl Workspace {
    /// The session `window` shows and its composer sends to.
    pub fn selected_in(&self, window: WindowId) -> Option<u64> {
        let selected = if window == WindowId::FIRST {
            self.selected
        } else {
            self.preferences.windows.get(window)?.selected
        };
        selected.filter(|&id| self.sessions.iter().any(|s| s.id == id))
    }

    pub(crate) fn shown_mut(&mut self, window: WindowId) -> StoreResult<Shown<'_>> {
        if window == WindowId::FIRST {
            return Ok(Shown {
                layouts: &mut self.preferences.project_layouts,
                tabs: &mut self.preferences.project_tabs,
                selected: &mut self.selected,
            });
        }
        let saved = self.preferences.windows.entry(window)?;
        Ok(Shown {
            layouts: &mut saved.layouts,
            tabs: &mut saved.tabs,
            selected: &mut saved.selected,
        })
    }

    fn exists(&self, session: u64) -> bool {
        self.sessions.iter().any(|s| s.id == session)
    }

    /// Records what `view.window` shows. Tabs holding a session that no longer exists are
    /// dropped, and tabs this version cannot read are kept until a view holds a session
    /// again.
    pub(crate) fn show(&mut self, view: WindowView) -> StoreResult<()> {
        let WindowView {
            window,
            project,
            layout,
            mut tabs,
            selected,
        } = view;
        if let Some(id) = selected.filter(|&id| !self.exists(id)) {
            return Err(invalid(format!("unknown session ID {id}")).into());
        }
        let sessions: Vec<u64> = self.sessions.iter().map(|s| s.id).collect();
        tabs.retain(|_, session| sessions.contains(&session));
        let shown = self.shown_mut(window)?;
        if let Some(layout) = layout {
            shown.layouts.set(&project, Some(layout.into()))?;
        }
        let unreadable = matches!(shown.tabs.get(&project), Some(SavedTabs::Unsupported(_)));
        if !(unreadable && tabs.is_empty()) {
            shown.tabs.set(
                &project,
                (!tabs.is_empty()).then_some(SavedTabs::Supported(tabs)),
            )?;
        }
        *shown.selected = selected;
        Ok(())
    }

    /// Adds a window showing `selected`, or nothing, and returns its id.
    fn add_window(
        &mut self,
        project: Option<&ProjectId>,
        selected: Option<u64>,
        placement: Option<Placement>,
    ) -> StoreResult<WindowId> {
        if let Some(id) = selected.filter(|&id| !self.exists(id)) {
            return Err(invalid(format!("unknown session ID {id}")).into());
        }
        let windows = self.preferences.windows.list_mut()?;
        let id = windows
            .iter()
            .map(|w| w.id.0)
            .max()
            .unwrap_or(WindowId::FIRST.0)
            .checked_add(1)
            .map(WindowId)
            .ok_or_else(|| invalid("window ID exhausted"))?;
        windows.push(SavedWindow {
            placement,
            project: project.map(|p| p.as_saved().to_owned()),
            selected,
            ..SavedWindow::new(id)
        });
        Ok(id)
    }
}

impl Store {
    /// Saves a new window showing `selected`, or nothing, in one write, and returns its
    /// id. Fails without writing when the saved windows are from a newer version.
    pub fn open_window(
        &mut self,
        project: Option<&ProjectId>,
        selected: Option<u64>,
        placement: Option<Placement>,
    ) -> StoreResult<Applied<WindowId>> {
        let mut next = self.workspace.clone();
        let id = next.add_window(project, selected, placement)?;
        let applied = self.commit(next, self.next_id)?;
        Ok(Applied {
            value: id,
            durability: applied.durability,
            revision: applied.revision,
        })
    }

    /// Saves what two windows show after a tab moved from `from.window` to `to.window`, in
    /// one write, so a crash leaves the session's tab in exactly one of them.
    pub fn move_tab(&mut self, from: WindowView, to: WindowView) -> StoreResult<Applied<()>> {
        if from.window == to.window {
            return Err(invalid("a tab moves between two windows").into());
        }
        let mut next = self.workspace.clone();
        next.show(from)?;
        next.show(to)?;
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    /// Saves what `from.window` shows once a tab left it and a new window showing
    /// `session`, in one write, and returns the new window's id.
    pub fn move_tab_to_new_window(
        &mut self,
        from: WindowView,
        session: u64,
        placement: Option<Placement>,
    ) -> StoreResult<Applied<WindowId>> {
        let mut next = self.workspace.clone();
        let project = from.project.clone();
        next.show(from)?;
        let id = next.add_window(Some(&project), Some(session), placement)?;
        let applied = self.commit(next, self.next_id)?;
        Ok(Applied {
            value: id,
            durability: applied.durability,
            revision: applied.revision,
        })
    }

    /// Forgets `window` in one write, and with it its layouts, tabs and selection. Its
    /// sessions are left as they are. When the first window closes, the window that
    /// opened next takes its place, with everything it showed, and its id is returned.
    /// The last window cannot close; it stays saved so the next launch opens it.
    pub fn close_window(&mut self, window: WindowId) -> StoreResult<Applied<Option<WindowId>>> {
        let mut next = self.workspace.clone();
        let windows = next.preferences.windows.list_mut()?;
        if window != WindowId::FIRST && windows.iter().all(|w| w.id != window) {
            return Err(invalid(format!("unknown window {}", window.0)).into());
        }
        let Some(heir) = windows
            .iter()
            .position(|w| w.id != window && w.id != WindowId::FIRST)
            .filter(|_| window == WindowId::FIRST)
            .map(|index| windows.remove(index))
        else {
            if window == WindowId::FIRST {
                return Err(invalid("the last window stays open").into());
            }
            windows.retain(|w| w.id != window);
            let applied = self.commit(next, self.next_id)?;
            return Ok(Applied {
                value: None,
                durability: applied.durability,
                revision: applied.revision,
            });
        };
        let first = next.preferences.windows.entry(WindowId::FIRST)?;
        first.placement = heir.placement;
        first.project = heir.project.clone();
        first.unknown.extend(heir.unknown);
        // The heir's layouts and tabs win for every project it showed, its current one
        // included when it kept the default layout there; the first window's stay for
        // every other project, so closing it loses no layout.
        let mut shown: Vec<ProjectId> = heir.layouts.iter().map(|(project, _)| project).collect();
        shown.extend(heir.tabs.iter().map(|(project, _)| project));
        shown.extend(heir.project.as_deref().map(ProjectId::from_saved));
        next.preferences
            .project_layouts
            .inherit(heir.layouts, &shown);
        next.preferences.project_tabs.inherit(heir.tabs, &shown);
        next.selected = heir.selected.filter(|&id| next.exists(id));
        let applied = self.commit(next, self.next_id)?;
        Ok(Applied {
            value: Some(heir.id),
            durability: applied.durability,
            revision: applied.revision,
        })
    }

    /// Saves the project `window` shows.
    pub fn set_window_project(
        &mut self,
        window: WindowId,
        project: &ProjectId,
    ) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        next.preferences.windows.entry(window)?.project = Some(project.as_saved().to_owned());
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    /// Saves where `window` is, how big it is and, when known, the project it shows.
    pub fn set_placement(
        &mut self,
        window: WindowId,
        placement: Placement,
        project: Option<&ProjectId>,
    ) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        let saved = next.preferences.windows.entry(window)?;
        saved.placement = Some(placement);
        if let Some(project) = project {
            saved.project = Some(project.as_saved().to_owned());
        }
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    /// Saves the drafts, and each window's placement and project, collected for Quit
    /// in one write. Saved windows from a newer version are kept as they are, without
    /// the windows, so they never stop Quit.
    pub fn save_for_quit(
        &mut self,
        drafts: Vec<(u64, String)>,
        windows: Vec<(WindowId, Placement, Option<ProjectId>)>,
    ) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        for (id, text) in drafts {
            next.sessions
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or_else(|| invalid(format!("unknown session ID {id}")))?
                .draft = text;
        }
        if let SavedWindows::Supported(_) = next.preferences.windows {
            for (window, placement, project) in windows {
                let saved = next.preferences.windows.entry(window)?;
                saved.placement = Some(placement);
                if let Some(project) = project {
                    saved.project = Some(project.as_saved().to_owned());
                }
            }
        }
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }
}

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
