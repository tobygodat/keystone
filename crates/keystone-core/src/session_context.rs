use crate::ProjectId;
use crate::machine::{MachineId, RemoteFolder};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::io;
use std::path::Path;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorkingDirectory(String);

impl WorkingDirectory {
    pub fn new(directory: &Path) -> io::Result<Self> {
        let project = ProjectId::new(directory)?;
        Ok(Self(
            project
                .path()
                .to_str()
                .expect("ProjectId requires lossless UTF-8")
                .to_owned(),
        ))
    }

    pub fn path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextOrigin {
    Created,
    Chosen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionContext<'a> {
    Recorded {
        directory: &'a WorkingDirectory,
        origin: ContextOrigin,
    },
    /// A folder on another machine. Nothing here may read it.
    Remote {
        machine: &'a MachineId,
        folder: &'a RemoteFolder,
    },
    LegacyMissing,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeContext {
    pub directory: WorkingDirectory,
    pub repository: WorkingDirectory,
    pub branch: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum SavedSessionContext {
    #[default]
    Missing,
    Recorded {
        directory: WorkingDirectory,
        origin: ContextOrigin,
        worktree: Option<WorktreeContext>,
    },
    Remote {
        machine: MachineId,
        folder: RemoteFolder,
    },
    Unsupported(Value),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordedContext {
    version: u32,
    directory: String,
    origin: ContextOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree: Option<RecordedWorktree>,
}

/// Version 3: a folder on another machine.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteContext {
    version: u32,
    machine: MachineId,
    directory: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordedWorktree {
    repository: String,
    branch: String,
}

impl SavedSessionContext {
    pub(crate) fn recorded(directory: &WorkingDirectory, origin: ContextOrigin) -> Self {
        Self::Recorded {
            directory: directory.clone(),
            origin,
            worktree: None,
        }
    }

    pub(crate) fn remote(machine: &MachineId, folder: &RemoteFolder) -> Self {
        Self::Remote {
            machine: machine.clone(),
            folder: folder.clone(),
        }
    }

    pub(crate) fn machine(&self) -> Option<&MachineId> {
        match self {
            Self::Remote { machine, .. } => Some(machine),
            _ => None,
        }
    }

    pub(crate) fn with_worktree(context: &WorktreeContext) -> Self {
        Self::Recorded {
            directory: context.directory.clone(),
            origin: ContextOrigin::Created,
            worktree: Some(context.clone()),
        }
    }

    pub(crate) fn worktree(&self) -> Option<&WorktreeContext> {
        match self {
            Self::Recorded { worktree, .. } => worktree.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    pub(crate) fn context(&self) -> SessionContext<'_> {
        match self {
            Self::Missing => SessionContext::LegacyMissing,
            Self::Recorded {
                directory, origin, ..
            } => SessionContext::Recorded {
                directory,
                origin: *origin,
            },
            Self::Remote { machine, folder } => SessionContext::Remote { machine, folder },
            Self::Unsupported(_) => SessionContext::Unsupported,
        }
    }
}

impl<'de> Deserialize<'de> for SavedSessionContext {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        if let Ok(remote) = serde_json::from_value::<RemoteContext>(raw.clone())
            && remote.version == 3
            && let Ok(folder) = RemoteFolder::new(&remote.directory)
            && folder.wire() == remote.directory
        {
            return Ok(Self::Remote {
                machine: remote.machine,
                folder: RemoteFolder::from_saved(remote.directory),
            });
        }
        if let Ok(recorded) = serde_json::from_value::<RecordedContext>(raw.clone())
            && ((recorded.version == 1 && raw.get("worktree").is_none())
                || (recorded.version == 2
                    && recorded.worktree.as_ref().is_some_and(|worktree| {
                        Path::new(&worktree.repository).is_absolute() && !worktree.branch.is_empty()
                    })))
            && Path::new(&recorded.directory).is_absolute()
        {
            let directory = WorkingDirectory(recorded.directory);
            let worktree = recorded.worktree.map(|worktree| WorktreeContext {
                directory: directory.clone(),
                repository: WorkingDirectory(worktree.repository),
                branch: worktree.branch,
            });
            return Ok(Self::Recorded {
                directory,
                origin: recorded.origin,
                worktree,
            });
        }
        Ok(Self::Unsupported(raw))
    }
}

impl Serialize for SavedSessionContext {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => serializer.serialize_none(),
            Self::Recorded {
                directory,
                origin,
                worktree,
            } => RecordedContext {
                version: if worktree.is_some() { 2 } else { 1 },
                directory: directory.as_str().to_owned(),
                origin: *origin,
                worktree: worktree.as_ref().map(|worktree| RecordedWorktree {
                    repository: worktree.repository.as_str().to_owned(),
                    branch: worktree.branch.clone(),
                }),
            }
            .serialize(serializer),
            Self::Remote { machine, folder } => RemoteContext {
                version: 3,
                machine: machine.clone(),
                directory: folder.wire().to_owned(),
            }
            .serialize(serializer),
            Self::Unsupported(raw) => raw.serialize(serializer),
        }
    }
}
