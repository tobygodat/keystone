use crate::conversation::Recovery;
use crate::presentation::ConversationFolder;
use crate::*;
use gpui::EventEmitter;
use keystone_agent::idle::{IdleCandidate, IdlePolicy};
use keystone_agent::recovery::{self, RecoveryBatch, Snapshot, TurnEvidence};
use keystone_agent::{Catalog, Commands, LaunchProfile};
use keystone_core::SessionContext;
use keystone_core::{
    AccountId, AccountUsage, Applied, Composition, LimitWindow, Placement, ProjectId,
    SavedComposition, SessionTabs, ViewId, WindowId, WindowView, WorkingDirectory,
};
use std::collections::BTreeMap;

#[cfg(all(test, feature = "ui-tests"))]
#[path = "app_state_tests.rs"]
mod tests;

#[path = "orchestrator.rs"]
mod orchestrator;

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
}

#[derive(Default)]
pub(crate) struct RuntimeChanged {
    pub rows: Vec<RowChange>,
    pub sidebar: bool,
}
pub(crate) struct RowChange {
    pub session: u64,
    pub rows: usize,
    pub changed: Vec<Range<usize>>,
    pub replies: Vec<(usize, usize, keystone_agent::ItemStatus)>,
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
    pub(crate) notifier: crate::notifications::Notifier,
}
impl EventEmitter<RuntimeChanged> for AppState {}

/// Most Claude store reads at once. A read stuck on a slow disk keeps its slot.
const RECOVERY_READERS: usize = 2;

/// How long a widget edit waits before it is logged, so typing logs the widget once
/// rather than at every key.
const WIDGET_SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(750);
/// How long a recovery check may take before the app stops waiting for it.
pub(crate) const RECOVERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

const UNREAD: &str = "Keystone could not read this conversation's saved history, so it cannot tell whether a turn was left open";

const NO_THREAD: &str = "Keystone never saved this conversation's provider ID, so it cannot check what the provider recorded. Sending starts a new provider conversation";

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

    fn record_loss(&mut self, session: u64, lost: u64) {
        if lost > 0 {
            let lost = lost.max(self.logs.lost_entries(session));
            let entries = if lost == 1 {
                "1 log entry was".to_owned()
            } else {
                format!("{lost} log entries were")
            };
            self.log_losses.insert(session, format!("Session {session}: {entries} lost (log budget exceeded). Later entries are still saved, and after a restart the conversation marks the gap."));
        }
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
    #[cfg(test)]
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

    /// Saves every window's draft, placement and project in one write, then everything
    /// still waiting to be saved. The one save path for Quit, however it was asked for.
    pub(crate) fn save(
        &mut self,
        drafts: Vec<ComposerDraft>,
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
            };
            if let Err(error) = result {
                self.error = Some(format!("{message}. {error}"));
                return;
            }
            self.pending.pop_front();
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

    /// Creates a session an agent starts in `worktree`, selected in no window.
    pub(crate) fn create_worktree_unselected(
        &mut self,
        provider: Provider,
        worktree: &keystone_core::WorktreeContext,
        now: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self
            .store
            .create_worktree_unselected(provider, worktree, now);
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Creates a session an agent starts, selected in no window.
    pub(crate) fn create_unselected(
        &mut self,
        provider: Provider,
        directory: &WorkingDirectory,
        now: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self.store.create_unselected(provider, directory, now);
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
        let logs = Logs::with_database(store.database(), wake.clone());
        // A small log budget for scripted native runs; release builds keep the default.
        #[cfg(debug_assertions)]
        let logs = match std::env::var("KEYSTONE_TEST_LOG_BUDGET_BYTES")
            .ok()
            .and_then(|bytes| bytes.parse().ok())
        {
            Some(bytes) => logs.with_budget(bytes),
            None => logs,
        };
        Self {
            removing_worktrees: Arc::default(),
            error: None,
            shutdown_error: None,
            codex: BTreeMap::new(),
            claude: BTreeMap::new(),
            conversations: HashMap::new(),
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
            #[cfg(test)]
            test_idle: None,
            reviewing: HashMap::new(),
            #[cfg(test)]
            test_claude_home: None,
            #[cfg(test)]
            save_calls: 0,
            #[cfg(test)]
            test_programs: None,
            notifier: Default::default(),
            store,
        }
    }
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }
    pub(crate) fn publish(&mut self, cx: &mut Context<Self>) {
        cx.emit(std::mem::take(&mut self.changes));
        cx.notify();
        self.quit_if_idle(cx);
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
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
        if self.phase != Phase::Running {
            return;
        }
        let Some(key) = self.catalog_key(session, provider) else {
            return;
        };
        let directory = key.directory.clone();
        let account = key.account.clone();
        if self.catalogs.ask(key, session) {
            let refused = match self.connection(provider, account.as_ref()) {
                Ok(connection) => {
                    connection.request_catalog(session, directory.as_str());
                    None
                }
                Err(error) => Some(error),
            };
            if let Some(error) = refused {
                let failed = Commands::Failed(error.clone());
                self.catalogs
                    .apply_commands(provider, account.clone(), session, failed);
                self.catalogs
                    .apply(provider, account, session, Catalog::Failed(error));
            }
            self.publish(cx);
        }
    }

    pub(crate) fn retry_catalog(
        &mut self,
        session: u64,
        provider: Provider,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running {
            return;
        }
        if let Some(key) = self.catalog_key(session, provider) {
            self.catalogs.clear(&key);
        }
        self.request_catalog(session, provider, cx);
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
        let lost = if rejected {
            self.logs.lost_entries(session)
        } else {
            0
        };
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
        self.record_loss(session, lost);
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
                    // Events held during the load, such as an approval or a turn's
                    // end, apply only now.
                    sidebar_changed = true;
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
                    lost,
                } => {
                    if let Some(conversation) = self.conversations.get_mut(&session) {
                        conversation.save_error = error
                            .filter(|_| retryable)
                            .map(|error| format!("This conversation is not being saved. {error}"));
                    }
                    self.record_loss(session, lost);
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
            let failed = turn_failed(&event);
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
                );
            let thread = self.conversation(session).apply(event);
            self.commit(session);
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
                    for (session, call, text) in undelivered {
                        this.send_widget_action(session, &call, &text, cx);
                    }
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
    fn profile(
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
                        "The account \u{201c}{}\u{201d} now resolves to {}, which overlaps the account \u{201c}{}\u{201d}, so nothing was sent. Give each account a folder of its own.",
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
                        "The account \u{201c}{}\u{201d} uses {}, which overlaps a default home Keystone was started with ({}), so nothing was sent. Start Keystone without that setting, or start a new session.",
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
                "This session's {provider} account \u{201c}{id}\u{201d} is not set up in Keystone, so nothing was sent. Add it again in Settings \u{2192} Accounts, or start a new session."
            )),
        }
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
        cx.spawn(async move |this, cx| {
            let batch = cx
                .background_executor()
                .spawn(async move { recovery::reconcile(&neutral, open_turn, &snapshot) })
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
        self.send_message_from(draft, None, true, cx)
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
        };
        if self.send_message_from(draft, None, false, cx) {
            return;
        }
        let conversation = self.conversation(session);
        if conversation.error.is_none() {
            conversation.error =
                Some("The widget's answer was not sent. Press its button again to retry.".into());
        }
        self.commit(session);
        self.publish(cx);
    }

    fn send_message_from(
        &mut self,
        draft: ComposerDraft,
        caller: Option<keystone_agent::mcp::Caller>,
        clear_draft: bool,
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
        if text.is_empty() {
            return false;
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
                "Not sent. Could not finish saving changes. Your draft is kept. Send will retry saving. {error}"
            ));
            self.commit(id);
            self.refresh(cx);
            return false;
        }
        let sent = match self.connection(provider, account.as_ref()) {
            Ok(connection) => match caller {
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
                ),
            }
            .map_err(str::to_owned),
            Err(error) => Err(error),
        };
        if let Err(reason) = sent {
            // Shown in the session's transcript, where a draft save cannot clear it.
            self.conversation(id).error = Some(reason);
            self.commit(id);
            self.publish(cx);
            return false;
        }
        self.conversation(id).push_local(&text);
        self.commit(id);
        self.persist(PendingSave::Message(id, text.clone(), now_ms()));
        if clear_draft {
            self.persist(PendingSave::Draft(id, String::new()));
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
                "Not sent. This folder's worktree is being removed. Your draft is kept.".into(),
                cx,
            );
            return None;
        }
        if let Some(worktree) = self.session(session).and_then(|s| s.worktree()) {
            let error = if !keystone_core::worktree::is_intact(worktree) {
                Some(format!(
                    "Not sent. This session's worktree folder is missing or is no longer its worktree: {}. Its branch {} may still exist. Your draft is kept.",
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
                _ => Err(
                    "Not sent. This conversation's saved folder is unavailable. Your draft is kept.",
                ),
            },
            ConversationFolder::LegacyMissing => Err(
                "Not sent. Choose a conversation folder using the folder button beside the model. Your draft is kept.",
            ),
            ConversationFolder::Unsupported => Err(
                "Not sent. This conversation has saved folder data that this version cannot use. Your draft is kept.",
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
