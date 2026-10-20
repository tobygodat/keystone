use crate::presentation::ConversationFolder;
use crate::*;
use gpui::EventEmitter;
use keystone_core::SessionContext;
use keystone_core::{Applied, ProjectId, SavedComposition, WorkingDirectory};

#[cfg(all(test, feature = "ui-tests"))]
#[path = "app_state_tests.rs"]
mod tests;

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
    pub selected: bool,
}
pub(crate) struct AppState {
    store: Store,
    pub(crate) error: Option<String>,
    pub(crate) shutdown_error: Option<String>,
    codex: Option<CodexConnection>,
    claude: Option<ClaudeConnection>,
    conversations: HashMap<u64, Conversation>,
    catalogs: Catalogs,
    logs: Logs,
    log_losses: std::collections::BTreeMap<u64, String>,
    wake: Wake,
    changes: RuntimeChanged,
    pub(crate) phase: Phase,
    pending: std::collections::VecDeque<PendingSave>,
    #[cfg(test)]
    pub(crate) save_calls: usize,
}
impl EventEmitter<RuntimeChanged> for AppState {}
impl AppState {
    pub(crate) fn log_losses(&self) -> impl Iterator<Item = &String> {
        self.log_losses.values()
    }

    fn record_loss(&mut self, session: u64, lost: u64) {
        if lost > 0 {
            let lost = lost.max(self.logs.lost_entries(session));
            self.log_losses.insert(session, format!("Session {session}: {lost} entries were lost (log budget exceeded). Further log writes are disabled until restart."));
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
    #[cfg(test)]
    pub(crate) fn test_codex(&self) -> &Option<CodexConnection> {
        &self.codex
    }
    #[cfg(test)]
    pub(crate) fn test_set_codex(&mut self, value: Option<CodexConnection>) {
        self.codex = value;
    }
    #[cfg(test)]
    pub(crate) fn test_claude(&self) -> &Option<ClaudeConnection> {
        &self.claude
    }
    #[cfg(test)]
    pub(crate) fn test_set_claude(&mut self, value: Option<ClaudeConnection>) {
        self.claude = value;
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

    pub(crate) fn save(
        &mut self,
        draft: Option<ComposerDraft>,
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
        if let Some(draft) = draft {
            match self.store.set_draft(draft.session, draft.text) {
                Ok(_) => self.pending.retain(
                    |update| !matches!(update, PendingSave::Draft(id, _) if *id == draft.session),
                ),
                Err(error) => errors.push(format!("Draft not saved. {error}")),
            }
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
        drop(self.codex.take());
        drop(self.claude.take());
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
            && let Some((provider, approval)) = self.approval(id)
            && let Some(connection) = self.started(provider)
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

    pub(crate) fn set_project_layout(
        &mut self,
        project: &ProjectId,
        layout: Option<SavedComposition>,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.set_project_layout(project, layout);
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
            && self
                .conversations
                .get(&id)
                .is_some_and(Conversation::awaiting_approval)
        {
            return Err(std::io::Error::other(
                "Answer the approval request before settling this session.",
            )
            .into());
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
        id: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<()>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.select(id);
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

    pub(crate) fn create(
        &mut self,
        provider: Provider,
        directory: &WorkingDirectory,
        now: u64,
        cx: &mut Context<Self>,
    ) -> Result<Applied<u64>, Box<dyn std::error::Error>> {
        if self.phase == Phase::Exiting {
            return Err(std::io::Error::other("Keystone is exiting.").into());
        }

        let result = self.store.create(provider, directory, now);
        self.changes.sidebar = true;
        self.publish(cx);
        result
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
        let sessions_dir = store
            .path()
            .parent()
            .unwrap_or(Path::new("."))
            .join("sessions");
        Self {
            store,
            error: None,
            shutdown_error: None,
            codex: None,
            claude: None,
            conversations: HashMap::new(),
            catalogs: Catalogs::default(),
            logs: Logs::new(sessions_dir, wake.clone()),
            log_losses: Default::default(),
            wake,
            changes: RuntimeChanged::default(),
            phase: Phase::Running,
            pending: Default::default(),
            #[cfg(test)]
            save_calls: 0,
        }
    }
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }
    pub(crate) fn publish(&mut self, cx: &mut Context<Self>) {
        cx.emit(std::mem::take(&mut self.changes));
        cx.notify();
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.changes.sidebar = true;
        self.publish(cx);
    }

    pub(crate) fn selected_session(&self) -> Option<&keystone_core::Session> {
        let workspace = self.store.workspace();
        let id = workspace.selected()?;
        workspace.sessions().iter().find(|s| s.id() == id)
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

    pub(crate) fn catalog_key(&self, session: u64, provider: Provider) -> Option<CatalogKey> {
        match self.conversation_folder(session)? {
            presentation::ConversationFolder::Recorded(directory) => Some(CatalogKey {
                provider,
                directory,
            }),
            _ => None,
        }
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
        if self.catalogs.ask(key, session) {
            self.connection(provider)
                .request_catalog(session, directory.as_str());
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

    pub(crate) fn commit(&mut self, session: u64) {
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return;
        };
        let lost = match self.logs.append(session, conversation.take_writes()) {
            Ok(()) => 0,
            Err(_) => self.logs.lost_entries(session),
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
            selected: self.store.workspace().selected() == Some(session),
        });
        self.record_loss(session, lost);
    }

    pub(crate) fn pump(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Exiting {
            return;
        }
        let reports = self.logs.drain();
        let mut events = Vec::new();
        // A catalog belongs to the connection it came from, whatever provider the
        // asking session has since moved to.
        for (provider, connection) in [
            (
                Provider::Codex,
                self.codex.as_ref().map(|c| c as &dyn Connection),
            ),
            (
                Provider::Claude,
                self.claude.as_ref().map(|c| c as &dyn Connection),
            ),
        ] {
            let Some(connection) = connection else {
                continue;
            };
            for (session, event) in connection.drain() {
                match event {
                    Event::Catalog(catalog) => {
                        self.catalogs.apply(provider, session, catalog);
                    }
                    Event::ProviderNotice { .. } if self.provider(session) != Some(provider) => {}
                    event => events.push((session, event)),
                }
            }
        }
        self.receive(reports, events, cx);
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
        for report in reports {
            let session = match report {
                Report::Loaded { session, entries } => {
                    if let Some(conversation) = self.conversations.get_mut(&session) {
                        conversation.restore(entries);
                    }
                    // Events held during the load, such as an approval or a turn's
                    // end, apply only now.
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
            let failed = turn_failed(&event);
            match (&event, failed) {
                (Event::TurnStarted, _) => self.persist(PendingSave::TurnStart(session)),
                (_, Some(failed)) => self.persist(PendingSave::TurnEnd(session, failed, now_ms())),
                _ => {}
            }
            sidebar_changed |= failed.is_some()
                || matches!(
                    event,
                    Event::ApprovalRequested(_) | Event::ApprovalResolved { .. }
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

        // Streamed text redraws only this view. The sidebar refreshes when a row changes.
        if sidebar_changed {
            self.refresh(cx);
        } else {
            self.publish(cx);
        }
    }

    fn connection(&mut self, provider: Provider) -> &dyn Connection {
        let wake = self.wake.clone();
        match provider {
            Provider::Codex => self.codex.get_or_insert_with(|| {
                CodexConnection::new(
                    std::env::var_os("KEYSTONE_CODEX_PATH").map(PathBuf::from),
                    env!("CARGO_PKG_VERSION"),
                    wake,
                )
            }),
            Provider::Claude => self.claude.get_or_insert_with(|| {
                ClaudeConnection::new(
                    std::env::var_os("KEYSTONE_CLAUDE_PATH").map(PathBuf::from),
                    wake,
                )
            }),
        }
    }

    fn started(&self, provider: Provider) -> Option<&dyn Connection> {
        match provider {
            Provider::Codex => self.codex.as_ref().map(|c| c as &dyn Connection),
            Provider::Claude => self.claude.as_ref().map(|c| c as &dyn Connection),
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
        let result = self.logs.flush_pending();
        self.receive(self.logs.drain(), Vec::new(), cx);
        result.map_err(|error| format!("Could not save conversation logs before closing. {error}"))
    }

    pub(crate) fn send_message(&mut self, draft: ComposerDraft, cx: &mut Context<Self>) -> bool {
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
        let Some(directory) = self.dispatch_directory(id, cx) else {
            return false;
        };
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
        let sent = self.connection(provider).send(
            id,
            cwd,
            resume.as_deref(),
            session.model(),
            approval,
            text.clone(),
        );
        if let Err(reason) = sent {
            // Shown in the session's transcript, where a draft save cannot clear it.
            self.conversation(id).error = Some(reason.to_owned());
            self.commit(id);
            self.publish(cx);
            return false;
        }
        self.conversation(id).push_local(&text);
        self.commit(id);
        self.persist(PendingSave::Message(id, text.clone(), now_ms()));
        self.persist(PendingSave::Draft(id, String::new()));
        self.refresh(cx);
        true
    }

    pub(crate) fn stop_turn(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.phase != Phase::Running {
            return;
        }
        if let Some(connection) = self.provider(id).and_then(|p| self.started(p)) {
            connection.command(Command::Interrupt { session: id });
        }
        self.publish(cx);
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
        if let Some(connection) = self.provider(id).and_then(|p| self.started(p)) {
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

    pub(crate) fn dispatch_directory(
        &mut self,
        session: u64,
        cx: &mut Context<Self>,
    ) -> Option<WorkingDirectory> {
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
