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

impl<T> ProjectMap<T> {
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
