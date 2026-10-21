use crate::composition::SavedComposition;
use crate::windows::WindowId;
use crate::{Applied, Store, StoreResult, Workspace, invalid};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectId(String);

impl ProjectId {
    pub fn new(directory: &Path) -> io::Result<Self> {
        let canonical = directory.canonicalize().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot resolve project directory {}: {error}",
                    directory.display()
                ),
            )
        })?;
        if !canonical.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("project path {} is not a directory", canonical.display()),
            ));
        }
        canonical
            .into_os_string()
            .into_string()
            .map(Self)
            .map_err(|path| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "project directory {} is not valid UTF-8",
                        Path::new(&path).display()
                    ),
                )
            })
    }

    /// The project a session's recorded folder would be, as it was resolved when the
    /// session recorded it, so a folder that has since gone still names one.
    pub fn of(directory: &crate::WorkingDirectory) -> Self {
        Self(directory.as_str().to_owned())
    }

    pub fn path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// The folder's own name, or the whole path for the root.
    pub fn name(&self) -> &str {
        self.path()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&self.0)
    }

    /// Whether `directory` is this project's folder or inside it.
    pub fn contains(&self, directory: &Path) -> bool {
        directory.starts_with(self.path())
    }

    pub(crate) fn as_saved(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_saved(saved: &str) -> Self {
        Self(saved.to_owned())
    }
}

/// A folder the user opened as a project, listed in its own projection. Forgetting
/// one only marks this entry removed: its folder, its sessions and its layouts stay,
/// and the mark keeps its sessions' folder from being offered again.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Project {
    path: String,
    /// Unix milliseconds. Projects are listed most recent first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_opened: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    removed: bool,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

impl Project {
    pub(crate) fn new(id: &ProjectId, last_opened: u64) -> Self {
        Self {
            path: id.0.clone(),
            last_opened: Some(last_opened),
            removed: false,
            unknown: Map::new(),
        }
    }

    pub fn id(&self) -> ProjectId {
        ProjectId(self.path.clone())
    }

    pub fn last_opened(&self) -> Option<u64> {
        self.last_opened
    }

    /// Whether the user took this project off the list.
    pub fn removed(&self) -> bool {
        self.removed
    }

    pub(crate) fn opened(&mut self, now: u64) {
        self.last_opened = Some(now);
        self.removed = false;
    }

    pub(crate) fn is(&self, id: &ProjectId) -> bool {
        self.path == id.0
    }
}

/// What every project saved under one preference, by project directory. A value
/// that is not a map is kept as is and reads as empty.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum ProjectMap<T> {
    Saved(BTreeMap<String, T>),
    Unexpected(Value),
}

pub(crate) type ProjectLayouts = ProjectMap<SavedComposition>;

impl<T> Default for ProjectMap<T> {
    fn default() -> Self {
        Self::Saved(BTreeMap::new())
    }
}

impl<T: Clone> ProjectMap<T> {
    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Self::Saved(saved) if saved.is_empty())
    }

    pub(crate) fn get(&self, project: &ProjectId) -> Option<&T> {
        match self {
            Self::Saved(saved) => saved.get(&project.0),
            Self::Unexpected(_) => None,
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (ProjectId, &T)> {
        let saved = match self {
            Self::Saved(saved) => Some(saved),
            Self::Unexpected(_) => None,
        };
        saved
            .into_iter()
            .flatten()
            .map(|(project, value)| (ProjectId(project.clone()), value))
    }

    /// Takes `heir`'s value for each of `projects`, none when `heir` cannot be read, and
    /// keeps this map's for the rest. When this map cannot be read, `heir` replaces it.
    pub(crate) fn inherit(&mut self, heir: Self, projects: &[ProjectId]) {
        let Self::Saved(mine) = self else {
            *self = heir;
            return;
        };
        for project in projects {
            match heir.get(project) {
                Some(value) => mine.insert(project.0.clone(), value.clone()),
                None => mine.remove(&project.0),
            };
        }
    }

    pub(crate) fn set(&mut self, project: &ProjectId, value: Option<T>) -> io::Result<()> {
        let Self::Saved(saved) = self else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "saved project preferences are not a map",
            ));
        };
        match value {
            Some(value) => saved.insert(project.0.clone(), value),
            None => saved.remove(&project.0),
        };
        Ok(())
    }
}

impl Workspace {
    /// Lists `project` as opened at `now` unless it is listed already.
    pub(crate) fn list_project(&mut self, project: &ProjectId, now: u64) {
        match self.projects.iter_mut().find(|p| p.is(project)) {
            Some(listed) if listed.removed => listed.opened(now),
            Some(_) => {}
            None => self.projects.push(Project::new(project, now)),
        }
    }
}

impl Store {
    /// Shows `project` in `window` with `selected`, or nothing, and lists the project as
    /// opened at `now`, in one write. When the saved windows are from a newer version,
    /// the first window's project is left out and they stay as they are. `left`, the project the window showed before,
    /// joins the list just before it when it is not listed yet, so leaving a project
    /// never loses it.
    pub fn open_project(
        &mut self,
        window: WindowId,
        project: &ProjectId,
        left: Option<&ProjectId>,
        selected: Option<u64>,
        now: u64,
    ) -> StoreResult<Applied<()>> {
        if let Some(id) = selected.filter(|&id| self.workspace.sessions.iter().all(|s| s.id != id))
        {
            return Err(invalid(format!("unknown session ID {id}")).into());
        }
        let mut next = self.workspace.clone();
        match next.preferences.windows.entry(window) {
            Ok(saved) => saved.project = Some(project.0.clone()),
            Err(_) if window == WindowId::FIRST => {}
            Err(error) => return Err(error),
        }
        *next.shown_mut(window)?.selected = selected;
        if let Some(left) = left.filter(|left| *left != project) {
            next.list_project(left, now.saturating_sub(1));
        }
        match next.projects.iter_mut().find(|p| p.is(project)) {
            Some(listed) => listed.opened(now),
            None => next.projects.push(Project::new(project, now)),
        }
        self.commit(next, self.next_id)
    }

    /// Takes `project` off the list. Nothing on disk changes, and its sessions and
    /// layouts stay saved, so opening the folder again brings them back.
    pub fn forget_project(&mut self, project: &ProjectId) -> StoreResult<Applied<()>> {
        if !self
            .workspace
            .projects
            .iter()
            .any(|p| p.is(project) && !p.removed)
        {
            return Err(invalid(format!(
                "{} is not on the project list",
                project.path().display()
            ))
            .into());
        }
        let mut next = self.workspace.clone();
        if let Some(listed) = next.projects.iter_mut().find(|p| p.is(project)) {
            listed.removed = true;
        }
        self.commit(next, self.next_id)
    }
}

#[cfg(test)]
#[path = "project_tests.rs"]
mod tests;
