use crate::composition::SavedComposition;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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

    pub fn path(&self) -> &Path {
        Path::new(&self.0)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum ProjectLayouts {
    Saved(BTreeMap<String, SavedComposition>),
    Unexpected(Value),
}

impl Default for ProjectLayouts {
    fn default() -> Self {
        Self::Saved(BTreeMap::new())
    }
}

impl ProjectLayouts {
    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Self::Saved(layouts) if layouts.is_empty())
    }

    pub(crate) fn get(&self, project: &ProjectId) -> Option<&SavedComposition> {
        match self {
            Self::Saved(layouts) => layouts.get(&project.0),
            Self::Unexpected(_) => None,
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (ProjectId, &SavedComposition)> {
        let layouts = match self {
            Self::Saved(layouts) => Some(layouts),
            Self::Unexpected(_) => None,
        };
        layouts
            .into_iter()
            .flatten()
            .map(|(project, layout)| (ProjectId(project.clone()), layout))
    }

    pub(crate) fn set(
        &mut self,
        project: &ProjectId,
        layout: Option<SavedComposition>,
    ) -> io::Result<()> {
        let Self::Saved(layouts) = self else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "saved project layouts are not a map",
            ));
        };
        match layout {
            Some(layout) => layouts.insert(project.0.clone(), layout),
            None => layouts.remove(&project.0),
        };
        Ok(())
    }
}
