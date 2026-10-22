use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use unicode_segmentation::UnicodeSegmentation;

mod account;
#[cfg(test)]
#[path = "account_tests.rs"]
mod account_tests;
pub mod attachment;
mod composition;
mod database;
mod delivery;
pub mod integrations;
mod layout;
mod project;
mod prompt;
#[cfg(test)]
#[path = "prompt_tests.rs"]
mod prompt_tests;
mod session_context;
mod tabs;
mod usage;
mod windows;
pub mod worktree;

pub use account::{Account, AccountId, expand_home, homes_overlap, lexical};
pub use attachment::Attachment;
pub use composition::{
    Axis, Child, Component, Composition, Edit, Node, NodePath, Place, Problem, SavedComposition,
    Size, ViewId,
};
pub use database::{Database, StoredEvent, database_path};
pub use delivery::{Delivery, DeliveryState};
pub use layout::{
    DIVIDER_WIDTH, MAIN_MIN_WIDTH, SavedLayout, SidebarLayout, SidebarSide, SidebarWidth,
};
pub use project::{Project, ProjectId};
use project::{ProjectLayouts, ProjectMap};
pub use prompt::{Prompt, PromptFields};
use session_context::SavedSessionContext;
pub use session_context::{ContextOrigin, SessionContext, WorkingDirectory, WorktreeContext};
use tabs::SavedTabs;
pub use tabs::SessionTabs;
pub use usage::{AccountUsage, LimitWindow, Reading};
use windows::SavedWindows;
pub use windows::{Placement, WindowId, WindowView};

type StoreResult<T> = Result<T, Box<dyn Error>>;

const FORMAT_VERSION: u32 = 1;

#[derive(Debug)]
pub struct Applied<T> {
    pub value: T,
    pub durability: Durability,
    /// The transaction that holds this change. A no-op names the current revision.
    pub revision: Revision,
}

/// A position in one [`Store`]'s write order, issued by it and meaningful only to it.
/// Ask that store with [`Store::is_durable`] whether the snapshot has been confirmed.
#[derive(Clone, Copy, Debug)]
pub struct Revision(u64);

struct PendingSync {
    directory: File,
    oldest_unconfirmed: Revision,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Durability {
    Synced,
    DirectorySyncPending(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
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

/// A pull request made, or found, for a session's branch through the user's `gh`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}

/// Why a session needs the user, in priority order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Attention {
    Approval,
    /// An agent-built widget waits for the user to answer it.
    Input,
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
    #[serde(default, skip_serializing_if = "SavedSessionContext::is_missing")]
    execution_context: SavedSessionContext,
    settled: bool,
    draft: String,
    /// Images attached to the draft, sent with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    draft_attachments: Vec<Attachment>,
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
    /// The provider-native model the user asked for, such as Claude's `opus` or Codex's
    /// `gpt-6-astra`. `None` leaves the choice to the provider. Fixed once the session
    /// has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Set just before the first message goes to the provider, so the provider and
    /// model stay fixed even if nothing else of that send was saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_locked: Option<bool>,
    /// The provider account the session runs under. `None` is the provider's default
    /// account, which every session saved before accounts existed uses. Fixed, with
    /// the provider and model, once the session has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<AccountId>,
    /// The session this one continues: a handoff carried its context here in a new
    /// session, leaving that one unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handed_off_from: Option<u64>,
    /// The pull request last made or found for the session's branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pull_request: Option<PullRequest>,
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
    /// A session not yet sent from, with a placeholder title and an empty draft.
    fn new(id: u64, provider: Provider, directory: &WorkingDirectory, now: u64) -> Self {
        Session {
            id,
            title: format!("New {provider} session"),
            title_source: TitleSource::Placeholder,
            provider,
            execution_context: SavedSessionContext::recorded(directory, ContextOrigin::Created),
            settled: false,
            draft: String::new(),
            draft_attachments: Vec::new(),
            provider_thread: None,
            created_at: Some(now),
            updated_at: Some(now),
            viewed_at: Some(now),
            last_turn: None,
            approval_mode: ApprovalMode::default(),
            approval_overridden: false,
            model: None,
            model_locked: Some(false),
            account: None,
            handed_off_from: None,
            pull_request: None,
            unknown: Map::new(),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn context(&self) -> SessionContext<'_> {
        self.execution_context.context()
    }

    pub fn worktree(&self) -> Option<&WorktreeContext> {
        self.execution_context.worktree()
    }

    pub fn settled(&self) -> bool {
        self.settled
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    pub fn draft_attachments(&self) -> &[Attachment] {
        &self.draft_attachments
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

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn model_locked(&self) -> Option<bool> {
        self.model_locked
    }

    /// The account the session runs under, or `None` for its provider's default.
    pub fn account(&self) -> Option<&AccountId> {
        self.account.as_ref()
    }

    /// The session this one was handed off from, if any.
    pub fn handed_off_from(&self) -> Option<u64> {
        self.handed_off_from
    }

    pub fn pull_request(&self) -> Option<&PullRequest> {
        self.pull_request.as_ref()
    }

    /// Whether the session has sent anything to its provider. The provider and model
    /// are fixed from then on. A session saved before the lock existed counts as sent.
    pub fn started(&self) -> bool {
        self.model_locked.unwrap_or(true)
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

    /// What the session needs from the user, if anything. `awaiting_approval`,
    /// `awaiting_input` (a widget waits for an answer) and `running` describe the live
    /// provider in this process. A turn saved as running that no live provider runs was
    /// cut off when Keystone closed, so it failed.
    pub fn attention(
        &self,
        awaiting_approval: bool,
        awaiting_input: bool,
        running: bool,
    ) -> Option<Attention> {
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
        } else if awaiting_input {
            Some(Attention::Input)
        } else if failed {
            Some(Attention::Failed)
        } else if running {
            None
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
    /// The code font family. `None` uses the first installed default.
    pub mono_font: Option<String>,
    /// The interface's base text size in pixels. `None` uses the built-in 13.
    pub text_size: Option<u8>,
}

/// Which desktop notifications Keystone sends. Each is sent only for a session the
/// user cannot see. Missing fields load as on, so older state files notify.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Notifications {
    /// Off sends none, whatever each event's own switch says.
    pub enabled: bool,
    pub turn_finished: bool,
    pub approval: bool,
    pub turn_failed: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            turn_finished: true,
            approval: true,
            turn_failed: true,
        }
    }
}

impl Notifications {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Preferences {
    // Before `unknown`, so its keys are taken before the map collects the rest.
    #[serde(flatten)]
    appearance: Appearance,
    #[serde(skip_serializing_if = "Notifications::is_default")]
    notifications: Notifications,
    sidebar_layout: SavedLayout,
    #[serde(skip_serializing_if = "ProjectLayouts::is_empty")]
    project_layouts: ProjectLayouts,
    #[serde(skip_serializing_if = "ProjectMap::is_empty")]
    project_tabs: ProjectMap<SavedTabs>,
    #[serde(skip_serializing_if = "SavedWindows::is_empty")]
    windows: SavedWindows,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

impl Preferences {
    pub fn appearance(&self) -> &Appearance {
        &self.appearance
    }

    pub fn notifications(&self) -> Notifications {
        self.notifications
    }

    pub fn sidebar_layout(&self) -> &SavedLayout {
        &self.sidebar_layout
    }

    pub fn project_layout(&self, project: &ProjectId) -> Option<&SavedComposition> {
        self.layout_in(project, WindowId::FIRST)
    }

    pub fn project_layouts(&self) -> impl Iterator<Item = (ProjectId, &SavedComposition)> {
        self.layouts_in(WindowId::FIRST)
    }

    /// The composition the first window draws for `project`. Until it saves its own,
    /// a project shows the sidebar layout every project shared before compositions
    /// existed.
    pub fn composition(&self, project: &ProjectId) -> Composition {
        self.composition_in(project, WindowId::FIRST)
    }

    /// The session `view` of `project` holds in the first window.
    pub fn held(&self, project: &ProjectId, view: ViewId) -> Option<u64> {
        self.held_in(project, WindowId::FIRST, view)
    }

    /// Which session each of `project`'s conversation views holds in the first window.
    pub fn session_tabs(&self, project: &ProjectId) -> SessionTabs {
        self.session_tabs_in(project, WindowId::FIRST)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Workspace {
    sessions: Vec<Session>,
    selected: Option<u64>,
    #[serde(default)]
    preferences: Preferences,
    /// Accounts the user added, in the order they were added. Saved in their own
    /// projection, never in the workspace payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    accounts: Vec<Account>,
    /// Folders the user opened as projects, in the order they were first listed.
    /// Saved in their own projection, never in the workspace payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    projects: Vec<Project>,
    /// Saved prompts, in the user's order. Saved in their own projection, never in
    /// the workspace payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    prompts: Vec<Prompt>,
    /// Unfinished commit messages, by the folder whose changes they describe. Saved
    /// in their own projection, never in the workspace payload.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    commit_drafts: BTreeMap<String, String>,
    /// Launched sessions' results on their way to their parents, in launch order.
    /// Saved in their own projection, never in the workspace payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deliveries: Vec<Delivery>,
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

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn account(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.iter().find(|account| account.id() == id)
    }

    /// The projects the user opened and has not removed, most recently opened first.
    pub fn projects(&self) -> Vec<&Project> {
        let mut projects: Vec<(usize, &Project)> = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, project)| !project.removed())
            .collect();
        projects.sort_by_key(|(position, project)| {
            std::cmp::Reverse((project.last_opened(), *position))
        });
        projects.into_iter().map(|(_, project)| project).collect()
    }

    /// The unfinished commit message for `folder`, empty when there is none.
    pub fn commit_draft(&self, folder: &str) -> &str {
        self.commit_drafts.get(folder).map_or("", String::as_str)
    }

    /// The projects the user took off the list.
    pub fn removed_projects(&self) -> impl Iterator<Item = ProjectId> + '_ {
        self.projects
            .iter()
            .filter(|project| project.removed())
            .map(Project::id)
    }

    pub fn prompts(&self) -> &[Prompt] {
        &self.prompts
    }

    pub fn prompt(&self, id: u64) -> Option<&Prompt> {
        self.prompts.iter().find(|prompt| prompt.id() == id)
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
    database: Database,
    database_revision: i64,
    workspace: Workspace,
    next_id: u64,
    unknown: Map<String, Value>,
    pending: Option<PendingSync>,
    writes: u64,
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub sync_directory: fn(&File) -> io::Result<()>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        Self::from_database(Database::open(path)?)
    }

    pub fn in_memory() -> StoreResult<Self> {
        Self::from_database(Database::in_memory()?)
    }

    pub fn from_database(database: Database) -> StoreResult<Self> {
        let (snapshot, database_revision) = database.snapshot()?;
        Ok(Self {
            path: database.path().to_path_buf(),
            database,
            database_revision,
            workspace: snapshot.workspace,
            next_id: snapshot.next_id,
            unknown: snapshot.unknown,
            pending: None,
            writes: 0,
            #[cfg(any(test, feature = "test-support"))]
            sync_directory: File::sync_all,
        })
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn database(&self) -> Database {
        self.database.clone()
    }

    #[doc(hidden)]
    pub fn saved_snapshot(&self) -> StoreResult<Value> {
        let (snapshot, _) = self.database.snapshot()?;
        Ok(serde_json::to_value(snapshot)?)
    }

    /// The SQLite database this store saves to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many workspace transactions this store has committed since it opened.
    #[doc(hidden)]
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// Creates a session and selects it. `now` is unix milliseconds.
    pub fn create(
        &mut self,
        provider: Provider,
        directory: &WorkingDirectory,
        now: u64,
    ) -> StoreResult<Applied<u64>> {
        self.create_showing(provider, directory, now, WindowId::FIRST, None)
    }

    /// Creates a session and selects it in `window`. Given a project's layout and
    /// tabs, saves them for `window` with `view` holding the new session, in the same
    /// write. Saved layouts or tabs this build cannot change are left as they are, and
    /// the session is still created.
    pub fn create_showing(
        &mut self,
        provider: Provider,
        directory: &WorkingDirectory,
        now: u64,
        window: WindowId,
        show: Option<(&ProjectId, Composition, SessionTabs, ViewId)>,
    ) -> StoreResult<Applied<u64>> {
        self.add_session(
            |id| Session::new(id, provider, directory, now),
            Opening::Window(window, show),
        )
    }

    /// Creates a session an agent in `parent` launches, selected in no window, with
    /// its delivery in the same write, so a launched session always has one.
    pub fn create_unselected(
        &mut self,
        provider: Provider,
        directory: &WorkingDirectory,
        now: u64,
        parent: u64,
    ) -> StoreResult<Applied<u64>> {
        self.add_session(
            |id| Session::new(id, provider, directory, now),
            Opening::Launched(parent),
        )
    }

    pub fn create_worktree_showing(
        &mut self,
        provider: Provider,
        context: &WorktreeContext,
        now: u64,
        window: WindowId,
        show: Option<(&ProjectId, Composition, SessionTabs, ViewId)>,
    ) -> StoreResult<Applied<u64>> {
        self.add_worktree_session(provider, context, now, Opening::Window(window, show))
    }

    /// Creates a session an agent in `parent` launches in `context`, as
    /// [`Store::create_unselected`] does.
    pub fn create_worktree_unselected(
        &mut self,
        provider: Provider,
        context: &WorktreeContext,
        now: u64,
        parent: u64,
    ) -> StoreResult<Applied<u64>> {
        self.add_worktree_session(provider, context, now, Opening::Launched(parent))
    }

    fn add_worktree_session(
        &mut self,
        provider: Provider,
        context: &WorktreeContext,
        now: u64,
        place: Opening,
    ) -> StoreResult<Applied<u64>> {
        self.add_session(
            |id| {
                let mut session = Session::new(id, provider, &context.directory, now);
                session.execution_context = SavedSessionContext::with_worktree(context);
                session
            },
            place,
        )
    }

    /// Creates a session that continues `source` on `provider`'s default account and
    /// selects it in `window`, in one save: the same folder, `source`'s title (or, for
    /// a placeholder, one naming `source`), a link back to it, and `draft` in its
    /// composer, saving `show` with it as [`Store::create_showing`] does. `source`
    /// itself is not changed. The new session has not started, so its provider,
    /// account and model can still change.
    pub fn hand_off(
        &mut self,
        source: u64,
        provider: Provider,
        draft: String,
        now: u64,
        window: WindowId,
        show: Option<(&ProjectId, Composition, SessionTabs, ViewId)>,
    ) -> StoreResult<Applied<u64>> {
        let from = self
            .workspace
            .sessions
            .iter()
            .find(|session| session.id == source)
            .ok_or_else(|| invalid(format!("unknown session ID {source}")))?;
        let SessionContext::Recorded { directory, .. } = from.context() else {
            return Err(invalid("choose this session's folder before handing it off").into());
        };
        let directory = directory.clone();
        let worktree = from.worktree().cloned();
        let pull_request = from.pull_request.clone();
        // A placeholder would give way to the handoff text's first words on Send.
        let title = match from.title_source {
            TitleSource::Placeholder => format!("Continued from session {source}"),
            _ => from.title.clone(),
        };
        self.add_session(
            |id| {
                let mut session = Session::new(id, provider, &directory, now);
                if let Some(worktree) = &worktree {
                    session.execution_context = SavedSessionContext::with_worktree(worktree);
                }
                session.title = title;
                session.title_source = TitleSource::FirstMessage;
                session.draft = draft;
                session.handed_off_from = Some(source);
                session.pull_request = pull_request;
                session
            },
            Opening::Window(window, show),
        )
    }

    /// Adds the session `make` builds with the next ID, selecting it in `window` and
    /// saving `show` there as [`Store::create_showing`] does.
    fn add_session(
        &mut self,
        make: impl FnOnce(u64) -> Session,
        place: Opening,
    ) -> StoreResult<Applied<u64>> {
        let id = self.next_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| invalid("session ID exhausted"))?;
        let mut next = self.workspace.clone();
        next.sessions.push(make(id));
        let sessions: HashSet<u64> = next.sessions.iter().map(|s| s.id).collect();
        match place {
            Opening::Window(window, show) => {
                let shown = next.shown_mut(window)?;
                *shown.selected = Some(id);
                if let Some((project, layout, mut tabs, view)) = show {
                    tabs.retain(|_, session| sessions.contains(&session));
                    tabs.hold(view, id);
                    let (mut layouts, mut saved_tabs) = (shown.layouts.clone(), shown.tabs.clone());
                    if layouts
                        .set(project, Some(layout.into()))
                        .and_then(|_| saved_tabs.set(project, Some(SavedTabs::Supported(tabs))))
                        .is_ok()
                    {
                        *shown.layouts = layouts;
                        *shown.tabs = saved_tabs;
                    }
                }
            }
            Opening::Launched(parent) => {
                if !sessions.contains(&parent) {
                    return Err(invalid(format!("unknown session ID {parent}")).into());
                }
                next.deliveries.push(Delivery::launched(id, parent));
            }
        }
        let applied = self.commit(next, next_id)?;
        Ok(Applied {
            value: id,
            durability: applied.durability,
            revision: applied.revision,
        })
    }

    pub fn choose_legacy_directory(
        &mut self,
        session: u64,
        directory: &WorkingDirectory,
    ) -> StoreResult<Applied<()>> {
        let current = self
            .workspace
            .sessions
            .iter()
            .find(|current| current.id == session)
            .ok_or_else(|| invalid(format!("unknown session ID {session}")))?;
        match current.context() {
            SessionContext::LegacyMissing => self.update_session(session, |session| {
                session.execution_context =
                    SavedSessionContext::recorded(directory, ContextOrigin::Chosen);
            }),
            SessionContext::Recorded {
                directory: chosen,
                origin: ContextOrigin::Chosen,
            } if chosen == directory => Ok(self.finish_sync()),
            _ => Err(invalid("session execution directory cannot be replaced").into()),
        }
    }

    pub fn select(&mut self, id: u64) -> StoreResult<Applied<()>> {
        self.select_in(WindowId::FIRST, id)
    }

    /// Selects `id` in `window`, leaving every other window's selection as it is.
    pub fn select_in(&mut self, window: WindowId, id: u64) -> StoreResult<Applied<()>> {
        if !self
            .workspace
            .sessions
            .iter()
            .any(|session| session.id == id)
        {
            return Err(invalid(format!("unknown session ID {id}")).into());
        }
        if self.workspace.selected_in(window) == Some(id) {
            return Ok(self.finish_sync());
        }
        let mut next = self.workspace.clone();
        *next.shown_mut(window)?.selected = Some(id);
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    /// Acknowledges content the desktop actually showed in a focused conversation.
    pub fn mark_viewed(&mut self, id: u64, now: u64) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.mark_viewed(now))
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
            session.last_turn = Some(LastTurn::Running);
        })
    }

    /// Records a turn the provider started on its own, such as one opened by a steer
    /// that arrived after the previous turn ended.
    pub fn record_turn_start(&mut self, id: u64) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.last_turn = Some(LastTurn::Running))
    }

    /// Records the end of a turn without acknowledging its reply.
    pub fn record_turn_end(&mut self, id: u64, failed: bool, now: u64) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| {
            session.updated_at = Some(now);
            session.last_turn = Some(if failed {
                LastTurn::Failed
            } else {
                LastTurn::Finished
            });
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

    pub fn set_notifications(&mut self, notifications: Notifications) -> StoreResult<Applied<()>> {
        self.update_preferences(|preferences| preferences.notifications = notifications)
    }

    /// Saves exactly `layout`, readable or not, over the latest preferences, leaving
    /// every other preference as it is.
    pub fn set_sidebar_layout(&mut self, layout: SavedLayout) -> StoreResult<Applied<()>> {
        self.update_preferences(|preferences| preferences.sidebar_layout = layout)
    }

    /// Saves exactly `layout` for `project` alone, or removes its entry for `None`, so
    /// Undo can restore a project that had none. Fails without writing when the saved
    /// project layouts are not a map.
    pub fn set_project_layout(
        &mut self,
        project: &ProjectId,
        layout: Option<SavedComposition>,
    ) -> StoreResult<Applied<()>> {
        self.set_layout_in(project, WindowId::FIRST, layout)
    }

    /// Saves exactly `layout` for `project` in `window` alone, as
    /// [`Store::set_project_layout`] does for the first window.
    pub fn set_layout_in(
        &mut self,
        project: &ProjectId,
        window: WindowId,
        layout: Option<SavedComposition>,
    ) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        let sidebar = Composition::from_sidebar(next.preferences.sidebar_layout.shown());
        let mut shown = next.shown_mut(window)?;
        shown.layouts.set(project, layout)?;
        shown.forget_closed_tabs(project, &sidebar);
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    /// Saves what `project`'s window shows in one write, so a crash keeps all of it or
    /// none: its layout when given, which session each conversation view holds, and the
    /// selected session. Tabs holding a session that no longer exists are dropped. Tabs
    /// this version cannot read are kept until a view holds a session again.
    pub fn show(
        &mut self,
        project: &ProjectId,
        layout: Option<Composition>,
        tabs: SessionTabs,
        selected: Option<u64>,
    ) -> StoreResult<Applied<()>> {
        self.show_in(project, WindowId::FIRST, layout, tabs, selected)
    }

    /// Saves what `window` shows of `project` in one write, as [`Store::show`] does for
    /// the first window. Other windows are left as they are.
    pub fn show_in(
        &mut self,
        project: &ProjectId,
        window: WindowId,
        layout: Option<Composition>,
        tabs: SessionTabs,
        selected: Option<u64>,
    ) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        next.show(WindowView {
            window,
            project: project.clone(),
            layout,
            tabs,
            selected,
        })?;
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    pub fn set_provider_thread(&mut self, id: u64, thread: String) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.provider_thread = Some(thread))
    }

    /// Chooses the model the session's provider is asked for, or `None` for the
    /// provider's default. Refused once the session has started.
    pub fn set_model(&mut self, id: u64, model: Option<String>) -> StoreResult<Applied<()>> {
        self.update_unstarted(id, |session| session.model = model)
    }

    /// Changes the session's provider. The model and account return to the provider's
    /// defaults, and a placeholder title names the new provider. Refused once the
    /// session has started.
    pub fn set_provider(&mut self, id: u64, provider: Provider) -> StoreResult<Applied<()>> {
        self.update_unstarted(id, |session| {
            if session.provider != provider {
                session.account = None;
            }
            session.provider = provider;
            session.model = None;
            if session.title_source == TitleSource::Placeholder {
                session.title = format!("New {provider} session");
            }
        })
    }

    /// Chooses the account the session runs under: one of its provider's accounts, or
    /// `None` (or [`AccountId::DEFAULT`]) for the provider's default. A different
    /// account returns the model to the provider's default, since accounts may offer
    /// different models. Refused once the session has started, so a conversation
    /// never moves between accounts.
    pub fn set_account(&mut self, id: u64, account: Option<AccountId>) -> StoreResult<Applied<()>> {
        let provider = self.session_provider(id)?;
        self.set_provider_account(id, provider, account)
    }

    /// Moves the session to `provider` and one of its accounts in one save, as
    /// [`Store::set_provider`] then [`Store::set_account`] would.
    pub fn set_provider_account(
        &mut self,
        id: u64,
        provider: Provider,
        account: Option<AccountId>,
    ) -> StoreResult<Applied<()>> {
        let account = account.filter(|account| account.as_str() != AccountId::DEFAULT);
        self.session_provider(id)?;
        if let Some(account) = &account {
            match self.workspace.account(account) {
                Some(found) if found.provider() == provider => {}
                Some(_) => {
                    return Err(
                        invalid(format!("account {account} is not a {provider} account")).into(),
                    );
                }
                None => return Err(invalid(format!("unknown account {account}")).into()),
            }
        }
        self.update_unstarted(id, |session| {
            if session.provider != provider {
                session.provider = provider;
                if session.title_source == TitleSource::Placeholder {
                    session.title = format!("New {provider} session");
                }
                session.model = None;
            }
            if session.account != account {
                session.account = account;
                session.model = None;
            }
        })
    }

    fn session_provider(&self, id: u64) -> StoreResult<Provider> {
        Ok(self
            .workspace
            .sessions
            .iter()
            .find(|session| session.id == id)
            .ok_or_else(|| invalid(format!("unknown session ID {id}")))?
            .provider)
    }

    /// Adds an account for `provider` whose CLI runs with `home` as its config home.
    /// `home` must be absolute (see [`expand_home`]) and must not overlap another
    /// account's home or any of `reserved`, such as each provider's default home, so no
    /// two accounts share sign-in or transcripts. Returns the new account's ID.
    pub fn add_account(
        &mut self,
        provider: Provider,
        label: &str,
        home: PathBuf,
        reserved: &[PathBuf],
    ) -> StoreResult<Applied<AccountId>> {
        let label = self.check_account(provider, label, &home, reserved)?;
        let taken: Vec<AccountId> = self
            .workspace
            .accounts
            .iter()
            .map(|a| a.id().clone())
            .collect();
        let id = account::slug(provider, &label, &taken);
        let mut next = self.workspace.clone();
        next.accounts
            .push(Account::new(id.clone(), provider, label, home));
        let applied = self.commit(next, self.next_id)?;
        Ok(Applied {
            value: id,
            durability: applied.durability,
            revision: applied.revision,
        })
    }

    /// Gives an account a new label. Its ID and home never change.
    pub fn rename_account(&mut self, id: &AccountId, label: &str) -> StoreResult<Applied<()>> {
        let provider = self
            .workspace
            .account(id)
            .ok_or_else(|| invalid(format!("unknown account {id}")))?
            .provider();
        let label = self.account_label(provider, label, Some(id))?;
        let mut next = self.workspace.clone();
        if let Some(account) = next.accounts.iter_mut().find(|account| account.id() == id) {
            account.set_label(label);
        }
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    /// Removes an account Keystone no longer offers. Refused while any session uses
    /// it, so no session is ever moved to another account behind the user's back.
    /// Its home folder and everything in it stay on disk.
    pub fn remove_account(&mut self, id: &AccountId) -> StoreResult<Applied<()>> {
        if self.workspace.account(id).is_none() {
            return Err(invalid(format!("unknown account {id}")).into());
        }
        let users = self
            .workspace
            .sessions
            .iter()
            .filter(|session| session.account.as_ref() == Some(id))
            .count();
        if users > 0 {
            return Err(invalid(format!(
                "{users} session{} use{} this account; it stays until no session does",
                if users == 1 { "" } else { "s" },
                if users == 1 { "s" } else { "" },
            ))
            .into());
        }
        let mut next = self.workspace.clone();
        next.accounts.retain(|account| account.id() != id);
        self.commit(next, self.next_id)
    }

    /// `label` on one line, refused when blank or already naming another of
    /// `provider`'s accounts.
    /// The account name [`Store::add_account`] would save for `label`, or why it would
    /// refuse `label` and `home`, without saving anything.
    pub fn check_account(
        &self,
        provider: Provider,
        label: &str,
        home: &Path,
        reserved: &[PathBuf],
    ) -> StoreResult<String> {
        let label = self.account_label(provider, label, None)?;
        if !home.is_absolute() || account::lexical(home) != home {
            return Err(
                invalid("an account home must be an absolute path without `.` or `..`").into(),
            );
        }
        if let Some(other) = self
            .workspace
            .accounts
            .iter()
            .find(|account| homes_overlap(account.home(), home))
        {
            return Err(invalid(format!(
                "{} is already used by the account \u{201c}{}\u{201d}; each account needs a folder of its own",
                home.display(),
                other.label()
            ))
            .into());
        }
        if let Some(taken) = reserved.iter().find(|taken| homes_overlap(taken, home)) {
            return Err(invalid(format!(
                "{} overlaps {}, which a provider's own setup or another account uses; choose a separate folder",
                home.display(),
                taken.display()
            ))
            .into());
        }
        Ok(label)
    }

    fn account_label(
        &self,
        provider: Provider,
        label: &str,
        except: Option<&AccountId>,
    ) -> StoreResult<String> {
        let label = one_line(label).ok_or_else(|| invalid("an account name cannot be blank"))?;
        if label.eq_ignore_ascii_case(AccountId::DEFAULT) {
            return Err(invalid(
                "\u{201c}Default\u{201d} names each provider's own setup; choose another name",
            )
            .into());
        }
        let taken = self.workspace.accounts.iter().any(|account| {
            account.provider() == provider
                && Some(account.id()) != except
                && account.label().to_lowercase() == label.to_lowercase()
        });
        if taken {
            return Err(invalid(format!(
                "another {provider} account is already called \u{201c}{label}\u{201d}"
            ))
            .into());
        }
        Ok(label)
    }

    /// Saves a new prompt at the end of the list. Returns its ID.
    pub fn add_prompt(&mut self, fields: PromptFields) -> StoreResult<Applied<u64>> {
        let checked = fields.check(&self.workspace.prompts, None)?;
        let id = self.next_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| invalid("session ID space exhausted"))?;
        let mut next = self.workspace.clone();
        next.prompts.push(Prompt::new(id, checked));
        let applied = self.commit(next, next_id)?;
        Ok(Applied {
            value: id,
            durability: applied.durability,
            revision: applied.revision,
        })
    }

    /// Replaces a prompt's title, text, shortcut and project. Its place in the list
    /// and its use count stay.
    pub fn edit_prompt(&mut self, id: u64, fields: PromptFields) -> StoreResult<Applied<()>> {
        let checked = fields.check(&self.workspace.prompts, Some(id))?;
        self.update_prompt(id, |prompt| prompt.apply(checked))
    }

    /// Counts one more insert of the prompt.
    pub fn count_prompt_use(&mut self, id: u64) -> StoreResult<Applied<()>> {
        self.update_prompt(id, Prompt::count_use)
    }

    /// Moves a prompt to `to` in the list, clamped to its end.
    pub fn move_prompt(&mut self, id: u64, to: usize) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        let from = next
            .prompts
            .iter()
            .position(|prompt| prompt.id() == id)
            .ok_or_else(|| invalid(format!("unknown prompt {id}")))?;
        let prompt = next.prompts.remove(from);
        let to = to.min(next.prompts.len());
        next.prompts.insert(to, prompt);
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    pub fn remove_prompt(&mut self, id: u64) -> StoreResult<Applied<()>> {
        if self.workspace.prompt(id).is_none() {
            return Err(invalid(format!("unknown prompt {id}")).into());
        }
        let mut next = self.workspace.clone();
        next.prompts.retain(|prompt| prompt.id() != id);
        self.commit(next, self.next_id)
    }

    fn update_prompt(
        &mut self,
        id: u64,
        change: impl FnOnce(&mut Prompt),
    ) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        let prompt = next
            .prompts
            .iter_mut()
            .find(|prompt| prompt.id() == id)
            .ok_or_else(|| invalid(format!("unknown prompt {id}")))?;
        change(prompt);
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
    }

    /// Fixes the session's provider, account and model, for just before its first
    /// message is sent. A send whose later saves fail still leaves them fixed.
    pub fn lock_model(&mut self, id: u64) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.model_locked = Some(true))
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

    pub fn set_draft_attachments(
        &mut self,
        id: u64,
        attachments: Vec<Attachment>,
    ) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.draft_attachments = attachments)
    }

    pub fn set_pull_request(
        &mut self,
        id: u64,
        pull_request: PullRequest,
    ) -> StoreResult<Applied<()>> {
        self.update_session(id, |session| session.pull_request = Some(pull_request))
    }

    /// Keeps `text` as `folder`'s unfinished commit message, or forgets it when empty.
    pub fn set_commit_draft(&mut self, folder: &str, text: String) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        if text.is_empty() {
            next.commit_drafts.remove(folder);
        } else {
            next.commit_drafts.insert(folder.to_owned(), text);
        }
        if next == self.workspace {
            return Ok(self.finish_sync());
        }
        self.commit(next, self.next_id)
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

    /// A change allowed only before the session has sent anything.
    fn update_unstarted(
        &mut self,
        id: u64,
        change: impl FnOnce(&mut Session),
    ) -> StoreResult<Applied<()>> {
        let started = self
            .workspace
            .sessions
            .iter()
            .find(|session| session.id == id)
            .ok_or_else(|| invalid(format!("unknown session ID {id}")))?
            .started();
        if started {
            return Err(invalid(
                "the provider, account and model are fixed once a session has sent",
            )
            .into());
        }
        self.update_session(id, change)
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

    /// Whether the snapshot `revision` names has been confirmed by a directory sync.
    /// Only answers for revisions this store issued. Once true, it stays true.
    pub fn is_durable(&self, revision: Revision) -> bool {
        self.pending
            .as_ref()
            .is_none_or(|pending| revision.0 < pending.oldest_unconfirmed.0)
    }

    pub fn sync_pending(&mut self) -> io::Result<()> {
        if let Some(pending) = &self.pending {
            #[cfg(any(test, feature = "test-support"))]
            (self.sync_directory)(&pending.directory)?;
            #[cfg(not(any(test, feature = "test-support")))]
            pending.directory.sync_all()?;
            self.pending = None;
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
            revision: Revision(self.writes),
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
        let directory = if self.database.is_memory() {
            None
        } else {
            Some(File::open(parent)?)
        };
        self.database_revision =
            self.database
                .commit(&snapshot, &self.workspace, self.database_revision)?;
        self.writes += 1;
        self.workspace = snapshot.workspace;
        self.next_id = snapshot.next_id;
        if let Some(directory) = directory {
            let oldest_unconfirmed = self
                .pending
                .take()
                .map_or(Revision(self.writes), |pending| pending.oldest_unconfirmed);
            self.pending = Some(PendingSync {
                directory,
                oldest_unconfirmed,
            });
        }
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
    let mut projects = HashSet::new();
    for project in &snapshot.workspace.projects {
        let id = project.id();
        if !id.path().is_absolute() || !projects.insert(id) {
            return Err(invalid("invalid or duplicate project").into());
        }
    }
    let mut accounts = HashSet::new();
    for account in &snapshot.workspace.accounts {
        if account.id().as_str() == AccountId::DEFAULT
            || account.id().as_str().is_empty()
            || !accounts.insert(account.id())
        {
            return Err(invalid("invalid or duplicate account ID").into());
        }
    }
    let mut prompts = HashSet::new();
    for prompt in &snapshot.workspace.prompts {
        if prompt.id() == 0
            || prompt.id() >= snapshot.next_id
            || ids.contains(&prompt.id())
            || !prompts.insert(prompt.id())
        {
            return Err(invalid("invalid or duplicate prompt ID").into());
        }
    }
    delivery::validate(&snapshot.workspace)
}

/// Where a new session goes: selected in a window, saving `show` with it as
/// [`Store::create_showing`] does, or launched by an agent in a session, with the
/// delivery that brings its result back.
enum Opening<'a> {
    Window(
        WindowId,
        Option<(&'a ProjectId, Composition, SessionTabs, ViewId)>,
    ),
    Launched(u64),
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working_directory() -> WorkingDirectory {
        WorkingDirectory::new(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn draft_attachments_are_saved_with_the_session_and_cleared_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
        let image = Attachment {
            sha256: "ab".repeat(32),
            media_type: "image/png".into(),
            name: "shot.png".into(),
        };
        store.set_draft(1, "look".into()).unwrap();
        store.set_draft_attachments(1, vec![image.clone()]).unwrap();
        let reopened = Store::open(&path).unwrap();
        let session = &reopened.workspace().sessions()[0];
        assert_eq!(session.draft(), "look");
        assert_eq!(session.draft_attachments(), [image]);
        store.set_draft_attachments(1, Vec::new()).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert!(
            reopened.workspace().sessions()[0]
                .draft_attachments()
                .is_empty()
        );
        assert_eq!(reopened.workspace().sessions()[0].draft(), "look");
    }

    #[test]
    fn directory_sync_failure_keeps_applied_snapshot_and_retry_does_not_duplicate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store.sync_directory = |_| Err(io::Error::other("directory sync unavailable"));

        let created = store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
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
        let before_retry = json(&path);
        assert!(store.sync_pending().is_err());
        store.sync_directory = File::sync_all;
        store.sync_pending().unwrap();
        store.sync_pending().unwrap();
        assert!(store.pending.is_none());
        assert_eq!(json(&path), before_retry);
        assert_eq!(store.workspace().sessions().len(), 1);
        let next = store
            .create(Provider::Codex, &working_directory(), 3)
            .unwrap();
        assert_eq!(next.value, 2);
        assert_eq!(next.durability, Durability::Synced);
        assert_eq!(Store::open(&path).unwrap().workspace().sessions().len(), 2);
    }

    #[test]
    fn legacy_choice_sync_warning_is_applied_and_same_choice_retries_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(&path, r#"{"version":1,"next_id":2,"workspace":{"selected":1,"sessions":[{"id":1,"title":"Legacy","provider":"Codex","settled":false,"draft":"keep"}]}}"#).unwrap();
        let mut store = Store::open(&path).unwrap();
        let chosen = WorkingDirectory::new(directory.path()).unwrap();
        store.sync_directory = |_| Err(io::Error::other("directory sync unavailable"));
        let applied = store.choose_legacy_directory(1, &chosen).unwrap();
        assert!(matches!(
            applied.durability,
            Durability::DirectorySyncPending(_)
        ));
        let expected = SessionContext::Recorded {
            directory: &chosen,
            origin: ContextOrigin::Chosen,
        };
        assert_eq!(store.workspace().sessions()[0].context(), expected);
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].context(),
            expected
        );
        let before = json(&path);
        assert!(matches!(
            store
                .choose_legacy_directory(1, &chosen)
                .unwrap()
                .durability,
            Durability::DirectorySyncPending(_)
        ));
        assert!(store.sync_pending().is_err());
        assert!(
            store
                .choose_legacy_directory(1, &working_directory())
                .is_err()
        );
        assert!(store.sync_pending().is_err());
        store.sync_directory = File::sync_all;
        assert_eq!(
            store
                .choose_legacy_directory(1, &chosen)
                .unwrap()
                .durability,
            Durability::Synced
        );
        assert!(store.pending.is_none());
        assert_eq!(store.writes(), 1);
        assert_eq!(json(&path), before);
    }

    #[test]
    fn a_confirmed_choice_stays_confirmed_through_later_unrelated_debt() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let path = state.join("state.json");
        std::fs::write(&path, r#"{"version":1,"next_id":2,"workspace":{"selected":1,"sessions":[{"id":1,"title":"Legacy","provider":"Codex","settled":false,"draft":"keep"}]}}"#).unwrap();
        let mut store = Store::open(&path).unwrap();
        let chosen = WorkingDirectory::new(directory.path()).unwrap();

        store.sync_directory = |_| Err(io::Error::other("directory sync unavailable"));
        let choice = store.choose_legacy_directory(1, &chosen).unwrap();
        assert!(matches!(
            choice.durability,
            Durability::DirectorySyncPending(_)
        ));
        assert!(!store.is_durable(choice.revision));

        store.sync_directory = File::sync_all;
        let unrelated = store.set_draft(1, "typed later".into()).unwrap();
        assert_eq!(unrelated.durability, Durability::Synced);
        assert!(store.is_durable(choice.revision));
        assert!(store.is_durable(unrelated.revision));

        store.sync_directory = |_| Err(io::Error::other("directory sync unavailable"));
        let debt = store.set_approval_mode(1, ApprovalMode::ReadOnly).unwrap();
        assert!(matches!(
            debt.durability,
            Durability::DirectorySyncPending(_)
        ));
        assert!(
            store.is_durable(choice.revision),
            "confirmed once, confirmed for good"
        );
        assert!(store.is_durable(unrelated.revision));
        assert!(!store.is_durable(debt.revision));

        let writes = store.writes();
        let before = json(&path);
        let lock = rusqlite::Connection::open(store.path()).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let rejected = store.set_provider_thread(1, "rejected-thread".into());
        lock.execute_batch("ROLLBACK").unwrap();
        assert!(rejected.is_err());
        assert_eq!(
            store.writes(),
            writes,
            "a failure before the rename writes nothing"
        );
        assert_eq!(json(&path), before);
        assert!(store.workspace().sessions()[0].provider_thread().is_none());
        assert!(!store.is_durable(debt.revision));

        let retry = store.set_approval_mode(1, ApprovalMode::ReadOnly).unwrap();
        assert!(matches!(
            retry.durability,
            Durability::DirectorySyncPending(_)
        ));
        assert_eq!(
            store.writes(),
            writes,
            "a no-op retries the sync without a write"
        );
        assert!(!store.is_durable(retry.revision));

        store.sync_directory = File::sync_all;
        let repaired = store.set_approval_mode(1, ApprovalMode::ReadOnly).unwrap();
        assert_eq!(repaired.durability, Durability::Synced);
        assert_eq!(store.writes(), writes);
        assert!(store.is_durable(debt.revision));
        assert!(store.is_durable(retry.revision));
        let saved = json(&path).to_string();
        assert!(saved.contains(r#""origin":"chosen""#), "{saved}");
        assert!(!saved.contains("rejected-thread"), "{saved}");
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].context(),
            SessionContext::Recorded {
                directory: &chosen,
                origin: ContextOrigin::Chosen,
            }
        );
    }

    #[test]
    fn only_explicit_observation_acknowledges_a_reply() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
        store.record_turn_end(1, false, 3_000).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 4_000)
            .unwrap();
        store.select(1).unwrap();
        store.set_draft(1, "typing".into()).unwrap();
        assert_eq!(store.workspace().sessions()[0].viewed_at(), Some(1_000));
        store.record_message(1, "hi", 5_000).unwrap();
        assert_eq!(store.workspace().sessions()[0].viewed_at(), Some(1_000));
        assert_eq!(
            store.workspace().sessions()[0].attention(false, false, true),
            None
        );
        store.record_turn_end(1, false, 6_000).unwrap();
        assert_eq!(
            store.workspace().sessions()[0].attention(false, false, false),
            Some(Attention::Replied)
        );
        store.mark_viewed(1, 7_000).unwrap();
        assert_eq!(
            store.workspace().sessions()[0].attention(false, false, false),
            None
        );
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].viewed_at(),
            Some(7_000)
        );
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
        let saved = json(&path).to_string();
        assert!(!saved.contains("_at"), "{saved}");
        assert!(saved.contains(r#""version":1"#));
    }

    #[test]
    fn attention_prefers_approval_then_a_waiting_widget_then_failure_then_an_unseen_reply() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("state.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 2_000)
            .unwrap();
        let attention = |store: &Store, approval, running| {
            store.workspace().sessions()[0].attention(approval, false, running)
        };
        assert_eq!(attention(&store, false, false), None);

        store.record_turn_end(1, false, 3_000).unwrap();
        assert_eq!(attention(&store, false, false), Some(Attention::Replied));
        store.record_turn_end(1, true, 4_000).unwrap();
        assert_eq!(attention(&store, false, false), Some(Attention::Failed));
        assert_eq!(attention(&store, true, true), Some(Attention::Approval));
        // A widget waiting on the user comes after an approval, before anything else.
        let session = &store.workspace().sessions()[0];
        assert_eq!(session.attention(false, true, true), Some(Attention::Input));
        assert_eq!(
            session.attention(true, true, true),
            Some(Attention::Approval)
        );

        // Viewing a failure does not clear it. The next message does.
        store.select(1).unwrap();
        store.select(2).unwrap();
        store.mark_viewed(1, 6_000).unwrap();
        assert_eq!(attention(&store, false, false), Some(Attention::Failed));
        store.select(1).unwrap();
        store.record_message(1, "hi", 8_000).unwrap();
        assert_eq!(attention(&store, false, true), None);
        // A turn saved as running with nothing running was cut off by a restart.
        assert_eq!(attention(&store, false, false), Some(Attention::Failed));
        assert_eq!(
            Store::open(directory.path().join("state.json"))
                .unwrap()
                .workspace()
                .sessions()[0]
                .attention(false, false, false),
            Some(Attention::Failed)
        );
    }

    #[test]
    fn an_unseen_reply_holds_until_explicitly_observed() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("state.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 2_000)
            .unwrap();
        store.record_turn_end(1, false, 3_000).unwrap();
        let first = |store: &Store| store.workspace().sessions()[0].attention(false, false, false);

        store.select(1).unwrap();
        assert_eq!(first(&store), Some(Attention::Replied));
        store.select(2).unwrap();
        assert_eq!(first(&store), Some(Attention::Replied));
        store.mark_viewed(1, 5_000).unwrap();
        assert_eq!(first(&store), None);

        store.select(1).unwrap();
        store.record_turn_end(1, false, 7_000).unwrap();
        assert_eq!(first(&store), Some(Attention::Replied));
    }

    #[test]
    fn observation_clears_an_unseen_reply_after_the_clock_moves_back() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("state.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 2_000)
            .unwrap();
        store.record_turn_end(1, false, 10_000).unwrap();
        store.select(1).unwrap();
        store.select(2).unwrap();
        store.mark_viewed(1, 9_500).unwrap();
        assert_eq!(
            store.workspace().sessions()[0].attention(false, false, false),
            None
        );
    }

    #[test]
    fn the_first_message_titles_a_session_and_later_ones_do_not() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
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
            let id = store
                .create(Provider::Claude, &working_directory(), 1_000)
                .unwrap()
                .value;
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
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
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
    fn commit_drafts_are_kept_per_folder_through_restarts_until_cleared() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store
            .set_commit_draft("/repo", "Fix the build\n\nDetails".into())
            .unwrap();
        store
            .set_commit_draft("/repo/.keystone/worktrees/a", "Worktree work".into())
            .unwrap();
        let writes = store.writes();
        store
            .set_commit_draft("/repo", "Fix the build\n\nDetails".into())
            .unwrap();
        store.set_commit_draft("/elsewhere", String::new()).unwrap();
        assert_eq!(store.writes(), writes, "an unchanged draft writes nothing");

        let before = store.database().events_after(0, 1000).unwrap().len();
        store
            .set_commit_draft("/repo", "Fix the build\n\nMore details".into())
            .unwrap();
        let events = store.database().events_after(0, 1000).unwrap();
        let added: Vec<_> = events[before..]
            .iter()
            .map(|event| (event.kind.as_str(), event.payload.as_str()))
            .collect();
        assert_eq!(
            added,
            [("commit_draft.saved", r#""Fix the build\n\nMore details""#)],
            "a draft is its own small event, never a copy of the workspace"
        );
        store
            .set_commit_draft("/repo", "Fix the build\n\nDetails".into())
            .unwrap();

        let mut reopened = Store::open(&path).unwrap();
        let workspace = reopened.workspace();
        assert_eq!(workspace.commit_draft("/repo"), "Fix the build\n\nDetails");
        assert_eq!(
            workspace.commit_draft("/repo/.keystone/worktrees/a"),
            "Worktree work"
        );
        assert_eq!(workspace.commit_draft("/elsewhere"), "");

        reopened.set_commit_draft("/repo", String::new()).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.workspace().commit_draft("/repo"), "");
        assert_eq!(
            reopened
                .workspace()
                .commit_draft("/repo/.keystone/worktrees/a"),
            "Worktree work"
        );
    }

    #[test]
    fn a_pull_request_is_saved_on_its_session_through_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let project = WorkingDirectory::new(directory.path()).unwrap();
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Codex, &project, 1).unwrap();
        store.create(Provider::Claude, &project, 2).unwrap();
        let pull_request = PullRequest {
            number: 12,
            url: "https://github.com/owner/repo/pull/12".into(),
        };
        store.set_pull_request(1, pull_request.clone()).unwrap();
        let writes = store.writes();
        store.set_pull_request(1, pull_request.clone()).unwrap();
        assert_eq!(
            store.writes(),
            writes,
            "the same pull request writes nothing"
        );
        assert!(store.set_pull_request(9, pull_request.clone()).is_err());

        let reopened = Store::open(&path).unwrap();
        let sessions = reopened.workspace().sessions();
        assert_eq!(sessions[0].pull_request(), Some(&pull_request));
        assert_eq!(sessions[1].pull_request(), None);
        assert_eq!(
            serde_json::to_value(&sessions[0]).unwrap()["pull_request"],
            serde_json::json!({"number": 12, "url": "https://github.com/owner/repo/pull/12"})
        );
        assert!(
            serde_json::to_value(&sessions[1])
                .unwrap()
                .get("pull_request")
                .is_none()
        );
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
        assert!(!json(&path).to_string().contains("title_source"));

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
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
        store.record_message(1, "hi", 2_000).unwrap();
        store.record_turn_end(1, false, 3_000).unwrap();
        // A late steer opens another turn without a new message.
        store.record_turn_start(1).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.workspace().sessions()[0].attention(false, false, false),
            Some(Attention::Failed)
        );
    }

    fn seeded(path: &Path, layout: &str) {
        let database = database_path(path);
        if database.exists() {
            std::fs::remove_file(database).unwrap();
        }
        std::fs::write(
            path,
            format!(
                r#"{{"version":1,"next_id":2,"future":{{"a":[1,2]}},"workspace":{{"sessions":[{{"id":1,"title":"Old","provider":"Codex","settled":false,"draft":"keep","model":"m-1"}}],"selected":1,"panes":["chat"],"preferences":{{"theme":"Dark","accent":"teal"{layout}}}}}}}"#
            ),
        )
        .unwrap();
    }

    fn json(path: &Path) -> Value {
        if database_path(path).exists() {
            Store::open(path).unwrap().saved_snapshot().unwrap()
        } else {
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        }
    }

    fn saved_sidebar(saved: &Value) -> &Value {
        saved["workspace"]["preferences"]
            .as_object()
            .unwrap()
            .get("sidebar_layout")
            .expect("sidebar_layout saved")
    }

    fn appearance_keys_saved_once(path: &Path) {
        let text = json(path).to_string();
        for key in [r#""theme""#, r#""density""#, r#""ui_font""#] {
            assert_eq!(text.matches(key).count(), 1, "{key} in {text}");
        }
    }

    #[test]
    fn a_state_file_without_a_model_lock_loads_as_sent_and_keeps_no_model() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"version":1,"next_id":2,"workspace":{"sessions":[{"id":1,"title":"Old","provider":"Codex","settled":false,"draft":"keep"}],"selected":1}}"#,
        )
        .unwrap();
        let mut store = Store::open(&path).unwrap();
        let session = &store.workspace().sessions()[0];
        assert_eq!((session.model(), session.model_locked()), (None, None));
        // Nothing says this session never launched, so its provider and model stay.
        assert!(session.started());
        assert!(store.set_model(1, Some("fake-fast".into())).is_err());
        assert!(store.set_provider(1, Provider::Claude).is_err());
        store.set_draft(1, "edited".into()).unwrap();
        let saved = json(&path).to_string();
        assert!(saved.contains(r#""version":1"#));
        assert!(!saved.contains(r#""model":"#), "{saved}");
        assert!(Store::open(&path).unwrap().workspace().sessions()[0].started());
    }

    #[test]
    fn the_model_and_provider_are_fixed_once_the_session_is_locked() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
        let session = &store.workspace().sessions()[0];
        assert!(!session.started());
        assert_eq!(session.model_locked(), Some(false));
        let saved = json(&path).to_string();
        assert!(saved.contains(r#""model_locked":false"#), "{saved}");
        store.set_model(1, Some("fake-fast".into())).unwrap();

        store.lock_model(1).unwrap();
        let session = &store.workspace().sessions()[0];
        assert!(session.started() && session.last_turn().is_none());
        let writes = store.writes();
        assert!(store.set_model(1, None).is_err());
        assert!(store.set_provider(1, Provider::Claude).is_err());
        assert!(store.set_model(9, None).is_err());
        assert_eq!(store.writes(), writes);
        let reopened = Store::open(&path).unwrap();
        let session = &reopened.workspace().sessions()[0];
        assert_eq!(session.model(), Some("fake-fast"));
        assert_eq!(session.provider(), Provider::Codex);
        assert!(session.started());
    }

    #[test]
    fn changing_the_provider_resets_the_model_and_a_placeholder_title_only() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("state.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1_000)
            .unwrap();
        store.set_model(1, Some("fake-fast".into())).unwrap();
        store.set_provider(1, Provider::Claude).unwrap();
        let session = &store.workspace().sessions()[0];
        assert_eq!(session.provider(), Provider::Claude);
        assert_eq!(session.model(), None);
        assert_eq!(session.title(), "New Claude session");

        store.rename(2, "Release notes").unwrap();
        store.set_provider(2, Provider::Claude).unwrap();
        let session = &store.workspace().sessions()[1];
        assert_eq!(session.provider(), Provider::Claude);
        assert_eq!(session.title(), "Release notes");
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
            store
                .create(Provider::Claude, &working_directory(), 1_000)
                .unwrap();
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
                &serde_json::json!({"side": "left", "width_px": 264}),
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
                    ..Appearance::default()
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
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
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
        let saved = json(&path).to_string();
        assert!(saved.contains(r#""version":1"#), "{saved}");
        assert!(
            saved.contains(r#""sidebar_layout":{"side":"right","width_px":360}"#),
            "{saved}"
        );
    }

    #[test]
    fn notifications_load_as_on_and_save_only_once_changed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"version":1,"next_id":1,"workspace":{"sessions":[],"preferences":{"notifications":{"approval":false}}}}"#,
        )
        .unwrap();
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store.workspace().preferences().notifications(),
            Notifications {
                approval: false,
                ..Notifications::default()
            },
            "a missing switch loads as on"
        );

        store.set_notifications(Notifications::default()).unwrap();
        assert!(!json(&path).to_string().contains("notifications"));
        let off = Notifications {
            enabled: false,
            turn_finished: false,
            ..Notifications::default()
        };
        store.set_notifications(off).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.workspace().preferences().notifications(), off);
        assert_eq!(
            Store::open(directory.path().join("new.json"))
                .unwrap()
                .workspace()
                .preferences()
                .notifications(),
            Notifications::default(),
            "a new workspace notifies"
        );
    }

    fn tabbed() -> Composition {
        Composition::default()
            .edit(Edit::Open {
                view: ViewId(5),
                component: Component::Conversation,
                at: Place::TabWith(ViewId::MAIN),
                size: Size::Px(std::num::NonZeroU16::new(400).unwrap()),
            })
            .unwrap()
    }

    fn project_layouts(path: &Path) -> Option<Value> {
        json(path)["workspace"]["preferences"]
            .get("project_layouts")
            .cloned()
    }

    #[test]
    fn a_project_shows_the_legacy_sidebar_until_applied_and_undo_restores_absence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = ProjectId::new(a_dir.path()).unwrap();
        let b = ProjectId::new(b_dir.path()).unwrap();
        seeded(
            &path,
            r#","sidebar_layout":{"side":"right","width_px":360}"#,
        );
        let legacy = Composition::from_sidebar(SidebarLayout {
            side: SidebarSide::Right,
            width: SidebarWidth::new(360).unwrap(),
        });
        let mut store = Store::open(&path).unwrap();
        let preferences = store.workspace().preferences();
        assert_eq!(preferences.project_layout(&a), None);
        assert_eq!(preferences.composition(&a), legacy);
        assert_eq!(preferences.project_layouts().count(), 0);

        store.set_draft(1, "edited".into()).unwrap();
        store
            .set_appearance(Appearance {
                theme: ThemeMode::Light,
                ..Appearance::default()
            })
            .unwrap();
        assert_eq!(project_layouts(&path), None);

        let mine = tabbed();
        let applied = store
            .set_project_layout(&a, Some(mine.clone().into()))
            .unwrap();
        assert_eq!(applied.durability, Durability::Synced);
        assert_eq!(
            project_layouts(&path),
            Some(serde_json::json!({ a.path().to_str().unwrap(): mine }))
        );
        assert_eq!(
            saved_sidebar(&json(&path)),
            &serde_json::json!({"side": "right", "width_px": 360})
        );
        assert_eq!(store.workspace().preferences().composition(&b), legacy);

        store
            .set_project_layout(&b, Some(Composition::default().into()))
            .unwrap();
        store.set_draft(1, "newer".into()).unwrap();
        let reopened = Store::open(&path).unwrap();
        let preferences = reopened.workspace().preferences();
        assert_eq!(preferences.composition(&a), mine);
        assert_eq!(preferences.composition(&b), Composition::default());
        let mut both = vec![a.clone(), b.clone()];
        both.sort();
        assert_eq!(
            preferences
                .project_layouts()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            both
        );

        let mut store = reopened;
        store.set_project_layout(&a, None).unwrap();
        let writes = store.writes();
        store.set_project_layout(&a, None).unwrap();
        assert_eq!(store.writes(), writes);
        let reopened = Store::open(&path).unwrap();
        let preferences = reopened.workspace().preferences();
        assert_eq!(preferences.project_layout(&a), None);
        assert_eq!(preferences.composition(&a), legacy);
        assert_eq!(preferences.composition(&b), Composition::default());
        assert_eq!(preferences.appearance().theme, ThemeMode::Light);
        assert_eq!(reopened.workspace().sessions()[0].draft(), "newer");
        let saved = json(&path);
        assert_eq!(
            saved_sidebar(&saved),
            &serde_json::json!({"side": "right", "width_px": 360})
        );
        assert_eq!(saved["future"], serde_json::json!({"a": [1, 2]}));
        assert_eq!(saved["workspace"]["panes"], serde_json::json!(["chat"]));
        assert_eq!(saved["workspace"]["preferences"]["accent"], "teal");
        assert_eq!(saved["workspace"]["sessions"][0]["model"], "m-1");

        let mut store = reopened;
        store.set_project_layout(&b, None).unwrap();
        assert_eq!(project_layouts(&path), None);
    }

    fn project_tabs(path: &Path) -> Option<Value> {
        json(path)["workspace"]["preferences"]
            .get("project_tabs")
            .cloned()
    }

    #[test]
    fn a_state_from_before_tabs_loads_with_every_view_following_the_selection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let a = ProjectId::new(directory.path()).unwrap();
        seeded(&path, "");
        let mut store = Store::open(&path).unwrap();
        assert!(store.workspace().preferences().session_tabs(&a).is_empty());
        assert_eq!(store.workspace().selected(), Some(1));
        assert_eq!(
            store.workspace().preferences().composition(&a),
            Composition::default()
        );
        store.set_draft(1, "edited".into()).unwrap();
        assert_eq!(project_tabs(&path), None);
    }

    #[test]
    fn show_saves_the_layout_tabs_and_selection_in_one_write_and_restores_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let a = ProjectId::new(directory.path()).unwrap();
        let mut store = Store::open(&path).unwrap();
        for provider in [Provider::Codex, Provider::Claude, Provider::Codex] {
            store.create(provider, &working_directory(), 1).unwrap();
        }
        let split = Composition::default()
            .edit(Edit::Open {
                view: ViewId(3),
                component: Component::Conversation,
                at: Place::Beside {
                    target: ViewId::MAIN,
                    axis: Axis::Row,
                    after: true,
                },
                size: Size::Fill(std::num::NonZeroU16::MIN),
            })
            .unwrap();
        let mut tabs = SessionTabs::default();
        tabs.hold(ViewId::MAIN, 1);
        tabs.hold(ViewId(3), 2);
        tabs.hold(ViewId(4), 99);
        let writes = store.writes();
        store
            .show(&a, Some(split.clone()), tabs.clone(), Some(2))
            .unwrap();
        assert_eq!(store.writes(), writes + 1);
        store
            .show(&a, Some(split.clone()), tabs.clone(), Some(2))
            .unwrap();
        assert_eq!(store.writes(), writes + 1);
        assert_eq!(
            project_tabs(&path),
            Some(serde_json::json!({
                a.path().to_str().unwrap(): [
                    {"view": 0, "session": 1},
                    {"view": 3, "session": 2},
                ]
            }))
        );

        let mut reopened = Store::open(&path).unwrap();
        let preferences = reopened.workspace().preferences();
        assert_eq!(preferences.composition(&a), split);
        assert_eq!(
            preferences.session_tabs(&a).iter().collect::<Vec<_>>(),
            [(ViewId::MAIN, 1), (ViewId(3), 2)]
        );
        assert_eq!(reopened.workspace().selected(), Some(2));

        let writes = reopened.writes();
        assert!(
            reopened
                .show(&a, None, SessionTabs::default(), Some(99))
                .is_err()
        );
        assert_eq!(reopened.writes(), writes);
        reopened
            .show(&a, None, SessionTabs::default(), Some(3))
            .unwrap();
        assert_eq!(project_tabs(&path), None);
        let preferences = reopened.workspace().preferences();
        assert_eq!(preferences.composition(&a), split);
        assert_eq!(reopened.workspace().selected(), Some(3));

        let mut tabs = SessionTabs::default();
        tabs.hold(ViewId::MAIN, 1);
        tabs.hold(ViewId(3), 2);
        reopened.show(&a, None, tabs, Some(1)).unwrap();
        reopened
            .set_project_layout(&a, Some(Composition::default().into()))
            .unwrap();
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .workspace()
                .preferences()
                .session_tabs(&a)
                .iter()
                .collect::<Vec<_>>(),
            [(ViewId::MAIN, 1)],
            "a layout saved without view 3 forgets its tab"
        );
    }

    #[test]
    fn a_session_created_in_a_tab_is_saved_with_its_tab_in_one_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let project = ProjectId::new(directory.path()).unwrap();
        let mut store = Store::open(&path).unwrap();
        let first = store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap()
            .value;
        let mut tabs = SessionTabs::default();
        tabs.hold(ViewId::MAIN, first);
        let layout = Composition::default()
            .edit(Edit::Open {
                view: ViewId(3),
                component: Component::Conversation,
                at: Place::TabWith(ViewId::MAIN),
                size: Size::Fill(std::num::NonZeroU16::MIN),
            })
            .unwrap();
        let writes = store.writes();
        let second = store
            .create_showing(
                Provider::Claude,
                &working_directory(),
                2,
                WindowId::FIRST,
                Some((&project, layout.clone(), tabs, ViewId(3))),
            )
            .unwrap()
            .value;
        assert_eq!(store.writes(), writes + 1);
        let reopened = Store::open(&path).unwrap();
        let preferences = reopened.workspace().preferences();
        assert_eq!(reopened.workspace().selected(), Some(second));
        assert_eq!(preferences.composition(&project), layout);
        assert_eq!(
            preferences
                .session_tabs(&project)
                .iter()
                .collect::<Vec<_>>(),
            [(ViewId::MAIN, first), (ViewId(3), second)]
        );
    }

    #[test]
    fn unreadable_tabs_are_kept_and_only_refuse_a_tab_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let a = ProjectId::new(directory.path()).unwrap();
        seeded(&path, r#","project_tabs":"wide""#);
        let mut store = Store::open(&path).unwrap();
        assert!(store.workspace().preferences().session_tabs(&a).is_empty());
        store.set_draft(1, "edited".into()).unwrap();
        assert!(
            store
                .show(&a, None, SessionTabs::default(), Some(1))
                .is_err()
        );
        assert_eq!(project_tabs(&path), Some(serde_json::json!("wide")));

        let mine = serde_json::json!({a.path().to_str().unwrap(): {"views": "later"}});
        seeded(&path, &format!(r#","project_tabs":{mine}"#));
        let mut store = Store::open(&path).unwrap();
        assert!(store.workspace().preferences().session_tabs(&a).is_empty());
        store
            .show(
                &a,
                Some(Composition::default()),
                SessionTabs::default(),
                Some(1),
            )
            .unwrap();
        assert_eq!(
            project_tabs(&path),
            Some(mine),
            "nothing held, so it is kept"
        );

        let foreign = serde_json::json!({"/gone": {"views": "later"}});
        seeded(&path, &format!(r#","project_tabs":{foreign}"#));
        let mut store = Store::open(&path).unwrap();
        let mut tabs = SessionTabs::default();
        tabs.hold(ViewId::MAIN, 1);
        store.show(&a, None, tabs, Some(1)).unwrap();
        let mut expected = foreign.clone();
        expected[a.path().to_str().unwrap()] = serde_json::json!([{"view": 0, "session": 1}]);
        assert_eq!(project_tabs(&path), Some(expected));
    }

    #[test]
    fn unreadable_project_entries_and_an_unexpected_map_are_kept() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let a = ProjectId::new(directory.path()).unwrap();
        let foreign = serde_json::json!({
            "/gone/project": {"version": 2, "views": []},
            "/also/gone": "junk",
        });
        seeded(&path, &format!(r#","project_layouts":{foreign}"#));
        let mut store = Store::open(&path).unwrap();
        let listed: Vec<_> = store
            .workspace()
            .preferences()
            .project_layouts()
            .map(|(id, saved)| (id, saved.clone()))
            .collect();
        assert_eq!(listed.len(), 2);
        let (gone, raw) = &listed[1];
        assert_eq!(gone.path(), Path::new("/gone/project"));
        assert_eq!(raw.supported(), None);
        assert_eq!(
            store.workspace().preferences().composition(gone),
            Composition::default()
        );

        store.set_project_layout(&a, Some(tabbed().into())).unwrap();
        store
            .set_project_layout(gone, Some(Composition::default().into()))
            .unwrap();
        store.set_project_layout(gone, Some(raw.clone())).unwrap();
        store.set_draft(1, "edited".into()).unwrap();
        let mut expected = foreign.clone();
        expected[a.path().to_str().unwrap()] = serde_json::to_value(tabbed()).unwrap();
        assert_eq!(project_layouts(&path), Some(expected));

        seeded(&path, r#","project_layouts":"wide""#);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.workspace().preferences().project_layouts().count(), 0);
        assert_eq!(
            store.workspace().preferences().composition(&a),
            Composition::default()
        );
        store.set_draft(1, "edited".into()).unwrap();
        let before = json(&path);
        let writes = store.writes();
        assert!(store.set_project_layout(&a, Some(tabbed().into())).is_err());
        assert!(store.set_project_layout(&a, None).is_err());
        assert_eq!(store.writes(), writes);
        assert_eq!(json(&path), before);
        assert_eq!(project_layouts(&path), Some(serde_json::json!("wide")));
    }

    #[test]
    fn a_project_layout_fails_before_rename_or_warns_after_it() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let path = state.join("state.json");
        let a = ProjectId::new(directory.path()).unwrap();
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();

        store.sync_directory = |_| Err(io::Error::other("directory sync unavailable"));
        let applied = store.set_project_layout(&a, Some(tabbed().into())).unwrap();
        assert!(matches!(
            applied.durability,
            Durability::DirectorySyncPending(_)
        ));
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .workspace()
                .preferences()
                .composition(&a),
            tabbed()
        );
        store.sync_directory = File::sync_all;

        let before = json(&path);
        let lock = rusqlite::Connection::open(store.path()).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let failed = store.set_project_layout(&a, None);
        lock.execute_batch("ROLLBACK").unwrap();
        assert!(failed.is_err());
        assert_eq!(store.workspace().preferences().composition(&a), tabbed());
        assert_eq!(json(&path), before);
    }

    #[test]
    fn the_deepest_composition_reopens_from_the_state_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let a = ProjectId::new(directory.path()).unwrap();
        let mut deepest = Composition::default();
        for id in 10.. {
            let axis = if id % 2 == 0 { Axis::Row } else { Axis::Column };
            match deepest.edit(Edit::Open {
                view: ViewId(id),
                component: Component::Conversation,
                at: Place::Beside {
                    target: ViewId::MAIN,
                    axis,
                    after: true,
                },
                size: Size::Px(std::num::NonZeroU16::MAX),
            }) {
                Ok(deeper) => deepest = deeper,
                Err(problem) => {
                    assert_eq!(problem, Problem::TooDeep);
                    break;
                }
            }
        }
        let mut store = Store::open(&path).unwrap();
        store
            .set_project_layout(&a, Some(deepest.clone().into()))
            .unwrap();
        let reopened = Store::open(&path).unwrap();
        let saved = reopened
            .workspace()
            .preferences()
            .project_layout(&a)
            .unwrap();
        assert_eq!(saved.supported(), Some(&deepest));
    }

    #[test]
    fn project_ids_are_canonical_utf8_directories() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let id = ProjectId::new(&project).unwrap();
        assert!(id.path().is_absolute());
        assert_eq!(ProjectId::new(&project.join("../project/.")).unwrap(), id);
        std::os::unix::fs::symlink(&project, directory.path().join("link")).unwrap();
        assert_eq!(ProjectId::new(&directory.path().join("link")).unwrap(), id);

        let missing = ProjectId::new(&directory.path().join("missing")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert!(missing.to_string().contains("missing"), "{missing}");
        std::fs::write(directory.path().join("file"), "").unwrap();
        let file = ProjectId::new(&directory.path().join("file")).unwrap_err();
        assert_eq!(file.kind(), io::ErrorKind::NotADirectory);
        let lossy = directory
            .path()
            .join(std::ffi::OsStr::from_bytes(b"not-utf8-\xff"));
        std::fs::create_dir(&lossy).unwrap();
        let lossy = ProjectId::new(&lossy).unwrap_err();
        assert_eq!(lossy.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_handoff_is_a_new_linked_session_and_leaves_its_source_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut store = Store::open(&path).unwrap();
        let folder = working_directory();
        let source = store.create(Provider::Claude, &folder, 1).unwrap().value;
        store
            .record_message(source, "Fix the login bug", 2)
            .unwrap();
        store.set_draft(source, "half a thought".into()).unwrap();
        let home = directory.path().join("codex-work");
        let work = store
            .add_account(Provider::Codex, "Work", home, &[])
            .unwrap()
            .value;
        let before = store.workspace().sessions()[0].clone();

        let handed = store
            .hand_off(
                source,
                Provider::Codex,
                "context".into(),
                5,
                WindowId::FIRST,
                None,
            )
            .unwrap()
            .value;
        store
            .set_provider_account(handed, Provider::Codex, Some(work.clone()))
            .unwrap();

        assert_ne!(handed, source);
        assert_eq!(store.workspace().selected(), Some(handed));
        assert_eq!(store.workspace().sessions()[0], before);
        let reopened = Store::open(&path).unwrap();
        let sessions = reopened.workspace().sessions();
        assert_eq!(sessions[0], before);
        assert_eq!(sessions[0].handed_off_from(), None);
        let new = &sessions[1];
        assert_eq!(new.handed_off_from(), Some(source));
        assert_eq!(new.provider(), Provider::Codex);
        assert_eq!(new.account(), Some(&work));
        assert_eq!(new.draft(), "context");
        assert_eq!(new.title(), "Fix the login bug");
        assert!(!new.started());
        assert_eq!(new.model(), None);
        assert_eq!(
            new.context(),
            SessionContext::Recorded {
                directory: &folder,
                origin: ContextOrigin::Created,
            }
        );

        store.record_message(handed, "Carry on", 6).unwrap();
        assert_eq!(store.workspace().sessions()[1].title(), "Fix the login bug");
        store.set_provider(handed, Provider::Claude).ok();
        assert_eq!(
            store.workspace().sessions()[1].handed_off_from(),
            Some(source)
        );
    }

    #[test]
    fn a_handoff_keeps_the_pull_request_of_its_branch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let project = WorkingDirectory::new(directory.path()).unwrap();
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Claude, &project, 1).unwrap();
        let pull_request = PullRequest {
            number: 7,
            url: "https://github.com/owner/repo/pull/7".into(),
        };
        store.set_pull_request(1, pull_request.clone()).unwrap();
        let handed = store
            .hand_off(
                1,
                Provider::Codex,
                "context".into(),
                2,
                WindowId::FIRST,
                None,
            )
            .unwrap()
            .value;
        let reopened = Store::open(&path).unwrap();
        let new = reopened
            .workspace()
            .sessions()
            .iter()
            .find(|session| session.id() == handed)
            .unwrap();
        assert_eq!(new.pull_request(), Some(&pull_request));
    }

    #[test]
    fn a_handoff_is_refused_without_a_folder_and_names_its_source_for_a_placeholder_title() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(&path, r#"{"version":1,"next_id":2,"workspace":{"selected":1,"sessions":[{"id":1,"title":"Legacy","provider":"Codex","settled":false,"draft":""}]}}"#).unwrap();
        let mut store = Store::open(&path).unwrap();
        let refused = store
            .hand_off(
                1,
                Provider::Claude,
                "context".into(),
                1,
                WindowId::FIRST,
                None,
            )
            .unwrap_err();
        assert!(refused.to_string().contains("folder"), "{refused}");
        let writes = store.writes();
        assert!(
            store
                .hand_off(9, Provider::Claude, String::new(), 1, WindowId::FIRST, None)
                .is_err()
        );
        assert_eq!(store.writes(), writes);

        let placeholder = store
            .create(Provider::Claude, &working_directory(), 2)
            .unwrap()
            .value;
        let handed = store
            .hand_off(
                placeholder,
                Provider::Codex,
                String::new(),
                4,
                WindowId::FIRST,
                None,
            )
            .unwrap()
            .value;
        let new = store.workspace().sessions().last().unwrap();
        assert_eq!(new.id(), handed);
        assert_eq!(new.title(), format!("Continued from session {placeholder}"));
        assert_eq!(new.account(), None);
        store.record_message(handed, "Continuing from…", 5).unwrap();
        assert_eq!(
            store.workspace().sessions().last().unwrap().title(),
            format!("Continued from session {placeholder}")
        );
        store.rename(handed, "Pick this up").unwrap();
        assert_eq!(
            store.workspace().sessions().last().unwrap().title(),
            "Pick this up"
        );
    }
}
