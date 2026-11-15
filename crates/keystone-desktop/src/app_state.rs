use crate::conversation::Recovery;
use crate::presentation::ConversationFolder;
use crate::*;
use gpui::EventEmitter;
use keystone_agent::idle::{IdleCandidate, IdlePolicy};
use keystone_agent::recovery::{self, RecoveryBatch, Snapshot, TurnEvidence};
use keystone_agent::remote::{self, Link, RemoteConnection, Status as LinkStatus};
use keystone_agent::{Catalog, Commands, LaunchProfile};
use keystone_core::SessionContext;
use keystone_core::{
    AccountId, AccountUsage, Applied, Composition, LimitWindow, ModelChoice, Placement,
    ProjectColor, ProjectId, SaveError, SavedComposition, SessionTabs, ViewId, WindowId,
    WindowView, WorkingDirectory,
};
use keystone_core::{MachineId, RemoteFolder};
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[cfg(all(test, feature = "ui-tests"))]
#[path = "app_state_tests.rs"]
mod tests;

#[cfg(all(test, feature = "ui-tests"))]
#[path = "remote_tests.rs"]
mod remote_tests;

#[path = "orchestrator.rs"]
pub(crate) mod orchestrator;

#[path = "delivery.rs"]
pub(crate) mod delivery;

#[path = "checkpoints.rs"]
pub(crate) mod checkpoints;

#[path = "scheduled.rs"]
pub(crate) mod scheduled;

#[path = "setup.rs"]
pub(crate) mod setup;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    Running,
    Saving,
    Exiting,
}

/// What a message is, which decides what sending it may do.
#[derive(Clone, Copy, PartialEq)]
enum SendKind {
    /// The composer's draft, cleared once it is sent.
    Draft,
    /// A widget's action, the continue after a usage limit, or review comments from a
    /// diff. The draft stays.
    Widget,
    /// A launched session's result. Only ever a new turn, never a steer.
    Result,
    /// A scheduled task's prompt. Only ever a new turn, and the draft stays.
    Scheduled,
    /// A message its machine refused, sent again under its own ID
    /// ([`AppState::send_again`]). The draft stays.
    Again,
}

/// What became of a message given to `send_message_from`.
#[derive(Clone, Copy, PartialEq)]
enum Sent {
    Yes,
    No,
    /// An automatic message waits for changes made before it to be saved.
    Later,
}

/// The connection a session runs on: its machine, `None` for this one, then its
/// provider and account, `None` for the provider's default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    pub machine: Option<MachineId>,
    pub provider: Provider,
    pub account: Option<AccountId>,
}

/// An event drained from a connection, with its place in its machine's sequence when
/// it came from another machine's host. `event` is `None` for one the pump used up
/// itself, which still takes its place in order.
struct Placed {
    place: Option<remote::Place>,
    session: u64,
    event: Option<Event>,
}

/// A connection on another machine: the machine, provider and account it serves.
type RemoteKey = (MachineId, Provider, Option<AccountId>);

/// What a refused login's message says to do next.
const WAY_OUT: &str = "Pick another login in the model menu, then press Send again, or give this login its own folder in Settings \u{2192} Accounts.";

const REMOTE_LOGIN: &str =
    "Not sent. This session's account isn't a login on its machine any more. Start a new session.";

/// Where a message goes: a folder on this machine, checked here, or one on another
/// machine, which only its host checks.
enum Place {
    Local(WorkingDirectory),
    Remote(MachineId, RemoteFolder),
}

/// A machine being linked from Settings → Machines. It is saved only once its host
/// answers.
pub(crate) struct Linking {
    pub(crate) destination: String,
    /// The workspace's client ID, new when it has none yet.
    client: String,
    link: Option<(Link, Vec<(Route, RemoteConnection)>)>,
    /// Why linking failed, which the form shows until the next try.
    pub(crate) failed: Option<String>,
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
    /// Sessions whose agent opened the browser page, so windows showing them open the
    /// Browser surface.
    pub browser: Vec<u64>,
    /// Sessions whose saved draft the app put back, so a window showing one with an
    /// empty composer shows it.
    pub drafts: Vec<u64>,
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
    /// One link per linked machine, made on the first pump, so what happened there
    /// while Keystone was closed arrives without waiting for a message.
    links: BTreeMap<MachineId, Link>,
    /// One connection per provider and account on each linked machine.
    remote: HashMap<RemoteKey, RemoteConnection>,
    /// Each link's state and host reports as the app last applied them, so a change
    /// redraws the sidebar and a new report reaches the machine's conversations.
    shown_links: BTreeMap<MachineId, (LinkStatus, u64)>,
    /// The machine Settings → Machines is linking, until it is saved or given up.
    pub(crate) linking: Option<Linking>,
    /// Set by a quit the user didn't choose, such as the one after the last window
    /// closes. It leaves every remote session running, whatever its machine's setting.
    idle_quit: bool,
    conversations: HashMap<u64, Conversation>,
    /// For each session on another machine, the place in its machine's sequence of the
    /// last event this run handed to it, and the sessions whose log hasn't been given
    /// that place since. A commit saves the place with the rows its events added.
    handed: HashMap<u64, remote::Place>,
    through_due: BTreeSet<u64>,
    /// Each remote session's saved cursor as the log held it at launch, which each
    /// machine's link starts from.
    launch_cursors: HashMap<u64, remote::Place>,
    /// How many times each machine's link found its host's journal replaced, as the
    /// app last saw, and the sessions on such a machine whose turn may have been
    /// running in the journal that is gone ([`AppState::end_lost_turns`]).
    shown_journals: BTreeMap<MachineId, u64>,
    lost_turns: BTreeSet<u64>,
    /// The messages to other machines this run sent or asked about, by the ID each was
    /// saved under. Any other a session still has pending was sent before this launch,
    /// and its machine is asked what became of it ([`AppState::settle_pending`]).
    asked: HashSet<String>,
    /// The ID of the refused message [`AppState::send_again`] is sending.
    resending: Option<String>,
    /// Each session's checkpoints, as last read from its repository.
    checkpoints: HashMap<u64, checkpoints::State>,
    /// The tree each session's next or running turn starts from.
    turn_starts: checkpoints::Starts,
    /// The name this workspace's checkpoints are saved under.
    checkpoint_store: checkpoints::StoreName,
    /// Repositories whose files a restore is putting back. No message to a session in
    /// one goes until it ends.
    restoring: HashMap<PathBuf, checkpoints::InFlight>,
    /// Checkpoint saves queued and not done yet, by session.
    saving: HashMap<u64, usize>,
    /// New worktrees' setup scripts, by session, until the first message goes.
    setups: HashMap<u64, setup::Setup>,
    /// The shell setup scripts run with. Tests set `/bin/sh`.
    pub(crate) setup_shell: std::ffi::OsString,
    catalogs: Catalogs,
    logs: Logs,
    log_losses: std::collections::BTreeMap<u64, String>,
    wake: Wake,
    changes: RuntimeChanged,
    pub(crate) phase: Phase,
    /// Set when the last window closed while a turn ran. Keystone then quits, saving
    /// as Quit does, once no turn runs.
    pub(crate) quit_when_idle: bool,
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
    /// Idle processes offered while changes were still being saved, by session: the
    /// revision they wait for and the provider's candidate.
    idle_waiting: HashMap<u64, (keystone_core::Revision, IdleCandidate)>,
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
    /// The delivered results as the sidebar's search last asked for them, and the
    /// store revision they were read at.
    delivered:
        std::cell::RefCell<Option<(keystone_core::Revision, Arc<delivery::DeliveredResults>)>>,
    /// Set while ready results are being sent, so a send's own pump does not send
    /// them again.
    delivering: bool,
    /// Results saved as delivered and not sent yet: the launched session, its parent,
    /// the message and the revision it waits for, its own save or a later one its send
    /// found still saving. Each goes, to where its parent's work continues then, once
    /// that revision is on disk.
    unsent: Vec<(u64, u64, String, keystone_core::Revision)>,
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
    /// The command every new link runs in place of `ssh`, such as `keystone-host
    /// attach`. Without one a test's link runs nothing and fails.
    #[cfg(test)]
    pub(crate) test_transport: Option<Vec<std::ffi::OsString>>,
    /// How long `open_link` waits just before it registers the routes, so a test can
    /// tell whether the link started reading before them.
    #[cfg(test)]
    pub(crate) test_route_pause: Option<std::time::Duration>,
    /// Whether a test put its own Claude connection in place, which runs the program
    /// it names. A connection started without one runs the `claude` on `PATH`.
    #[cfg(test)]
    test_claude_installed: bool,
    /// Whether turns take checkpoints. Off in tests unless one turns it on, so a test
    /// whose session works in this repository never writes refs into it.
    #[cfg(test)]
    pub(crate) test_checkpoints: bool,
    pub(crate) notifier: crate::notifications::Notifier,
    pub(crate) pull_requests: crate::pull_request::Monitor,
    pub(crate) devices: crate::device::Devices,
    /// Where attached images are copied, beside the workspace database.
    pub(crate) attachments: keystone_core::attachment::Attachments,
    /// The browser preview's pages, one per project folder.
    pub(crate) browsers: Arc<keystone_agent::browser::Browsers>,
    /// Folders whose saved browser address a surface has already loaded again since
    /// launch, so it loads once, not in every window or after every failure.
    browser_restored: HashSet<String>,
    scheduler: scheduled::Scheduler,
}
impl EventEmitter<RuntimeChanged> for AppState {}
impl EventEmitter<crate::browser::BrowserChanged> for AppState {}

/// A wake for the browser's pages: each burst of frames and addresses emits one
/// [`crate::browser::BrowserChanged`].
fn browser_signal(cx: &mut Context<AppState>) -> Wake {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
    cx.spawn(async move |this, cx| {
        while rx.next().await.is_some() {
            while rx.try_recv().is_ok() {}
            if this
                .update(cx, |state, cx| {
                    state.save_browser_pages();
                    cx.emit(crate::browser::BrowserChanged)
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
    Arc::new(move || {
        let _ = tx.unbounded_send(());
    })
}

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

/// How long Quit waits for every machine to hear that Keystone is quitting.
const QUIT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const REMOTE_UNLINKED: &str =
    "Not sent. This session's machine isn't linked any more. Link it again in Settings → Machines.";
/// Where a restart left a remote session's turn open. The turn's own rows after it say
/// whether it went on there or Quit ended it.
const REMOTE_CLOSED: &str = "Keystone was closed during this turn on";

/// Never shown: a session on another machine never asks for a folder here.
const REMOTE_LOCAL_FOLDER: &str = "Not sent. This conversation's folder is on another machine.";

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
        self.store.set_sync_directory(sync);
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
        self.test_claude_installed = value.is_some();
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

    /// Starts quitting, unless a quit already runs. `user` is whether the user chose
    /// Quit; one that didn't leaves every remote session running.
    pub(crate) fn reserve_quit(&mut self, user: bool) -> bool {
        if self.phase != Phase::Running {
            return false;
        }
        self.phase = Phase::Saving;
        self.idle_quit = !user;
        true
    }

    pub(crate) fn quit_failed(&mut self, reason: String, cx: &mut Context<Self>) {
        self.phase = Phase::Running;
        self.idle_quit = false;
        self.shutdown_error = Some(format!("Not quit. {reason}"));
        self.publish(cx);
    }

    /// Whether the store's only news is that its changes are committed and not yet
    /// confirmed on disk, which the status line shows as a warning.
    pub(crate) fn save_unconfirmed(&self) -> bool {
        matches!(self.store.save_error(), Some(SaveError::Unconfirmed(_)))
            && self.error == self.save_status()
    }

    /// Why the store's changes are not saved, as the status line shows it.
    fn save_status(&self) -> Option<String> {
        self.store.save_error().map(|error| match error {
            SaveError::Unsaved(error) | SaveError::Stale(error) => format!("Not saved. {error}"),
            SaveError::Unconfirmed(error) => {
                format!("Saved, but not confirmed on disk. {error}")
            }
        })
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
        // A result saved as delivered and not sent yet goes after the next start.
        for (child, _, message, _) in std::mem::take(&mut self.unsent) {
            let _ = self
                .store
                .set_delivery(child, keystone_core::DeliveryState::Ready { message });
            self.deliveries_due = true;
        }
        let mut errors = Vec::new();
        let drafts = drafts.into_iter().map(|d| (d.session, d.text)).collect();
        for (project, text) in notes {
            if let Err(error) = self.store.set_note(&project, text) {
                errors.push(format!("Note not saved. {error}"));
            }
        }
        if let Err(error) = self.store.save_for_quit(drafts, windows, crate::now_ms()) {
            errors.push(format!("Draft not saved. {error}"));
        }
        let logs = self.flush_logs(cx);
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
        let unsaved = self.store.flush().err();
        if !errors.is_empty() || unsaved.is_some() {
            // One line gives every reason, the store's too, which the status line then
            // shows only there.
            self.error = self.save_status();
            let reason = errors
                .iter()
                .cloned()
                .chain(self.error.clone().or_else(|| unsaved.clone()))
                .collect::<Vec<_>>()
                .join(" · ");
            self.quit_failed(reason, cx);
            return Err(errors
                .into_iter()
                .chain(unsaved)
                .collect::<Vec<_>>()
                .join("\n"));
        }
        self.shutdown_error = None;
        self.changes.sidebar = true;
        self.publish(cx);
        Ok(())
    }

    pub(crate) fn teardown(&mut self, cx: &mut Context<Self>) {
        self.phase = Phase::Exiting;
        self.stop_restores();
        self.stop_setups();
        drop(self.orchestrator.take());
        self.pull_requests.stop();
        self.devices.stop();
        self.browsers.shut_down();
        self.codex.clear();
        self.claude.clear();
        self.leave_machines();
        self.publish(cx);
    }

    /// Tells each connected machine that Keystone is quitting, after acknowledging what
    /// it drained, and waits a moment for the word to arrive. A machine keeps its
    /// sessions running unless the user quit and the machine is set to end them. One
    /// that isn't connected hears nothing, and its sessions keep running.
    fn leave_machines(&mut self) {
        self.linking = None;
        for (id, link) in &self.links {
            let end = !self.idle_quit
                && self
                    .store
                    .workspace()
                    .machine(id)
                    .is_some_and(|machine| machine.end_on_quit());
            link.quit(!end);
        }
        self.remote.clear();
        let deadline = std::time::Instant::now() + QUIT_WAIT;
        for link in std::mem::take(&mut self.links).into_values() {
            link.close(deadline.saturating_duration_since(std::time::Instant::now()));
        }
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

    /// Saves the address each browser page is on, so it comes back after a relaunch.
    /// A page that failed to load keeps the address it tried.
    fn save_browser_pages(&mut self) {
        if self.phase == Phase::Exiting {
            return;
        }
        for (folder, url) in self.browsers.addresses() {
            if url.starts_with("http://") || url.starts_with("https://") {
                let _ = self.store.set_browser_page(&folder, &url);
            }
        }
    }

    /// The address `folder`'s page was on before this launch, the first time a surface
    /// asks for it.
    pub(crate) fn browser_to_restore(&mut self, folder: &str) -> Option<String> {
        if !self.browser_restored.insert(folder.to_owned())
            || self.browsers.existing(folder).is_some()
        {
            return None;
        }
        self.store
            .workspace()
            .browser_page(folder)
            .map(str::to_owned)
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

    /// Saves the model, effort and speed the session's next turns run with, then gives
    /// them to its connection if it started. A change after the first send is noted in
    /// the transcript, which keeps it across a restart.
    pub(crate) fn set_choice(
        &mut self,
        id: u64,
        choice: ModelChoice,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let changed = self
            .session(id)
            .is_some_and(|s| s.started() && s.choice() != choice);
        let result = self.store.set_choice(id, choice.clone());
        if result.is_ok() {
            if let Some(connection) = self.started(id) {
                connection.set_choice(id, choice);
            }
            if changed && let Some(provider) = self.provider(id) {
                let note = self.model_selection(id, provider).note();
                self.conversation(id).push_note(note);
                self.commit(id);
            }
        }
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
                    .filter_map(|account| account.home().map(std::path::Path::to_path_buf)),
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
            self.usage
                .remove(&usage_key(None, provider, Some(&added.value)));
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Adds a login on `machine` whose CLI uses `folder` there as its config home.
    /// Nothing here reads `folder`: the machine's host makes it on first use and
    /// refuses one that overlaps its default home.
    pub(crate) fn add_remote_account(
        &mut self,
        provider: Provider,
        label: &str,
        folder: &str,
        machine: &MachineId,
        cx: &mut Context<Self>,
    ) -> Result<Applied<AccountId>, String> {
        if self.phase == Phase::Exiting {
            return Err("Keystone is exiting.".into());
        }
        let home = RemoteFolder::new(folder)?;
        let result = self
            .store
            .add_remote_account(provider, label, machine, &home)
            .map_err(|error| error.to_string());
        if let Ok(added) = &result {
            self.usage
                .remove(&usage_key(Some(machine), provider, Some(&added.value)));
        }
        self.changes.sidebar = true;
        self.publish(cx);
        result
    }

    /// Moves a login on another machine to `folder` there, while none of its sessions
    /// has a message the machine took. Its connections and catalogs go with the old
    /// folder, so the next call carries the new one and the host starts it afresh.
    pub(crate) fn set_remote_account_home(
        &mut self,
        id: &AccountId,
        folder: &str,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, String> {
        if self.phase == Phase::Exiting {
            return Err("Keystone is exiting.".into());
        }
        let home = RemoteFolder::new(folder)?;
        let result = self
            .store
            .set_remote_account_home(id, &home)
            .map_err(|error| error.to_string());
        if result.is_ok() {
            self.remote
                .retain(|(_, _, account), _| account.as_ref() != Some(id));
            self.catalogs.forget_account(id);
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
            // Its readings here, or on the machine it was a login on.
            let mut saves = self.usage_saves.lock().unwrap();
            let suffix = format!("/{}", id.as_str());
            let gone = |key: &String| key.ends_with(&suffix);
            self.usage.retain(|key, _| !gone(key));
            saves.0.retain(|key, _| !gone(key));
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

    pub(crate) fn set_option_as_meta(
        &mut self,
        on: bool,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_option_as_meta(on);
        self.publish(cx);
        result
    }

    pub(crate) fn set_shortcut(
        &mut self,
        command: &str,
        keys: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self.store.set_shortcut(command, keys);
        self.publish(cx);
        result
    }

    pub(crate) fn reset_shortcuts(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }
        let result = self.store.reset_shortcuts();
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
        self.devices.window_closed(window);
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

    pub(crate) fn has_work(&self) -> bool {
        (!self.unsent.is_empty() && self.store.save_error().is_none())
            || self.conversations.iter().any(|(&id, conversation)| {
                // Quit leaves a session on another machine running, so its turn never
                // holds Keystone open with no window.
                !self.is_remote(id)
                    && (conversation.running || self.started(id).is_some_and(|c| c.has_work(id)))
            })
    }

    fn runs_turn(&self, id: u64) -> bool {
        self.conversations.get(&id).is_some_and(|c| c.running)
            || self.started(id).is_some_and(|c| c.is_running(id))
    }

    /// The lowest-numbered session whose running turn waits for the user to answer an
    /// approval request.
    pub(crate) fn asking(&self) -> Option<u64> {
        // A remote session's approval waits on its machine, through Quit.
        self.conversations
            .iter()
            .filter(|&(&id, c)| c.running && c.awaiting_approval() && !self.is_remote(id))
            .map(|(&session, _)| session)
            .min()
    }

    /// While no window is open: quits once no turn runs, and opens the windows again
    /// on the session asking when a turn asks for an approval, so the user can answer
    /// it.
    fn quit_if_idle(&mut self, cx: &mut Context<Self>) {
        if !self.quit_when_idle || self.phase != Phase::Running {
            return;
        }
        if !self.has_work() {
            self.quit_when_idle = false;
            let state = cx.entity();
            cx.defer(move |cx| {
                if state.read(cx).has_work() {
                    state.update(cx, |state, _| state.quit_when_idle = true);
                } else {
                    request_quit(cx, false);
                }
            });
        } else if let Some(session) = self.asking() {
            self.quit_when_idle = false;
            let state = cx.entity();
            cx.defer(move |cx| {
                notifications::show(session, cx);
                if open_windows(cx).is_empty() {
                    state.update(cx, |state, _| state.quit_when_idle = true);
                }
            });
        }
    }

    pub(crate) fn new(mut store: Store, cx: &mut Context<Self>) -> Self {
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
        store.set_wake(wake.clone());
        let logs = Logs::with_database(store.database(), wake.clone());
        // Without them a replay after a crash shows rows the log has saved again,
        // which is worse than nothing only in being noisy, so a failed read goes on.
        let launch_cursors = store
            .database()
            .remote_cursors()
            .unwrap_or_else(|error| {
                eprintln!("Keystone could not read where remote sessions were saved: {error}");
                Vec::new()
            })
            .into_iter()
            .map(|(session, journal, seq)| {
                let journal = journal.into();
                (session, remote::Place { journal, seq })
            })
            .collect();
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
        let browsers = Arc::new(keystone_agent::browser::Browsers::new(
            data_dir.join("browser"),
            browser_signal(cx),
        ));
        let mut state = Self {
            browsers,
            browser_restored: HashSet::new(),
            removing_worktrees: Arc::default(),
            error: None,
            shutdown_error: None,
            codex: BTreeMap::new(),
            claude: BTreeMap::new(),
            links: BTreeMap::new(),
            remote: HashMap::new(),
            shown_links: BTreeMap::new(),
            linking: None,
            idle_quit: false,
            conversations: HashMap::new(),
            handed: HashMap::new(),
            through_due: BTreeSet::new(),
            launch_cursors,
            shown_journals: BTreeMap::new(),
            lost_turns: BTreeSet::new(),
            asked: HashSet::new(),
            resending: None,
            checkpoints: HashMap::new(),
            turn_starts: checkpoints::Starts::default(),
            checkpoint_store: checkpoints::StoreName::new(store.database()),
            restoring: Default::default(),
            saving: HashMap::new(),
            setup_shell: keystone_agent::setup::shell_for(std::env::var_os("SHELL")),
            setups: HashMap::new(),
            catalogs: Catalogs::default(),
            logs,
            log_losses: Default::default(),
            wake,
            changes: RuntimeChanged::default(),
            phase: Phase::Running,
            quit_when_idle: false,
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
            idle_waiting: HashMap::new(),
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
            delivered: Default::default(),
            delivering: false,
            unsent: Vec::new(),
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
            test_transport: None,
            #[cfg(test)]
            test_route_pause: None,
            #[cfg(test)]
            test_claude_installed: false,
            #[cfg(test)]
            test_checkpoints: false,
            notifier: Default::default(),
            pull_requests: Default::default(),
            devices: crate::device::Devices::new(Arc::new(
                keystone_agent::device::LocalHost::default(),
            )),
            attachments,
            scheduler: Default::default(),
            store,
        };
        state.load_scheduled_tasks(cx);
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
    /// Asks the windows that show `session` to open the Browser surface.
    pub(crate) fn reveal_browser_for(&mut self, session: u64, cx: &mut Context<Self>) {
        self.changes.browser.push(session);
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
                place: CatalogPlace::Local(directory),
            }),
            // Another machine's catalog is its CLI's, asked through its host.
            presentation::ConversationFolder::Remote(machine, folder) => Some(CatalogKey {
                provider,
                account: self.catalog_account(session, provider),
                place: CatalogPlace::Remote(machine, folder),
            }),
            presentation::ConversationFolder::LegacyMissing
            | presentation::ConversationFolder::Unsupported => None,
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
        let mut default_model = None;
        let choices = match self.conversation_folder(session) {
            Some(
                presentation::ConversationFolder::Recorded(_)
                | presentation::ConversationFolder::Remote(..),
            ) => {
                let key = self
                    .catalog_key(session, provider)
                    .expect("a recorded or remote folder has a catalog");
                default_model = self.catalogs.default_model(&key);
                self.catalogs.choices(&key)
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
            machine: saved.and_then(|s| s.machine()).and_then(|id| {
                let machine = self.store.workspace().machine(id)?;
                Some((machine.id(), machine.label()))
            }),
            model: saved.and_then(|s| s.model()),
            effort: saved.and_then(|s| s.effort()),
            speed: saved.and_then(|s| s.speed()),
            reported_model: self
                .conversations
                .get(&session)
                .and_then(|c| c.reported_model.as_deref()),
            // What it last reported before this run, for the chip until it reports again.
            last_model_name: saved.and_then(|s| s.last_model_name()),
            default_model,
            // A message still being sent goes with the provider and login it was
            // sent with.
            started: saved
                .is_none_or(|s| s.started() || s.pending().iter().any(|send| !send.refused)),
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

    /// Asks Claude's catalog so the model chip can name what the session's default model
    /// runs as before the first message. A test build asks only once a scripted Claude
    /// is in place, so it never starts the one on `PATH`. A connection that another
    /// request already started is not enough: without a scripted program, it runs the
    /// `claude` on `PATH` (TOB-363).
    pub(crate) fn name_default_model(&mut self, session: u64, cx: &mut Context<Self>) {
        #[cfg(test)]
        if self.test_programs.is_none()
            && std::env::var_os("KEYSTONE_CLAUDE_PATH").is_none()
            && !self.test_claude_installed
        {
            return;
        }
        self.request_catalog(session, Provider::Claude, cx);
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
        let place = key.place.clone();
        if self.catalogs.ask(key, asker) {
            let refused = match &place {
                CatalogPlace::Local(directory) => {
                    match self.connection(provider, account.as_ref()) {
                        Ok(connection) => {
                            connection.request_catalog(asker, directory.as_str());
                            None
                        }
                        Err(error) => Some(error),
                    }
                }
                CatalogPlace::Remote(machine, folder) => {
                    match self.remote_connection(machine, provider, account.as_ref()) {
                        Ok(connection) => {
                            connection.request_catalog(asker, folder.wire());
                            None
                        }
                        Err(error) => Some(error),
                    }
                }
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
        // A loading conversation holds its events back until its history is restored,
        // so their rows come with the commit after that, and so does their place.
        let through = if conversation.is_loading() {
            None
        } else {
            self.through_due.remove(&session);
            self.handed.get(&session).cloned()
        };
        let rejected = self
            .logs
            .append_through(session, conversation.take_writes(), through)
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
        self.resume_scheduled_runs(cx);
        self.error = self.save_status();
        let failed = self.store.save_error().is_some();
        let store = &self.store;
        let offers: Vec<_> = self
            .idle_waiting
            .extract_if(|_, (saving, _)| failed || store.is_durable(*saving))
            .collect();
        for (session, (_, candidate)) in offers {
            self.offer_idle(session, candidate);
        }
        let reports = self.logs.drain();
        let mut events: Vec<Placed> = Vec::new();
        // A catalog belongs to the connection it came from, whatever provider or
        // account the asking session has since moved to.
        let local = |provider, account: &Option<AccountId>| Route {
            machine: None,
            provider,
            account: account.clone(),
        };
        let unplaced = |events: Vec<(u64, Event)>| {
            events
                .into_iter()
                .map(|(session, event)| (None, session, event))
                .collect::<Vec<_>>()
        };
        let drained: Vec<_> = self
            .codex
            .iter()
            .map(|(account, c)| (local(Provider::Codex, account), unplaced(c.drain())))
            .chain(
                self.claude
                    .iter()
                    .map(|(account, c)| (local(Provider::Claude, account), unplaced(c.drain()))),
            )
            .chain(self.remote.iter().map(|((machine, provider, account), c)| {
                let route = Route {
                    machine: Some(machine.clone()),
                    provider: *provider,
                    account: account.clone(),
                };
                (route, c.drain_placed())
            }))
            .collect();
        for (route, batch) in drained {
            let Route {
                provider,
                ref account,
                ..
            } = route;
            let remote = route.machine.is_some();
            for (place, session, event) in batch {
                // An event that applies no row still takes its place in order, so a
                // commit never saves a place past an event not yet handed over.
                let placed = |event| Placed {
                    place,
                    session,
                    event,
                };
                match event {
                    // A catalog goes where its asker asked it, here or on the machine
                    // it came from, and a machine's usage is kept as that machine's,
                    // so neither overwrites another's. MCP status is kept for this
                    // machine's sessions only. An event from a machine still takes its
                    // place, so the host may trim it.
                    Event::McpStatus(_) if remote => events.push(placed(None)),
                    Event::Catalog(catalog) => {
                        self.catalogs
                            .apply(provider, account.clone(), session, catalog);
                        if remote {
                            events.push(placed(None));
                        }
                    }
                    Event::Commands(commands) => {
                        self.catalogs
                            .apply_commands(provider, account.clone(), session, commands);
                        if remote {
                            events.push(placed(None));
                        }
                    }
                    Event::Usage(windows) => {
                        self.record_usage(
                            route.machine.as_ref(),
                            provider,
                            account.as_ref(),
                            windows,
                            cx,
                        );
                        if remote {
                            events.push(placed(None));
                        }
                    }
                    Event::McpStatus(servers) => {
                        self.record_mcp_status(provider, account.as_ref(), session, servers, cx)
                    }
                    Event::ProviderNotice { .. }
                        if self.route(session).as_ref() != Some(&route) =>
                    {
                        events.push(placed(None))
                    }
                    // An answer saves no row, but one from another machine still
                    // takes its place, so the host may trim it.
                    Event::ThreadsListed { ticket, result } => {
                        answer(&self.native, ticket, Native::Listed(result));
                        events.push(placed(None));
                    }
                    Event::HistoryRead { ticket, result } if self.import_reads.remove(&ticket) => {
                        answer(&self.native, ticket, Native::Read(result));
                        events.push(placed(None));
                    }
                    event => events.push(placed(Some(event))),
                }
            }
        }
        self.watch_links();
        self.receive_placed(reports, events, cx);
        self.settle_pending(cx);
        self.pump_setups(cx);
        self.pump_orchestrator(cx);
        let reads = std::mem::take(&mut *self.recovery_reads.lock().unwrap());
        for (session, ticket, result) in reads {
            self.finish_recovery(session, ticket, result, cx);
        }
        self.start_reads();
        self.save_results();
        self.deliver_results(cx);
        self.end_lost_turns(cx);
    }

    /// Ends, once, each turn that was running in a host journal that is gone. The new
    /// journal has no record of it, so its events will never come, and the session
    /// would otherwise show it running for good. It is ended as failed with a note
    /// saying how it ended there isn't known, not with anything made up. A session
    /// waits here until its host has reported every session after the replay, and
    /// until the replay's events are all applied, so a turn whose end did come is left
    /// alone.
    fn end_lost_turns(&mut self, cx: &mut Context<Self>) {
        if self.lost_turns.is_empty() || self.phase != Phase::Running {
            return;
        }
        let mut ends = Vec::new();
        for session in std::mem::take(&mut self.lost_turns) {
            let Some(machine) = self.session(session).and_then(|s| s.machine()).cloned() else {
                continue;
            };
            let (Some(link), Some(connection)) =
                (self.links.get(&machine), self.session_remote(session))
            else {
                continue;
            };
            if !link.journal_changes().1 || connection.has_events() {
                self.lost_turns.insert(session);
                continue;
            }
            let running = self.conversations.get(&session).is_some_and(|c| c.running)
                || self
                    .session(session)
                    .is_some_and(|s| s.last_turn() == Some(keystone_core::LastTurn::Running));
            let host = connection.is_running(session) || !connection.waiting(session).is_empty();
            if running && !host {
                let label = self
                    .store
                    .workspace()
                    .machine(&machine)
                    .map_or_else(|| machine.to_string(), |m| m.label().to_owned());
                let error = Some(format!(
                    "keystone-host on {label} lost its record of this session, so how this turn ended there isn't known"
                ));
                ends.push((
                    session,
                    Event::TurnCompleted {
                        status: keystone_agent::TurnStatus::Failed,
                        error,
                        stopped: false,
                    },
                ));
            }
        }
        if !ends.is_empty() {
            self.receive(Vec::new(), ends, cx);
        }
    }

    pub(crate) fn receive(
        &mut self,
        reports: Vec<Report>,
        events: Vec<(u64, Event)>,
        cx: &mut Context<Self>,
    ) {
        let events = events
            .into_iter()
            .map(|(session, event)| Placed {
                place: None,
                session,
                event: Some(event),
            })
            .collect();
        self.receive_placed(reports, events, cx);
    }

    fn receive_placed(
        &mut self,
        reports: Vec<Report>,
        events: Vec<Placed>,
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
                        if self.is_remote(session) {
                            self.apply_host_state(session);
                        }
                        let Some(conversation) = self.conversations.get_mut(&session) else {
                            continue;
                        };
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
                Report::Through { session, place } => {
                    for link in self.links.values() {
                        link.saved(session, &place);
                    }
                    continue;
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
        for Placed {
            place,
            session,
            event,
        } in events
        {
            if let Some(place) = place {
                self.handed.insert(session, place);
                self.through_due.insert(session);
            }
            let Some(event) = event else {
                continue;
            };
            let event = self.answer_send(session, event);
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
                // A result already taken is no longer working.
                if matches!(
                    self.store.workspace().delivery(session).map(|d| d.state()),
                    Some(keystone_core::DeliveryState::Working)
                ) {
                    self.finished
                        .entry(session)
                        .or_insert_with(|| delivery::Outcome::of(&event));
                }
            }
            match (&event, failed) {
                (Event::TurnStarted, _) => {
                    let _ = self.store.record_turn_start(session);
                    self.changes.sidebar = true;
                }
                (_, Some(failed)) => {
                    let _ = self.store.record_turn_end(session, failed, now_ms());
                }
                _ => {}
            }
            sidebar_changed |= failed.is_some()
                || matches!(
                    event,
                    Event::ApprovalRequested(_)
                        | Event::QuestionAsked(_)
                        | Event::ApprovalResolved { .. }
                        | Event::SurfaceShown { .. }
                        | Event::SurfaceStopped { .. }
                );
            let reported = match &event {
                Event::ModelReported { model } => self.session(session).and_then(|saved| {
                    let name = self
                        .model_selection(session, saved.provider())
                        .choices
                        .label(model);
                    (saved.last_model_name() != Some(name.as_str())).then_some(name)
                }),
                _ => None,
            };
            let thread = self.conversation(session).apply(event);
            if let Some(model) = reported {
                // Saved like the conversation ID below: the writer reports a failure.
                let _ = self.store.set_last_model_name(session, model);
            }
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
                    let _ = self.store.set_provider_thread(session, thread);
                    self.changes.sidebar = true;
                }
            }
        }

        // Events that added no row still move their sessions' saved places.
        for session in std::mem::take(&mut self.through_due) {
            if self.conversations.contains_key(&session) {
                self.commit(session);
            } else if let Some(through) = self.handed.get(&session).cloned() {
                let _ = self.logs.append_through(session, Vec::new(), Some(through));
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

    /// The latest usage readings of `provider`'s `account` on `machine`; `None` is
    /// the default account, and this machine.
    pub(crate) fn usage(
        &self,
        machine: Option<&MachineId>,
        provider: Provider,
        account: Option<&AccountId>,
    ) -> Option<&AccountUsage> {
        self.usage.get(&usage_key(machine, provider, account))
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
        let folder = match self.place(session) {
            Some(presentation::SessionPlace::Local(directory)) => Some(directory.path().to_owned()),
            Some(presentation::SessionPlace::Remote(_) | presentation::SessionPlace::Unusable)
            | None => None,
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
        machine: Option<&MachineId>,
        provider: Provider,
        account: Option<&AccountId>,
        windows: Vec<LimitWindow>,
        cx: &mut Context<Self>,
    ) {
        let key = usage_key(machine, provider, account);
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
    fn route(&self, session: u64) -> Option<Route> {
        self.session(session).map(|s| Route {
            machine: s.machine().cloned(),
            provider: s.provider(),
            account: s.account().cloned(),
        })
    }

    /// Whether the session runs on another machine.
    pub(crate) fn is_remote(&self, session: u64) -> bool {
        self.session(session).is_some_and(|s| s.machine().is_some())
    }

    /// The state of `machine`'s link, which the sidebar, the session header and the
    /// composer show. A machine with no link yet is connecting: the next pump makes one.
    pub(crate) fn machine_status(&self, machine: &MachineId) -> LinkStatus {
        self.links
            .get(machine)
            .map_or(LinkStatus::Connecting, Link::status)
    }

    /// The machine a session runs on and its link's state, for a session on another
    /// machine.
    pub(crate) fn session_machine(
        &self,
        session: u64,
    ) -> Option<(&keystone_core::Machine, LinkStatus)> {
        let id = self.session(session)?.machine()?;
        let machine = self.store.workspace().machine(id)?;
        Some((machine, self.machine_status(id)))
    }

    /// Why the session can't send or answer now: its machine isn't connected. `None`
    /// for a session on this machine, or on a connected one.
    pub(crate) fn offline(&self, session: u64) -> Option<String> {
        let (machine, status) = self.session_machine(session)?;
        crate::machines::offline_reason(machine.label(), &status)
    }

    /// For a session whose machine isn't connected, why its messages still sending
    /// wait, and the machine's name: "Couldn't reach desk", or while the first link
    /// of this run starts, "Connecting to desk".
    pub(crate) fn unreached(&self, session: u64) -> Option<(String, String)> {
        let (machine, status) = self.session_machine(session)?;
        let label = machine.label().to_owned();
        let reason = match status {
            LinkStatus::Connected => return None,
            LinkStatus::Connecting => format!("Connecting to {label}"),
            LinkStatus::Reconnecting | LinkStatus::Failed(_) => format!("Couldn't reach {label}"),
        };
        Some((reason, label))
    }

    /// For a session whose turn runs on a machine that can't be reached now, the line
    /// that says so. The turn ends when the machine says how it went, never before.
    pub(crate) fn turn_waits(&self, session: u64) -> Option<String> {
        let (machine, status) = self.session_machine(session)?;
        if status == LinkStatus::Connected {
            return None;
        }
        let running = self.conversations.get(&session).is_some_and(|c| c.running)
            || self
                .session(session)
                .is_some_and(|s| s.last_turn() == Some(keystone_core::LastTurn::Running));
        running.then(|| format!("Waiting for {} to say how this turn went", machine.label()))
    }

    /// What the host said about itself when `machine` last connected.
    pub(crate) fn machine_welcome(&self, machine: &MachineId) -> Option<remote::Welcome> {
        self.links.get(machine)?.welcome()
    }

    /// The command that reaches `destination`: `ssh`. A test runs its own transport,
    /// or nothing.
    fn transport(&self, destination: &str) -> Result<Vec<std::ffi::OsString>, String> {
        let ssh = remote::ssh(destination)?;
        #[cfg(test)]
        let ssh = {
            let _ = ssh;
            self.test_transport
                .clone()
                .unwrap_or_else(|| vec!["/nonexistent/keystone-test-ssh".into()])
        };
        Ok(ssh)
    }

    /// A link to `destination` with a connection for each provider and account the
    /// machine's sessions run on, Claude on its default login included, each of which
    /// wakes the pump on every event and state change. Each is registered before the
    /// link starts, so no event of theirs waits unclaimed.
    fn open_link(
        &self,
        destination: &str,
        client: &str,
        machine: Option<&MachineId>,
    ) -> Result<(Link, Vec<(Route, RemoteConnection)>), String> {
        let sessions = self
            .store
            .workspace()
            .sessions()
            .iter()
            .filter(|session| machine.is_some() && session.machine() == machine);
        let saved = sessions
            .clone()
            .filter_map(|session| {
                let cursor = self.launch_cursors.get(&session.id())?;
                Some((session.id(), cursor.clone()))
            })
            .collect();
        let mut routes = vec![(Provider::Claude, None)];
        for session in sessions {
            let route = (session.provider(), session.account().cloned());
            if !routes.contains(&route) {
                routes.push(route);
            }
        }
        // The host names the machine as the app does, by its label.
        let name = machine
            .and_then(|id| self.store.workspace().machine(id))
            .map_or(destination, keystone_core::Machine::label)
            .to_owned();
        let link = Link::prepared(
            self.transport(destination)?,
            client.to_owned(),
            saved,
            Some(name),
        );
        #[cfg(test)]
        if let Some(pause) = self.test_route_pause {
            std::thread::sleep(pause);
        }
        let connections = routes
            .into_iter()
            .filter_map(|(provider, account)| {
                let route = Route {
                    machine: machine.cloned(),
                    provider,
                    account,
                };
                match self.remote_on(&link, &route) {
                    Ok(connection) => Some((route, connection)),
                    Err(error) => {
                        eprintln!("Keystone could not reach {provider} on {destination}: {error}");
                        None
                    }
                }
            })
            .collect();
        // Only now does the link read, with every route it can hear from registered.
        link.start();
        Ok((link, connections))
    }

    /// A connection on `link` for `route`'s provider and account. An account other
    /// than the machine's default login must be one of that machine's logins, and
    /// carries its home there for the host to run it with.
    fn remote_on(&self, link: &Link, route: &Route) -> Result<RemoteConnection, String> {
        let login = match &route.account {
            None => None,
            Some(id) => {
                let account = self
                    .store
                    .workspace()
                    .account(id)
                    .filter(|account| account.provider() == route.provider)
                    .ok_or(REMOTE_LOGIN)?;
                let (on, home) = account.remote_home().ok_or(REMOTE_LOGIN)?;
                if route.machine.as_ref() != Some(on) {
                    return Err(REMOTE_LOGIN.into());
                }
                let login = remote::frame::Login {
                    label: account.label().to_owned(),
                    home: home.wire().to_owned(),
                };
                Some((id.to_string(), login))
            }
        };
        Ok(RemoteConnection::new(
            link,
            route.provider,
            login,
            self.wake.clone(),
        ))
    }

    /// Keeps `connections`, made for `machine`'s link, as that machine's.
    fn keep_remote(&mut self, machine: &MachineId, connections: Vec<(Route, RemoteConnection)>) {
        for (route, connection) in connections {
            self.remote
                .insert((machine.clone(), route.provider, route.account), connection);
        }
    }

    /// The connection a session on another machine runs on, once its link is made.
    fn session_remote(&self, session: u64) -> Option<&RemoteConnection> {
        let route = self.route(session)?;
        self.remote
            .get(&(route.machine?, route.provider, route.account))
    }

    /// Makes the link of each linked machine that has none, then notes each link's
    /// changes: a new state redraws the sidebar, and a host's report of its sessions
    /// reaches their conversations.
    fn watch_links(&mut self) {
        if self.phase != Phase::Running {
            return;
        }
        self.watch_linking();
        let workspace = self.store.workspace();
        let missing: Vec<(MachineId, String)> = workspace
            .machines()
            .iter()
            .filter(|machine| !self.links.contains_key(machine.id()))
            .map(|machine| (machine.id().clone(), machine.destination().to_owned()))
            .collect();
        if let Some(client) = workspace.remote_client().map(str::to_owned) {
            for (id, destination) in missing {
                match self.open_link(&destination, &client, Some(&id)) {
                    Ok((link, connections)) => {
                        self.links.insert(id.clone(), link);
                        self.keep_remote(&id, connections);
                    }
                    Err(error) => eprintln!("Keystone could not link {destination}: {error}"),
                }
            }
        }
        let replaced: Vec<(MachineId, u64)> = self
            .links
            .iter()
            .map(|(id, link)| (id.clone(), link.journal_changes().0))
            .filter(|(id, changes)| self.shown_journals.get(id).copied().unwrap_or(0) != *changes)
            .collect();
        for (id, changes) in replaced {
            self.shown_journals.insert(id.clone(), changes);
            let sessions: Vec<u64> = self
                .store
                .workspace()
                .sessions()
                .iter()
                .filter(|s| s.machine() == Some(&id))
                .map(|s| s.id())
                .collect();
            self.lost_turns.extend(sessions);
        }
        let changed: Vec<(MachineId, LinkStatus, u64)> = self
            .links
            .iter()
            .map(|(id, link)| (id.clone(), link.status(), link.reports()))
            .filter(|(id, status, reports)| {
                self.shown_links
                    .get(id)
                    .is_none_or(|shown| (&shown.0, shown.1) != (status, *reports))
            })
            .collect();
        for (id, status, reports) in changed {
            if status == LinkStatus::Connected {
                self.clear_offline_errors(&id);
            }
            let reported = self
                .shown_links
                .get(&id)
                .is_none_or(|shown| shown.1 != reports);
            self.shown_links.insert(id.clone(), (status, reports));
            self.changes.sidebar = true;
            if reported {
                self.apply_host_report(&id);
            }
        }
    }

    /// Drops the note that a message wasn't sent because `machine` wasn't connected,
    /// once it is again, so it doesn't read as current.
    fn clear_offline_errors(&mut self, machine: &MachineId) {
        let sessions: Vec<u64> = self
            .store
            .workspace()
            .sessions()
            .iter()
            .filter(|s| s.machine() == Some(machine))
            .map(|s| s.id())
            .collect();
        for session in sessions {
            if let Some(conversation) = self.conversations.get_mut(&session)
                && conversation.error.as_deref() == Some(remote::NOT_CONNECTED)
            {
                conversation.error = None;
                self.commit(session);
            }
        }
    }

    /// Puts what `machine`'s host last reported about each of its sessions into their
    /// loaded conversations: whether a turn runs there and the approvals it waits on.
    fn apply_host_report(&mut self, machine: &MachineId) {
        let sessions: Vec<u64> = self
            .store
            .workspace()
            .sessions()
            .iter()
            .filter(|s| s.machine() == Some(machine))
            .map(|s| s.id())
            .collect();
        for session in sessions {
            self.apply_host_state(session);
        }
    }

    /// Puts what its machine's host last reported about `session` into its
    /// conversation, once the host has reported and the history has loaded. A report
    /// that arrives during the load is applied when the load ends.
    /// Until the host has reported, as while its machine can't be reached, the turn a
    /// restart left open says only that Keystone was closed during it: it went on
    /// there, and the machine says how it ends.
    fn apply_host_state(&mut self, session: u64) {
        let Some(machine) = self.session(session).and_then(|s| s.machine()).cloned() else {
            return;
        };
        let label = self
            .store
            .workspace()
            .machine(&machine)
            .map_or_else(|| machine.to_string(), |m| m.label().to_owned());
        let note = format!("{REMOTE_CLOSED} {label}.");
        if self
            .links
            .get(&machine)
            .is_none_or(|link| link.reports() == 0)
        {
            if let Some(conversation) = self.conversations.get_mut(&session)
                && !conversation.is_loading()
            {
                conversation.closed_elsewhere(note);
                self.commit(session);
            }
            return;
        }
        let Some(connection) = self.session_remote(session) else {
            return;
        };
        let (running, waiting) = (connection.is_running(session), connection.waiting(session));
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return;
        };
        if conversation.is_loading() {
            return;
        }
        conversation.host_state(running, waiting, note);
        self.commit(session);
    }

    /// Follows the machine Settings → Machines is linking: saves it once its host
    /// answers, and keeps why it failed otherwise. A failed attempt isn't saved.
    fn watch_linking(&mut self) {
        let Some(linking) = &mut self.linking else {
            return;
        };
        let Some((link, _)) = &linking.link else {
            return;
        };
        let failed = match link.status() {
            LinkStatus::Connected => None,
            LinkStatus::Failed(reason) => Some(reason),
            LinkStatus::Connecting | LinkStatus::Reconnecting => match link.trouble() {
                Some(trouble) => Some(trouble),
                None => return,
            },
        };
        if let Some(reason) = failed {
            linking.link = None;
            linking.failed = Some(reason);
            self.changes.sidebar = true;
            return;
        }
        let Some(Linking {
            destination,
            client,
            link: Some((link, connections)),
            ..
        }) = self.linking.take()
        else {
            return;
        };
        match self.store.link_machine(&destination, client) {
            Ok(applied) => {
                self.links.insert(applied.value.clone(), link);
                self.keep_remote(&applied.value, connections);
            }
            Err(error) => {
                self.linking = Some(Linking {
                    destination,
                    client: String::new(),
                    link: None,
                    failed: Some(format!("Not saved. {error}")),
                })
            }
        }
        self.changes.sidebar = true;
    }

    /// Starts linking `destination`. It is saved once its host answers.
    pub(crate) fn link_machine(&mut self, destination: &str, cx: &mut Context<Self>) {
        if self.phase != Phase::Running {
            return;
        }
        let destination = destination.trim().to_owned();
        let client = self
            .store
            .workspace()
            .remote_client()
            .map_or_else(remote::new_client_id, str::to_owned);
        let refused = keystone_core::check_destination(&destination)
            .err()
            .or_else(|| {
                self.store
                    .workspace()
                    .machines()
                    .iter()
                    .find(|machine| machine.destination() == destination)
                    .map(|machine| format!("{} is already linked.", machine.label()))
            });
        let (link, failed) = match refused {
            Some(reason) => (None, Some(reason)),
            None => match self.open_link(&destination, &client, None) {
                Ok(link) => (Some(link), None),
                Err(error) => (None, Some(error)),
            },
        };
        self.linking = Some(Linking {
            destination,
            client,
            link,
            failed,
        });
        self.refresh(cx);
    }

    /// Tries a failed machine's link again at once.
    pub(crate) fn retry_machine(&mut self, machine: &MachineId, cx: &mut Context<Self>) {
        if let Some(link) = self.links.get(machine) {
            link.retry();
        }
        self.refresh(cx);
    }

    pub(crate) fn set_machine_end_on_quit(
        &mut self,
        machine: &MachineId,
        end: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let result = self
            .store
            .set_machine_end_on_quit(machine, end)
            .map(|_| ())
            .map_err(|error| error.to_string());
        self.refresh(cx);
        result
    }

    /// Forgets a machine no session runs on, and ends its link.
    pub(crate) fn remove_machine(
        &mut self,
        machine: &MachineId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let result = self
            .store
            .remove_machine(machine)
            .map(|_| ())
            .map_err(|error| error.to_string());
        if result.is_ok() {
            self.remote.retain(|(id, ..), _| id != machine);
            self.links.remove(machine);
            self.shown_links.remove(machine);
        }
        self.refresh(cx);
        result
    }

    /// Creates a Claude session in `folder` on `machine`, selected in `window`.
    pub(crate) fn create_remote(
        &mut self,
        machine: &MachineId,
        folder: &RemoteFolder,
        window: WindowId,
        show: Option<(&ProjectId, Composition, SessionTabs, ViewId)>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase != Phase::Running {
            return Err(std::io::Error::other("Keystone is quitting.").into());
        }
        let created = self
            .store
            .create_remote_showing(machine, folder, now_ms(), window, show);
        self.refresh(cx);
        created
    }

    /// The connection on `machine` for `provider`'s `account`, made on its link the
    /// first time a session needs it, before its first call, so the link holds every
    /// event of it for the pump.
    fn remote_connection(
        &mut self,
        machine: &MachineId,
        provider: Provider,
        account: Option<&AccountId>,
    ) -> Result<&dyn Connection, String> {
        if self.store.workspace().machine(machine).is_none() {
            return Err(REMOTE_UNLINKED.into());
        }
        self.watch_links();
        let key = (machine.clone(), provider, account.cloned());
        if !self.remote.contains_key(&key) {
            let link = self.links.get(machine).ok_or(REMOTE_UNLINKED)?;
            let route = Route {
                machine: Some(machine.clone()),
                provider,
                account: account.cloned(),
            };
            let connection = self.remote_on(link, &route)?;
            self.remote.insert(key.clone(), connection);
        }
        Ok(&self.remote[&key] as &dyn Connection)
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
            Some(account) if account.provider() == provider && account.home().is_none() => {
                Err(format!(
                    "Not sent. The account \u{201c}{}\u{201d} is a login on another machine.",
                    account.label()
                ))
            }
            Some(account) if account.provider() == provider => {
                let local = account.home().expect("checked above");
                let home = real_path(local);
                if let Some(other) = self
                    .store
                    .workspace()
                    .accounts()
                    .iter()
                    .filter(|other| other.id() != account.id())
                    .filter_map(|other| Some((other, other.home()?)))
                    .find(|(_, other)| keystone_core::homes_overlap(&real_path(other), &home))
                    .map(|(other, _)| other)
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
                        local.display(),
                        default.display()
                    ));
                }
                Ok(LaunchProfile::account(provider, account.label(), local))
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
                session.machine().is_none()
                    && session.provider() == provider
                    && (any_account || session.account() == account)
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
    /// everything queued so far, the process may end. Changes still being saved hold
    /// the offer until they are on disk, and the pump then makes it again. A failed save,
    /// or a session busy here, keeps it running; its next activity proves it again.
    fn offer_idle(&mut self, session: u64, candidate: IdleCandidate) {
        if self.phase != Phase::Running {
            return;
        }
        let thread_saved = self
            .session(session)
            .is_some_and(|s| s.provider_thread() == Some(candidate.thread.as_str()));
        let settled = self
            .conversations
            .get(&session)
            .is_some_and(|c| !c.is_loading() && !c.running && c.save_error.is_none());
        let declined = if !thread_saved {
            Some("its conversation ID is not saved yet")
        } else if !settled {
            Some("its conversation is loading, running or not being saved")
        } else if self.store.save_error().is_some() {
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
        let saving = self.store.revision();
        if !self.store.is_durable(saving) {
            self.store.wake_when_durable(saving);
            self.idle_waiting.insert(session, (saving, candidate));
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
        let Route {
            machine,
            provider,
            account,
        } = self.route(session)?;
        if let Some(machine) = machine {
            return self
                .remote
                .get(&(machine, provider, account))
                .map(|c| c as &dyn Connection);
        }
        match provider {
            Provider::Codex => self.codex.get(&account).map(|c| c as &dyn Connection),
            Provider::Claude => self.claude.get(&account).map(|c| c as &dyn Connection),
        }
    }

    /// Reads what the session's provider saved, from the session's own account, and
    /// holds the next message until the result is shown and saved. Read only: nothing
    /// is sent, resumed or answered.
    pub(crate) fn start_recovery(&mut self, session: u64, cx: &mut Context<Self>) {
        // Recovery reads the provider's files, which for a session on another machine
        // are there. Its host reports the session's state instead.
        if self.phase != Phase::Running || self.is_remote(session) {
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
                        Some(
                            ConversationFolder::Remote(..)
                            | ConversationFolder::LegacyMissing
                            | ConversationFolder::Unsupported,
                        )
                        | None => {
                            Err("this conversation has no saved folder to check against".into())
                        }
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
                .and_then(|account| account.home().map(std::path::Path::to_path_buf))
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
        self.send_message_from(draft, None, SendKind::Draft, cx) == Sent::Yes
    }

    /// Sends review comments from a diff as a message of their own. The draft stays.
    /// A refusal gives the reason the session's transcript shows, when it has one.
    pub(crate) fn send_review(
        &mut self,
        session: u64,
        text: String,
        cx: &mut Context<Self>,
    ) -> Result<(), Option<String>> {
        let draft = ComposerDraft {
            session,
            text,
            attachments: Vec::new(),
        };
        if self.send_message_from(draft, None, SendKind::Widget, cx) == Sent::Yes {
            return Ok(());
        }
        Err(self
            .conversations
            .get(&session)
            .and_then(|conversation| conversation.error.clone()))
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
        if self.send_message_from(draft, None, SendKind::Widget, cx) == Sent::Yes {
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
        if self.send_message_from(draft, None, SendKind::Widget, cx) == Sent::Yes {
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
    ) -> Sent {
        if self.phase != Phase::Running {
            return Sent::No;
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
            return Sent::No;
        };
        let text = draft.text.trim().to_owned();
        if text.is_empty() && draft.attachments.is_empty() {
            return Sent::No;
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
                return Sent::No;
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
                return Sent::No;
            }
            if conversation.is_loading()
                && session.provider_thread().is_some()
                && !self.send_unloaded.contains(&id)
            {
                self.wait_for_history(id, cx);
                return Sent::No;
            }
        }
        // A new worktree's setup script runs before its first message.
        if let Some(reason) = self.setup_holds_send(id) {
            if kind != SendKind::Result {
                self.conversation(id).error = Some(reason.into());
                self.commit(id);
                self.publish(cx);
            }
            return Sent::No;
        }
        // A result waits for a turn that started meanwhile rather than steering it.
        if matches!(kind, SendKind::Result | SendKind::Scheduled) && self.runs_turn(id) {
            return Sent::No;
        }
        // A result also waits for a restore in its repository to end.
        if self.restoring(id) {
            if kind != SendKind::Result {
                self.conversation(id).error = Some(checkpoints::RESTORING.into());
                self.commit(id);
                self.publish(cx);
            }
            return Sent::No;
        }
        // A folder on another machine is checked there, as part of the send.
        let place = match self.conversation_folder(id) {
            Some(ConversationFolder::Remote(machine, folder)) => Place::Remote(machine, folder),
            _ => match self.dispatch_directory(id, cx) {
                Some(directory) => Place::Local(directory),
                None => return Sent::No,
            },
        };
        // A session whose account is gone keeps its draft and stays unstarted.
        if let Place::Local(_) = place
            && let Err(error) = self.profile(session.provider(), session.account())
        {
            self.conversation(id).error = Some(error);
            self.commit(id);
            self.publish(cx);
            return Sent::No;
        }
        let cwd = match &place {
            Place::Local(directory) => directory.as_str().to_owned(),
            Place::Remote(_, folder) => folder.wire().to_owned(),
        };
        let cwd = cwd.as_str();
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
            return Sent::No;
        }
        // The provider and account are fixed from the first message on. Saved before the
        // message goes, so a crash after this leaves them fixed rather than free. A
        // message to another machine fixes them only once its host takes it.
        let remote = matches!(place, Place::Remote(..));
        if !remote
            && !session.started()
            && let Err(error) = self.store.lock_model(id)
        {
            self.conversation(id).error = Some(format!(
                "Not sent: the model choice could not be saved first. {error}"
            ));
            self.commit(id);
            self.publish(cx);
            return Sent::No;
        }
        let Some((provider, approval)) = self.approval(id) else {
            return Sent::No;
        };
        // The orchestrator's tools aren't offered on other machines yet.
        if let Place::Local(_) = place
            && let Err(error) = self.ensure_orchestrator()
        {
            self.conversation(id).error = Some(format!("Not sent: {error}"));
            self.commit(id);
            self.publish(cx);
            return Sent::No;
        }
        let account = session.account().cloned();
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
            return Sent::No;
        }
        // A message to another machine is saved as pending first, under the ID its
        // host's answer will name, so it is answered after a relaunch too. It isn't a
        // turn until the host takes it.
        let again = if kind == SendKind::Again {
            match self.resending.clone() {
                Some(send) if remote => Some(send),
                _ => return Sent::No,
            }
        } else {
            None
        };
        let message = match &again {
            Some(send) => send.clone(),
            None => self.conversation(id).new_message_id(),
        };
        if remote {
            let saved = match &again {
                // Pending again under its own ID, which the host's answer will name.
                Some(send) => match self.store.resend_send(id, send) {
                    Ok(applied) if applied.value.is_some() => Ok(()),
                    Ok(_) => return Sent::No,
                    Err(error) => Err(error),
                },
                None => self
                    .store
                    .record_pending_send(
                        id,
                        keystone_core::PendingSend {
                            id: message.clone(),
                            text: text.clone(),
                            attachments: draft.attachments.clone(),
                            refused: false,
                            ran: false,
                        },
                        now_ms(),
                    )
                    .map(|_| ()),
            };
            if let Err(error) = saved {
                self.conversation(id).error = Some(format!("Not sent: {error}"));
                self.commit(id);
                self.publish(cx);
                return Sent::No;
            }
            self.asked.insert(message.clone());
        }
        // The saves above must be on disk before the provider can see the message, and
        // so must any that an earlier refused send left queued. A failure to start the
        // provider arrives later as an error in this conversation. A refusal comes now,
        // and the text stays in the composer. A result never waits here: it is kept
        // and tried again once they are.
        if !self.store.is_durable(self.store.revision()) {
            if matches!(kind, SendKind::Result | SendKind::Scheduled) {
                self.unsend(id, remote.then_some(&message), again.is_some());
                return Sent::Later;
            }
            self.store.retry();
            if let Err(error) = self
                .store
                .wait(self.store.revision(), keystone_core::WRITER_WAIT)
            {
                let error = error.map(|error| error.to_string()).unwrap_or_default();
                self.unsend(id, remote.then_some(&message), again.is_some());
                self.conversation(id).error = Some(
                    format!("Not sent. Could not finish saving changes. {error}")
                        .trim_end()
                        .to_owned(),
                );
                self.commit(id);
                self.refresh(cx);
                return Sent::No;
            }
        }
        // A message that starts a turn first reads the files the turn starts from.
        let steer = self.runs_turn(id);
        // A checkpoint reads the folder's files, which for a remote session are there.
        if let Place::Local(_) = place {
            self.begin_checkpoint(id, cwd, steer);
        }
        let connection = match &place {
            Place::Local(_) => self.connection(provider, account.as_ref()),
            Place::Remote(machine, _) => {
                self.remote_connection(machine, provider, account.as_ref())
            }
        };
        let sent = match connection {
            Ok(connection) => {
                // A native fork's first message opens the provider's fork of its source.
                if resume.is_none()
                    && let Some(point) = session.fork_point()
                {
                    connection.set_fork(id, point.clone());
                }
                if remote {
                    connection.tag_send(id, &message);
                }
                match caller {
                    Some(caller) => connection.send_from(
                        id,
                        cwd,
                        resume.as_deref(),
                        &session.choice(),
                        approval,
                        text.clone(),
                        caller,
                    ),
                    None => connection.send(
                        id,
                        cwd,
                        resume.as_deref(),
                        &session.choice(),
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
            self.unsend(id, remote.then_some(&message), again.is_some());
            // Shown in the session's transcript, where a draft save cannot clear it.
            self.conversation(id).error = Some(reason);
            self.commit(id);
            self.publish(cx);
            return Sent::No;
        }
        for attachment in &draft.attachments {
            self.attachments.touch(attachment);
        }
        self.setup_sent(id);
        if again.is_some() {
            // Its bubble, with its images, is already there: it reads as sending again.
            self.conversation(id).send_again(&message);
        } else {
            self.conversation(id)
                .push_message(crate::conversation::Sent {
                    id: message,
                    text: text.clone(),
                    attachments: draft.attachments.clone(),
                    sending: remote,
                });
        }
        self.commit(id);
        if !remote {
            let _ = self.store.record_message(id, &text, now_ms());
        }
        if kind == SendKind::Draft {
            let _ = self.store.set_draft(id, String::new());
            if !draft.attachments.is_empty() {
                let _ = self.store.set_draft_attachments(id, Vec::new());
            }
        }
        self.refresh(cx);
        Sent::Yes
    }

    /// Saves what the machine said about a message sent to it: taken, it is the
    /// session's running turn and fixes its provider and login; refused, the session
    /// stays as it was, the message stays in its bubble to be sent again, and a refused
    /// login says how to go on. Returns the event to show.
    fn answer_send(&mut self, session: u64, event: Event) -> Event {
        match event {
            Event::Taken { send } => {
                let _ = self.store.take_send(session, &send);
                self.changes.sidebar = true;
                Event::Taken { send }
            }
            Event::NotSent {
                send,
                message,
                login,
            } => {
                let _ = self.store.refuse_send(session, &send);
                self.changes.sidebar = true;
                let unfixed = self.session(session).is_some_and(|s| !s.started());
                let message = if login && unfixed {
                    format!("{message}. {WAY_OUT}")
                } else {
                    message
                };
                Event::NotSent {
                    send,
                    message,
                    login,
                }
            }
            event => event,
        }
    }

    /// Undoes the saving of a message that never went: a new one is forgotten, and one
    /// sent again is refused as it was, still in its bubble to be sent again.
    fn unsend(&mut self, session: u64, send: Option<&String>, again: bool) {
        let Some(send) = send else {
            return;
        };
        let _ = if again {
            self.store.refuse_send(session, send)
        } else {
            self.store.withdraw_send(session, send)
        };
    }

    /// The message its machine refused as `send`, if the session holds one.
    pub(crate) fn refused_send(
        &self,
        session: u64,
        send: &str,
    ) -> Option<keystone_core::PendingSend> {
        self.session(session)?
            .pending()
            .iter()
            .find(|p| p.id == send && p.refused)
            .cloned()
    }

    /// Sends the message its machine refused as `send` again, under that same ID, with
    /// its text and images, through the login the session has now. The host runs a
    /// refused message as new and answers one it took with its recorded `ok`, running
    /// nothing, so pressing it can never run a message twice. The draft is untouched.
    pub(crate) fn send_again(&mut self, session: u64, send: &str, cx: &mut Context<Self>) -> bool {
        let Some(refused) = self.refused_send(session, send) else {
            return false;
        };
        self.resending = Some(send.to_owned());
        let sent = self.send_message_from(
            ComposerDraft {
                session,
                text: refused.text,
                attachments: refused.attachments,
            },
            None,
            SendKind::Again,
            cx,
        );
        self.resending = None;
        sent == Sent::Yes
    }

    /// Takes the message its machine refused as `send` out of its bubble into the empty
    /// composer, text and images, to write a new message from. Its bubble reads
    /// "Edited". `composer_empty` is what the composer on screen holds, which the saved
    /// draft can lag behind; with anything in it this does nothing.
    pub(crate) fn edit_refused(
        &mut self,
        session: u64,
        send: &str,
        composer_empty: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.phase != Phase::Running || !composer_empty {
            return false;
        }
        let Some(refused) = self.refused_send(session, send) else {
            return false;
        };
        if self.store.withdraw_send(session, send).is_err() {
            return false;
        }
        let _ = self.store.set_draft(session, refused.text);
        let _ = self
            .store
            .set_draft_attachments(session, refused.attachments);
        self.changes.drafts.push(session);
        self.conversation(session).edited(send);
        self.commit(session);
        self.changes.sidebar = true;
        self.refresh(cx);
        true
    }

    /// Asks each machine what became of the messages its sessions sent before this
    /// launch and still have pending, once it is connected. Only the machine says a
    /// message wasn't sent: while it can't be reached, a message stays pending, its
    /// bubble says so, and it is asked about when the machine is reached, in this run
    /// or a later one. The user may [`AppState::discard_send`] it meanwhile.
    fn settle_pending(&mut self, _cx: &mut Context<Self>) {
        if self.phase != Phase::Running {
            return;
        }
        self.watch_links();
        let waiting: Vec<_> = self
            .store
            .workspace()
            .sessions()
            .iter()
            .filter_map(|s| {
                let machine = s.machine()?.clone();
                let sends: Vec<String> = s
                    .pending()
                    .iter()
                    .filter(|send| !send.refused && !self.asked.contains(&send.id))
                    .map(|send| send.id.clone())
                    .collect();
                (!sends.is_empty())
                    .then(|| (s.id(), machine, s.provider(), s.account().cloned(), sends))
            })
            .collect();
        for (session, machine, provider, account, sends) in waiting {
            // Made whatever the link's state, so its connecting wakes the app to ask.
            if self
                .remote_connection(&machine, provider, account.as_ref())
                .is_err()
                || self.links.get(&machine).map(Link::status) != Some(LinkStatus::Connected)
            {
                continue;
            }
            let key = (machine, provider, account);
            for send in sends {
                if self.remote[&key].recall(session, &send).is_ok() {
                    self.asked.insert(send);
                }
            }
        }
    }

    /// Gives up the message the session sent as `send` while its machine can't say what
    /// became of it, so the session is free again: it leaves the session's pending
    /// messages, its call is never sent again, and its bubble reads as discarded. The
    /// machine may already have run it, so its text doesn't go back to the draft; if
    /// the machine's journal shows it ran, its turn shows and its bubble reads as sent.
    pub(crate) fn discard_send(&mut self, session: u64, send: &str, cx: &mut Context<Self>) {
        if self.phase != Phase::Running {
            return;
        }
        let Some(saved) = self.session(session) else {
            return;
        };
        let (machine, provider, account) = match saved.machine() {
            Some(machine) => (machine.clone(), saved.provider(), saved.account().cloned()),
            None => return,
        };
        if !saved.pending().iter().any(|p| p.id == send && !p.refused) {
            return;
        }
        let _ = self.store.withdraw_send(session, send);
        self.asked.insert(send.to_owned());
        if self
            .remote_connection(&machine, provider, account.as_ref())
            .is_ok()
        {
            self.remote[&(machine, provider, account)].abandon(send);
        }
        self.conversation(session).discard(send);
        self.commit(session);
        self.changes.sidebar = true;
        self.refresh(cx);
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
        // An answer waits, unsent and unrecorded, until the machine is connected again.
        if self.offline(id).is_some() {
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

    /// Sends the answers given to question `request`, or declines it. Answers that
    /// are not complete yet are not sent.
    pub(crate) fn reply(
        &mut self,
        id: u64,
        request: &RequestId,
        decline: bool,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Running {
            return;
        }
        let conversation = self.conversation(id);
        let ready = conversation.questions.iter().any(|q| {
            &q.question.request == request && (decline || q.question.problem(&q.answers).is_none())
        });
        let Some(asked) = ready.then(|| conversation.take_question(request)).flatten() else {
            return;
        };
        if let Some(connection) = self.started(id) {
            connection.command(Command::Reply {
                session: id,
                request: asked.question.request,
                answers: (!decline).then_some(asked.answers),
            });
        }
        self.refresh(cx);
    }

    /// Changes the answer to one field of a waiting question.
    pub(crate) fn edit_answer(
        &mut self,
        id: u64,
        request: &RequestId,
        field: usize,
        edit: impl FnOnce(&mut keystone_agent::Answer),
        cx: &mut Context<Self>,
    ) {
        self.conversation(id).edit_answer(request, field, edit);
        cx.notify();
    }

    /// Where `session` works. See [`presentation::SessionPlace`].
    pub(crate) fn place(&self, session: u64) -> Option<presentation::SessionPlace> {
        use presentation::SessionPlace;
        Some(match self.session(session)?.context() {
            SessionContext::Recorded { directory, .. } => SessionPlace::Local(directory.clone()),
            SessionContext::Remote { machine, .. } => {
                SessionPlace::Remote(self.machine_label(machine))
            }
            SessionContext::LegacyMissing | SessionContext::Unsupported => SessionPlace::Unusable,
        })
    }

    /// What `session` needs from the user, as its sidebar row says. A turn on another
    /// machine outlives Keystone, so one saved as running is running until its host says
    /// how it ended, rather than cut off by the last quit as a local one is.
    pub(crate) fn attention(
        &self,
        session: &keystone_core::Session,
    ) -> Option<keystone_core::Attention> {
        let live = self.conversations.get(&session.id());
        let elsewhere = self.session_machine(session.id()).is_some()
            && session.last_turn() == Some(keystone_core::LastTurn::Running);
        session.attention(
            live.is_some_and(|c| !c.approvals.is_empty()),
            live.is_some_and(crate::Conversation::awaiting_input),
            live.is_some_and(|c| c.running) || elsewhere,
        )
    }

    /// Leave to read this machine's files for `session`, which only a session working
    /// here has. See [`crate::local_files`].
    pub(crate) fn local_files(&self, session: u64) -> Option<crate::local_files::LocalFiles> {
        crate::local_files::LocalFiles::of(self.session(session)?)
    }

    /// The label of `machine`, or its ID once it is no longer linked.
    pub(crate) fn machine_label(&self, machine: &MachineId) -> String {
        self.store
            .workspace()
            .machine(machine)
            .map_or_else(|| machine.to_string(), |m| m.label().to_owned())
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
                SessionContext::Remote { machine, folder } => {
                    ConversationFolder::Remote(machine.clone(), folder.clone())
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
            ConversationFolder::Remote(..) => Err(REMOTE_LOCAL_FOLDER),
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

/// Where an account's usage is saved: the provider and the account ID, or `default`,
/// after `machine/<id>/` for an account on another machine, whose default login is
/// not this machine's.
fn usage_key(
    machine: Option<&MachineId>,
    provider: Provider,
    account: Option<&AccountId>,
) -> String {
    let provider = match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    let machine = machine.map_or_else(String::new, |id| format!("machine/{id}/"));
    format!(
        "{machine}{provider}/{}",
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
