use super::*;
use keystone_agent::mcp::{Caller, HandlerError, Server, mode_allowed};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

type Reply = Result<Value, String>;

struct Request {
    caller: Caller,
    tool: String,
    arguments: Value,
    deadline: Instant,
    reply: Sender<Result<Value, HandlerError>>,
}

trait Records {
    fn lookup(&self, caller: u64, key: &str, input: &[u8; 32]) -> Result<Option<Reply>, String>;
    fn has_capacity(&self) -> bool;
    fn record(&mut self, caller: u64, key: String, input: [u8; 32], reply: Reply);
    fn child(&mut self, session: u64, parent: u64);
    fn created_by(&self, session: u64) -> Option<u64>;
}

#[derive(Default)]
struct MemoryRecords {
    requests: HashMap<(u64, String), ([u8; 32], Reply)>,
    parents: HashMap<u64, u64>,
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

    fn child(&mut self, session: u64, parent: u64) {
        self.parents.insert(session, parent);
    }

    fn created_by(&self, session: u64) -> Option<u64> {
        self.parents.get(&session).copied()
    }
}

pub(super) struct Orchestrator {
    pub(super) server: Server,
    requests: Receiver<Request>,
    records: Box<dyn Records>,
    draining: bool,
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
            let reply = if Instant::now() >= request.deadline {
                Err(HandlerError::Deadline)
            } else {
                self.orchestrate(request.caller, &request.tool, request.arguments, cx)
                    .map_err(HandlerError::from)
            };
            let _ = request.reply.send(reply);
        }
        if let Some(orchestrator) = &mut self.orchestrator {
            orchestrator.draining = false;
        }
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

    fn target(&self, id: u64, project: &WorkingDirectory) -> Result<ApprovalMode, String> {
        let session = self.session(id).ok_or("Unknown target session")?;
        match session.context() {
            SessionContext::Recorded { directory, .. } if directory == project => {
                Ok(session.approval_mode())
            }
            _ => Err("Target must belong to the caller's project".into()),
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
            let records = &self.orchestrator.as_ref().unwrap().records;
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
                    "send":"steer-only; idle or unsteerable targets return busy", "wait_max_seconds":50}),
                )
            }
            "session_launch" => {
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
                if let Some(cwd) = launch.cwd
                    && cwd != project.as_str()
                {
                    return Err("Launch directory must be the caller's project".into());
                }
                if launch
                    .model
                    .as_ref()
                    .is_some_and(|m| m.is_empty() || m.len() > 256)
                {
                    return Err("model must contain 1 to 256 bytes".into());
                }
                let selected = self.store.workspace().selected();
                let id = self
                    .create(provider, project, now_ms(), cx)
                    .map_err(|e| e.to_string())?
                    .value;
                self.orchestrator
                    .as_mut()
                    .unwrap()
                    .records
                    .child(id, caller.session);
                let setup = (|| -> Result<(), String> {
                    if let Some(selected) = selected {
                        self.select(selected, cx).map_err(|e| e.to_string())?;
                    }
                    self.set_provider_account(id, provider, account, cx)
                        .map_err(|e| e.to_string())?;
                    self.set_approval_mode(id, selected_mode, cx)
                        .map_err(|e| e.to_string())?;
                    self.set_model(id, launch.model, cx)
                        .map_err(|e| e.to_string())?;
                    Ok(())
                })();
                if let Err(error) = setup {
                    return Ok(
                        json!({"session":id,"sent":false,"error":error,"created_by":{"session":caller.session}}),
                    );
                }
                let sent = self.send_message_from(
                    ComposerDraft {
                        session: id,
                        text: launch.message,
                    },
                    Some(caller),
                    cx,
                );
                Ok(json!({"session":id,"sent":sent,"created_by":{"session":caller.session}}))
            }
            "session_send" => {
                let send: SendMessage = decode(arguments)?;
                message(&send.message)?;
                let target_mode = self.target(send.session, project)?;
                if !mode_allowed(target_mode, ceiling) {
                    return Err("Target approval mode exceeds the caller's ceiling".into());
                }
                self.provider(send.session).ok_or("Unknown target")?;
                let connection = self.started(send.session).ok_or("busy: target is idle")?;
                connection
                    .steer_from(send.session, send.message.clone(), caller)
                    .map_err(str::to_owned)?;
                self.conversation(send.session).push_local(&send.message);
                self.commit(send.session);
                self.persist(PendingSave::Message(send.session, send.message, now_ms()));
                self.refresh(cx);
                Ok(json!({"session":send.session,"sent":true,"delivery":"steer-requested"}))
            }
            "session_read" => {
                let read: Read = decode(arguments)?;
                self.target(read.session, project)?;
                let conversation = self.conversation(read.session);
                if conversation.is_loading() {
                    return Ok(
                        json!({"session":read.session,"loading":true,"running":conversation.running,"rows":[],"next":read.after}),
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
                    .orchestrator
                    .as_ref()
                    .unwrap()
                    .records
                    .created_by(read.session);
                Ok(
                    json!({"session":read.session,"loading":false,"running":running,"rows":rows,
                    "next":next,"has_more":begin.saturating_add(rows.len())<total,
                    "error":error,"load_error":load_error,"save_error":save_error,
                    "created_by":created_by.map(|session|json!({"session":session}))}),
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
                self.target(wait.session, project)?;
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
                Ok(json!({"session":wait.session,"running":running,"error":error}))
            }
            "session_interrupt" => {
                let interrupt: Interrupt = decode(arguments)?;
                let target_mode = self.target(interrupt.session, project)?;
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
        records.child(3, 1);
        assert_eq!(records.created_by(3), Some(1));
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
