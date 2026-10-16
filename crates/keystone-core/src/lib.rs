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
    /// Unix milliseconds. Sessions saved before timestamps existed load as `None`
    /// and show no time; a time is never invented for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<u64>,
    /// When the session last changed: created, a message sent, or a turn ended.
    /// Draft saves leave it alone, so rows do not move while the user types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<u64>,
    /// When the user last saw the session's latest state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    viewed_at: Option<u64>,
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

    pub fn created_at(&self) -> Option<u64> {
        self.created_at
    }

    pub fn updated_at(&self) -> Option<u64> {
        self.updated_at
    }

    pub fn viewed_at(&self) -> Option<u64> {
        self.viewed_at
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

    fn leave_selected(&mut self, now: u64) {
        let selected = self.selected;
        if let Some(session) = self.sessions.iter_mut().find(|s| Some(s.id) == selected) {
            session.viewed_at = Some(now);
        }
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

    /// Creates a session and selects it. `now` is unix milliseconds.
    pub fn create(&mut self, provider: Provider, now: u64) -> StoreResult<Applied<u64>> {
        let id = self.next_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| invalid("session ID exhausted"))?;
        let mut next = self.workspace.clone();
        next.leave_selected(now);
        next.sessions.push(Session {
            id,
            title: format!("New {provider} session"),
            provider,
            settled: false,
            draft: String::new(),
            provider_thread: None,
            created_at: Some(now),
            updated_at: Some(now),
            viewed_at: Some(now),
        });
        next.selected = Some(id);
        let applied = self.commit(next, next_id)?;
        Ok(Applied {
            value: id,
            durability: applied.durability,
        })
    }

    /// Selects a session. The session being left counts as viewed at `now`. The newly
    /// selected one does not, so a reply it was showing stays unread until the user leaves.
    pub fn select(&mut self, id: u64, now: u64) -> StoreResult<Applied<()>> {
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
        next.leave_selected(now);
        next.selected = Some(id);
        self.commit(next, self.next_id)
    }

    /// Marks the selected session viewed, for when the app quits while showing it.
    pub fn leave(&mut self, now: u64) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        next.leave_selected(now);
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    /// Records a message sent from the selected session.
    pub fn record_message(&mut self, id: u64, now: u64) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| {
            session.updated_at = Some(now);
            session.viewed_at = Some(now);
        })
    }

    /// Records the end of a turn. The selected session's user sees it end.
    pub fn record_turn_end(&mut self, id: u64, now: u64) -> StoreResult<Applied<()>> {
        let selected = self.workspace.selected == Some(id);
        self.update_session(id, |session| {
            session.updated_at = Some(now);
            if selected {
                session.viewed_at = Some(now);
            }
        })
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

        let created = store.create(Provider::Claude, 1).unwrap();
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
            store.select(1, 2).unwrap().durability,
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
        let next = store.create(Provider::Codex, 3).unwrap();
        assert_eq!(next.value, 2);
        assert_eq!(next.durability, Durability::Synced);
        assert_eq!(Store::open(&path).unwrap().workspace().sessions().len(), 2);
    }

    #[test]
    fn timestamps_follow_create_send_turn_end_and_leave_but_not_drafts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        let times = |store: &Store, id: u64| {
            let session = &store.workspace().sessions()[id as usize - 1];
            (
                session.created_at(),
                session.updated_at(),
                session.viewed_at(),
            )
        };

        store.create(Provider::Codex, 1_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(1_000), Some(1_000)));
        store.set_draft(1, "typing".into()).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(1_000), Some(1_000)));
        store.record_message(1, 2_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(2_000), Some(2_000)));
        // A turn that ends while its session is selected is seen as it ends.
        store.record_turn_end(1, 3_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(3_000), Some(3_000)));

        // Creating a second session leaves the first.
        store.create(Provider::Claude, 4_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(3_000), Some(4_000)));
        // A turn ending elsewhere is unseen.
        store.record_turn_end(1, 5_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(5_000), Some(4_000)));
        // Selecting it does not count as viewing it; leaving it does.
        store.select(1, 6_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(5_000), Some(4_000)));
        assert_eq!(times(&store, 2), (Some(4_000), Some(4_000), Some(6_000)));
        store.leave(7_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(5_000), Some(7_000)));
        assert_eq!(times(&Store::open(&path).unwrap(), 1), times(&store, 1));
    }

    #[test]
    fn a_state_file_from_before_timestamps_loads_without_times() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"version":1,"next_id":2,"workspace":{"sessions":[{"id":1,"title":"Old","provider":"Codex","settled":false,"draft":"keep"}],"selected":1}}"#,
        )
        .unwrap();
        let mut store = Store::open(&path).unwrap();
        let session = &store.workspace().sessions()[0];
        assert_eq!(
            (
                session.created_at(),
                session.updated_at(),
                session.viewed_at()
            ),
            (None, None, None)
        );
        assert_eq!(session.draft(), "keep");

        // Saving it again writes no invented times.
        store.set_draft(1, "edited".into()).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("_at"), "{saved}");
        assert!(saved.contains(r#""version":1"#));
    }
}
