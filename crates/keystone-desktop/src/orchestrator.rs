use super::*;
use keystone_agent::mcp::{Caller, HandlerError, Server, mode_allowed};
use keystone_core::{DeliveryState, WorktreeContext, worktree};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

type Reply = Result<Value, String>;

struct Request {
    caller: Caller,
    tool: String,
    arguments: Value,
    deadline: Instant,
    reply: Sender<Result<Value, HandlerError>>,
    /// For a launch that names a worktree, its git work's result, found off the UI thread.
    prepared: Option<Prepared>,
}

/// The worktree a launch works in, or why it may not.
type Prepared = Result<Option<WorktreeContext>, String>;

/// Whether a caller working in `project`, in its worktree `own` if any, may reach a
/// session in `directory`, in its worktree `theirs` if any: the same folder, or within
/// one repository its root and its worktrees.
fn reachable(
    project: &WorkingDirectory,
    own: Option<&WorktreeContext>,
    directory: &WorkingDirectory,
    theirs: Option<&WorktreeContext>,
) -> bool {
    directory == project
        || theirs.map_or(directory, |w| &w.repository) == own.map_or(project, |w| &w.repository)
}

const LAUNCH_FOLDER_REFUSED: &str =
    "Launch directory must be the caller's project or a worktree of its repository";

/// The worktree a launch from `project` works in. `cwd` may name a registered worktree of
/// the caller's repository; `name` creates one, only for a caller that itself works in a
/// worktree. Without either, a child shares the caller's folder. Runs git.
fn launch_worktree(
    project: &WorkingDirectory,
    own: Option<WorktreeContext>,
    cwd: Option<&str>,
    name: Option<&str>,
) -> Prepared {
    match (cwd, name) {
        (Some(_), Some(_)) => Err("Give cwd or worktree, not both".into()),
        (None, Some(name)) => {
            let own =
                own.ok_or("Only a session working in a worktree may create another worktree")?;
            if !worktree::is_intact(&own) {
                return Err(
                    "The caller's worktree folder is missing or no longer its worktree".into(),
                );
            }
            let created = worktree::create(own.directory.path(), name, None)
                .map_err(|e| format!("Worktree not created: {e}"))?;
            Ok(Some(created))
        }
        (None, None) => Ok(own),
        (Some(cwd), None) if cwd == project.as_str() => Ok(own),
        (Some(cwd), None) => {
            let repository = own.as_ref().map_or(project, |w| &w.repository);
            match worktree::inspect(repository.path(), std::path::Path::new(cwd)) {
                Ok(found) if &found.repository == repository => Ok(Some(found)),
                _ => Err(LAUNCH_FOLDER_REFUSED.into()),
            }
        }
    }
}

trait Records {
    fn lookup(&self, caller: u64, key: &str, input: &[u8; 32]) -> Result<Option<Reply>, String>;
    fn has_capacity(&self) -> bool;
    fn record(&mut self, caller: u64, key: String, input: [u8; 32], reply: Reply);
}

#[derive(Default)]
struct MemoryRecords {
    requests: HashMap<(u64, String), ([u8; 32], Reply)>,
}

impl Records for MemoryRecords {
    fn lookup(&self, caller: u64, key: &str, input: &[u8; 32]) -> Result<Option<Reply>, String> {
        match self.requests.get(&(caller, key.to_owned())) {
            Some((original, reply)) if original == input => Ok(Some(reply.clone())),
            Some(_) => Err("client_request_id was already used for a different request".into()),
            None => Ok(None),
        }
    }

    fn has_capacity(&self) -> bool {
        self.requests.len() < 4096
    }

    fn record(&mut self, caller: u64, key: String, input: [u8; 32], reply: Reply) {
        self.requests.insert((caller, key), (input, reply));
    }
}

pub(super) struct Orchestrator {
    pub(super) server: Server,
    requests: Receiver<Request>,
    records: Box<dyn Records>,
    draining: bool,
    /// The prepared worktree of the launch being answered.
    prepared: Option<Prepared>,
    /// Launches whose git work is running, by caller and request ID.
    preparing: HashSet<(u64, String)>,
    #[cfg(test)]
    last_caller: Option<Caller>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    provider: String,
    account: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    cwd: Option<String>,
    worktree: Option<String>,
    message: String,
    #[serde(rename = "client_request_id")]
    _client_request_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendMessage {
    session: u64,
    message: String,
    #[serde(rename = "client_request_id")]
    _client_request_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    session: u64,
    after: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    session: u64,
    timeout: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Interrupt {
    session: u64,
}

fn decode<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments)
        .map_err(|_| "Invalid arguments: use the tool's input schema".into())
}

fn message(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > 256 * 1024 {
        return Err("message must contain 1 to 262144 bytes of nonempty text".into());
    }
    Ok(())
}

fn mode(value: &str) -> Result<ApprovalMode, String> {
    ApprovalMode::ALL
        .into_iter()
        .find(|mode| mode.slug() == value)
        .ok_or_else(|| "Unknown approval mode".into())
}

impl AppState {
    pub(super) fn ensure_orchestrator(&mut self) -> Result<(), String> {
        if self.orchestrator.is_some() {
            return Ok(());
        }
        let (tx, requests) = mpsc::sync_channel(32);
        let wake = self.wake.clone();
        let server = Server::start(Arc::new(move |caller, tool, arguments, budget| {
            let (reply, received) = mpsc::channel();
            tx.try_send(Request {
                caller,
                tool: tool.to_owned(),
                arguments,
                deadline: Instant::now() + budget,
                reply,
                prepared: None,
            })
            .map_err(|_| HandlerError::from("Keystone is busy or shutting down"))?;
            wake();
            received.recv_timeout(budget).map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => HandlerError::Deadline,
                mpsc::RecvTimeoutError::Disconnected => {
                    HandlerError::from("Keystone is shutting down")
                }
            })?
        }))
        .map_err(|error| format!("Could not start the local orchestrator: {error}"))?;
        let issuer = server.issuer();
        for connection in self
            .codex
            .values()
            .map(|c| c as &dyn Connection)
            .chain(self.claude.values().map(|c| c as &dyn Connection))
        {
            connection.set_mcp(issuer.clone());
        }
        self.orchestrator = Some(Orchestrator {
            server,
            requests,
            records: Box::<MemoryRecords>::default(),
            draining: false,
            prepared: None,
            preparing: HashSet::new(),
            #[cfg(test)]
            last_caller: None,
        });
        Ok(())
    }

    pub(super) fn pump_orchestrator(&mut self, cx: &mut Context<Self>) {
        let Some(orchestrator) = &mut self.orchestrator else {
            return;
        };
        if orchestrator.draining {
            return;
        }
        orchestrator.draining = true;
        let requests: Vec<_> = orchestrator.requests.try_iter().take(32).collect();
        for request in requests {
            match self.launch_job(&request) {
                Some(Err(busy)) => {
                    let _ = request.reply.send(Err(HandlerError::from(busy)));
                }
                Some(Ok((project, own))) => {
                    let (cwd, name) = (
                        request.arguments["cwd"].as_str().map(str::to_owned),
                        request.arguments["worktree"].as_str().map(str::to_owned),
                    );
                    cx.spawn(async move |this, cx| {
                        let prepared = cx
                            .background_spawn(async move {
                                launch_worktree(&project, own, cwd.as_deref(), name.as_deref())
                            })
                            .await;
                        let _ = this.update(cx, |state, cx| {
                            state.answer_request(
                                Request {
                                    prepared: Some(prepared),
                                    ..request
                                },
                                cx,
                            )
                        });
                    })
                    .detach();
                }
                None => self.answer_request(request, cx),
            }
        }
        if let Some(orchestrator) = &mut self.orchestrator {
            orchestrator.draining = false;
        }
    }

    /// The caller's folder and worktree when `request` is a valid, new launch whose
    /// worktree needs git, which then runs off the UI thread, or why it must wait.
    /// Anything else, including a request already answered, is answered at once.
    fn launch_job(
        &mut self,
        request: &Request,
    ) -> Option<Result<(WorkingDirectory, Option<WorktreeContext>), &'static str>> {
        if request.tool != "session_launch"
            || request.prepared.is_some()
            || Instant::now() >= request.deadline
        {
            return None;
        }
        let cwd = request.arguments["cwd"].as_str();
        let name = request.arguments["worktree"].as_str();
        let (project, ceiling) = self.caller(request.caller).ok()?;
        if name.is_none() && cwd.is_none_or(|cwd| cwd == project.as_str()) {
            return None;
        }
        self.validate_launch(request.arguments.clone(), ceiling)
            .ok()?;
        let key = request.arguments["client_request_id"]
            .as_str()
            .filter(|key| !key.is_empty() && key.len() <= 128)?;
        let input: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&json!({"tool": request.tool, "arguments": request.arguments}))
                .unwrap(),
        )
        .into();
        let orchestrator = self.orchestrator.as_mut()?;
        if !matches!(
            orchestrator
                .records
                .lookup(request.caller.session, key, &input),
            Ok(None)
        ) || !orchestrator.records.has_capacity()
        {
            return None;
        }
        if !orchestrator
            .preparing
            .insert((request.caller.session, key.to_owned()))
        {
            return Some(Err("busy: this launch is still being prepared"));
        }
        let own = self
            .session(request.caller.session)
            .and_then(|s| s.worktree().cloned());
        Some(Ok((project, own)))
    }

    /// A launch that created a worktree completes even past its deadline, so the
    /// worktree gets its session and a retry finds the recorded answer.
    fn answer_request(&mut self, request: Request, cx: &mut Context<Self>) {
        let created = match (&request.prepared, request.arguments["worktree"].is_string()) {
            (Some(Ok(Some(worktree))), true) => Some(worktree.clone()),
            _ => None,
        };
        if request.prepared.is_some()
            && let (Some(orchestrator), Some(key)) = (
                &mut self.orchestrator,
                request.arguments["client_request_id"].as_str(),
            )
        {
            orchestrator
                .preparing
                .remove(&(request.caller.session, key.to_owned()));
        }
        let reply = if created.is_none() && Instant::now() >= request.deadline {
            Err(HandlerError::Deadline)
        } else {
            if let Some(orchestrator) = &mut self.orchestrator {
                orchestrator.prepared = request.prepared;
            }
            let reply = self
                .orchestrate(request.caller, &request.tool, request.arguments, cx)
                .map_err(|error| match &created {
                    Some(worktree) if !error.contains("was created and is kept") => format!(
                        "{error} The worktree {} on branch {} was created and is kept.",
                        worktree.directory.as_str(),
                        worktree.branch
                    ),
                    _ => error,
                })
                .map_err(HandlerError::from);
            if let Some(orchestrator) = &mut self.orchestrator {
                orchestrator.prepared = None;
            }
            reply
        };
        let _ = request.reply.send(reply);
    }

    /// A launch's arguments, checked before anything is created.
    fn validate_launch(
        &self,
        arguments: Value,
        ceiling: ApprovalMode,
    ) -> Result<(Launch, Provider, Option<AccountId>, ApprovalMode), String> {
        let launch: Launch = decode(arguments)?;
        message(&launch.message)?;
        let provider = match launch.provider.as_str() {
            "claude" => Provider::Claude,
            "codex" => Provider::Codex,
            _ => return Err("Unknown provider".into()),
        };
        let account = launch_account(&self.store, provider, launch.account.as_deref())?;
        let selected_mode = launch
            .mode
            .as_deref()
            .map(mode)
            .transpose()?
            .unwrap_or(ceiling);
        if !mode_allowed(selected_mode, ceiling) {
            return Err("Target approval mode exceeds the caller's ceiling".into());
        }
        if launch
            .model
            .as_ref()
            .is_some_and(|m| m.is_empty() || m.len() > 256)
        {
            return Err("model must contain 1 to 256 bytes".into());
        }
        Ok((launch, provider, account, selected_mode))
    }

    fn caller(&self, caller: Caller) -> Result<(WorkingDirectory, ApprovalMode), String> {
        if self.phase != Phase::Running
            || !self
                .orchestrator
                .as_ref()
                .is_some_and(|o| o.server.issuer().is_live(caller))
        {
            return Err("Caller process is no longer live".into());
        }
        let session = self.session(caller.session).ok_or("Unknown caller")?;
        if !self
            .started(caller.session)
            .is_some_and(|connection| connection.is_running(caller.session))
        {
            return Err("Caller must have a running turn".into());
        }
        let SessionContext::Recorded { directory, .. } = session.context() else {
            return Err("Caller has no supported project directory".into());
        };
        Ok((directory.clone(), session.approval_mode()))
    }

    /// The caller may reach a session in its own folder, and within one repository, the
    /// sessions in that repository's root and its worktrees.
    fn target(
        &self,
        id: u64,
        caller: u64,
        project: &WorkingDirectory,
    ) -> Result<ApprovalMode, String> {
        let session = self.session(id).ok_or("Unknown target session")?;
        let SessionContext::Recorded { directory, .. } = session.context() else {
            return Err("Target must belong to the caller's project".into());
        };
        let own = self.session(caller).and_then(|s| s.worktree());
        if reachable(project, own, directory, session.worktree()) {
            Ok(session.approval_mode())
        } else {
            Err("Target must belong to the caller's project".into())
        }
    }

    fn orchestrate(
        &mut self,
        caller: Caller,
        tool: &str,
        arguments: Value,
        cx: &mut Context<Self>,
    ) -> Reply {
        let (project, ceiling) = self.caller(caller)?;
        #[cfg(test)]
        {
            self.orchestrator.as_mut().unwrap().last_caller = Some(caller);
        }
        let input: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&json!({"tool": tool, "arguments": arguments})).unwrap(),
        )
        .into();
        let key = if matches!(tool, "session_launch" | "session_send") {
            let key = arguments["client_request_id"]
                .as_str()
                .filter(|key| !key.is_empty() && key.len() <= 128)
                .ok_or("client_request_id must contain 1 to 128 bytes")?;
            let orchestrator = self.orchestrator.as_ref().unwrap();
            if orchestrator
                .preparing
                .contains(&(caller.session, key.to_owned()))
            {
                return Err("busy: this launch is still being prepared".into());
            }
            let records = &orchestrator.records;
            if let Some(reply) = records.lookup(caller.session, key, &input)? {
                return reply;
            }
            if !records.has_capacity() {
                return Err("This app run's request limit was reached; restart Keystone".into());
            }
            Some(key.to_owned())
        } else {
            None
        };
        let reply = self.orchestrate_once(caller, &project, ceiling, tool, arguments, cx);
        if let Some(key) = key {
            self.orchestrator.as_mut().unwrap().records.record(
                caller.session,
                key,
                input,
                reply.clone(),
            );
        }
        reply
    }

    fn orchestrate_once(
        &mut self,
        caller: Caller,
        project: &WorkingDirectory,
        ceiling: ApprovalMode,
        tool: &str,
        arguments: Value,
        cx: &mut Context<Self>,
    ) -> Reply {
        match tool {
            "capabilities" => {
                if arguments != json!({}) {
                    return Err("capabilities takes no arguments".into());
                }
                let providers: Vec<_> = [Provider::Claude, Provider::Codex]
                    .into_iter()
                    .map(|provider| {
                        self.request_catalog(caller.session, provider, cx);
                        let (models, status) = match self.catalogs.choices(&CatalogKey {
                            provider,
                            account: self.catalog_account(caller.session, provider),
                            directory: project.clone(),
                        }) {
                            Choices::Ready(models) => (
                                models.iter().map(|m| json!({"id":m.model,"label":m.label})).collect::<Vec<_>>(),
                                "ready",
                            ),
                            Choices::Failed(_) => (Vec::new(), "unavailable"),
                            _ => (Vec::new(), "loading"),
                        };
                        let accounts: Vec<_> = std::iter::once(json!({"id":AccountId::DEFAULT,"label":"Default"}))
                            .chain(
                                self.store
                                    .workspace()
                                    .accounts()
                                    .iter()
                                    .filter(|a| a.provider() == provider)
                                    .map(|a| json!({"id":a.id().as_str(),"label":a.label()})),
                            )
                            .collect();
                        json!({"provider": match provider {Provider::Claude=>"claude", Provider::Codex=>"codex"},
                            "accounts":accounts, "models":models,"catalog":status})
                    })
                    .collect();
                Ok(
                    json!({"providers":providers,"project":project.as_str(),"mode_ceiling":ceiling.slug(),
                    "provider_settings":"inherits the provider's own setting; no Ask ceiling",
                    "send":"steer-only; idle or unsteerable targets return busy", "wait_max_seconds":50,
                    "results":"when a session you launched ends its turn, its result starts your next turn once you are idle, unless your session_wait saw it end first"}),
                )
            }
            "session_launch" => {
                let (launch, provider, account, selected_mode) =
                    self.validate_launch(arguments, ceiling)?;
                let prepared = self.orchestrator.as_mut().unwrap().prepared.take();
                let worktree = match prepared {
                    Some(prepared) => prepared?,
                    None => launch_worktree(
                        project,
                        self.session(caller.session)
                            .and_then(|s| s.worktree().cloned()),
                        launch.cwd.as_deref(),
                        launch.worktree.as_deref(),
                    )?,
                };
                let id = match &worktree {
                    Some(worktree) => self.create_worktree_unselected(
                        provider,
                        worktree,
                        now_ms(),
                        caller.session,
                        cx,
                    ),
                    None => self.create_unselected(provider, project, now_ms(), caller.session, cx),
                }
                .map_err(|e| match (&worktree, &launch.worktree) {
                    (Some(worktree), Some(_)) => format!(
                        "{e} The worktree {} on branch {} was created and is kept.",
                        worktree.directory.as_str(),
                        worktree.branch
                    ),
                    _ => e.to_string(),
                })?
                .value;
                let setup = (|| -> Result<(), String> {
                    self.set_provider_account(id, provider, account, cx)
                        .map_err(|e| e.to_string())?;
                    self.set_approval_mode(id, selected_mode, cx)
                        .map_err(|e| e.to_string())?;
                    self.set_model(id, launch.model, cx)
                        .map_err(|e| e.to_string())?;
                    Ok(())
                })();
                if let Err(error) = setup {
                    self.persist(PendingSave::Delivery(id, DeliveryState::Unsent));
                    return Ok(
                        json!({"session":id,"sent":false,"error":error,"created_by":{"session":caller.session}}),
                    );
                }
                let sent = self.send_message_from(
                    ComposerDraft {
                        session: id,
                        text: launch.message,
                        attachments: Vec::new(),
                    },
                    Some(caller),
                    SendKind::Draft,
                    cx,
                );
                if !sent {
                    self.persist(PendingSave::Delivery(id, DeliveryState::Unsent));
                }
                Ok(json!({"session":id,"sent":sent,"created_by":{"session":caller.session}}))
            }
            "session_send" => {
                let send: SendMessage = decode(arguments)?;
                message(&send.message)?;
                let target_mode = self.target(send.session, caller.session, project)?;
                if !mode_allowed(target_mode, ceiling) {
                    return Err("Target approval mode exceeds the caller's ceiling".into());
                }
                self.provider(send.session).ok_or("Unknown target")?;
                let connection = self.started(send.session).ok_or("busy: target is idle")?;
                connection
                    .steer_from(send.session, send.message.clone(), caller)
                    .map_err(str::to_owned)?;
                self.conversation(send.session)
                    .push_sent(&send.message, Vec::new());
                self.commit(send.session);
                self.persist(PendingSave::Message(send.session, send.message, now_ms()));
                self.refresh(cx);
                Ok(json!({"session":send.session,"sent":true,"delivery":"steer-requested"}))
            }
            "session_read" => {
                let read: Read = decode(arguments)?;
                self.target(read.session, caller.session, project)?;
                let handed_off_from = self
                    .session(read.session)
                    .and_then(|session| session.handed_off_from())
                    .map(|session| json!({"session":session}));
                let conversation = self.conversation(read.session);
                if conversation.is_loading() {
                    return Ok(
                        json!({"session":read.session,"loading":true,"running":conversation.running,"rows":[],"next":read.after,
                        "handed_off_from":handed_off_from}),
                    );
                }
                let begin = match read.after {
                    Some(index) => index.checked_add(1).ok_or("after is too large")?,
                    None => 0,
                };
                let limit = read.limit.unwrap_or(50).clamp(1, 200);
                const BUDGET: usize = 256 * 1024;
                let cap = keystone_agent::cap_output;
                let mut rows = Vec::new();
                let mut bytes = 0;
                let mut next = read.after;
                let mut streaming = false;
                for (index, row) in conversation.rows.iter().enumerate().skip(begin).take(limit) {
                    let mut shown_row = match row {
                        Row::Item(item) => {
                            let mut shown = item.clone();
                            shown.text = cap(&item.text);
                            if let ItemKind::Command { command, .. } = &mut shown.kind {
                                *command = cap(command);
                            }
                            json!({"position":index,"item":shown,"truncated":shown != *item})
                        }
                        Row::TurnEnded { status, error } => {
                            let shown = error.as_deref().map(cap);
                            json!({"position":index,"turn_ended":status,"error":shown,
                            "truncated":shown.as_deref() != error.as_deref()})
                        }
                        Row::Decision { action, allowed } => {
                            let shown = cap(action);
                            json!({"position":index,"decision":shown,"allowed":allowed,
                            "truncated":shown != *action})
                        }
                        Row::Recovery(report) => {
                            json!({"position":index,"recovery":report,"truncated":false})
                        }
                        Row::Attachments(attachments) => json!({"position":index,
                            "attachments":attachments.iter().map(|a| json!({
                                "name":a.name,"media_type":a.media_type,"sha256":a.sha256
                            })).collect::<Vec<_>>(),"truncated":false}),
                        // What the user did in it, not how it is drawn.
                        Row::Surface(surface) => json!({"position":index,"surface":{
                            "call":surface.call,"surface_id":surface.surface_id,
                            "status":surface.status,"data":surface.data,
                            "last_action":surface.last_action},"truncated":false}),
                    };
                    let mut size = serde_json::to_vec(&shown_row).unwrap().len();
                    if size > BUDGET {
                        shown_row = json!({"position":index,"omitted_bytes":size,"truncated":true});
                        size = serde_json::to_vec(&shown_row).unwrap().len();
                    }
                    if !rows.is_empty() && bytes + size > BUDGET {
                        break;
                    }
                    bytes += size;
                    streaming |= matches!(row, Row::Item(item) if item.status == keystone_agent::ItemStatus::InProgress);
                    if !streaming {
                        next = Some(index);
                    }
                    rows.push(shown_row);
                }
                let running = conversation.running;
                let total = conversation.rows.len();
                let error = conversation.error.as_deref().map(cap);
                let load_error = conversation.load_error.as_deref().map(cap);
                let save_error = conversation.save_error.as_deref().map(cap);
                let created_by = self
                    .store
                    .workspace()
                    .delivery(read.session)
                    .map(|delivery| delivery.parent());
                Ok(
                    json!({"session":read.session,"loading":false,"running":running,"rows":rows,
                    "next":next,"has_more":begin.saturating_add(rows.len())<total,
                    "error":error,"load_error":load_error,"save_error":save_error,
                    "created_by":created_by.map(|session|json!({"session":session})),
                    "handed_off_from":handed_off_from}),
                )
            }
            "session_wait" => {
                let wait: Wait = decode(arguments)?;
                if wait
                    .timeout
                    .is_some_and(|timeout| !timeout.is_finite() || timeout < 0.)
                {
                    return Err("timeout must be nonnegative seconds".into());
                }
                self.target(wait.session, caller.session, project)?;
                self.provider(wait.session).ok_or("Unknown target")?;
                let running = self
                    .conversations
                    .get(&wait.session)
                    .is_some_and(|c| c.running)
                    || self
                        .started(wait.session)
                        .is_some_and(|c| c.is_running(wait.session));
                let error = self
                    .conversations
                    .get(&wait.session)
                    .and_then(|c| c.error.as_deref())
                    .map(keystone_agent::cap_output);
                // The parent saw its launched session's turn end, so no turn of its own
                // is started for the result.
                // A result still being saved counts as ended too.
                let ended = |state: &DeliveryState| {
                    match state {
                    DeliveryState::Ready { .. } => true,
                    DeliveryState::Working => {
                        self.finished.contains_key(&wait.session)
                            || self.pending.iter().any(|save| {
                                matches!(save, PendingSave::Delivery(child, DeliveryState::Ready { .. }) if *child == wait.session)
                            })
                    }
                    _ => false,
                }
                };
                if !running
                    && self
                        .store
                        .workspace()
                        .delivery(wait.session)
                        .is_some_and(|d| {
                            (d.parent() == caller.session
                                || self.recipient(d.parent()) == caller.session)
                                && ended(d.state())
                        })
                {
                    self.finished.remove(&wait.session);
                    self.persist(PendingSave::Delivery(wait.session, DeliveryState::Observed));
                }
                Ok(json!({"session":wait.session,"running":running,"error":error}))
            }
            "session_interrupt" => {
                let interrupt: Interrupt = decode(arguments)?;
                let target_mode = self.target(interrupt.session, caller.session, project)?;
                if !mode_allowed(target_mode, ceiling) {
                    return Err("Target approval mode exceeds the caller's ceiling".into());
                }
                self.stop_turn_from(interrupt.session, Some(caller), cx)?;
                Ok(json!({"session":interrupt.session,"interrupted":true}))
            }
            _ => Err("Unknown tool".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_records_are_scoped_and_reject_different_retries() {
        let mut records = MemoryRecords::default();
        let input = [1; 32];
        records.record(1, "key".into(), input, Ok(json!({"session":3})));
        assert_eq!(
            records.lookup(1, "key", &input).unwrap(),
            Some(Ok(json!({"session":3})))
        );
        assert_eq!(records.lookup(2, "key", &input).unwrap(), None);
        assert!(records.lookup(1, "key", &[2; 32]).is_err());
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "orchestrator_tests.rs"]
mod integration_tests;

/// The account a launch names: `"default"` or none for the provider's default, else
/// the ID of one of that provider's added accounts, such as `claude-work`.
fn launch_account(
    store: &Store,
    provider: Provider,
    account: Option<&str>,
) -> Result<Option<AccountId>, String> {
    match account {
        None | Some(AccountId::DEFAULT) => Ok(None),
        Some(id) => store
            .workspace()
            .accounts()
            .iter()
            .find(|a| a.provider() == provider && a.id().as_str() == id)
            .map(|a| Some(a.id().clone()))
            .ok_or_else(|| format!("Unknown {provider} account {id}; capabilities lists them")),
    }
}
