use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

type StoreResult<T> = Result<T, Box<dyn Error>>;

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Provider {
    Claude,
    Codex,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claude => f.write_str("Claude"),
            Self::Codex => f.write_str("Codex"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Session {
    id: u64,
    title: String,
    provider: Provider,
    settled: bool,
    draft: String,
}

impl Session {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn settled(&self) -> bool {
        self.settled
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Workspace {
    sessions: Vec<Session>,
    selected: Option<u64>,
}

impl Workspace {
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn selected(&self) -> Option<u64> {
        self.selected
    }
}

#[derive(Deserialize, Serialize)]
struct Snapshot {
    version: u32,
    next_id: u64,
    workspace: Workspace,
}

pub struct Store {
    path: PathBuf,
    workspace: Workspace,
    next_id: u64,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref().to_path_buf();
        let snapshot: Snapshot = match File::open(&path) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    workspace: Workspace::default(),
                    next_id: 1,
                });
            }
            Err(error) => return Err(error.into()),
        };
        validate(&snapshot)?;
        Ok(Self {
            path,
            workspace: snapshot.workspace,
            next_id: snapshot.next_id,
        })
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn create(&mut self, provider: Provider) -> StoreResult<u64> {
        let id = self.next_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| invalid("session ID exhausted"))?;
        let mut next = self.workspace.clone();
        next.sessions.push(Session {
            id,
            title: format!("New {provider} session"),
            provider,
            settled: false,
            draft: String::new(),
        });
        next.selected = Some(id);
        self.commit(next, next_id)?;
        Ok(id)
    }

    pub fn select(&mut self, id: u64) -> StoreResult<()> {
        if !self
            .workspace
            .sessions
            .iter()
            .any(|session| session.id == id)
        {
            return Err(invalid(format!("unknown session ID {id}")).into());
        }
        if self.workspace.selected == Some(id) {
            return Ok(());
        }
        let mut next = self.workspace.clone();
        next.selected = Some(id);
        self.commit(next, self.next_id)
    }

    pub fn set_settled(&mut self, id: u64, settled: bool) -> StoreResult<()> {
        self.update_session(id, |session| session.settled = settled)
    }

    pub fn set_draft(&mut self, id: u64, text: String) -> StoreResult<()> {
        self.update_session(id, |session| session.draft = text)
    }

    fn update_session(&mut self, id: u64, change: impl FnOnce(&mut Session)) -> StoreResult<()> {
        let mut next = self.workspace.clone();
        let session = next
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .ok_or_else(|| invalid(format!("unknown session ID {id}")))?;
        change(session);
        self.commit(next, self.next_id)
    }

    fn commit(&mut self, workspace: Workspace, next_id: u64) -> StoreResult<()> {
        let snapshot = Snapshot {
            version: FORMAT_VERSION,
            next_id,
            workspace,
        };
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut file = NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, &snapshot)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        self.workspace = snapshot.workspace;
        self.next_id = snapshot.next_id;
        Ok(())
    }
}

fn validate(snapshot: &Snapshot) -> StoreResult<()> {
    if snapshot.version != FORMAT_VERSION {
        return Err(invalid(format!(
            "unsupported state version {} (expected {FORMAT_VERSION})",
            snapshot.version
        ))
        .into());
    }
    if snapshot.next_id == 0 {
        return Err(invalid("next session ID must be positive").into());
    }
    let mut ids = HashSet::new();
    for session in &snapshot.workspace.sessions {
        if session.id == 0 || session.id >= snapshot.next_id || !ids.insert(session.id) {
            return Err(invalid("invalid or duplicate session ID").into());
        }
    }
    if snapshot
        .workspace
        .selected
        .is_some_and(|selected| !ids.contains(&selected))
    {
        return Err(invalid("selected session does not exist").into());
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
