use crate::ProjectId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::io;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
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
    LegacyMissing,
    Unsupported,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum SavedSessionContext {
    #[default]
    Missing,
    Recorded {
        directory: WorkingDirectory,
        origin: ContextOrigin,
    },
    Unsupported(Value),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordedContext {
    version: u32,
    directory: String,
    origin: ContextOrigin,
}

impl SavedSessionContext {
    pub(crate) fn recorded(directory: &WorkingDirectory, origin: ContextOrigin) -> Self {
        Self::Recorded {
            directory: directory.clone(),
            origin,
        }
    }

    pub(crate) fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    pub(crate) fn context(&self) -> SessionContext<'_> {
        match self {
            Self::Missing => SessionContext::LegacyMissing,
            Self::Recorded { directory, origin } => SessionContext::Recorded {
                directory,
                origin: *origin,
            },
            Self::Unsupported(_) => SessionContext::Unsupported,
        }
    }
}

impl<'de> Deserialize<'de> for SavedSessionContext {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        if let Ok(recorded) = serde_json::from_value::<RecordedContext>(raw.clone())
            && recorded.version == 1
            && Path::new(&recorded.directory).is_absolute()
        {
            return Ok(Self::Recorded {
                directory: WorkingDirectory(recorded.directory),
                origin: recorded.origin,
            });
        }
        Ok(Self::Unsupported(raw))
    }
}

impl Serialize for SavedSessionContext {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => serializer.serialize_none(),
            Self::Recorded { directory, origin } => RecordedContext {
                version: 1,
                directory: directory.as_str().to_owned(),
                origin: *origin,
            }
            .serialize(serializer),
            Self::Unsupported(raw) => raw.serialize(serializer),
        }
    }
}
