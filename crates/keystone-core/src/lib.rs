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

#[derive(Debug)]
pub struct Applied<T> {
    pub value: T,
    pub durability: Durability,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Durability {
    Synced,
    DirectorySyncPending(String),
}

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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Session {
    id: u64,
    title: String,
    provider: Provider,
    settled: bool,
    draft: String,
    /// The provider's own conversation ID, used to resume after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_thread: Option<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    approval_mode: ApprovalMode,
    /// Whether Keystone sent this session's provider an approval override. Providers keep
    /// an override on the conversation, so returning to the provider's own settings must
    /// restore them explicitly, even after a restart.
    #[serde(default, skip_serializing_if = "is_default")]
    approval_overridden: bool,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// How much a session's agent may do before asking. Each provider maps a mode to its own
/// settings; see `docs/design/providers.md`. The default sends nothing, so the user's own
/// provider configuration decides, as it did before modes existed.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ApprovalMode {
    #[default]
    Provider,
    ReadOnly,
    Ask,
    Auto,
    FullAccess,
}

impl ApprovalMode {
    pub const ALL: [ApprovalMode; 5] = [
        ApprovalMode::Provider,
        ApprovalMode::ReadOnly,
        ApprovalMode::Ask,
        ApprovalMode::Auto,
        ApprovalMode::FullAccess,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Provider => "Provider settings",
            Self::ReadOnly => "Read only",
            Self::Ask => "Ask first",
            Self::Auto => "Auto",
            Self::FullAccess => "Full access",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Provider => "Use your own Codex or Claude Code configuration",
            Self::ReadOnly => "Reads files; asks before any change",
            Self::Ask => "Asks before commands not known to be safe",
            Self::Auto => "Edits and runs in the project; asks to go further",
            Self::FullAccess => "Never asks, with no sandbox",
        }
    }

    /// A short stable name, used in automation selectors.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::FullAccess => "full-access",
        }
    }
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

    pub fn provider_thread(&self) -> Option<&str> {
        self.provider_thread.as_deref()
    }

    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode
    }

    pub fn approval_overridden(&self) -> bool {
        self.approval_overridden
    }
}

/// Which palette to use. `System` follows the window's light or dark appearance.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum Density {
    Compact,
    #[default]
    Default,
    Spacious,
}

/// Appearance settings. Missing fields load as defaults, so older state files stay valid.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub theme: ThemeMode,
    pub density: Density,
    /// The interface font family. `None` uses the built-in default.
    pub ui_font: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Workspace {
    sessions: Vec<Session>,
    selected: Option<u64>,
    #[serde(default)]
    preferences: Preferences,
}

impl Workspace {
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn selected(&self) -> Option<u64> {
        self.selected
    }

    pub fn preferences(&self) -> &Preferences {
        &self.preferences
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
    pending_directory: Option<File>,
    writes: u64,
    #[cfg(test)]
    sync_directory: fn(&File) -> io::Result<()>,
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
                    pending_directory: None,
                    writes: 0,
                    #[cfg(test)]
                    sync_directory: File::sync_all,
                });
            }
            Err(error) => return Err(error.into()),
        };
        validate(&snapshot)?;
        Ok(Self {
            path,
            workspace: snapshot.workspace,
            next_id: snapshot.next_id,
            pending_directory: None,
            writes: 0,
            #[cfg(test)]
            sync_directory: File::sync_all,
        })
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// The workspace file this store saves to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many times this store has written the workspace file since it opened.
    #[doc(hidden)]
    pub fn writes(&self) -> u64 {
        self.writes
    }

    pub fn create(&mut self, provider: Provider) -> StoreResult<Applied<u64>> {
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
            provider_thread: None,
            approval_mode: ApprovalMode::default(),
            approval_overridden: false,
        });
        next.selected = Some(id);
        let applied = self.commit(next, next_id)?;
        Ok(Applied {
            value: id,
            durability: applied.durability,
        })
    }

    pub fn select(&mut self, id: u64) -> StoreResult<Applied<()>> {
        if !self
            .workspace
            .sessions
            .iter()
            .any(|session| session.id == id)
        {
            return Err(invalid(format!("unknown session ID {id}")).into());
        }
        if self.workspace.selected == Some(id) {
            return Ok(self.finish_sync());
        }
        let mut next = self.workspace.clone();
        next.selected = Some(id);
        self.commit(next, self.next_id)
    }

    pub fn set_settled(&mut self, id: u64, settled: bool) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.settled = settled)
    }

    pub fn set_preferences(&mut self, preferences: Preferences) -> StoreResult<Applied<()>> {
        if self.workspace.preferences == preferences {
            return Ok(self.finish_sync());
        }
        let mut next = self.workspace.clone();
        next.preferences = preferences;
        self.commit(next, self.next_id)
    }

    pub fn set_provider_thread(&mut self, id: u64, thread: String) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.provider_thread = Some(thread))
    }

    /// Takes effect from the session's next turn. Any mode but the default also records,
    /// in the same write, that the provider may now hold an override, so the record is
    /// saved before an override can be sent. The record is never cleared: returning to
    /// the default then restores the provider's own settings explicitly.
    pub fn set_approval_mode(&mut self, id: u64, mode: ApprovalMode) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| {
            session.approval_mode = mode;
            session.approval_overridden |= mode != ApprovalMode::Provider;
        })
    }

    /// Records that the provider may hold an approval override for this session.
    pub fn set_approval_overridden(&mut self, id: u64) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.approval_overridden = true)
    }

    pub fn set_draft(&mut self, id: u64, text: String) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.draft = text)
    }

    fn update_session(
        &mut self,
        id: u64,
        change: impl FnOnce(&mut Session),
    ) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        let session = next
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .ok_or_else(|| invalid(format!("unknown session ID {id}")))?;
        change(session);
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    pub fn sync_pending(&mut self) -> io::Result<()> {
        if let Some(directory) = &self.pending_directory {
            #[cfg(test)]
            (self.sync_directory)(directory)?;
            #[cfg(not(test))]
            directory.sync_all()?;
            self.pending_directory = None;
        }
        Ok(())
    }

    fn finish_sync(&mut self) -> Applied<()> {
        Applied {
            value: (),
            durability: match self.sync_pending() {
                Ok(()) => Durability::Synced,
                Err(error) => Durability::DirectorySyncPending(error.to_string()),
            },
        }
    }

    fn commit(&mut self, workspace: Workspace, next_id: u64) -> StoreResult<Applied<()>> {
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
        let directory = File::open(parent)?;
        let mut file = NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, &snapshot)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        self.writes += 1;
        self.workspace = snapshot.workspace;
        self.next_id = snapshot.next_id;
        self.pending_directory = Some(directory);
        Ok(self.finish_sync())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_sync_failure_keeps_applied_snapshot_and_retry_does_not_duplicate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store.sync_directory = |_| Err(io::Error::other("directory sync unavailable"));

        let created = store.create(Provider::Claude).unwrap();
        assert_eq!(created.value, 1);
        assert_eq!(
            created.durability,
            Durability::DirectorySyncPending("directory sync unavailable".into())
        );
        assert_eq!(store.workspace().selected(), Some(1));
        assert_eq!(store.workspace().sessions().len(), 1);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.workspace().selected(), Some(1));
        assert_eq!(reopened.workspace().sessions()[0].id(), 1);
        assert_eq!(reopened.workspace().sessions().len(), 1);
        assert_eq!(reopened.next_id, 2);
        assert!(matches!(
            store.select(1).unwrap().durability,
            Durability::DirectorySyncPending(_)
        ));
        assert!(matches!(
            store.set_draft(1, String::new()).unwrap().durability,
            Durability::DirectorySyncPending(_)
        ));
        let before_retry = std::fs::read(&path).unwrap();
        assert!(store.sync_pending().is_err());
        store.sync_directory = File::sync_all;
        store.sync_pending().unwrap();
        store.sync_pending().unwrap();
        assert!(store.pending_directory.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before_retry);
        assert_eq!(store.workspace().sessions().len(), 1);
        let next = store.create(Provider::Codex).unwrap();
        assert_eq!(next.value, 2);
        assert_eq!(next.durability, Durability::Synced);
        assert_eq!(Store::open(&path).unwrap().workspace().sessions().len(), 2);
    }
}
