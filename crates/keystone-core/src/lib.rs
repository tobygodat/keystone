use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use unicode_segmentation::UnicodeSegmentation;

mod layout;

pub use layout::{
    DIVIDER_WIDTH, MAIN_MIN_WIDTH, SavedLayout, SidebarLayout, SidebarSide, SidebarWidth,
};

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

/// How a session's latest turn stands. Saved with the session, so a failed turn still
/// shows after a restart without reading every session's log.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LastTurn {
    Running,
    /// Completed, or ended by the user: Stop, or a denial that ends the turn.
    Finished,
    /// Failed, or interrupted by anything but the user.
    Failed,
}

/// Why a session needs the user, in priority order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Attention {
    Approval,
    Failed,
    Replied,
}

/// Where a session's title came from. Only a placeholder gives way to the first message,
/// so a rename is never overwritten. Files saved before titles changed load as a placeholder.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
enum TitleSource {
    #[default]
    Placeholder,
    FirstMessage,
    Renamed,
}

/// The longest title taken from a message, in user-perceived characters.
const TITLE_LENGTH: usize = 48;

/// `text` on one line with its whitespace collapsed, or `None` when it is blank.
fn one_line(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then_some(line)
}

/// A title for a session from its first message. Past [`TITLE_LENGTH`] it is cut, at a
/// word end when one falls in the second half, and ends in an ellipsis.
fn message_title(text: &str) -> Option<String> {
    let line = one_line(text)?;
    let Some((cut, _)) = line.grapheme_indices(true).nth(TITLE_LENGTH) else {
        return Some(line);
    };
    let head = &line[..cut];
    let head = match head.rfind(' ') {
        Some(space) if space >= cut / 2 => &head[..space],
        _ => head,
    };
    Some(format!("{}…", head.trim_end()))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Session {
    id: u64,
    title: String,
    #[serde(default, skip_serializing_if = "is_default")]
    title_source: TitleSource,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_turn: Option<LastTurn>,
    #[serde(default, skip_serializing_if = "is_default")]
    approval_mode: ApprovalMode,
    /// Whether Keystone sent this session's provider an approval override. Providers keep
    /// an override on the conversation, so returning to the provider's own settings must
    /// restore them explicitly, even after a restart.
    #[serde(default, skip_serializing_if = "is_default")]
    approval_overridden: bool,
    #[serde(flatten)]
    unknown: Map<String, Value>,
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

    pub fn created_at(&self) -> Option<u64> {
        self.created_at
    }

    pub fn updated_at(&self) -> Option<u64> {
        self.updated_at
    }

    pub fn viewed_at(&self) -> Option<u64> {
        self.viewed_at
    }

    pub fn last_turn(&self) -> Option<LastTurn> {
        self.last_turn
    }

    /// Never earlier than the last change, so a clock set back still clears it.
    fn mark_viewed(&mut self, now: u64) {
        self.viewed_at = Some(self.updated_at.map_or(now, |updated| updated.max(now)));
    }

    /// What the session needs from the user, if anything. `awaiting_approval` and
    /// `running` describe the live provider in this process. A turn saved as running
    /// that no live provider runs was cut off when Keystone closed, so it failed.
    pub fn attention(&self, awaiting_approval: bool, running: bool) -> Option<Attention> {
        let failed = match self.last_turn {
            Some(LastTurn::Failed) => true,
            Some(LastTurn::Running) => !running,
            _ => false,
        };
        let unseen = self
            .updated_at
            .is_some_and(|updated| self.viewed_at.is_none_or(|viewed| updated > viewed));
        if awaiting_approval {
            Some(Attention::Approval)
        } else if failed {
            Some(Attention::Failed)
        } else if unseen {
            Some(Attention::Replied)
        } else {
            None
        }
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

/// Missing fields load as defaults, so older state files stay valid.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: ThemeMode,
    pub density: Density,
    /// The interface font family. `None` uses the built-in default.
    pub ui_font: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Preferences {
    // Before `unknown`, so its keys are taken before the map collects the rest.
    #[serde(flatten)]
    appearance: Appearance,
    sidebar_layout: SavedLayout,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

impl Preferences {
    pub fn appearance(&self) -> &Appearance {
        &self.appearance
    }

    pub fn sidebar_layout(&self) -> &SavedLayout {
        &self.sidebar_layout
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Workspace {
    sessions: Vec<Session>,
    selected: Option<u64>,
    #[serde(default)]
    preferences: Preferences,
    #[serde(flatten)]
    unknown: Map<String, Value>,
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
            session.mark_viewed(now);
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Snapshot {
    version: u32,
    next_id: u64,
    workspace: Workspace,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

pub struct Store {
    path: PathBuf,
    workspace: Workspace,
    next_id: u64,
    unknown: Map<String, Value>,
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
                    unknown: Map::new(),
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
            unknown: snapshot.unknown,
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
            title_source: TitleSource::Placeholder,
            provider,
            settled: false,
            draft: String::new(),
            provider_thread: None,
            created_at: Some(now),
            updated_at: Some(now),
            viewed_at: Some(now),
            last_turn: None,
            approval_mode: ApprovalMode::default(),
            approval_overridden: false,
            unknown: Map::new(),
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

    /// Records a message sent from the selected session. It starts a turn, which also
    /// clears a failure shown for the last one. The first message titles a session that
    /// still has its placeholder title.
    pub fn record_message(&mut self, id: u64, text: &str, now: u64) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| {
            if session.title_source == TitleSource::Placeholder
                && let Some(title) = message_title(text)
            {
                session.title = title;
                session.title_source = TitleSource::FirstMessage;
            }
            session.updated_at = Some(now);
            session.mark_viewed(now);
            session.last_turn = Some(LastTurn::Running);
        })
    }

    /// Records a turn the provider started on its own, such as one opened by a steer
    /// that arrived after the previous turn ended.
    pub fn record_turn_start(&mut self, id: u64) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.last_turn = Some(LastTurn::Running))
    }

    /// Records the end of a turn. The selected session's user sees it end.
    pub fn record_turn_end(&mut self, id: u64, failed: bool, now: u64) -> StoreResult<Applied<()>> {
        let selected = self.workspace.selected == Some(id);
        self.update_session(id, |session| {
            session.updated_at = Some(now);
            session.last_turn = Some(if failed {
                LastTurn::Failed
            } else {
                LastTurn::Finished
            });
            if selected {
                session.mark_viewed(now);
            }
        })
    }

    /// Gives the session the user's own title, on one line. A message never replaces it.
    /// A blank title is refused and changes nothing.
    pub fn rename(&mut self, id: u64, title: &str) -> StoreResult<Applied<()>> {
        let title = one_line(title).ok_or_else(|| invalid("a title cannot be blank"))?;
        self.update_session(id, |session| {
            if session.title != title {
                session.title = title;
                session.title_source = TitleSource::Renamed;
            }
        })
    }

    pub fn set_settled(&mut self, id: u64, settled: bool) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.settled = settled)
    }

    /// Saves the appearance over the latest preferences. The sidebar layout and any
    /// preference this version does not know stay as they are.
    pub fn set_appearance(&mut self, appearance: Appearance) -> StoreResult<Applied<()>> {
        self.update_preferences(|preferences| preferences.appearance = appearance)
    }

    /// Saves exactly `layout`, readable or not, over the latest preferences, leaving
    /// every other preference as it is.
    pub fn set_sidebar_layout(&mut self, layout: SavedLayout) -> StoreResult<Applied<()>> {
        self.update_preferences(|preferences| preferences.sidebar_layout = layout)
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

    fn update_preferences(
        &mut self,
        change: impl FnOnce(&mut Preferences),
    ) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        change(&mut next.preferences);
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
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
            unknown: self.unknown.clone(),
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
        store.record_message(1, "hi", 2_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(2_000), Some(2_000)));
        // A turn that ends while its session is selected is seen as it ends.
        store.record_turn_end(1, false, 3_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(3_000), Some(3_000)));

        // Creating a second session leaves the first.
        store.create(Provider::Claude, 4_000).unwrap();
        assert_eq!(times(&store, 1), (Some(1_000), Some(3_000), Some(4_000)));
        // A turn ending elsewhere is unseen.
        store.record_turn_end(1, false, 5_000).unwrap();
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

    #[test]
    fn attention_prefers_approval_then_failure_then_an_unseen_reply() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("state.json")).unwrap();
        store.create(Provider::Codex, 1_000).unwrap();
        store.create(Provider::Codex, 2_000).unwrap();
        let attention = |store: &Store, approval, running| {
            store.workspace().sessions()[0].attention(approval, running)
        };
        assert_eq!(attention(&store, false, false), None);

        store.record_turn_end(1, false, 3_000).unwrap();
        assert_eq!(attention(&store, false, false), Some(Attention::Replied));
        store.record_turn_end(1, true, 4_000).unwrap();
        assert_eq!(attention(&store, false, false), Some(Attention::Failed));
        assert_eq!(attention(&store, true, true), Some(Attention::Approval));

        // Viewing a failure does not clear it. The next message does.
        store.select(1, 5_000).unwrap();
        store.select(2, 6_000).unwrap();
        assert_eq!(attention(&store, false, false), Some(Attention::Failed));
        store.select(1, 7_000).unwrap();
        store.record_message(1, "hi", 8_000).unwrap();
        assert_eq!(attention(&store, false, true), None);
        // A turn saved as running with nothing running was cut off by a restart.
        assert_eq!(attention(&store, false, false), Some(Attention::Failed));
        assert_eq!(
            Store::open(directory.path().join("state.json"))
                .unwrap()
                .workspace()
                .sessions()[0]
                .attention(false, false),
            Some(Attention::Failed)
        );
    }

    #[test]
    fn an_unseen_reply_holds_while_selected_and_clears_on_leave() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("state.json")).unwrap();
        store.create(Provider::Codex, 1_000).unwrap();
        store.create(Provider::Codex, 2_000).unwrap();
        store.record_turn_end(1, false, 3_000).unwrap();
        let first = |store: &Store| store.workspace().sessions()[0].attention(false, false);

        store.select(1, 4_000).unwrap();
        assert_eq!(first(&store), Some(Attention::Replied));
        store.select(2, 5_000).unwrap();
        assert_eq!(first(&store), None);

        // A reply that lands while the session is open is seen as it lands.
        store.select(1, 6_000).unwrap();
        store.record_turn_end(1, false, 7_000).unwrap();
        assert_eq!(first(&store), None);
    }

    #[test]
    fn leaving_clears_an_unseen_reply_after_the_clock_moves_back() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("state.json")).unwrap();
        store.create(Provider::Codex, 1_000).unwrap();
        store.create(Provider::Codex, 2_000).unwrap();
        store.record_turn_end(1, false, 10_000).unwrap();
        store.select(1, 9_000).unwrap();
        store.select(2, 9_500).unwrap();
        assert_eq!(
            store.workspace().sessions()[0].attention(false, false),
            None
        );
    }

    #[test]
    fn the_first_message_titles_a_session_and_later_ones_do_not() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Codex, 1_000).unwrap();
        let title = |store: &Store| store.workspace().sessions()[0].title().to_owned();
        assert_eq!(title(&store), "New Codex session");
        let writes = store.writes();
        store.rename(1, " New Codex session ").unwrap();
        assert_eq!(store.writes(), writes, "an unchanged title is not a rename");

        store
            .record_message(1, "\n  Fix the   login\tbug\nwith details below", 2_000)
            .unwrap();
        assert_eq!(title(&store), "Fix the login bug with details below");
        store.record_message(1, "Something else", 3_000).unwrap();
        assert_eq!(title(&Store::open(&path).unwrap()), title(&store));
    }

    #[test]
    fn a_long_first_message_is_cut_between_characters_and_at_a_word() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("state.json")).unwrap();
        let mut title_of = |text: &str| {
            let id = store.create(Provider::Claude, 1_000).unwrap().value;
            store.record_message(id, text, 2_000).unwrap();
            store.workspace().sessions()[id as usize - 1]
                .title()
                .to_owned()
        };

        let family = "👨\u{200d}👩\u{200d}👧";
        let text = format!("{}{family}{family}", "a".repeat(47));
        assert_eq!(title_of(&text), format!("{}{family}…", "a".repeat(47)));
        let text = format!("{}e\u{301}xyz", "b".repeat(47));
        assert_eq!(title_of(&text), format!("{}e\u{301}…", "b".repeat(47)));
        let words = "Refactor the session sidebar so rows keep their place while typing";
        assert_eq!(
            title_of(words),
            "Refactor the session sidebar so rows keep their…"
        );
    }

    #[test]
    fn a_rename_is_kept_through_messages_and_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Codex, 1_000).unwrap();
        store.create(Provider::Codex, 1_000).unwrap();
        let title = |store: &Store, id: u64| {
            store.workspace().sessions()[id as usize - 1]
                .title()
                .to_owned()
        };

        store.rename(1, "  Release  notes\n").unwrap();
        store.record_message(1, "Draft the notes", 2_000).unwrap();
        assert_eq!(title(&store, 1), "Release notes");
        store.record_message(2, "Draft the notes", 2_000).unwrap();
        store.rename(2, "Notes, take two").unwrap();
        store.record_message(2, "And again", 3_000).unwrap();
        assert_eq!(title(&store, 2), "Notes, take two");

        let writes = store.writes();
        assert!(store.rename(1, " \n\t").is_err());
        assert!(store.rename(9, "Missing").is_err());
        assert_eq!(store.writes(), writes);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(title(&reopened, 1), "Release notes");
        assert_eq!(title(&reopened, 2), "Notes, take two");
    }

    #[test]
    fn a_session_saved_before_titles_changed_takes_its_first_message() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"version":1,"next_id":2,"workspace":{"sessions":[{"id":1,"title":"New Codex session","provider":"Codex","settled":false,"draft":""}],"selected":1}}"#,
        )
        .unwrap();
        let mut store = Store::open(&path).unwrap();
        store.set_draft(1, "typing".into()).unwrap();
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("title_source")
        );

        store.record_message(1, "Port the parser", 1_000).unwrap();
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].title(),
            "Port the parser"
        );
    }

    #[test]
    fn a_turn_the_provider_starts_on_its_own_fails_if_cut_off() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Codex, 1_000).unwrap();
        store.record_message(1, "hi", 2_000).unwrap();
        store.record_turn_end(1, false, 3_000).unwrap();
        // A late steer opens another turn without a new message.
        store.record_turn_start(1).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.workspace().sessions()[0].attention(false, false),
            Some(Attention::Failed)
        );
    }

    fn seeded(path: &Path, layout: &str) {
        std::fs::write(
            path,
            format!(
                r#"{{"version":1,"next_id":2,"future":{{"a":[1,2]}},"workspace":{{"sessions":[{{"id":1,"title":"Old","provider":"Codex","settled":false,"draft":"keep","model":"m-1"}}],"selected":1,"panes":["chat"],"preferences":{{"theme":"Dark","accent":"teal"{layout}}}}}}}"#
            ),
        )
        .unwrap();
    }

    fn json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn saved_sidebar(saved: &Value) -> &Value {
        saved["workspace"]["preferences"]
            .as_object()
            .unwrap()
            .get("sidebar_layout")
            .expect("sidebar_layout saved")
    }

    fn appearance_keys_saved_once(path: &Path) {
        let text = std::fs::read_to_string(path).unwrap();
        for key in [r#""theme""#, r#""density""#, r#""ui_font""#] {
            assert_eq!(text.matches(key).count(), 1, "{key} in {text}");
        }
    }

    #[test]
    fn unknown_fields_and_an_unreadable_layout_survive_every_other_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        for layout in [
            r#","sidebar_layout":{"side":"top","width_px":360}"#,
            r#","sidebar_layout":{"side":"right","width_px":9000}"#,
            r#","sidebar_layout":{"side":"right","width_px":360,"pinned":true}"#,
            r#","sidebar_layout":{"width_px":360}"#,
            r#","sidebar_layout":"wide""#,
            r#","sidebar_layout":null"#,
        ] {
            seeded(&path, layout);
            let raw = saved_sidebar(&json(&path)).clone();
            let mut store = Store::open(&path).unwrap();
            let preferences = store.workspace().preferences();
            assert_eq!(
                preferences.sidebar_layout().shown(),
                SidebarLayout::default(),
                "{layout}"
            );
            assert_eq!(preferences.appearance().theme, ThemeMode::Dark, "{layout}");
            let unreadable = preferences.sidebar_layout().clone();

            store.set_draft(1, "edited".into()).unwrap();
            store
                .set_appearance(Appearance {
                    theme: ThemeMode::Light,
                    ..Appearance::default()
                })
                .unwrap();
            store.create(Provider::Claude, 1_000).unwrap();
            store.rename(1, "Renamed").unwrap();

            appearance_keys_saved_once(&path);
            let saved = json(&path);
            let reopened = Store::open(&path).unwrap();
            assert_eq!(reopened.workspace().sessions()[0].draft(), "edited");
            assert_eq!(
                reopened.workspace().preferences().appearance(),
                &Appearance {
                    theme: ThemeMode::Light,
                    ..Appearance::default()
                }
            );
            assert_eq!(
                saved["future"],
                serde_json::json!({"a": [1, 2]}),
                "{layout}"
            );
            assert_eq!(saved["workspace"]["panes"], serde_json::json!(["chat"]));
            let sessions = &saved["workspace"]["sessions"];
            assert_eq!(sessions[0]["model"], "m-1");
            assert!(sessions[1].get("model").is_none());
            assert_eq!(saved["workspace"]["preferences"]["accent"], "teal");
            assert_eq!(saved_sidebar(&saved), &raw, "{layout}");

            let mut store = reopened;
            store
                .set_sidebar_layout(SidebarLayout::default().into())
                .unwrap();
            let saved = json(&path);
            assert_eq!(
                saved_sidebar(&saved),
                &serde_json::json!({"side": "left", "width_px": 302}),
                "{layout}"
            );
            assert_eq!(saved["future"], serde_json::json!({"a": [1, 2]}));
            assert_eq!(saved["workspace"]["preferences"]["accent"], "teal");
            assert_eq!(saved["workspace"]["sessions"][0]["model"], "m-1");

            store
                .set_appearance(Appearance {
                    theme: ThemeMode::Dark,
                    density: Density::Compact,
                    ui_font: Some("Inter".into()),
                })
                .unwrap();
            store.set_draft(1, "newer".into()).unwrap();
            store.set_sidebar_layout(unreadable.clone()).unwrap();
            let saved = json(&path);
            assert_eq!(saved_sidebar(&saved), &raw, "{layout}");
            let preferences = &saved["workspace"]["preferences"];
            assert_eq!(preferences["theme"], "Dark", "{layout}");
            assert_eq!(preferences["density"], "Compact", "{layout}");
            assert_eq!(preferences["ui_font"], "Inter", "{layout}");
            assert_eq!(preferences["accent"], "teal");
            assert_eq!(saved["workspace"]["sessions"][0]["draft"], "newer");
            assert_eq!(saved["future"], serde_json::json!({"a": [1, 2]}));

            store.set_draft(1, "later".into()).unwrap();
            let reopened = Store::open(&path).unwrap();
            assert_eq!(saved_sidebar(&json(&path)), &raw, "{layout}");
            assert_eq!(
                reopened.workspace().preferences().sidebar_layout(),
                &unreadable
            );
            assert_eq!(reopened.workspace().sessions()[0].draft(), "later");
            assert_eq!(reopened.workspace().sessions().len(), 2);
            appearance_keys_saved_once(&path);
        }
    }

    #[test]
    fn a_supported_or_missing_layout_loads_and_saves_as_itself() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        seeded(
            &path,
            r#","sidebar_layout":{"side":"right","width_px":360}"#,
        );
        let mut store = Store::open(&path).unwrap();
        let layout = store.workspace().preferences().sidebar_layout().shown();
        assert_eq!((layout.side, layout.width.px()), (SidebarSide::Right, 360));
        store.set_draft(1, "edited".into()).unwrap();
        assert_eq!(
            saved_sidebar(&json(&path)),
            &serde_json::json!({"side": "right", "width_px": 360})
        );

        seeded(&path, "");
        let mut store = Store::open(&path).unwrap();
        let preferences = store.workspace().preferences();
        assert_eq!(preferences.sidebar_layout(), &SavedLayout::default());
        assert_eq!(
            preferences.appearance(),
            &Appearance {
                theme: ThemeMode::Dark,
                ..Appearance::default()
            }
        );
        let writes = store.writes();
        store
            .set_sidebar_layout(SidebarLayout::default().into())
            .unwrap();
        assert_eq!(store.writes(), writes);
        store.set_draft(1, "edited".into()).unwrap();
        appearance_keys_saved_once(&path);
        let saved = json(&path);
        assert_eq!(saved["workspace"]["preferences"]["density"], "Default");
        assert_eq!(saved["workspace"]["preferences"]["accent"], "teal");
        assert_eq!(
            Store::open(&path).unwrap().workspace().preferences(),
            store.workspace().preferences()
        );
    }

    #[test]
    fn saving_the_sidebar_layout_keeps_other_preferences_and_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Claude, 1).unwrap();
        store.set_draft(1, "draft".into()).unwrap();
        store
            .set_appearance(Appearance {
                theme: ThemeMode::Light,
                ui_font: Some("Inter".into()),
                ..Appearance::default()
            })
            .unwrap();

        let layout = SidebarLayout {
            side: SidebarSide::Right,
            width: SidebarWidth::new(360).unwrap(),
        };
        let applied = store.set_sidebar_layout(layout.into()).unwrap();
        assert_eq!(applied.durability, Durability::Synced);
        let writes = store.writes();
        store.set_sidebar_layout(layout.into()).unwrap();
        assert_eq!(store.writes(), writes);

        let reopened = Store::open(&path).unwrap();
        let preferences = reopened.workspace().preferences();
        assert_eq!(preferences.sidebar_layout(), &SavedLayout::from(layout));
        assert_eq!(preferences.appearance().theme, ThemeMode::Light);
        assert_eq!(preferences.appearance().ui_font.as_deref(), Some("Inter"));
        assert_eq!(reopened.workspace().sessions()[0].draft(), "draft");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(r#""version":1"#), "{saved}");
        assert!(
            saved.contains(r#""sidebar_layout":{"side":"right","width_px":360}"#),
            "{saved}"
        );
    }
}
