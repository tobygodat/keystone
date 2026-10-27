use crate::conversation::Recovery;
use crate::presentation::ConversationFolder;
use crate::*;
use gpui::EventEmitter;
use keystone_agent::idle::{IdleCandidate, IdlePolicy};
use keystone_agent::recovery::{self, RecoveryBatch, Snapshot, TurnEvidence};
use keystone_agent::{Catalog, Commands, LaunchProfile};
use keystone_core::SessionContext;
use keystone_core::{
    AccountId, AccountUsage, Applied, Composition, LimitWindow, Placement, ProjectColor, ProjectId,
    SavedComposition, SessionTabs, ViewId, WindowId, WindowView, WorkingDirectory,
};
use std::collections::BTreeMap;

#[cfg(all(test, feature = "ui-tests"))]
#[path = "app_state_tests.rs"]
mod tests;

#[path = "orchestrator.rs"]
mod orchestrator;

#[path = "delivery.rs"]
pub(crate) mod delivery;

#[path = "checkpoints.rs"]
pub(crate) mod checkpoints;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    Running,
    Saving,
    Exiting,
}

enum PendingSave {
    TurnStart(u64),
    TurnEnd(u64, bool, u64),
    Thread(u64, String),
    Message(u64, String, u64),
    Draft(u64, String),
    DraftAttachments(u64, Vec<keystone_core::Attachment>),
    Delivery(u64, keystone_core::DeliveryState),
}

/// What a message is, which decides what sending it may do.
#[derive(Clone, Copy, PartialEq)]
enum SendKind {
    /// The composer's draft, cleared once it is sent.
    Draft,
    /// A widget's action, or the continue after a usage limit. The draft stays.
    Widget,
    /// A launched session's result. Only ever a new turn, never a steer.
    Result,
}

#[derive(Default)]
pub(crate) struct RuntimeChanged {
    pub rows: Vec<RowChange>,
    pub sidebar: bool,
    /// Sessions whose agent finished a file change or a turn, so their folders'
    /// changes may differ.
    pub files: Vec<u64>,
    /// Folders whose saved commit message changed, with the new message.
    pub commit_drafts: Vec<(String, String)>,
}
pub(crate) struct RowChange {
    pub session: u64,
    pub rows: usize,
    pub changed: Vec<Range<usize>>,
    pub replies: Vec<(usize, usize, keystone_agent::ItemStatus)>,
}
/// What a provider answered an import request with.
pub(crate) enum Native {
    Listed(Result<Vec<keystone_agent::import::Listed>, String>),
    Read(Result<keystone_agent::recovery::Snapshot, String>),
}

/// Records `native` as the answer to import request `ticket` if a window still waits
/// on it.
fn answer(waiting: &std::sync::Mutex<HashMap<u64, Option<Native>>>, ticket: u64, native: Native) {
    if let Some(slot) = waiting.lock().unwrap().get_mut(&ticket) {
        *slot = Some(native);
    }
}

pub(crate) struct AppState {
    store: Store,
    /// Worktree folders being removed, which nothing may start in meanwhile.
    removing_worktrees: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    pub(crate) error: Option<String>,
    pub(crate) shutdown_error: Option<String>,
    /// One app-server per Codex account, keyed by account; `None` is the default.
    codex: BTreeMap<Option<AccountId>, CodexConnection>,
    /// One connection per Claude account, each with a process per session.
    claude: BTreeMap<Option<AccountId>, ClaudeConnection>,
    conversations: HashMap<u64, Conversation>,
    /// Each session's checkpoints, as last read from its repository.
    checkpoints: HashMap<u64, checkpoints::State>,
    /// The tree each session's next or running turn starts from.
    turn_starts: checkpoints::Starts,
    /// The name this workspace's checkpoints are saved under, once read.
    checkpoint_store: Option<String>,
    /// Repositories whose files a restore is putting back. No message to a session in
    /// one goes until it ends.
    restoring: std::collections::HashSet<PathBuf>,
    /// Checkpoint saves queued and not done yet, by session.
    saving: HashMap<u64, usize>,
    catalogs: Catalogs,
    logs: Logs,
    log_losses: std::collections::BTreeMap<u64, String>,
    wake: Wake,
    changes: RuntimeChanged,
    pub(crate) phase: Phase,
    /// Set when the last window closed while a turn ran. Keystone then quits, saving
    /// as Quit does, once no turn runs.
    pub(crate) quit_when_idle: bool,
    pending: std::collections::VecDeque<PendingSave>,
    orchestrator: Option<orchestrator::Orchestrator>,
    /// Each session's current recovery request, from its check until its batch saves.
    recoveries: HashMap<u64, u64>,
    next_ticket: u64,
    /// Finished reads of Claude stores: session, ticket and what was read.
    recovery_reads: Arc<std::sync::Mutex<Vec<ClaudeResult>>>,
    /// Claude store reads waiting for one of the [`RECOVERY_READERS`].
    queued_reads: std::collections::VecDeque<ClaudeRead>,
    /// Reads running now, including any abandoned past their deadline.
    active_reads: Arc<std::sync::atomic::AtomicUsize>,
    /// Import requests a window waits on, by ticket, each with its answer once it has
    /// arrived. An answer to a request no window waits on any more is dropped.
    native: Arc<std::sync::Mutex<HashMap<u64, Option<Native>>>>,
    /// Codex history reads made for an import rather than a recovery.
    import_reads: std::collections::HashSet<u64>,
    /// Sessions whose provider exited while their history was loading.
    recover_after_load: std::collections::HashSet<u64>,
    /// Reports still waiting for an acknowledgement while Retry checks again.
    reviewing: HashMap<u64, keystone_agent::recovery::RecoveryReport>,
    /// Sessions whose user chose to send before their history arrived.
    send_unloaded: std::collections::HashSet<u64>,
    /// Sessions with widget edits waiting for [`WIDGET_SAVE_DELAY`] to log them.
    widget_saves: std::collections::HashSet<u64>,
    /// Idle processes waiting for their session's log to save, by session: the log
    /// checkpoint's ticket and the provider's candidate.
    idle_saves: HashMap<u64, (u64, IdleCandidate)>,
    /// Each account's latest usage readings, by [`usage_key`], as saved.
    usage: BTreeMap<String, AccountUsage>,
    /// Readings waiting to be saved off the UI thread, the newest per account, and
    /// whether a save is running.
    usage_saves: Arc<std::sync::Mutex<(BTreeMap<String, AccountUsage>, bool)>>,
    /// Where providers keep each account's files, for Settings → Integrations. The
    /// inherited environment, except in tests, which start with none and name fixture
    /// folders, so no test reads the user's own config.
    pub(crate) config_env: keystone_agent::ConfigEnv,
    /// What each session last reported about its MCP servers. Kept for the run only.
    mcp_status: BTreeMap<u64, SessionMcpStatus>,
    /// Launched sessions whose turn ended, by session, with how it ended. Each one's
    /// result is saved as ready once its history has loaded.
    finished: HashMap<u64, delivery::Outcome>,
    /// Something changed that may let a ready result start its parent's turn.
    deliveries_due: bool,
    /// Set while ready results are being sent, so a send's own pump does not send
    /// them again.
    delivering: bool,
    /// Sessions with a widget press that goes as a message after the current batch. A
    /// result waits for it, so the press is not sent into the result's turn.
    pressing: std::collections::HashSet<u64>,
    /// Idle eviction for new Claude connections, in place of the debug-build setting.
    #[cfg(test)]
    pub(crate) test_idle: Option<IdlePolicy>,
    /// The default Claude account's config dir, in place of the inherited one.
    #[cfg(test)]
    pub(crate) test_claude_home: Option<PathBuf>,
    #[cfg(test)]
    pub(crate) save_calls: usize,
    /// The `codex` and `claude` every new connection runs, in place of the ones found.
    #[cfg(test)]
    pub(crate) test_programs: Option<(PathBuf, PathBuf)>,
    /// Whether turns take checkpoints. Off in tests unless one turns it on, so a test
    /// whose session works in this repository never writes refs into it.
    #[cfg(test)]
    pub(crate) test_checkpoints: bool,
    pub(crate) notifier: crate::notifications::Notifier,
    pub(crate) pull_requests: crate::pull_request::Monitor,
    /// Where attached images are copied, beside the workspace database.
    pub(crate) attachments: keystone_core::attachment::Attachments,
}
impl EventEmitter<RuntimeChanged> for AppState {}

/// Deletes, off the UI thread, attached-image copies past their retention that no
/// draft holds. See [`keystone_core::attachment::keep_for`].
fn sweep_attachments(
    store: &Store,
    attachments: &keystone_core::attachment::Attachments,
    cx: &mut Context<AppState>,
) {
    let Some(keep_for) = keystone_core::attachment::keep_for() else {
        return;
    };
    let keep: std::collections::HashSet<String> = store
        .workspace()
        .sessions()
        .iter()
        .flat_map(|session| session.draft_attachments())
        .map(|attachment| attachment.sha256.clone())
        .collect();
    let attachments = attachments.clone();
    cx.background_spawn(async move {
        if let Err(error) = attachments.sweep(&keep, keep_for, std::time::SystemTime::now()) {
            eprintln!("Keystone could not clear old attached images: {error}");
        }
    })
    .detach();
}

/// Most Claude store reads at once. A read stuck on a slow disk keeps its slot.
const RECOVERY_READERS: usize = 2;

/// What Continue sends to a session its usage limit stopped. Toby may change it.
pub(crate) const CONTINUE_TEXT: &str = "Your usage limit has reset. Continue where you left off, \
     without repeating work that is already done.";

/// How often a due continue checks again while its history loads or a quit is saving.
const CONTINUE_POLL_MS: u64 = 250;

/// How long a due continue waits to retry after its cancellation could not be saved.
const CONTINUE_RETRY_MS: u64 = 5_000;

/// How long a widget edit waits before it is logged, so typing logs the widget once
/// rather than at every key.
const WIDGET_SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(750);
/// How long a recovery check may take before the app stops waiting for it.
pub(crate) const RECOVERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

const UNREAD: &str = "Keystone could not read this conversation's history";

const NO_THREAD: &str = "this conversation's provider ID was never saved, so sending starts a new provider conversation";

/// An MCP server's status and when Keystone received it, in unix milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReportedStatus {
    pub status: keystone_core::integrations::ServerStatus,
    pub at_ms: u64,
}

/// One session's last MCP report: the connection it came from, the session's folder,
/// and when Keystone received it, in unix milliseconds.
struct SessionMcpStatus {
    provider: Provider,
    account: Option<AccountId>,
    folder: Option<PathBuf>,
    servers: Vec<keystone_core::integrations::ServerStatus>,
    at_ms: u64,
}

/// A finished Claude store read: session, ticket and what it found.
type ClaudeResult = (u64, u64, Result<Snapshot, String>);

struct ClaudeRead {
    session: u64,
    ticket: u64,
    root: PathBuf,
    thread: String,
    cwd: PathBuf,
}
impl AppState {
    pub(crate) fn log_losses(&self) -> impl Iterator<Item = &String> {
        self.log_losses.values()
    }

    /// Shows what a restore of the session would show as lost now.
    fn record_loss(&mut self, session: u64) {
        let lost = self.logs.lost_entries(session);
        if lost == 0 {
            self.log_losses.remove(&session);
            return;
        }
        let entries = if lost == 1 {
            "1 log entry was".to_owned()
        } else {
            format!("{lost} log entries were")
        };
        self.log_losses.insert(
            session,
            format!("Session {session}: {entries} lost (log budget exceeded)."),
        );
    }
    pub(crate) fn conversations(&self) -> &HashMap<u64, Conversation> {
        &self.conversations
    }
    #[cfg(test)]
    pub(crate) fn test_conversation(&mut self, id: u64) -> &mut Conversation {
        self.conversation(id)
    }
    #[cfg(test)]
    pub(crate) fn test_conversations_mut(&mut self) -> &mut HashMap<u64, Conversation> {
        &mut self.conversations
    }
    #[cfg(test)]
    pub(crate) fn test_catalogs_mut(&mut self) -> &mut Catalogs {
        &mut self.catalogs
    }
    #[cfg(all(test, feature = "ui-tests"))]
    pub(crate) fn test_sync_directory(&mut self, sync: fn(&std::fs::File) -> std::io::Result<()>) {
        self.store.sync_directory = sync;
    }
    /// The default account's connection, else any started one.
    #[cfg(test)]
    pub(crate) fn test_codex(&self) -> Option<&CodexConnection> {
        self.codex.values().next()
    }
    #[cfg(test)]
    pub(crate) fn test_set_codex(&mut self, value: Option<CodexConnection>) {
        match value {
            Some(value) => self.codex.insert(None, value),
            None => self.codex.remove(&None),
        };
    }
    #[cfg(test)]
    pub(crate) fn test_codex_accounts(&self) -> Vec<Option<AccountId>> {
        self.codex.keys().cloned().collect()
    }
    /// The default account's connection, else any started one.
    #[cfg(test)]
    pub(crate) fn test_claude(&self) -> Option<&ClaudeConnection> {
        self.claude.values().next()
    }
    #[cfg(test)]
    pub(crate) fn test_set_claude(&mut self, value: Option<ClaudeConnection>) {
        match value {
            Some(value) => self.claude.insert(None, value),
            None => self.claude.remove(&None),
        };
    }
    #[cfg(test)]
    pub(crate) fn test_claude_accounts(&self) -> Vec<Option<AccountId>> {
        self.claude.keys().cloned().collect()
    }
    #[cfg(test)]
    pub(crate) fn test_set_logs(&mut self, value: Logs) {
        self.logs = value;
    }
    #[cfg(test)]
    pub(crate) fn test_wake(&self) -> &Wake {
        &self.wake
    }

    pub(crate) fn reserve_quit(&mut self) -> bool {
        if self.phase != Phase::Running {
            return false;
        }
        self.phase = Phase::Saving;
        true
    }

    pub(crate) fn quit_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.phase = Phase::Running;
        self.shutdown_error = Some(format!(
            "Could not save before quitting. Fix storage and retry Quit. {error}"
        ));
        self.publish(cx);
    }

    /// Saves each unsaved project note, then every window's draft, placement and project
    /// in one write, then everything still waiting to be saved. The one save path for
    /// Quit, however it was asked for.
    pub(crate) fn save(
        &mut self,
        drafts: Vec<ComposerDraft>,
        notes: Vec<(ProjectId, String)>,
        windows: Vec<(WindowId, Placement, Option<ProjectId>)>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.phase == Phase::Exiting {
            return Err("Keystone is already exiting.".into());
        }
        self.phase = Phase::Saving;
        #[cfg(test)]
        {
            self.save_calls += 1;
        }
        let mut errors = Vec::new();
        let sessions: Vec<u64> = drafts.iter().map(|d| d.session).collect();
        let drafts = drafts.into_iter().map(|d| (d.session, d.text)).collect();
        for (project, text) in notes {
            if let Err(error) = self.store.set_note(&project, text) {
                errors.push(format!("Note not saved. {error}"));
            }
        }
        match self.store.save_for_quit(drafts, windows, crate::now_ms()) {
            Ok(_) => self.pending.retain(
                |update| !matches!(update, PendingSave::Draft(id, _) if sessions.contains(id)),
            ),
            Err(error) => errors.push(format!("Draft not saved. {error}")),
        }
        let logs = self.flush_logs(cx);
        self.retry_updates();
        if let Some(error) = &self.error {
            errors.push(error.clone());
        }
        match logs {
            Err(error) => errors.push(error),
            Ok(()) => {
                for (session, conversation) in &self.conversations {
                    if let Some(error) = &conversation.save_error {
                        errors.push(format!("Session {session}: {error}"));
                    }
                }
            }
        }
        if let Err(error) = self.store.sync_pending() {
            errors.push(format!("Workspace directory sync failed. {error}"));
        }
        if !errors.is_empty() {
            let error = errors.join("\n");
            self.quit_failed(error.clone(), cx);
            return Err(error);
        }
        self.shutdown_error = None;
        self.changes.sidebar = true;
        self.publish(cx);
        Ok(())
    }

    pub(crate) fn teardown(&mut self, cx: &mut Context<Self>) {
        self.phase = Phase::Exiting;
        drop(self.orchestrator.take());
        self.pull_requests.stop();
        self.codex.clear();
        self.claude.clear();
        self.publish(cx);
    }

    fn persist(&mut self, update: PendingSave) {
        self.pending.push_back(update);
        self.retry_updates();
    }

    fn retry_updates(&mut self) {
        while let Some(update) = self.pending.front() {
            let (result, message) = match update {
                PendingSave::TurnStart(id) => (
                    self.store.record_turn_start(*id),
                    "Could not save the session",
                ),
                PendingSave::TurnEnd(id, failed, now) => (
                    self.store.record_turn_end(*id, *failed, *now),
                    "Could not save the session",
                ),
                PendingSave::Thread(id, thread) => (
                    self.store.set_provider_thread(*id, thread.clone()),
                    "Could not save the conversation ID",
                ),
                PendingSave::Message(id, text, now) => (
                    self.store.record_message(*id, text, *now),
                    "Could not save the session",
                ),
                PendingSave::Draft(id, text) => {
                    (self.store.set_draft(*id, text.clone()), "Draft not saved")
                }
                PendingSave::DraftAttachments(id, attachments) => (
                    self.store.set_draft_attachments(*id, attachments.clone()),
                    "Draft not saved",
                ),
                PendingSave::Delivery(child, state) => (
                    self.store.set_delivery(*child, state.clone()),
                    "Result not saved",
                ),
            };
            if let Err(error) = result {
                self.error = Some(format!("{message}. {error}"));
                return;
            }
            // A result whose ready save waited goes now.
            if let Some(PendingSave::Delivery(_, keystone_core::DeliveryState::Ready { .. })) =
                self.pending.pop_front()
            {
                self.deliveries_due = true;
                (self.wake)();
            }
            self.changes.sidebar = true;
        }
        self.error = None;
    }

    pub(crate) fn sync_pending(&mut self, cx: &mut Context<Self>) -> std::io::Result<()> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting."));
        }

        let result = self.store.sync_pending();
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn set_draft(
        &mut self,
        id: u64,
        text: String,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_draft(id, text);
        if result.is_ok() {
            self.pending.retain(
                |update| !matches!(update, PendingSave::Draft(session, _) if *session == id),
            );
            self.retry_updates();
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Saves the images attached to the session's draft. Every window reads them from
    /// the store, so a session open in two windows shows the same ones.
    pub(crate) fn set_draft_attachments(
        &mut self,
        id: u64,
        attachments: Vec<keystone_core::Attachment>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self.store.set_draft_attachments(id, attachments);
        if result.is_ok() {
            self.pending.retain(|update| {
                !matches!(update, PendingSave::DraftAttachments(session, _) if *session == id)
            });
            self.retry_updates();
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn set_note(
        &mut self,
        project: &ProjectId,
        text: String,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self.store.set_note(project, text);
        self.publish(cx);
        result
    }

    pub(crate) fn commit_draft(&self, folder: &str) -> String {
        self.store.workspace().commit_draft(folder).to_owned()
    }

    /// Saves `folder`'s commit message and shows it in every window's Changes view.
    pub(crate) fn set_commit_draft(
        &mut self,
        folder: &str,
        text: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.phase == Phase::Exiting {
            return Err("Keystone is exiting.".into());
        }
        if self.store.workspace().commit_draft(folder) == text {
            return Ok(());
        }
        self.store
            .set_commit_draft(folder, text.clone())
            .map_err(|error| error.to_string())?;
        self.changes.commit_drafts.push((folder.to_owned(), text));
        self.publish(cx);
        Ok(())
    }

    /// Saves the pull request made or found for `session`'s branch, and shows it.
    pub(crate) fn set_pull_request(
        &mut self,
        session: u64,
        pull_request: keystone_core::PullRequest,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.phase == Phase::Exiting {
            return Err("Keystone is exiting.".into());
        }
        self.store
            .set_pull_request(session, pull_request)
            .map_err(|error| error.to_string())?;
        self.publish(cx);
        // Asked about at once, so its first report, whose alerts count as seen, comes
        // before checks run on a new pull request.
        self.refresh_pull_request(session, cx);
        Ok(())
    }

    /// Saves gh's `report` on `link`, linking it to `session` when gh found it by the
    /// session's branch. A report on another pull request than the one the session
    /// links now is dropped.
    pub(crate) fn record_pull_request(
        &mut self,
        session: u64,
        link: keystone_core::PullRequest,
        report: keystone_core::PullRequestReport,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.phase == Phase::Exiting {
            return Err("Keystone is exiting.".into());
        }
        match self.session(session).map(|s| s.pull_request()) {
            None => return Ok(()),
            Some(Some(linked)) if linked.url != link.url => return Ok(()),
            Some(Some(_)) => {}
            Some(None) => {
                self.store
                    .set_pull_request(session, link.clone())
                    .map_err(|error| error.to_string())?;
            }
        }
        self.store
            .record_pull_request(session, &link.url, report)
            .map_err(|error| error.to_string())?;
        self.refresh(cx);
        Ok(())
    }

    /// The user has looked at `session`'s pull request, so its dot clears.
    pub(crate) fn see_pull_request(&mut self, session: u64, cx: &mut Context<Self>) {
        if self.phase == Phase::Running && self.store.see_pull_request(session).is_ok() {
            self.refresh(cx);
        }
    }

    pub(crate) fn set_approval_mode(
        &mut self,
        id: u64,
        mode: ApprovalMode,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_approval_mode(id, mode);
        if result.is_ok()
            && let Some((_, approval)) = self.approval(id)
            && let Some(connection) = self.started(id)
        {
            connection.set_approval(id, approval);
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    #[cfg(test)]
    pub(crate) fn lock_model(
        &mut self,
        id: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.lock_model(id);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn set_provider(
        &mut self,
        id: u64,
        provider: Provider,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_provider(id, provider);
        if result.is_ok() {
            self.conversation(id).clear_provider_notice();
            self.commit(id);
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn set_model(
        &mut self,
        id: u64,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_model(id, model);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Moves the session to `provider` and one of its accounts in one save.
    pub(crate) fn set_provider_account(
        &mut self,
        id: u64,
        provider: Provider,
        account: Option<AccountId>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_provider_account(id, provider, account);
        if result.is_ok() {
            self.conversation(id).clear_provider_notice();
            self.commit(id);
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Adds an account whose CLI uses `folder` as its config home, creating the folder,
    /// private to the user, when it does not exist yet. Keystone never signs it in: the
    /// user runs [`keystone_agent::sign_in_command`] in a terminal.
    pub(crate) fn add_account(
        &mut self,
        provider: Provider,
        label: &str,
        folder: &str,
        cx: &mut Context<Self>,
    ) -> Result<Applied<AccountId>, String> {
        if self.phase == Phase::Exiting {
            return Err("Keystone is exiting.".into());
        }
        let user_home = std::env::var_os("HOME").map(PathBuf::from);
        let home = real_path(&keystone_core::expand_home(folder, user_home.as_deref())?);
        if home.parent().is_none() {
            return Err("The account folder cannot be the filesystem root.".into());
        }
        let taken: Vec<PathBuf> = [Provider::Claude, Provider::Codex]
            .into_iter()
            .filter_map(keystone_agent::default_home)
            .chain(
                self.store
                    .workspace()
                    .accounts()
                    .iter()
                    .map(|account| account.home().to_owned()),
            )
            .flat_map(|home| [real_path(&home), home])
            .collect();
        self.store
            .check_account(provider, label, &real_path(&home), &taken)
            .map_err(|error| error.to_string())?;
        let remove_created = |first: &std::path::Path| {
            for folder in home.ancestors() {
                let _ = std::fs::remove_dir(folder);
                if folder == first {
                    break;
                }
            }
        };
        let created = match std::fs::metadata(&home) {
            Ok(metadata) if metadata.is_dir() => None,
            Ok(_) => return Err(format!("{} is not a folder.", home.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let first = home
                    .ancestors()
                    .take_while(|folder| !folder.exists())
                    .last()
                    .unwrap_or(&home)
                    .to_owned();
                use std::os::unix::fs::DirBuilderExt;
                if let Err(error) = std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&home)
                {
                    remove_created(&first);
                    return Err(format!("Could not create {}. {error}", home.display()));
                }
                Some(first)
            }
            Err(error) => return Err(format!("Could not use {}. {error}", home.display())),
        };
        let result = std::fs::canonicalize(&home)
            .map_err(|error| format!("Could not use {}. {error}", home.display()))
            .and_then(|real| {
                self.store
                    .add_account(provider, label, real, &taken)
                    .map_err(|error| error.to_string())
            });
        if let (Err(_), Some(first)) = (&result, &created) {
            remove_created(first);
        }
        // The store forgot any readings left under a reused ID; so does this run.
        if let Ok(added) = &result {
            self.usage.remove(&usage_key(provider, Some(&added.value)));
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn rename_account(
        &mut self,
        id: &AccountId,
        label: &str,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.rename_account(id, label);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Applies one change to the saved prompts, which windows show from the store.
    pub(crate) fn change_prompts<T>(
        &mut self,
        change: impl FnOnce(&mut keystone_core::Store) -> Result<Applied<T>, Box<dyn std::error::Error>>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<T>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = change(&mut self.store);
        self.publish(cx);
        result
    }

    /// Removes an account no session uses, ending any model probe still running on
    /// it. Its folder stays on disk.
    pub(crate) fn remove_account(
        &mut self,
        id: &AccountId,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.remove_account(id);
        if result.is_ok() {
            let key = Some(id.clone());
            self.codex.remove(&key);
            self.claude.remove(&key);
            self.catalogs.forget_account(id);
            // The store forgot its saved readings in the same transaction; a save
            // still queued must not write them back.
            let mut saves = self.usage_saves.lock().unwrap();
            for provider in [Provider::Claude, Provider::Codex] {
                let key = usage_key(provider, Some(id));
                self.usage.remove(&key);
                saves.0.remove(&key);
            }
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    #[cfg(test)]
    pub(crate) fn set_project_layout(
        &mut self,
        project: &ProjectId,
        layout: Option<SavedComposition>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        self.set_layout(project, WindowId::FIRST, layout, cx)
    }

    pub(crate) fn set_layout(
        &mut self,
        project: &ProjectId,
        window: WindowId,
        layout: Option<SavedComposition>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_layout_in(project, window, layout);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn show(
        &mut self,
        project: &ProjectId,
        window: WindowId,
        layout: Option<Composition>,
        tabs: SessionTabs,
        selected: Option<u64>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.show_in(project, window, layout, tabs, selected);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn set_appearance(
        &mut self,
        appearance: Appearance,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_appearance(appearance);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn set_notifications(
        &mut self,
        notifications: keystone_core::Notifications,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_notifications(notifications);
        self.publish(cx);
        result
    }

    pub(crate) fn set_sounds(
        &mut self,
        sounds: keystone_core::Sounds,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_sounds(sounds);
        self.publish(cx);
        result
    }

    pub(crate) fn set_settled(
        &mut self,
        id: u64,
        settled: bool,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        if settled
            && let Some(reason) = self
                .conversations
                .get(&id)
                .and_then(Conversation::settle_blocker)
        {
            return Err(std::io::Error::other(reason).into());
        }
        let result = self.store.set_settled(id, settled);
        if !settled {
            self.deliveries_due();
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn rename(
        &mut self,
        id: u64,
        title: &str,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.rename(id, title);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    #[cfg(test)]
    pub(crate) fn record_turn_end(
        &mut self,
        id: u64,
        failed: bool,
        now: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.record_turn_end(id, failed, now);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn mark_viewed(
        &mut self,
        id: u64,
        now: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.mark_viewed(id, now);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn select(
        &mut self,
        window: WindowId,
        id: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.select_in(window, id);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn choose_legacy_directory(
        &mut self,
        session: u64,
        directory: &WorkingDirectory,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.choose_legacy_directory(session, directory);
        if result.is_ok() {
            self.conversation(session).error = None;
            self.commit(session);
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    #[cfg(test)]
    pub(crate) fn create(
        &mut self,
        provider: Provider,
        directory: &WorkingDirectory,
        now: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        self.create_showing(provider, directory, now, WindowId::FIRST, None, cx)
    }

    /// Creates a session, saving `show` with it in the same write. See
    /// [`keystone_core::Store::create_showing`].
    pub(crate) fn create_showing(
        &mut self,
        provider: Provider,
        directory: &WorkingDirectory,
        now: u64,
        window: WindowId,
        show: Option<(&ProjectId, Composition, SessionTabs, ViewId)>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self
            .store
            .create_showing(provider, directory, now, window, show);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Hands `source` off in `window`, saving `show` with the new session. See
    /// [`keystone_core::Store::hand_off`].
    pub(crate) fn hand_off(
        &mut self,
        source: u64,
        provider: Provider,
        draft: String,
        now: u64,
        window: WindowId,
        show: Option<(&ProjectId, Composition, SessionTabs, ViewId)>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self
            .store
            .hand_off(source, provider, draft, now, window, show);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Asks for a second opinion on `source` in `window`, saving `show` with the new
    /// session. See [`keystone_core::Store::second_opinion`].
    pub(crate) fn second_opinion(
        &mut self,
        source: u64,
        provider: Provider,
        draft: String,
        now: u64,
        window: WindowId,
        show: Option<(&ProjectId, Composition, SessionTabs, ViewId)>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self
            .store
            .second_opinion(source, provider, draft, now, window, show);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Forks `source` in `window`, saving `show` with the new session. See
    /// [`keystone_core::Store::fork`]. A native fork's conversation starts as `log`, the
    /// source's rows it keeps, shown at once and saved as the fork's own log.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fork(
        &mut self,
        source: u64,
        point: Option<keystone_core::ForkPoint>,
        draft: String,
        log: Vec<keystone_agent::log::Write>,
        now: u64,
        window: WindowId,
        show: Option<(&ProjectId, Composition, SessionTabs, ViewId)>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self.store.fork(source, point, draft, now, window, show);
        if let Ok(applied) = &result {
            let fork = applied.value;
            let mut conversation = Conversation::loading();
            conversation.restore(Ok(log.iter().map(|write| write.entry.clone()).collect()));
            self.conversations.insert(fork, conversation);
            if self.logs.append(fork, log).is_err() {
                self.record_loss(fork);
            }
            self.commit(fork);
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Imports `provider`'s conversation `thread` under `account` as a new session in
    /// `directory`, selected in `window`, whose log starts with `snapshot`'s newest
    /// turns. See [`keystone_core::Store::import`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn import(
        &mut self,
        provider: Provider,
        account: Option<AccountId>,
        directory: &WorkingDirectory,
        thread: String,
        title: &str,
        snapshot: &keystone_agent::recovery::Snapshot,
        now: u64,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let entries = keystone_agent::import::log(snapshot)
            .map_err(|error| std::io::Error::other(format!("Not imported. {error}")))?;
        let result = self
            .store
            .import(provider, account, directory, thread, title, now, window);
        if let Ok(applied) = &result {
            let session = applied.value;
            let mut conversation = Conversation::loading();
            conversation.restore(Ok(entries.clone()));
            self.conversations.insert(session, conversation);
            let last = entries.len().saturating_sub(1);
            let writes = entries
                .into_iter()
                .enumerate()
                .map(|(n, entry)| keystone_agent::log::Write {
                    entry,
                    sync: n == last,
                })
                .collect();
            if self.logs.append(session, writes).is_err() {
                self.record_loss(session);
            }
            self.commit(session);
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Creates a session that works in `worktree`, saving `show` with it as
    /// [`AppState::create_showing`] does.
    pub(crate) fn create_worktree_showing(
        &mut self,
        provider: Provider,
        worktree: &keystone_core::WorktreeContext,
        now: u64,
        window: WindowId,
        show: Option<(&ProjectId, Composition, SessionTabs, ViewId)>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self
            .store
            .create_worktree_showing(provider, worktree, now, window, show);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Creates a session an agent in `parent` starts in `worktree`, as
    /// [`AppState::create_unselected`] does.
    pub(crate) fn create_worktree_unselected(
        &mut self,
        provider: Provider,
        worktree: &keystone_core::WorktreeContext,
        now: u64,
        parent: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self
            .store
            .create_worktree_unselected(provider, worktree, now, parent);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Creates the quick composer's session. See [`keystone_core::Store::create_quick`].
    pub(crate) fn create_quick(
        &mut self,
        provider: Provider,
        account: Option<AccountId>,
        model: Option<String>,
        directory: &WorkingDirectory,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        let result = self
            .store
            .create_quick(provider, account, model, directory, now_ms());
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn set_quick_draft(
        &mut self,
        draft: keystone_core::QuickDraft,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        self.store.set_quick_draft(draft)
    }

    /// Creates a session an agent in `parent` starts, selected in no window, with the
    /// record that brings its result back to `parent`.
    pub(crate) fn create_unselected(
        &mut self,
        provider: Provider,
        directory: &WorkingDirectory,
        now: u64,
        parent: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self
            .store
            .create_unselected(provider, directory, now, parent);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Saves a new window showing `selected`, or nothing. See
    /// [`keystone_core::Store::open_window`].
    pub(crate) fn open_window(
        &mut self,
        project: Option<&ProjectId>,
        selected: Option<u64>,
        placement: Option<Placement>,
    ) -> Result<Applied<WindowId>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        self.store.open_window(project, selected, placement)
    }

    /// Saves both windows a tab moved between in one write. See
    /// [`keystone_core::Store::move_tab`].
    pub(crate) fn move_tab(
        &mut self,
        from: WindowView,
        to: WindowView,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        let result = self.store.move_tab(from, to);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Saves the window a tab left and a new window holding it in one write. See
    /// [`keystone_core::Store::move_tab_to_new_window`].
    pub(crate) fn move_tab_to_new_window(
        &mut self,
        from: WindowView,
        session: u64,
        placement: Option<Placement>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<WindowId>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        let result = self.store.move_tab_to_new_window(from, session, placement);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Shows `project` in `window`. See [`keystone_core::Store::open_project`].
    pub(crate) fn open_project(
        &mut self,
        window: WindowId,
        project: &ProjectId,
        left: Option<&ProjectId>,
        selected: Option<u64>,
        now: u64,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        self.store
            .open_project(window, project, left, selected, now)
            .map_err(Into::into)
    }

    /// Every window repaints, since another may show the same project.
    pub(crate) fn set_project_color(
        &mut self,
        project: &ProjectId,
        color: Option<ProjectColor>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        let result = self.store.set_project_color(project, color);
        self.changes.sidebar = true;
        self.publish(cx);
        result.map_err(Into::into)
    }

    /// Takes `project` off the project list. See
    /// [`keystone_core::Store::forget_project`].
    pub(crate) fn forget_project(
        &mut self,
        project: &ProjectId,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        self.store.forget_project(project).map_err(Into::into)
    }

    /// Forgets a closed window, keeping its sessions and their turns as they are. See
    /// [`keystone_core::Store::close_window`].
    pub(crate) fn close_window(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<Applied<Option<WindowId>>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        let result = self.store.close_window(window);
        self.pull_requests.window_closed(window);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    pub(crate) fn set_placement(
        &mut self,
        window: WindowId,
        placement: Placement,
        project: Option<&ProjectId>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        self.store.set_placement(window, placement, project)
    }

    /// Whether any turn runs, so Keystone keeps running without a window until it ends.
    pub(crate) fn turn_running(&self) -> bool {
        self.conversations.values().any(|c| c.running)
    }

    /// Whether a running turn waits for the user to answer an approval request.
    pub(crate) fn awaiting_answer(&self) -> bool {
        self.conversations
            .values()
            .any(|c| c.running && c.awaiting_approval())
    }

    /// While no window is open: quits once no turn runs, and opens the windows again
    /// when a turn asks for an approval, so the user can answer it.
    fn quit_if_idle(&mut self, cx: &mut Context<Self>) {
        if !self.quit_when_idle || self.phase != Phase::Running {
            return;
        }
        if !self.turn_running() {
            self.quit_when_idle = false;
            cx.defer(request_quit);
        } else if self.awaiting_answer() {
            self.quit_when_idle = false;
            let state = cx.entity();
            cx.defer(move |cx| windows::reopen(state, cx));
        }
    }

    pub(crate) fn new(store: Store, cx: &mut Context<Self>) -> Self {
        let (wake_tx, mut wake_rx) = futures::channel::mpsc::unbounded::<()>();
        cx.spawn(async move |this, cx| {
            while wake_rx.next().await.is_some() {
                if this.update(cx, |this, cx| this.pump(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let wake: Wake = Arc::new(move || {
            let _ = wake_tx.unbounded_send(());
        });
        // The first pump sends results that were ready when Keystone last closed.
        wake();
        let logs = Logs::with_database(store.database(), wake.clone());
        let data_dir = store.path().parent().unwrap_or(std::path::Path::new("."));
        let attachments = keystone_core::attachment::Attachments::in_data_dir(data_dir);
        crate::sounds::SoundPlayer::install(data_dir, cx);
        sweep_attachments(&store, &attachments, cx);
        cx.set_global(crate::attachments::AttachmentsDir(
            attachments.dir().to_owned(),
        ));
        // A small log budget for scripted native runs; release builds keep the default.
        #[cfg(debug_assertions)]
        let logs = match std::env::var("KEYSTONE_TEST_LOG_BUDGET_BYTES")
            .ok()
            .and_then(|bytes| bytes.parse().ok())
        {
            Some(bytes) => logs.with_budget(bytes),
            None => logs,
        };
        let mut state = Self {
            removing_worktrees: Arc::default(),
            error: None,
            shutdown_error: None,
            codex: BTreeMap::new(),
            claude: BTreeMap::new(),
            conversations: HashMap::new(),
            checkpoints: HashMap::new(),
            turn_starts: checkpoints::Starts::default(),
            checkpoint_store: None,
            restoring: Default::default(),
            saving: HashMap::new(),
            catalogs: Catalogs::default(),
            logs,
            log_losses: Default::default(),
            wake,
            changes: RuntimeChanged::default(),
            phase: Phase::Running,
            quit_when_idle: false,
            pending: Default::default(),
            orchestrator: None,
            recoveries: HashMap::new(),
            next_ticket: 0,
            recovery_reads: Arc::default(),
            queued_reads: Default::default(),
            native: Arc::default(),
            import_reads: Default::default(),
            active_reads: Arc::default(),
            recover_after_load: Default::default(),
            widget_saves: Default::default(),
            send_unloaded: Default::default(),
            idle_saves: HashMap::new(),
            usage: store
                .database()
                .usage()
                .unwrap_or_else(|error| {
                    eprintln!("Keystone could not load saved usage: {error}");
                    Vec::new()
                })
                .into_iter()
                .map(|(key, mut usage)| {
                    usage.prune(now_ms());
                    (key, usage)
                })
                .collect(),
            usage_saves: Arc::default(),
            #[cfg(not(test))]
            config_env: keystone_agent::ConfigEnv::inherited(),
            #[cfg(test)]
            config_env: keystone_agent::ConfigEnv::default(),
            mcp_status: BTreeMap::new(),
            finished: delivery::unsaved_results(store.workspace()),
            // Results saved before a restart may be ready for their parents.
            deliveries_due: true,
            delivering: false,
            pressing: Default::default(),
            #[cfg(test)]
            test_idle: None,
            reviewing: HashMap::new(),
            #[cfg(test)]
            test_claude_home: None,
            #[cfg(test)]
            save_calls: 0,
            #[cfg(test)]
            test_programs: None,
            #[cfg(test)]
            test_checkpoints: false,
            notifier: Default::default(),
            pull_requests: Default::default(),
            attachments,
            store,
        };
        // A continue scheduled before a restart still goes once it is due. One that came
        // due while Keystone was closed is not sent: it waits for the user on the card.
        let now = now_ms();
        let scheduled: Vec<_> = state
            .store
            .workspace()
            .sessions()
            .iter()
            .filter_map(|session| Some((session.id(), session.continue_at()?)))
            .collect();
        for (session, at) in scheduled {
            if at > now {
                Self::wait_to_continue(session, at, at - now, cx);
            } else if let Err(error) = state.store.set_continue_at(session, None) {
                eprintln!("Keystone could not clear a missed continue: {error}");
            }
        }
        state
    }
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }
    pub(crate) fn publish(&mut self, cx: &mut Context<Self>) {
        cx.emit(std::mem::take(&mut self.changes));
        cx.notify();
        self.quit_if_idle(cx);
    }
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.changes.sidebar = true;
        self.publish(cx);
    }

    pub(crate) fn session(&self, id: u64) -> Option<&keystone_core::Session> {
        self.store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == id)
    }

    pub(crate) fn provider(&self, session: u64) -> Option<Provider> {
        self.store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == session)
            .map(|s| s.provider())
    }

    pub(crate) fn approval(&self, session: u64) -> Option<(Provider, ApprovalState)> {
        let session = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == session)?;
        Some((
            session.provider(),
            ApprovalState {
                mode: session.approval_mode(),
                overridden: session.approval_overridden(),
            },
        ))
    }

    /// The account a catalog for `provider` is asked from in this session: the
    /// session's own for its provider, else that provider's default, which a session
    /// moving to it would start on.
    fn catalog_account(&self, session: u64, provider: Provider) -> Option<AccountId> {
        self.session(session)
            .filter(|s| s.provider() == provider)
            .and_then(|s| s.account().cloned())
    }

    pub(crate) fn catalog_key(&self, session: u64, provider: Provider) -> Option<CatalogKey> {
        match self.conversation_folder(session)? {
            presentation::ConversationFolder::Recorded(directory) => Some(CatalogKey {
                provider,
                account: self.catalog_account(session, provider),
                directory,
            }),
            _ => None,
        }
    }

    /// The commands the session's account lists in its folder: `None` while they load
    /// or before anything asked for them.
    pub(crate) fn commands(&self, session: u64, provider: Provider) -> Option<&Commands> {
        self.catalogs
            .commands(&self.catalog_key(session, provider)?)
    }

    pub(crate) fn model_selection(
        &self,
        session: u64,
        provider: Provider,
    ) -> model_selector::Selection<'_> {
        let saved = self.session(session);
        let choices = match self.conversation_folder(session) {
            Some(presentation::ConversationFolder::Recorded(directory)) => {
                self.catalogs.choices(&CatalogKey {
                    provider,
                    account: self.catalog_account(session, provider),
                    directory,
                })
            }
            Some(presentation::ConversationFolder::LegacyMissing) => {
                Choices::Unavailable("Choose a conversation folder to load choices.")
            }
            Some(presentation::ConversationFolder::Unsupported) | None => Choices::Unavailable(
                "Choices need this conversation's saved folder, which this version cannot use.",
            ),
        };
        model_selector::Selection {
            provider,
            account: saved
                .filter(|s| s.provider() == provider)
                .and_then(|s| s.account()),
            accounts: self.store.workspace().accounts(),
            model: saved.and_then(|s| s.model()),
            reported_model: self
                .conversations
                .get(&session)
                .and_then(|c| c.reported_model.as_deref()),
            started: saved.is_none_or(|s| s.started()),
            choices,
        }
    }

    pub(crate) fn request_catalog(
        &mut self,
        session: u64,
        provider: Provider,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.catalog_key(session, provider) {
            self.request_catalog_for(key, session, cx);
        }
    }

    pub(crate) fn catalog_choices(&self, key: &CatalogKey) -> Choices<'_> {
        self.catalogs.choices(key)
    }

    /// Asks `key`'s catalog once per app run, for `asker`, whose ID the answer comes
    /// back under.
    pub(crate) fn request_catalog_for(
        &mut self,
        key: CatalogKey,
        asker: u64,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running {
            return;
        }
        let (provider, account) = (key.provider, key.account.clone());
        let directory = key.directory.clone();
        if self.catalogs.ask(key, asker) {
            let refused = match self.connection(provider, account.as_ref()) {
                Ok(connection) => {
                    connection.request_catalog(asker, directory.as_str());
                    None
                }
                Err(error) => Some(error),
            };
            if let Some(error) = refused {
                let failed = Commands::Failed(error.clone());
                self.catalogs
                    .apply_commands(provider, account.clone(), asker, failed);
                self.catalogs
                    .apply(provider, account, asker, Catalog::Failed(error));
            }
            self.publish(cx);
        }
    }

    /// Asks `key`'s catalog again, as Retry does.
    pub(crate) fn retry_catalog_for(
        &mut self,
        key: CatalogKey,
        asker: u64,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running {
            return;
        }
        self.catalogs.clear(&key);
        self.request_catalog_for(key, asker, cx);
    }

    pub(crate) fn retry_catalog(
        &mut self,
        session: u64,
        provider: Provider,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.catalog_key(session, provider) {
            self.retry_catalog_for(key, session, cx);
        }
    }

    fn conversation(&mut self, session: u64) -> &mut Conversation {
        let logs = &self.logs;
        self.conversations.entry(session).or_insert_with(|| {
            logs.load(session);
            Conversation::loading()
        })
    }

    pub(crate) fn ensure_history(&mut self, session: u64) {
        if self.phase != Phase::Exiting {
            self.conversation(session);
        }
    }

    /// Logs what the session's conversation changed. Returns whether the log budget
    /// rejected any of it.
    pub(crate) fn commit(&mut self, session: u64) -> bool {
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return false;
        };
        let rejected = self
            .logs
            .append(session, conversation.take_writes())
            .is_err();
        let changed = conversation.take_changes();
        let replies = changed
            .iter()
            .cloned()
            .flatten()
            .filter_map(|index| match conversation.rows.get(index)? {
                Row::Item(item) if item.kind == ItemKind::AgentMessage => {
                    Some((index, item.text.len(), item.status))
                }
                _ => None,
            })
            .collect();
        self.changes.rows.push(RowChange {
            session,
            rows: conversation.rows.len(),
            changed,
            replies,
        });
        self.record_loss(session);
        rejected
    }

    pub(crate) fn pump(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Exiting {
            return;
        }
        let reports = self.logs.drain();
        let mut events = Vec::new();
        // A catalog belongs to the connection it came from, whatever provider or
        // account the asking session has since moved to.
        let drained: Vec<_> = self
            .codex
            .iter()
            .map(|(account, c)| (Provider::Codex, account.clone(), c.drain()))
            .chain(
                self.claude
                    .iter()
                    .map(|(account, c)| (Provider::Claude, account.clone(), c.drain())),
            )
            .collect();
        for (provider, account, batch) in drained {
            for (session, event) in batch {
                match event {
                    Event::Catalog(catalog) => {
                        self.catalogs
                            .apply(provider, account.clone(), session, catalog);
                    }
                    Event::Commands(commands) => {
                        self.catalogs
                            .apply_commands(provider, account.clone(), session, commands);
                    }
                    Event::Usage(windows) => {
                        self.record_usage(provider, account.as_ref(), windows, cx)
                    }
                    Event::McpStatus(servers) => {
                        self.record_mcp_status(provider, account.as_ref(), session, servers, cx)
                    }
                    Event::ProviderNotice { .. }
                        if self.route(session) != Some((provider, account.clone())) => {}
                    Event::ThreadsListed { ticket, result } => {
                        answer(&self.native, ticket, Native::Listed(result));
                    }
                    Event::HistoryRead { ticket, result } if self.import_reads.remove(&ticket) => {
                        answer(&self.native, ticket, Native::Read(result));
                    }
                    event => events.push((session, event)),
                }
            }
        }
        self.receive(reports, events, cx);
        self.pump_orchestrator(cx);
        let reads = std::mem::take(&mut *self.recovery_reads.lock().unwrap());
        for (session, ticket, result) in reads {
            self.finish_recovery(session, ticket, result, cx);
        }
        self.start_reads();
        self.save_results();
        self.deliver_results(cx);
    }

    pub(crate) fn receive(
        &mut self,
        reports: Vec<Report>,
        events: Vec<(u64, Event)>,
        cx: &mut Context<Self>,
    ) {
        if self.phase == Phase::Exiting {
            return;
        }
        if reports.is_empty() && events.is_empty() {
            self.publish(cx);
            return;
        }
        let mut sidebar_changed = false;
        let mut recover = Vec::new();
        let mut undelivered = Vec::new();
        let mut alerts = Vec::new();
        for report in reports {
            let session = match report {
                Report::Loaded { session, entries } => {
                    let unread = entries.is_err()
                        && self
                            .session(session)
                            .is_some_and(|saved| saved.provider_thread().is_some());
                    if let Some(conversation) = self.conversations.get_mut(&session) {
                        conversation.restore(entries);
                        if conversation.recovery == Some(Recovery::Loading) {
                            conversation.recovery = None;
                        }
                        // Without the history Keystone cannot tell whether a turn was
                        // left open, so the next message waits for the user.
                        if unread && conversation.recovery.is_none() {
                            conversation.recovery = Some(Recovery::Unavailable(UNREAD.into()));
                        }
                        if conversation.needs_recovery() || self.recover_after_load.remove(&session)
                        {
                            recover.push(session);
                        }
                    }
                    // A turn that ended during the load was given its checkpoint just now.
                    self.turn_ended(session, cx);
                    self.read_checkpoints(session, cx);
                    // Events held during the load, such as an approval or a turn's
                    // end, apply only now.
                    sidebar_changed = true;
                    self.deliveries_due = true;
                    session
                }
                Report::Checkpoint {
                    session,
                    ticket,
                    error,
                    lost,
                } => {
                    match self.idle_saves.get(&session) {
                        // A process outlives any loss in its session, as before markers
                        // were saved as they happen.
                        Some((saved, _)) if *saved == ticket => self.idle_saved(
                            session,
                            error.or_else(|| {
                                (lost > 0)
                                    .then(|| format!("Session {session}: log entries were lost"))
                            }),
                        ),
                        // Earlier loss is marked in the log and does not hold back a
                        // recovery saved after it; a rejected batch is held for Retry
                        // when it is queued.
                        _ => self.recovery_saved(session, ticket, error),
                    }
                    sidebar_changed = true;
                    session
                }
                Report::Saved {
                    session,
                    error,
                    retryable,
                } => {
                    if let Some(conversation) = self.conversations.get_mut(&session) {
                        conversation.save_error = error
                            .filter(|_| retryable)
                            .map(|error| format!("This conversation is not being saved. {error}"));
                    }
                    self.record_loss(session);
                    session
                }
            };
            self.commit(session);
        }
        for (session, event) in events {
            match event {
                Event::HistoryRead { ticket, result } => {
                    self.finish_recovery(session, ticket, result, cx);
                    continue;
                }
                Event::RecoveryNeeded => recover.push(session),
                Event::IdleCandidate(candidate) => {
                    self.offer_idle(session, candidate);
                    continue;
                }
                Event::ActionUndelivered { call, text } => {
                    self.pressing.insert(session);
                    undelivered.push((session, call, text));
                    continue;
                }
                _ => {}
            }
            if let Some(alert) = crate::notifications::Alert::of(&event) {
                alerts.push((session, alert));
            }
            // A held follow-up starts as the turn ends, so the session is not finished.
            if matches!(event, Event::TurnStarted) {
                alerts.retain(|&(s, alert)| {
                    s != session || alert != crate::notifications::Alert::TurnFinished
                });
            }
            if matches!(&event, Event::ItemCompleted(item) if matches!(item.kind, ItemKind::FileChange { .. }))
                || matches!(event, Event::TurnCompleted { .. })
            {
                self.changes.files.push(session);
            }
            let failed = turn_failed(&event);
            if failed.is_some() {
                self.deliveries_due = true;
                // A result whose save is queued is already taken.
                if matches!(
                    self.store.workspace().delivery(session).map(|d| d.state()),
                    Some(keystone_core::DeliveryState::Working)
                ) && !self.pending.iter().any(
                    |save| matches!(save, PendingSave::Delivery(child, _) if *child == session),
                ) {
                    self.finished
                        .entry(session)
                        .or_insert_with(|| delivery::Outcome::of(&event));
                }
            }
            match (&event, failed) {
                (Event::TurnStarted, _) => self.persist(PendingSave::TurnStart(session)),
                (_, Some(failed)) => self.persist(PendingSave::TurnEnd(session, failed, now_ms())),
                _ => {}
            }
            sidebar_changed |= failed.is_some()
                || matches!(
                    event,
                    Event::ApprovalRequested(_)
                        | Event::ApprovalResolved { .. }
                        | Event::SurfaceShown { .. }
                        | Event::SurfaceStopped { .. }
                );
            let thread = self.conversation(session).apply(event);
            self.commit(session);
            self.turn_ended(session, cx);
            // Before the batch's later events, so the result is this turn's reply.
            if failed.is_some() && self.finished.contains_key(&session) {
                self.save_results();
            }
            if let Some(thread) = thread {
                let known = self
                    .store
                    .workspace()
                    .sessions()
                    .iter()
                    .any(|s| s.id() == session && s.provider_thread() == Some(thread.as_str()));
                if !known {
                    self.persist(PendingSave::Thread(session, thread));
                }
            }
        }

        self.save_results();
        // After the batch: an exit's turn end is applied before its check starts.
        for session in recover {
            match self.conversations.get(&session) {
                Some(conversation) if conversation.is_loading() => {
                    self.recover_after_load.insert(session);
                }
                Some(conversation) if !conversation.running && conversation.recovery.is_none() => {
                    self.start_recovery(session, cx);
                    sidebar_changed = true;
                }
                _ => {}
            }
        }
        // A press the provider no longer waited for goes as a message, after this batch
        // so it lands after the turn's end.
        if !undelivered.is_empty() {
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |this, cx| {
                    for (session, call, text) in &undelivered {
                        this.send_widget_action(*session, call, text, cx);
                    }
                    for (session, ..) in undelivered {
                        this.pressing.remove(&session);
                    }
                    this.deliveries_due();
                });
            })
            .detach();
        }
        if !alerts.is_empty() {
            cx.defer(move |cx| crate::notifications::offer(alerts, cx));
        }
        // Streamed text redraws only this view. The sidebar refreshes when a row changes.
        if sidebar_changed {
            self.refresh(cx);
        } else {
            self.publish(cx);
        }
    }

    /// The latest usage readings of `provider`'s `account`; `None` is the default.
    pub(crate) fn usage(
        &self,
        provider: Provider,
        account: Option<&AccountId>,
    ) -> Option<&AccountUsage> {
        self.usage.get(&usage_key(provider, account))
    }

    /// Keeps what a session of the account reported about its MCP servers, in place
    /// of what it reported before.
    pub(crate) fn record_mcp_status(
        &mut self,
        provider: Provider,
        account: Option<&AccountId>,
        session: u64,
        servers: Vec<keystone_core::integrations::ServerStatus>,
        cx: &mut Context<Self>,
    ) {
        let folder = match self.conversation_folder(session) {
            Some(ConversationFolder::Recorded(directory)) => Some(directory.path().to_owned()),
            _ => None,
        };
        self.mcp_status.insert(
            session,
            SessionMcpStatus {
                provider,
                account: account.cloned(),
                folder,
                servers,
                at_ms: now_ms(),
            },
        );
        cx.notify();
    }

    /// The MCP servers the account's open sessions in `project` last reported, by name,
    /// each from the newest report that names it. A session in a subfolder may load
    /// other servers of the same names, so it is left out, and so is one whose
    /// conversation has closed, since its servers stopped with it.
    pub(crate) fn mcp_status(
        &self,
        provider: Provider,
        account: Option<&AccountId>,
        project: &std::path::Path,
    ) -> BTreeMap<String, ReportedStatus> {
        let key = account.cloned();
        let open = |session: u64| match provider {
            Provider::Codex => self.codex.get(&key).is_some_and(|c| c.is_open(session)),
            Provider::Claude => self
                .claude
                .get(&key)
                .is_some_and(|c| c.pid(session).is_some()),
        };
        let mut reported = BTreeMap::<String, ReportedStatus>::new();
        let reports = self.mcp_status.iter().filter(|(session, report)| {
            report.provider == provider
                && report.account == key
                && report.folder.as_deref() == Some(project)
                && open(**session)
        });
        for (_, report) in reports {
            for status in &report.servers {
                if reported
                    .get(&status.name)
                    .is_none_or(|known| known.at_ms <= report.at_ms)
                {
                    reported.insert(
                        status.name.clone(),
                        ReportedStatus {
                            status: status.clone(),
                            at_ms: report.at_ms,
                        },
                    );
                }
            }
        }
        reported
    }

    pub(crate) fn has_usage(&self) -> bool {
        self.usage.values().any(|usage| !usage.readings.is_empty())
    }

    /// Keeps a report of the account's windows and saves the result off the UI
    /// thread, the newest per account. A failed save leaves the reading shown for this
    /// run, and the account's next report saves every window again.
    fn record_usage(
        &mut self,
        provider: Provider,
        account: Option<&AccountId>,
        windows: Vec<LimitWindow>,
        cx: &mut Context<Self>,
    ) {
        let key = usage_key(provider, account);
        let usage = self.usage.entry(key.clone()).or_default();
        usage.record(windows, now_ms());
        let mut saves = self.usage_saves.lock().unwrap();
        saves.0.insert(key, usage.clone());
        if std::mem::replace(&mut saves.1, true) {
            return;
        }
        drop(saves);
        let (saves, database) = (self.usage_saves.clone(), self.store.database());
        cx.background_spawn(async move {
            loop {
                let batch = {
                    let mut saves = saves.lock().unwrap();
                    if saves.0.is_empty() {
                        saves.1 = false;
                        return;
                    }
                    std::mem::take(&mut saves.0)
                };
                for (key, usage) in batch {
                    if let Err(error) = database.record_usage(&key, &usage) {
                        eprintln!("Keystone could not save usage for {key}: {error}");
                    }
                }
            }
        })
        .detach();
    }

    /// The provider and account a session runs on; `None` as the account is the
    /// provider's default.
    fn route(&self, session: u64) -> Option<(Provider, Option<AccountId>)> {
        self.session(session)
            .map(|s| (s.provider(), s.account().cloned()))
    }

    /// How `provider`'s CLI is launched for `account`. An account this workspace no
    /// longer has, or one of another provider, is refused rather than run as some
    /// other account.
    pub(crate) fn profile(
        &self,
        provider: Provider,
        account: Option<&AccountId>,
    ) -> Result<LaunchProfile, String> {
        let Some(id) = account else {
            return Ok(LaunchProfile::inherited());
        };
        match self.store.workspace().account(id) {
            Some(account) if account.provider() == provider => {
                let home = real_path(account.home());
                if let Some(other) = self
                    .store
                    .workspace()
                    .accounts()
                    .iter()
                    .filter(|other| other.id() != account.id())
                    .find(|other| keystone_core::homes_overlap(&real_path(other.home()), &home))
                {
                    return Err(format!(
                        "Not sent. The account \u{201c}{}\u{201d} now resolves to {}, which overlaps the account \u{201c}{}\u{201d}. Give each account a folder of its own.",
                        account.label(),
                        home.display(),
                        other.label()
                    ));
                }
                if let Some(default) = [Provider::Claude, Provider::Codex]
                    .into_iter()
                    .filter_map(keystone_agent::default_home)
                    .find(|default| keystone_core::homes_overlap(&real_path(default), &home))
                {
                    return Err(format!(
                        "Not sent. The account \u{201c}{}\u{201d} uses {}, which overlaps a default home Keystone was started with ({}). Start Keystone without that setting, or start a new session.",
                        account.label(),
                        account.home().display(),
                        default.display()
                    ));
                }
                Ok(LaunchProfile::account(
                    provider,
                    account.label(),
                    account.home(),
                ))
            }
            _ => Err(format!(
                "Not sent. This session's {provider} account \u{201c}{id}\u{201d} is not set up. Add it again in Settings \u{2192} Accounts, or start a new session."
            )),
        }
    }

    /// The CLI Keystone runs for `provider`: `KEYSTONE_CLAUDE_PATH` or
    /// `KEYSTONE_CODEX_PATH`, else `None` for the one on `PATH`.
    pub(crate) fn provider_program(&self, provider: Provider) -> Option<PathBuf> {
        // A test never runs the CLI it would find on `PATH`.
        #[cfg(test)]
        return Some(match &self.test_programs {
            Some((codex, _)) if provider == Provider::Codex => codex.clone(),
            Some((_, claude)) if provider == Provider::Claude => claude.clone(),
            _ => PathBuf::from("/nonexistent/keystone-test-cli"),
        });
        #[cfg(not(test))]
        std::env::var_os(match provider {
            Provider::Codex => "KEYSTONE_CODEX_PATH",
            Provider::Claude => "KEYSTONE_CLAUDE_PATH",
        })
        .map(PathBuf::from)
    }

    /// Whether a session of the account, or with `any_account` of any account of the
    /// provider, has its provider process running.
    pub(crate) fn has_running_session(
        &self,
        provider: Provider,
        account: Option<&AccountId>,
        any_account: bool,
    ) -> bool {
        self.store
            .workspace()
            .sessions()
            .iter()
            .filter(|session| {
                session.provider() == provider && (any_account || session.account() == account)
            })
            .any(|session| {
                let key = session.account().cloned();
                match provider {
                    Provider::Codex => self
                        .codex
                        .get(&key)
                        .is_some_and(|c| c.is_open(session.id())),
                    Provider::Claude => self
                        .claude
                        .get(&key)
                        .is_some_and(|c| c.pid(session.id()).is_some()),
                }
            })
    }

    /// The account's connection, started on first use.
    fn connection(
        &mut self,
        provider: Provider,
        account: Option<&AccountId>,
    ) -> Result<&dyn Connection, String> {
        let profile = self.profile(provider, account)?;
        let wake = self.wake.clone();
        let key = account.cloned();
        #[allow(unused_mut)]
        let (mut codex, mut claude) = (
            std::env::var_os("KEYSTONE_CODEX_PATH").map(PathBuf::from),
            std::env::var_os("KEYSTONE_CLAUDE_PATH").map(PathBuf::from),
        );
        #[cfg(test)]
        if let Some((test_codex, test_claude)) = &self.test_programs {
            (codex, claude) = (Some(test_codex.clone()), Some(test_claude.clone()));
        }
        let connection: &dyn Connection = match provider {
            Provider::Codex => self.codex.entry(key).or_insert_with(|| {
                CodexConnection::with_profile(codex, env!("CARGO_PKG_VERSION"), profile, wake)
            }),
            Provider::Claude => {
                let idle = self.idle_policy();
                self.claude.entry(key).or_insert_with(|| {
                    let connection = ClaudeConnection::with_profile(claude, profile, wake);
                    if let Some(policy) = idle {
                        connection.enable_idle_eviction(policy);
                    }
                    connection
                })
            }
        };
        if let Some(orchestrator) = &self.orchestrator {
            connection.set_mcp(orchestrator.server.issuer());
        }
        Ok(connection)
    }

    /// Idle eviction for new Claude connections. Off: no Claude profile has a proven
    /// quiescence contract (`docs/design/providers.md`). Debug builds take a short
    /// interval from `KEYSTONE_TEST_CLAUDE_IDLE_MS`, for scripted providers: only with
    /// `KEYSTONE_CLAUDE_PATH` naming the Claude to run, never the one on `PATH`.
    fn idle_policy(&self) -> Option<IdlePolicy> {
        #[cfg(test)]
        if let Some(policy) = &self.test_idle {
            return Some(policy.clone());
        }
        #[cfg(debug_assertions)]
        if let Some(ms) = std::env::var("KEYSTONE_TEST_CLAUDE_IDLE_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            && std::env::var_os("KEYSTONE_CLAUDE_PATH").is_some()
        {
            return Some(IdlePolicy::after(std::time::Duration::from_millis(ms)));
        }
        None
    }

    /// A provider proved the session's process idle, and every event before that is
    /// applied. Once the conversation ID is saved and synced and a log checkpoint saves
    /// everything queued so far, the process may end. Anything not saved, or a session
    /// busy here, keeps it running; its next activity proves it again.
    fn offer_idle(&mut self, session: u64, candidate: IdleCandidate) {
        if self.phase != Phase::Running {
            return;
        }
        let thread_saved = self
            .session(session)
            .is_some_and(|s| s.provider_thread() == Some(candidate.thread.as_str()))
            && !self
                .pending
                .iter()
                .any(|update| matches!(update, PendingSave::Thread(id, _) if *id == session));
        let settled = self
            .conversations
            .get(&session)
            .is_some_and(|c| !c.is_loading() && !c.running && c.save_error.is_none());
        let declined = if !thread_saved {
            Some("its conversation ID is not saved yet")
        } else if !settled {
            Some("its conversation is loading, running or not being saved")
        } else if self.store.sync_pending().is_err() {
            Some("the workspace could not be saved")
        } else {
            None
        };
        if let Some(reason) = declined {
            if let Some(connection) = self.started(session) {
                connection.decline_idle(session, candidate, format!("Keystone kept it: {reason}"));
            }
            return;
        }
        self.next_ticket += 1;
        self.idle_saves
            .insert(session, (self.next_ticket, candidate));
        self.logs.checkpoint(session, self.next_ticket);
    }

    /// The log saved, or failed to save, everything before an idle candidate.
    fn idle_saved(&mut self, session: u64, error: Option<String>) {
        let Some((_, candidate)) = self.idle_saves.remove(&session) else {
            return;
        };
        if self.phase != Phase::Running {
            return;
        }
        let Some(connection) = self.started(session) else {
            return;
        };
        match error {
            None => connection.evict_idle(session, candidate),
            Some(error) => connection.decline_idle(
                session,
                candidate,
                format!("Keystone kept it: the conversation could not be saved: {error}"),
            ),
        }
    }

    /// The connection a session's account already has, if any.
    fn started(&self, session: u64) -> Option<&dyn Connection> {
        let (provider, account) = self.route(session)?;
        match provider {
            Provider::Codex => self.codex.get(&account).map(|c| c as &dyn Connection),
            Provider::Claude => self.claude.get(&account).map(|c| c as &dyn Connection),
        }
    }

    /// Reads what the session's provider saved, from the session's own account, and
    /// holds the next message until the result is shown and saved. Read only: nothing
    /// is sent, resumed or answered.
    pub(crate) fn start_recovery(&mut self, session: u64, cx: &mut Context<Self>) {
        if self.phase != Phase::Running {
            return;
        }
        let Some(saved) = self.session(session).cloned() else {
            return;
        };
        let Some(thread) = saved.provider_thread().map(str::to_owned) else {
            self.conversation(session).recovery = Some(Recovery::Unavailable(NO_THREAD.into()));
            self.commit(session);
            return;
        };
        self.next_ticket += 1;
        let ticket = self.next_ticket;
        self.recoveries.insert(session, ticket);
        self.conversation(session).recovery = Some(Recovery::Checking);
        self.commit(session);
        let account = saved.account().cloned();
        match saved.provider() {
            Provider::Claude => {
                let read = self.claude_root(account.as_ref()).and_then(|root| {
                    match self.conversation_folder(session) {
                        Some(ConversationFolder::Recorded(directory)) => Ok(ClaudeRead {
                            session,
                            ticket,
                            root,
                            thread,
                            cwd: directory.path().to_owned(),
                        }),
                        _ => Err("this conversation has no saved folder to check against".into()),
                    }
                });
                match read {
                    Ok(read) => {
                        self.queued_reads.push_back(read);
                        self.start_reads();
                    }
                    Err(error) => return self.finish_recovery(session, ticket, Err(error), cx),
                }
            }
            Provider::Codex => {
                if let Err(error) = self.connection(Provider::Codex, account.as_ref()) {
                    return self.finish_recovery(session, ticket, Err(error), cx);
                }
                if let Some(codex) = self.codex.get(&account) {
                    codex.read_history(session, thread, ticket);
                }
            }
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RECOVERY_DEADLINE).await;
            let _ = this.update(cx, |this, cx| {
                let error = "the provider did not answer in time".to_owned();
                this.finish_recovery(session, ticket, Err(error), cx);
            });
        })
        .detach();
    }

    /// Asks `provider`'s `account` for the conversations it has that ran in `cwd`, for
    /// an import. The answer is [`Native::Listed`] under the returned ticket. Read only.
    pub(crate) fn list_native(
        &mut self,
        provider: Provider,
        account: Option<&AccountId>,
        cwd: &std::path::Path,
    ) -> u64 {
        let ticket = self.ask_native();
        match provider {
            Provider::Claude => {
                let root = self.claude_root(account);
                let cwd = cwd.to_owned();
                self.answer_native(
                    ticket,
                    move || {
                        Native::Listed(
                            root.and_then(|root| {
                                keystone_agent::import::claude_sessions(&root, &cwd)
                            }),
                        )
                    },
                    |error| Native::Listed(Err(error)),
                );
            }
            Provider::Codex => match self.codex_for(account) {
                Ok(codex) => codex.list_threads(&cwd.to_string_lossy(), ticket),
                Err(error) => {
                    answer(&self.native, ticket, Native::Listed(Err(error)));
                }
            },
        }
        ticket
    }

    /// Reads the newest turns of `provider`'s conversation `thread` under `account`, for
    /// an import. The answer is [`Native::Read`] under the returned ticket. Read only:
    /// nothing is resumed or sent.
    pub(crate) fn read_native(
        &mut self,
        provider: Provider,
        account: Option<&AccountId>,
        cwd: &std::path::Path,
        thread: String,
    ) -> u64 {
        let ticket = self.ask_native();
        match provider {
            Provider::Claude => {
                let root = self.claude_root(account);
                let cwd = cwd.to_owned();
                self.answer_native(
                    ticket,
                    move || {
                        Native::Read(
                            root.and_then(|root| recovery::claude::read(&root, &thread, &cwd)),
                        )
                    },
                    |error| Native::Read(Err(error)),
                );
            }
            Provider::Codex => match self.codex_for(account) {
                Ok(codex) => {
                    codex.read_import(thread, ticket);
                    self.import_reads.insert(ticket);
                }
                Err(error) => {
                    answer(&self.native, ticket, Native::Read(Err(error)));
                }
            },
        }
        ticket
    }

    fn ask_native(&mut self) -> u64 {
        self.next_ticket += 1;
        self.native.lock().unwrap().insert(self.next_ticket, None);
        self.next_ticket
    }

    /// The answer to import request `ticket`, once it has arrived.
    pub(crate) fn take_native(&mut self, ticket: u64) -> Option<Native> {
        let mut native = self.native.lock().unwrap();
        let answer = native.get_mut(&ticket)?.take()?;
        native.remove(&ticket);
        Some(answer)
    }

    /// Stops waiting on import request `ticket`, dropping its answer now or when it
    /// arrives.
    pub(crate) fn abandon_native(&mut self, ticket: u64) {
        self.native.lock().unwrap().remove(&ticket);
    }

    /// Answers `ticket` with what `read` returns, on a thread of its own, since it
    /// reads the provider's store from disk.
    fn answer_native(
        &self,
        ticket: u64,
        read: impl FnOnce() -> Native + Send + 'static,
        fail: fn(String) -> Native,
    ) {
        let (native, wake) = (self.native.clone(), self.wake.clone());
        let spawned = std::thread::Builder::new()
            .name("import-read".into())
            .spawn(move || {
                answer(&native, ticket, read());
                wake();
            });
        if let Err(error) = spawned {
            let error = format!("could not start reading: {error}");
            answer(&self.native, ticket, fail(error));
            (self.wake)();
        }
    }

    fn codex_for(&mut self, account: Option<&AccountId>) -> Result<&CodexConnection, String> {
        self.connection(Provider::Codex, account)?;
        let account = account.cloned();
        Ok(self.codex.get(&account).expect("started above"))
    }

    /// The config dir of a Claude account: the one Keystone gave it, or for the
    /// default account the one Claude Code itself uses.
    fn claude_root(&self, account: Option<&AccountId>) -> Result<PathBuf, String> {
        self.profile(Provider::Claude, account)?;
        match account {
            Some(id) => self
                .store
                .workspace()
                .account(id)
                .map(|account| account.home().to_owned())
                .ok_or_else(|| "this session's account is not set up".into()),
            None => {
                #[cfg(test)]
                if let Some(home) = &self.test_claude_home {
                    return Ok(home.clone());
                }
                keystone_agent::default_home(Provider::Claude)
                    .ok_or_else(|| "Claude Code's config folder is unknown: HOME is not set".into())
            }
        }
    }

    /// Starts queued Claude store reads while a reader is free. Each read runs on a
    /// thread of its own, so a stuck disk holds up nothing but its slot.
    fn start_reads(&mut self) {
        use std::sync::atomic::Ordering;
        while self.active_reads.load(Ordering::SeqCst) < RECOVERY_READERS
            && let Some(read) = self.queued_reads.pop_front()
        {
            // A read abandoned at its deadline, or replaced by a retry, is not run.
            if self.recoveries.get(&read.session) != Some(&read.ticket) {
                continue;
            }
            self.active_reads.fetch_add(1, Ordering::SeqCst);
            let (reads, active, wake) = (
                self.recovery_reads.clone(),
                self.active_reads.clone(),
                self.wake.clone(),
            );
            let spawned = std::thread::Builder::new()
                .name("recovery-read".into())
                .spawn(move || {
                    let result = recovery::claude::read(&read.root, &read.thread, &read.cwd);
                    reads
                        .lock()
                        .unwrap()
                        .push((read.session, read.ticket, result));
                    active.fetch_sub(1, Ordering::SeqCst);
                    wake();
                });
            if let Err(error) = spawned {
                self.active_reads.fetch_sub(1, Ordering::SeqCst);
                self.recovery_reads.lock().unwrap().push((
                    read.session,
                    read.ticket,
                    Err(format!("could not start reading: {error}")),
                ));
                (self.wake)();
            }
        }
    }

    /// Takes a finished read, if it is still the session's current one, and reconciles
    /// it with the shown rows off the UI thread.
    fn finish_recovery(
        &mut self,
        session: u64,
        ticket: u64,
        result: Result<Snapshot, String>,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running || self.recoveries.get(&session) != Some(&ticket) {
            return;
        }
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return;
        };
        if conversation.recovery != Some(Recovery::Checking) {
            return;
        }
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(reason) => {
                self.recoveries.remove(&session);
                self.reviewing.remove(&session);
                conversation.recovery = Some(Recovery::Unavailable(reason));
                self.commit(session);
                self.refresh(cx);
                return;
            }
        };
        let (neutral, open_turn) = (conversation.neutral_items(), conversation.open_turn());
        let left_out = matches!(
            conversation.rows.first(),
            Some(crate::conversation::Row::OlderNotImported)
        );
        cx.spawn(async move |this, cx| {
            let batch = cx
                .background_executor()
                .spawn(async move {
                    let snapshot = if left_out {
                        keystone_agent::import::imported_part(snapshot, &neutral)
                    } else {
                        snapshot
                    };
                    recovery::reconcile(&neutral, open_turn, &snapshot)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_reconciled(session, ticket, batch, cx)
            });
        })
        .detach();
    }

    /// Shows and logs what was recovered, then waits for the log to confirm it saved.
    /// Operations name the exact rows they change, so rows that moved on meanwhile are
    /// left alone.
    fn apply_reconciled(
        &mut self,
        session: u64,
        ticket: u64,
        batch: Option<recovery::RecoveryBatch>,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running || self.recoveries.get(&session) != Some(&ticket) {
            return;
        }
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return;
        };
        if conversation.recovery != Some(Recovery::Checking) {
            return;
        }
        // A retry that found nothing new saves nothing and keeps the earlier report.
        let batch = batch.filter(|batch| {
            !(self.reviewing.contains_key(&session) && batch.operations.is_empty())
        });
        match batch {
            Some(batch) => {
                let report = batch.report.clone();
                conversation.apply_recovery(batch.clone());
                conversation.recovery = Some(Recovery::Saving {
                    report,
                    error: None,
                });
                self.save_recovery(session, ticket, batch);
            }
            None => {
                self.recoveries.remove(&session);
                conversation.recovery = self.reviewing.remove(&session).map(Recovery::Review);
                self.commit(session);
                self.deliveries_due = true;
            }
        }
        self.refresh(cx);
    }

    /// Commits a shown recovery batch, then asks the log to confirm it. A batch the log
    /// rejects over its budget is held for Retry instead of being confirmed.
    fn save_recovery(&mut self, session: u64, ticket: u64, batch: RecoveryBatch) {
        if !self.commit(session) {
            self.logs.checkpoint(session, ticket);
        } else if let Some(conversation) = self.conversations.get_mut(&session) {
            conversation.recovery = Some(Recovery::Unsaved(batch));
        }
    }

    /// The log confirmed, or failed to confirm, a recovery batch.
    fn recovery_saved(&mut self, session: u64, ticket: u64, error: Option<String>) {
        if self.recoveries.get(&session) != Some(&ticket) {
            return;
        }
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return;
        };
        let Some(Recovery::Saving { report, .. }) = conversation.recovery.clone() else {
            return;
        };
        conversation.recovery = match error {
            Some(error) => Some(Recovery::Saving {
                report,
                error: Some(error),
            }),
            None => {
                self.recoveries.remove(&session);
                self.deliveries_due = true;
                let earlier = self.reviewing.remove(&session);
                report
                    .needs_acknowledgement()
                    .then_some(report)
                    .or(earlier)
                    .map(Recovery::Review)
            }
        };
    }

    /// The user read a partial, conflicting or unavailable recovery and lets the next
    /// message go. It does not send anything.
    pub(crate) fn continue_after_recovery(&mut self, session: u64, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return;
        };
        if matches!(
            conversation.recovery,
            Some(Recovery::Review(_) | Recovery::Unavailable(_) | Recovery::Unsaved(_))
        ) {
            if matches!(conversation.recovery, Some(Recovery::Unsaved(_))) {
                self.recoveries.remove(&session);
            }
            conversation.recovery = None;
            self.reviewing.remove(&session);
            if conversation.is_loading() {
                self.send_unloaded.insert(session);
            }
            self.commit(session);
            self.deliveries_due();
            self.refresh(cx);
        }
    }

    /// Holds Send until the history arrives, which decides whether a check is needed.
    /// A history that has not arrived by the deadline is reported as unread.
    fn wait_for_history(&mut self, session: u64, cx: &mut Context<Self>) {
        self.conversation(session).recovery = Some(Recovery::Loading);
        self.commit(session);
        self.publish(cx);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RECOVERY_DEADLINE).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(conversation) = this.conversations.get_mut(&session)
                    && conversation.recovery == Some(Recovery::Loading)
                {
                    conversation.recovery = Some(Recovery::Unavailable(UNREAD.into()));
                    this.commit(session);
                    this.refresh(cx);
                }
            });
        })
        .detach();
    }

    /// Checks again, or retries saving a recovery that could not be saved.
    pub(crate) fn retry_recovery(&mut self, session: u64, cx: &mut Context<Self>) {
        if self.phase != Phase::Running {
            return;
        }
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return;
        };
        match conversation.recovery.clone() {
            Some(Recovery::Saving {
                report,
                error: Some(_),
            }) => {
                conversation.recovery = Some(Recovery::Saving {
                    report,
                    error: None,
                });
                if let Some(ticket) = self.recoveries.get(&session) {
                    self.logs.checkpoint(session, *ticket);
                }
                self.commit(session);
                self.refresh(cx);
            }
            Some(Recovery::Unsaved(batch)) => {
                conversation.recovery = Some(Recovery::Saving {
                    report: batch.report.clone(),
                    error: None,
                });
                conversation.log_recovery(batch.clone());
                if let Some(ticket) = self.recoveries.get(&session).copied() {
                    self.save_recovery(session, ticket, batch);
                }
                self.refresh(cx);
            }
            // Rows still loading are not Keystone's history: the check waits for them.
            Some(Recovery::Unavailable(_)) if conversation.is_loading() => {
                self.wait_for_history(session, cx);
            }
            Some(Recovery::Unavailable(_)) => {
                self.start_recovery(session, cx);
                self.refresh(cx);
            }
            Some(Recovery::Review(report)) => {
                // The turn the report is about is checked again as it was, and the
                // report keeps waiting for its acknowledgement.
                conversation.recheck(report.turn != TurnEvidence::Unaffected);
                self.reviewing.insert(session, report);
                self.start_recovery(session, cx);
                self.refresh(cx);
            }
            _ => {}
        }
    }

    pub(crate) fn flush_logs(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let deadline = std::time::Instant::now() + LOAD_WAIT;
        loop {
            let all = self.logs.wait_for_loads(deadline);
            self.pump(cx);
            // Applying events can start loading another session; finish that too.
            let loading: Vec<u64> = self
                .conversations
                .iter()
                .filter(|(_, c)| c.is_loading())
                .map(|(id, _)| *id)
                .collect();
            if loading.is_empty() {
                break;
            }
            if !all || std::time::Instant::now() >= deadline {
                for session in loading {
                    self.conversations
                        .get_mut(&session)
                        .expect("listed above")
                        .restore(Err("it did not load during the shutdown attempt; restart to retry history loading".into()));
                    self.commit(session);
                }
                break;
            }
        }
        let edited: Vec<u64> = self.widget_saves.drain().collect();
        for session in edited {
            self.save_widgets(session);
        }
        let result = self.logs.flush_pending();
        self.receive(self.logs.drain(), Vec::new(), cx);
        result.map_err(|error| format!("Could not save conversation logs before closing. {error}"))
    }

    pub(crate) fn send_message(&mut self, draft: ComposerDraft, cx: &mut Context<Self>) -> bool {
        self.send_message_from(draft, None, SendKind::Draft, cx)
    }

    /// Sends [`CONTINUE_TEXT`] to a session whose last turn hit its usage limit, at the
    /// user's request. The composer's draft is left as it is.
    pub(crate) fn continue_after_limit(&mut self, session: u64, cx: &mut Context<Self>) -> bool {
        // Only while the conversation still ends at the limit, after applying whatever
        // arrived: never into a turn the provider started meanwhile, and never twice.
        self.pump(cx);
        if self.conversation(session).usage_limit().is_none() {
            return false;
        }
        let draft = ComposerDraft {
            session,
            text: CONTINUE_TEXT.into(),
            attachments: Vec::new(),
        };
        if self.send_message_from(draft, None, SendKind::Widget, cx) {
            return true;
        }
        let conversation = self.conversation(session);
        if conversation.error.is_none() {
            conversation.error = Some("Continue was not sent.".into());
        }
        self.commit(session);
        self.publish(cx);
        false
    }

    /// Saves the user's request to continue `session` at `at` (unix milliseconds), and
    /// waits for it. `None` cancels it.
    pub(crate) fn schedule_continue(
        &mut self,
        session: u64,
        at: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running
            || at.is_some() && self.conversation(session).usage_limit().is_none()
        {
            return;
        }
        if let Err(error) = self.store.set_continue_at(session, at) {
            self.conversation(session).error = Some(match at {
                Some(_) => format!("Continue was not scheduled. {error}"),
                None => format!("Continue was not cancelled. {error}"),
            });
            self.commit(session);
        } else if let Some(at) = at {
            Self::wait_to_continue(session, at, at.saturating_sub(now_ms()), cx);
        }
        self.refresh(cx);
    }

    /// Wakes after `delay_ms` to send the continue scheduled for `at`, if it still is.
    /// Long waits wake every minute to read the wall clock again, since the timer's
    /// clock stops while the computer sleeps.
    fn wait_to_continue(session: u64, at: u64, delay_ms: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    delay_ms.min(crate::limit::CLOCK_CHECK_MS),
                ))
                .await;
            let _ = this.update(cx, |this, cx| this.continue_when_due(session, at, cx));
        })
        .detach();
    }

    /// The scheduled continue for `at` is due. It is cancelled before it is sent, so
    /// it goes at most once. Gone when the user cancelled or moved it, or a turn
    /// started; dropped, saying so, when the conversation no longer ends at the limit.
    /// Found more than `CONTINUE_MISSED_MS` late, it is only cancelled, and the card
    /// offers Continue, as for one that came due while Keystone was closed.
    fn continue_when_due(&mut self, session: u64, at: u64, cx: &mut Context<Self>) {
        if self.phase == Phase::Exiting
            || self.session(session).and_then(|s| s.continue_at()) != Some(at)
        {
            return;
        }
        let now = now_ms();
        if now < at {
            return Self::wait_to_continue(session, at, at - now, cx);
        }
        // A quit in progress may still fail, and the history decides whether the
        // conversation still ends at the limit.
        if self.phase != Phase::Running || self.conversation(session).is_loading() {
            return Self::wait_to_continue(session, at, CONTINUE_POLL_MS, cx);
        }
        if let Err(error) = self.store.set_continue_at(session, None) {
            self.conversation(session).error = Some(format!(
                "The scheduled continue will retry shortly. {error}"
            ));
            self.commit(session);
            self.refresh(cx);
            return Self::wait_to_continue(session, at, CONTINUE_RETRY_MS, cx);
        }
        self.refresh(cx);
        if now > at + crate::limit::CONTINUE_MISSED_MS {
            return;
        }
        self.pump(cx);
        if self.conversation(session).usage_limit().is_none() {
            let conversation = self.conversation(session);
            if !conversation.running {
                conversation.error = Some(
                    "The scheduled continue was not sent: this conversation no longer ends at its usage limit."
                        .into(),
                );
                self.commit(session);
                self.publish(cx);
            }
            return;
        }
        self.continue_after_limit(session, cx);
    }

    /// Writes an input's value into a widget. The widget is logged a moment later, so
    /// typing logs it once.
    pub(crate) fn edit_widget(
        &mut self,
        session: u64,
        call: &str,
        pointer: &str,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running {
            return;
        }
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return;
        };
        match conversation.edit_surface(call, pointer, value) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                conversation.error = Some(format!("Not kept: {error}."));
                self.commit(session);
                self.publish(cx);
                return;
            }
        }
        self.commit(session);
        if self.widget_saves.insert(session) {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(WIDGET_SAVE_DELAY).await;
                let _ = this.update(cx, |this, cx| {
                    this.widget_saves.remove(&session);
                    this.save_widgets(session);
                    this.publish(cx);
                });
            })
            .detach();
        }
        self.publish(cx);
    }

    fn save_widgets(&mut self, session: u64) {
        if let Some(conversation) = self.conversations.get_mut(&session) {
            conversation.save_surfaces();
            self.commit(session);
        }
    }

    /// Presses a widget's Button. The action goes back as the tool's result while the
    /// provider's call waits for it, and as a message otherwise.
    pub(crate) fn press_widget(
        &mut self,
        session: u64,
        call: &str,
        component: &str,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running {
            return;
        }
        let Some(press) = self
            .conversations
            .get_mut(&session)
            .and_then(|conversation| conversation.press(call, component, now_ms()))
        else {
            return;
        };
        self.commit(session);
        match self.started(session).filter(|_| press.waiting) {
            Some(connection) => connection.command(Command::Act {
                session,
                call: press.call,
                text: press.action,
            }),
            None => self.send_widget_action(session, &press.call, &press.action, cx),
        }
        self.refresh(cx);
    }

    /// Sends a widget's action as a message: a steer while a turn runs, otherwise a new
    /// turn. The composer's draft is left as it is.
    fn send_widget_action(
        &mut self,
        session: u64,
        call: &str,
        action: &str,
        cx: &mut Context<Self>,
    ) {
        let draft = ComposerDraft {
            session,
            text: keystone_agent::surface::message_for(call, action),
            attachments: Vec::new(),
        };
        if self.send_message_from(draft, None, SendKind::Widget, cx) {
            return;
        }
        let conversation = self.conversation(session);
        if conversation.error.is_none() {
            conversation.error = Some("The widget's answer was not sent.".into());
        }
        self.commit(session);
        self.publish(cx);
    }

    fn send_message_from(
        &mut self,
        draft: ComposerDraft,
        caller: Option<keystone_agent::mcp::Caller>,
        kind: SendKind,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.phase != Phase::Running {
            return false;
        }
        let id = draft.session;
        let Some(session) = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == id)
            .cloned()
        else {
            return false;
        };
        let text = draft.text.trim().to_owned();
        if text.is_empty() && draft.attachments.is_empty() {
            return false;
        }
        // An image whose copy is gone or cannot be opened keeps the draft, with a reason,
        // rather than going without it.
        let mut images = Vec::new();
        for attachment in &draft.attachments {
            let path = attachment.path(self.attachments.dir());
            let refused = match std::fs::File::open(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(format!(
                    "Not sent: {} is no longer in Keystone's data folder. Remove it and attach it again.",
                    attachment.name
                )),
                Err(error) => Some(format!(
                    "Not sent: {} could not be opened in Keystone's data folder: {error}. Remove it and attach it again.",
                    attachment.name
                )),
                Ok(_) => None,
            };
            if let Some(refused) = refused {
                self.conversation(id).error = Some(refused);
                self.commit(id);
                self.publish(cx);
                return false;
            }
            images.push(keystone_agent::Image {
                path,
                media_type: attachment.media_type.clone(),
                base64: None,
            });
        }
        // Apply anything that already arrived, such as the end of the last turn, so
        // the message is shown in the right turn.
        self.pump(cx);
        // Recovery holds the message, and the draft stays, until its result is shown,
        // saved and, when partial, acknowledged. A started conversation also waits for
        // its history, which decides whether recovery is needed.
        if let Some(conversation) = self.conversations.get_mut(&id) {
            if conversation.recovery.is_some() {
                return false;
            }
            if conversation.is_loading()
                && session.provider_thread().is_some()
                && !self.send_unloaded.contains(&id)
            {
                self.wait_for_history(id, cx);
                return false;
            }
        }
        // A result waits for a turn that started meanwhile rather than steering it.
        if kind == SendKind::Result
            && (self.conversations.get(&id).is_some_and(|c| c.running)
                || self.started(id).is_some_and(|c| c.is_running(id)))
        {
            return false;
        }
        // A result also waits for a restore in its repository to end.
        if self.restoring(id) {
            if kind != SendKind::Result {
                self.conversation(id).error = Some(checkpoints::RESTORING.into());
                self.commit(id);
                self.publish(cx);
            }
            return false;
        }
        let Some(directory) = self.dispatch_directory(id, cx) else {
            return false;
        };
        // A session whose account is gone keeps its draft and stays unstarted.
        if let Err(error) = self.profile(session.provider(), session.account()) {
            self.conversation(id).error = Some(error);
            self.commit(id);
            self.publish(cx);
            return false;
        }
        let cwd = directory.as_str();
        // Read after pumping: the pump may have just saved a newer thread ID.
        let resume = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == id)
            .and_then(|s| s.provider_thread().map(str::to_owned));
        // A mode other than the default can leave an override with the provider, so that
        // is saved before the message can carry one. Sessions saved by an earlier build
        // may have a mode without the record.
        let mode = session.approval_mode();
        if mode != ApprovalMode::Provider
            && !session.approval_overridden()
            && let Err(error) = self.store.set_approval_overridden(id)
        {
            self.conversation(id).error = Some(format!(
                "Not sent: the approval mode could not be saved first. {error}"
            ));
            self.commit(id);
            self.publish(cx);
            return false;
        }
        // The provider and model are fixed from the first message on. Saved before the
        // message goes, so a crash after this leaves them fixed rather than free.
        if !session.started() {
            if let Err(error) = self.store.lock_model(id) {
                self.conversation(id).error = Some(format!(
                    "Not sent: the model choice could not be saved first. {error}"
                ));
                self.commit(id);
                self.publish(cx);
                return false;
            }
        }
        let Some((provider, approval)) = self.approval(id) else {
            return false;
        };
        if let Err(error) = self.ensure_orchestrator() {
            self.conversation(id).error = Some(format!("Not sent: {error}"));
            self.commit(id);
            self.publish(cx);
            return false;
        }
        let account = session.account().cloned();
        // A failure to start the provider arrives later as an error in this
        // conversation. A refusal comes now, and the text stays in the composer.
        if let Err(error) = self.store.sync_pending() {
            self.conversation(id).error = Some(format!(
                "Not sent. Could not finish saving changes. {error}"
            ));
            self.commit(id);
            self.refresh(cx);
            return false;
        }
        // A message the user sends replaces any continue they scheduled. Cancelled first,
        // so a restart can never send that continue as well; a send that then fails leaves
        // it cancelled and the card offers it again.
        if session.continue_at().is_some()
            && let Err(error) = self.store.set_continue_at(id, None)
        {
            self.conversation(id).error = Some(format!(
                "Not sent: the scheduled continue could not be cancelled first. {error}"
            ));
            self.commit(id);
            self.refresh(cx);
            return false;
        }
        // A message that starts a turn first reads the files the turn starts from.
        let steer = self.conversations.get(&id).is_some_and(|c| c.running)
            || self.started(id).is_some_and(|c| c.is_running(id));
        self.begin_checkpoint(id, cwd, steer);
        let sent = match self.connection(provider, account.as_ref()) {
            Ok(connection) => {
                // A native fork's first message opens the provider's fork of its source.
                if resume.is_none()
                    && let Some(point) = session.fork_point()
                {
                    connection.set_fork(id, point.clone());
                }
                match caller {
                    Some(caller) => connection.send_from(
                        id,
                        cwd,
                        resume.as_deref(),
                        session.model(),
                        approval,
                        text.clone(),
                        caller,
                    ),
                    None => connection.send(
                        id,
                        cwd,
                        resume.as_deref(),
                        session.model(),
                        approval,
                        text.clone(),
                        images,
                    ),
                }
                .map_err(str::to_owned)
            }
            Err(error) => Err(error),
        };
        if let Err(reason) = sent {
            // Shown in the session's transcript, where a draft save cannot clear it.
            self.conversation(id).error = Some(reason);
            self.commit(id);
            self.publish(cx);
            return false;
        }
        for attachment in &draft.attachments {
            self.attachments.touch(attachment);
        }
        self.conversation(id)
            .push_sent(&text, draft.attachments.clone());
        self.commit(id);
        self.persist(PendingSave::Message(id, text.clone(), now_ms()));
        if kind == SendKind::Draft {
            self.persist(PendingSave::Draft(id, String::new()));
            if !draft.attachments.is_empty() {
                self.persist(PendingSave::DraftAttachments(id, Vec::new()));
            }
        }
        self.refresh(cx);
        true
    }

    pub(crate) fn stop_turn(&mut self, id: u64, cx: &mut Context<Self>) {
        let _ = self.stop_turn_from(id, None, cx);
    }

    fn stop_turn_from(
        &mut self,
        id: u64,
        caller: Option<keystone_agent::mcp::Caller>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.phase != Phase::Running {
            return Err("Keystone is shutting down".into());
        }
        if let Some(connection) = self.started(id) {
            match caller {
                Some(caller) => connection
                    .interrupt_from(id, caller)
                    .map_err(str::to_owned)?,
                None => connection.command(Command::Interrupt { session: id }),
            }
        }
        self.publish(cx);
        Ok(())
    }

    pub(crate) fn answer(
        &mut self,
        id: u64,
        request: RequestId,
        allow: bool,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running {
            return;
        }
        // The buttons take focus from the composer; give it back for the next message.
        let Some(approval) = self.conversation(id).decide(&request, allow) else {
            return;
        };
        self.commit(id);
        if let Some(connection) = self.started(id) {
            connection.command(Command::Answer {
                session: id,
                request: approval.request,
                allow,
            });
        }
        self.refresh(cx);
    }

    pub(crate) fn conversation_folder(&self, session: u64) -> Option<ConversationFolder> {
        self.store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == session)
            .map(|s| match s.context() {
                SessionContext::Recorded { directory, .. } => {
                    ConversationFolder::Recorded(directory.clone())
                }
                SessionContext::LegacyMissing => ConversationFolder::LegacyMissing,
                SessionContext::Unsupported => ConversationFolder::Unsupported,
            })
    }

    /// Marks `worktree` as being removed until the returned hold drops, so no session
    /// starts work there meanwhile.
    /// `None` while another removal holds it.
    pub(crate) fn hold_worktree(
        &self,
        worktree: &keystone_core::WorktreeContext,
    ) -> Option<WorktreeHold> {
        let key = worktree.directory.as_str().to_owned();
        if !self.removing_worktrees.lock().unwrap().insert(key.clone()) {
            return None;
        }
        Some(WorktreeHold {
            removing: self.removing_worktrees.clone(),
            key,
        })
    }

    pub(crate) fn dispatch_directory(
        &mut self,
        session: u64,
        cx: &mut Context<Self>,
    ) -> Option<WorkingDirectory> {
        if let Some(ConversationFolder::Recorded(directory)) = self.conversation_folder(session)
            && self
                .removing_worktrees
                .lock()
                .unwrap()
                .iter()
                .any(|removing| directory.path().starts_with(removing))
        {
            self.folder_error(
                session,
                "Not sent. This folder's worktree is being removed.".into(),
                cx,
            );
            return None;
        }
        if let Some(worktree) = self.session(session).and_then(|s| s.worktree()) {
            let error = if !keystone_core::worktree::is_intact(worktree) {
                Some(format!(
                    "Not sent. This session's worktree folder is missing or is no longer its worktree: {}. Its branch {} may still exist.",
                    worktree.directory.as_str(),
                    worktree.branch
                ))
            } else {
                None
            };
            if let Some(error) = error {
                self.folder_error(session, error, cx);
                return None;
            }
        }
        let result = match self.conversation_folder(session)? {
            ConversationFolder::Recorded(directory) => match std::fs::metadata(directory.path()) {
                Ok(metadata)
                    if metadata.is_dir()
                        && std::ffi::CString::new(directory.as_str()).is_ok_and(
                            |path| unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 },
                        ) =>
                {
                    Ok(directory)
                }
                _ => Err("Not sent. This conversation's saved folder is unavailable."),
            },
            ConversationFolder::LegacyMissing => Err(
                "Not sent. Choose a conversation folder using the folder button beside the model.",
            ),
            ConversationFolder::Unsupported => Err(
                "Not sent. This conversation has saved folder data that this version cannot use.",
            ),
        };
        match result {
            Ok(directory) => Some(directory),
            Err(error) => {
                self.folder_error(session, error.into(), cx);
                None
            }
        }
    }

    pub(crate) fn folder_error(&mut self, session: u64, error: String, cx: &mut Context<Self>) {
        if self.phase == Phase::Exiting {
            return;
        }
        self.conversation(session).error = Some(error);
        self.commit(session);
        self.refresh(cx);
    }
}

/// `path` with every symlink in its existing part resolved, and `..` in the rest
/// resolved lexically, so two spellings of one folder compare equal before it exists.
fn real_path(path: &std::path::Path) -> PathBuf {
    let Some((ancestor, real)) = path
        .ancestors()
        .find_map(|ancestor| Some((ancestor, std::fs::canonicalize(ancestor).ok()?)))
    else {
        return path.to_owned();
    };
    let joined = real.join(
        path.strip_prefix(ancestor)
            .unwrap_or(std::path::Path::new("")),
    );
    keystone_core::lexical(&joined)
}

/// Where an account's usage is saved: the provider and the account ID, or `default`.
fn usage_key(provider: Provider, account: Option<&AccountId>) -> String {
    let provider = match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    format!(
        "{provider}/{}",
        account.map_or(AccountId::DEFAULT, AccountId::as_str)
    )
}

/// See [`AppState::hold_worktree`].
pub(crate) struct WorktreeHold {
    removing: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    key: String,
}

impl Drop for WorktreeHold {
    fn drop(&mut self) {
        self.removing.lock().unwrap().remove(&self.key);
    }
}
