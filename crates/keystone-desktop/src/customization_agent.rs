//! Asks Claude Code or Codex to interpret a sidebar request. Each request runs in a
//! fresh provider session, read only, in an empty temporary directory, with every
//! approval it asks for denied. One worker thread owns the connection, so starting
//! and ending provider processes never blocks the UI.

use keystone_agent::{
    ApprovalState, ClaudeConnection, CodexConnection, Command, Connection, Event, ItemKind,
    ItemStatus, TurnStatus, Wake,
};
use keystone_core::{ApprovalMode, Provider, SidebarLayout, SidebarSide, SidebarWidth};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const OUTPUT_LIMIT: usize = 16 * 1024;
pub const REASON_LIMIT: usize = 256;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

const SESSION: u64 = 1;

#[derive(Clone, Debug, PartialEq)]
pub enum Proposal {
    Change(SidebarLayout),
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Proposal(Proposal),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    pub request: u64,
    pub provider: Provider,
    pub prompt: String,
    pub current: SidebarLayout,
}

enum Message {
    Ask(Ask),
    Cancel(u64),
    Events,
    Shutdown,
}

enum Next {
    Wait,
    Serve(Ask),
    Stop,
}

/// Where to find each provider. `None` looks it up on the `PATH`, as sessions do.
#[derive(Clone, Default)]
pub struct Programs {
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
}

pub struct Agent {
    messages: Sender<Message>,
    results: Arc<Mutex<Vec<(u64, Outcome)>>>,
}

impl Agent {
    /// `wake` runs on the worker whenever a result is ready to [`Agent::drain`].
    pub fn new(programs: Programs, timeout: Duration, wake: Wake) -> Self {
        let (messages, inbox) = mpsc::channel();
        let results: Arc<Mutex<Vec<(u64, Outcome)>>> = Arc::default();
        let worker = Worker {
            programs,
            timeout,
            inbox,
            events: messages.clone(),
            results: results.clone(),
            wake,
        };
        std::thread::Builder::new()
            .name("customize-agent".into())
            .spawn(move || worker.run())
            .expect("spawn customization worker");
        Self { messages, results }
    }

    /// Starts `ask`, ending any request still running. Its result arrives through
    /// [`Agent::drain`] under `ask.request`.
    pub fn ask(&self, ask: Ask) {
        let _ = self.messages.send(Message::Ask(ask));
    }

    /// Ends `request` if it is running. Nothing is reported for it.
    pub fn cancel(&self, request: u64) {
        let _ = self.messages.send(Message::Cancel(request));
    }

    pub fn drain(&self) -> Vec<(u64, Outcome)> {
        std::mem::take(&mut self.results.lock().unwrap())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.messages.send(Message::Shutdown);
    }
}

struct Worker {
    programs: Programs,
    timeout: Duration,
    inbox: Receiver<Message>,
    events: Sender<Message>,
    results: Arc<Mutex<Vec<(u64, Outcome)>>>,
    wake: Wake,
}

impl Worker {
    fn run(self) {
        let mut next = Next::Wait;
        loop {
            let ask = match std::mem::replace(&mut next, Next::Wait) {
                Next::Serve(ask) => ask,
                Next::Stop => return,
                Next::Wait => match self.inbox.recv() {
                    Ok(Message::Ask(ask)) => ask,
                    Ok(Message::Shutdown) | Err(_) => return,
                    Ok(_) => continue,
                },
            };
            next = self.latest_pending(self.serve(ask));
        }
    }

    fn latest_pending(&self, mut next: Next) -> Next {
        while let Ok(message) = self.inbox.try_recv() {
            match (message, &next) {
                (_, Next::Stop) | (Message::Shutdown, _) => return Next::Stop,
                (Message::Ask(ask), _) => next = Next::Serve(ask),
                (Message::Cancel(request), Next::Serve(ask)) if ask.request == request => {
                    next = Next::Wait
                }
                _ => {}
            }
        }
        next
    }

    fn serve(&self, ask: Ask) -> Next {
        let deadline = Instant::now() + self.timeout;
        let directory = match tempfile::Builder::new()
            .prefix("keystone-customize-")
            .tempdir()
        {
            Ok(directory) => directory,
            Err(error) => {
                self.report(
                    ask.request,
                    Outcome::Failed(format!("Could not create a working directory. {error}")),
                );
                return Next::Wait;
            }
        };
        let events = Mutex::new(self.events.clone());
        let wake: Wake = Arc::new(move || {
            let _ = events.lock().unwrap().send(Message::Events);
        });
        let connection: Box<dyn Connection> = match ask.provider {
            Provider::Claude => Box::new(ClaudeConnection::new(self.programs.claude.clone(), wake)),
            Provider::Codex => Box::new(CodexConnection::new(
                self.programs.codex.clone(),
                env!("CARGO_PKG_VERSION"),
                wake,
            )),
        };
        let cwd = directory.path().to_string_lossy().into_owned();
        self.exchange(ask, connection.as_ref(), &cwd, deadline)
    }

    fn exchange(
        &self,
        ask: Ask,
        connection: &dyn Connection,
        cwd: &str,
        deadline: Instant,
    ) -> Next {
        let approval = ApprovalState {
            mode: ApprovalMode::ReadOnly,
            overridden: true,
        };
        if let Err(reason) = connection.send(SESSION, cwd, None, approval, instruction(&ask)) {
            self.report(ask.request, Outcome::Failed(reason.to_owned()));
            return Next::Wait;
        }
        let mut reply = Reply::default();
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(wait) {
                Ok(Message::Events) => {
                    for (_, event) in connection.drain() {
                        if Instant::now() >= deadline {
                            return self.time_out(&ask, connection);
                        }
                        if let Event::ApprovalRequested(approval) = &event {
                            connection.command(Command::Answer {
                                session: SESSION,
                                request: approval.request.clone(),
                                allow: false,
                            });
                        }
                        if let Some(outcome) = reply.apply(event) {
                            self.report(ask.request, outcome);
                            return Next::Wait;
                        }
                    }
                }
                Ok(Message::Cancel(request)) if request == ask.request => {
                    connection.command(Command::Interrupt { session: SESSION });
                    return Next::Wait;
                }
                Ok(Message::Cancel(_)) => {}
                Ok(Message::Ask(replacement)) => {
                    connection.command(Command::Interrupt { session: SESSION });
                    return Next::Serve(replacement);
                }
                Ok(Message::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    connection.command(Command::Interrupt { session: SESSION });
                    return Next::Stop;
                }
                Err(RecvTimeoutError::Timeout) => return self.time_out(&ask, connection),
            }
        }
    }

    fn time_out(&self, ask: &Ask, connection: &dyn Connection) -> Next {
        connection.command(Command::Interrupt { session: SESSION });
        let seconds = self.timeout.as_secs().max(1);
        self.report(
            ask.request,
            Outcome::Failed(format!("No answer within {seconds} seconds.")),
        );
        Next::Wait
    }

    fn report(&self, request: u64, outcome: Outcome) {
        self.results.lock().unwrap().push((request, outcome));
        (self.wake)();
    }
}

#[derive(Default)]
struct Reply {
    counted: HashMap<String, usize>,
    total: usize,
    last: Option<String>,
}

impl Reply {
    fn count(&mut self, id: String, bytes: usize) -> bool {
        let counted = self.counted.entry(id).or_default();
        self.total += bytes.saturating_sub(*counted);
        *counted = (*counted).max(bytes);
        self.total > OUTPUT_LIMIT
    }

    fn apply(&mut self, event: Event) -> Option<Outcome> {
        let too_long = || {
            Some(Outcome::Failed(format!(
                "The reply was longer than {} KiB.",
                OUTPUT_LIMIT / 1024
            )))
        };
        match event {
            Event::ItemStarted(item) if item.kind == ItemKind::AgentMessage => {
                self.counted.entry(item.id).or_default();
            }
            Event::ItemDelta { item_id, text } => {
                if let Some(&counted) = self.counted.get(&item_id)
                    && self.count(item_id, counted + text.len())
                {
                    return too_long();
                }
            }
            Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage => {
                if self.count(item.id, item.text.len()) {
                    return too_long();
                }
                if item.status == ItemStatus::Completed {
                    self.last = Some(item.text);
                }
            }
            Event::TurnCompleted { status, error, .. } => {
                return Some(match (status, self.last.take()) {
                    (TurnStatus::Completed, Some(text)) => match parse(&text) {
                        Ok(proposal) => Outcome::Proposal(proposal),
                        Err(problem) => {
                            Outcome::Failed(format!("The reply could not be used: {problem}."))
                        }
                    },
                    (TurnStatus::Completed, None) => Outcome::Failed("The reply was empty.".into()),
                    (_, _) => Outcome::Failed(
                        error.unwrap_or_else(|| "The request ended before it was answered.".into()),
                    ),
                });
            }
            Event::Failed { message } => return Some(Outcome::Failed(message)),
            _ => {}
        }
        None
    }
}

fn instruction(ask: &Ask) -> String {
    let (min, max) = (SidebarWidth::MIN, SidebarWidth::MAX);
    format!(
        "You are setting the layout of the Keystone desktop app's session sidebar. \
         Only two things can change: which side of the window it is on (left or right) \
         and its width, a whole number of logical pixels from {min} to {max}. \
         It is now on the {side} at {width} px.\n\n\
         Do not use any tools or read any files. Reply with exactly one JSON object on \
         one line and nothing else: no prose and no code fences.\n\
         If the request can be met by side and width alone, give the complete resulting \
         layout: {{\"kind\":\"change\",\"version\":1,\"side\":\"left\",\"width_px\":302}}\n\
         Otherwise, explain in one short sentence (at most 200 characters): \
         {{\"kind\":\"unsupported\",\"version\":1,\"reason\":\"Only sidebar side and width are supported.\"}}\n\n\
         The request:\n{prompt}",
        side = ask.current.side.label(),
        width = ask.current.width.px(),
        prompt = ask.prompt,
    )
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Wire {
    Change {
        version: u32,
        side: SidebarSide,
        width_px: u16,
    },
    Unsupported {
        version: u32,
        reason: String,
    },
}

/// Reads a reply: exactly one JSON object of a known kind, nothing around it but
/// whitespace, with no unknown fields and every value in range.
pub fn parse(text: &str) -> Result<Proposal, String> {
    let wire =
        serde_json::from_str::<Wire>(text.trim()).map_err(|error| match error.classify() {
            serde_json::error::Category::Data => error.to_string(),
            _ => "it was not a single JSON object".to_owned(),
        })?;
    let version = match &wire {
        Wire::Change { version, .. } | Wire::Unsupported { version, .. } => *version,
    };
    if version != 1 {
        return Err(format!("version {version} is not supported"));
    }
    match wire {
        Wire::Change { side, width_px, .. } => {
            let width = SidebarWidth::new(width_px).ok_or_else(|| {
                format!(
                    "width {width_px} px is outside {}–{} px",
                    SidebarWidth::MIN,
                    SidebarWidth::MAX
                )
            })?;
            Ok(Proposal::Change(SidebarLayout { side, width }))
        }
        Wire::Unsupported { reason, .. } => {
            let reason = reason.trim();
            if reason.is_empty() || reason.chars().count() > REASON_LIMIT {
                return Err(format!("the reason must be 1–{REASON_LIMIT} characters"));
            }
            Ok(Proposal::Unsupported(reason.to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_agent::Item;

    fn layout(side: SidebarSide, width: u16) -> SidebarLayout {
        SidebarLayout {
            side,
            width: SidebarWidth::new(width).unwrap(),
        }
    }

    #[test]
    fn parse_accepts_only_the_exact_contract() {
        assert_eq!(
            parse(r#"{"kind":"change","version":1,"side":"right","width_px":360}"#),
            Ok(Proposal::Change(layout(SidebarSide::Right, 360)))
        );
        assert_eq!(
            parse(" {\"kind\":\"unsupported\",\"version\":1,\"reason\":\"No colors.\"}\n"),
            Ok(Proposal::Unsupported("No colors.".into()))
        );
        let long_reason = format!(
            r#"{{"kind":"unsupported","version":1,"reason":"{}"}}"#,
            "x".repeat(REASON_LIMIT + 1)
        );
        for bad in [
            "Sure! Moving it right.",
            "```json\n{\"kind\":\"change\",\"version\":1,\"side\":\"right\",\"width_px\":360}\n```",
            r#"{"kind":"change","version":1,"side":"right","width_px":360} done"#,
            r#"{"kind":"change","version":1,"side":"right","width_px":360}{"kind":"change"}"#,
            r#"{"kind":"change","version":1,"side":"right","width_px":360,"color":"red"}"#,
            r#"{"kind":"change","version":2,"side":"right","width_px":360}"#,
            r#"{"kind":"change","version":1,"side":"top","width_px":360}"#,
            r#"{"kind":"change","version":1,"side":"right","width_px":900}"#,
            r#"{"kind":"change","version":1,"side":"right","width_px":360.5}"#,
            r#"{"kind":"change","version":1,"side":"right"}"#,
            r#"{"kind":"resize","version":1,"width_px":360}"#,
            r#"{"version":1,"side":"right","width_px":360}"#,
            r#"{"kind":"unsupported","version":1,"reason":"No.","side":"left"}"#,
            r#"{"kind":"unsupported","version":1,"reason":"  "}"#,
            long_reason.as_str(),
            "[]",
            "",
        ] {
            assert!(parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    fn message(id: &str, text: &str, status: ItemStatus) -> Item {
        Item {
            id: id.into(),
            kind: ItemKind::AgentMessage,
            text: text.into(),
            status,
        }
    }

    fn end(status: TurnStatus) -> Event {
        Event::TurnCompleted {
            status,
            error: None,
            stopped: false,
        }
    }

    #[test]
    fn only_the_last_message_of_a_completed_turn_counts() {
        let mut reply = Reply::default();
        assert_eq!(reply.apply(Event::TurnStarted), None);
        assert_eq!(
            reply.apply(Event::ItemCompleted(message(
                "a",
                "Thinking…",
                ItemStatus::Completed
            ))),
            None
        );
        let json = r#"{"kind":"change","version":1,"side":"right","width_px":360}"#;
        reply.apply(Event::ItemStarted(message("b", "", ItemStatus::InProgress)));
        assert_eq!(
            reply.apply(Event::ItemDelta {
                item_id: "b".into(),
                text: json.into()
            }),
            None
        );
        reply.apply(Event::ItemCompleted(message(
            "b",
            json,
            ItemStatus::Completed,
        )));
        assert_eq!(
            reply.apply(end(TurnStatus::Completed)),
            Some(Outcome::Proposal(Proposal::Change(layout(
                SidebarSide::Right,
                360
            ))))
        );

        let mut interrupted = Reply::default();
        interrupted.apply(Event::ItemCompleted(message(
            "b",
            json,
            ItemStatus::Completed,
        )));
        assert!(matches!(
            interrupted.apply(end(TurnStatus::Interrupted)),
            Some(Outcome::Failed(_))
        ));

        let mut prose = Reply::default();
        prose.apply(Event::ItemCompleted(message(
            "c",
            "Moved it.",
            ItemStatus::Completed,
        )));
        assert!(matches!(
            prose.apply(end(TurnStatus::Completed)),
            Some(Outcome::Failed(problem)) if problem.contains("could not be used")
        ));
    }

    #[test]
    fn output_past_the_limit_fails_while_streaming() {
        let mut reply = Reply::default();
        reply.apply(Event::ItemStarted(message("a", "", ItemStatus::InProgress)));
        let chunk = "x".repeat(4096);
        for _ in 0..4 {
            assert_eq!(
                reply.apply(Event::ItemDelta {
                    item_id: "a".into(),
                    text: chunk.clone()
                }),
                None
            );
        }
        assert!(matches!(
            reply.apply(Event::ItemDelta {
                item_id: "a".into(),
                text: "x".into()
            }),
            Some(Outcome::Failed(problem)) if problem.contains("16 KiB")
        ));
    }

    fn too_long(outcome: Option<Outcome>) -> bool {
        matches!(outcome, Some(Outcome::Failed(problem)) if problem.contains("16 KiB"))
    }

    #[test]
    fn output_counts_across_messages_completed_without_deltas() {
        let mut reply = Reply::default();
        let half = "x".repeat(OUTPUT_LIMIT / 2);
        let completed = |id| Event::ItemCompleted(message(id, &half, ItemStatus::Completed));
        assert_eq!(reply.apply(completed("a")), None);
        assert_eq!(reply.apply(completed("b")), None);
        assert!(too_long(reply.apply(Event::ItemCompleted(message(
            "c",
            "x",
            ItemStatus::Completed
        )))));
    }

    #[test]
    fn a_streamed_message_is_not_counted_again_when_it_completes() {
        let mut reply = Reply::default();
        let most = "x".repeat(OUTPUT_LIMIT - 10);
        reply.apply(Event::ItemStarted(message("a", "", ItemStatus::InProgress)));
        reply.apply(Event::ItemDelta {
            item_id: "a".into(),
            text: most[..100].into(),
        });
        assert_eq!(
            reply.apply(Event::ItemCompleted(message(
                "a",
                &most,
                ItemStatus::Completed
            ))),
            None
        );
        assert!(too_long(reply.apply(Event::ItemCompleted(message(
            "b",
            &"x".repeat(11),
            ItemStatus::Completed
        )))));
    }

    struct Late {
        events: Mutex<Vec<(u64, Event)>>,
        inbox: Mutex<Sender<Message>>,
        delay: Duration,
        commands: Mutex<Vec<Command>>,
    }

    impl Connection for Late {
        fn send(
            &self,
            _: u64,
            _: &str,
            _: Option<&str>,
            _: ApprovalState,
            _: String,
        ) -> Result<(), &'static str> {
            std::thread::sleep(self.delay);
            let _ = self.inbox.lock().unwrap().send(Message::Events);
            Ok(())
        }
        fn set_approval(&self, _: u64, _: ApprovalState) {}
        fn command(&self, command: Command) {
            self.commands.lock().unwrap().push(command);
        }
        fn drain(&self) -> Vec<(u64, Event)> {
            std::mem::take(&mut self.events.lock().unwrap())
        }
    }

    #[test]
    fn events_queued_past_the_deadline_time_out() {
        let (events, inbox) = mpsc::channel();
        let worker = Worker {
            programs: Programs::default(),
            timeout: Duration::from_millis(20),
            inbox,
            events: events.clone(),
            results: Arc::default(),
            wake: Arc::new(|| {}),
        };
        let json = r#"{"kind":"change","version":1,"side":"right","width_px":360}"#;
        let connection = Late {
            events: Mutex::new(vec![
                (
                    SESSION,
                    Event::ItemCompleted(message("a", json, ItemStatus::Completed)),
                ),
                (SESSION, end(TurnStatus::Completed)),
            ]),
            inbox: Mutex::new(events),
            delay: Duration::from_millis(60),
            commands: Mutex::default(),
        };
        let ask = Ask {
            request: 7,
            provider: Provider::Claude,
            prompt: "right".into(),
            current: SidebarLayout::default(),
        };
        let deadline = Instant::now() + worker.timeout;
        assert!(matches!(
            worker.exchange(ask, &connection, "/", deadline),
            Next::Wait
        ));
        let results = std::mem::take(&mut *worker.results.lock().unwrap());
        assert!(
            matches!(results.as_slice(), [(7, Outcome::Failed(problem))] if problem.contains("No answer")),
            "{results:?}"
        );
        assert!(matches!(
            connection.commands.lock().unwrap().as_slice(),
            [Command::Interrupt { session: SESSION }]
        ));
    }
}

#[cfg(all(test, feature = "ui-tests"))]
pub(crate) mod tests_support {
    use std::path::{Path, PathBuf};

    pub(crate) fn wrapper(dir: &Path, name: &str, env: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let fake = exe.parent().unwrap().parent().unwrap().join(name);
        assert!(
            fake.exists(),
            "build it with `cargo build -p keystone-agent --bins`"
        );
        let script = dir.join(format!("{name}-wrapper"));
        std::fs::write(
            &script,
            format!("#!/bin/sh\n{env} exec '{}' \"$@\"\n", fake.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod provider_tests {
    use super::tests_support::wrapper;
    use super::*;

    fn agent(programs: Programs, timeout: Duration) -> (Agent, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let wake: Wake = Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        });
        (Agent::new(programs, timeout, wake), rx)
    }

    fn ask(request: u64, provider: Provider) -> Ask {
        Ask {
            request,
            provider,
            prompt: "Put it on the right".into(),
            current: SidebarLayout::default(),
        }
    }

    fn next(agent: &Agent, woken: &mpsc::Receiver<()>) -> Vec<(u64, Outcome)> {
        woken
            .recv_timeout(Duration::from_secs(20))
            .expect("an answer");
        agent.drain()
    }

    const CHANGE: &str = r#"{"kind":"change","version":1,"side":"right","width_px":360}"#;

    #[test]
    fn both_providers_answer_with_a_validated_proposal() {
        let dir = tempfile::tempdir().unwrap();
        let reply =
            format!("KEYSTONE_FAKE_CLAUDE_REPLY='{CHANGE}' KEYSTONE_FAKE_CLAUDE_DELAY_MS=1");
        let claude = wrapper(dir.path(), "keystone-fake-claude", &reply);
        let reply = format!("KEYSTONE_FAKE_CODEX_REPLY='{CHANGE}' KEYSTONE_FAKE_CODEX_DELAY_MS=1");
        let codex = wrapper(dir.path(), "keystone-fake-codex", &reply);
        let (agent, woken) = agent(
            Programs {
                claude: Some(claude),
                codex: Some(codex),
            },
            REQUEST_TIMEOUT,
        );
        let expected = Outcome::Proposal(Proposal::Change(SidebarLayout {
            side: SidebarSide::Right,
            width: SidebarWidth::new(360).unwrap(),
        }));
        agent.ask(ask(1, Provider::Claude));
        assert_eq!(next(&agent, &woken), vec![(1, expected.clone())]);
        agent.ask(ask(2, Provider::Codex));
        assert_eq!(next(&agent, &woken), vec![(2, expected)]);
    }

    #[test]
    fn prose_and_a_missing_provider_fail_without_a_proposal() {
        let dir = tempfile::tempdir().unwrap();
        let claude = wrapper(
            dir.path(),
            "keystone-fake-claude",
            "KEYSTONE_FAKE_CLAUDE_REPLY='Sure, moved it.' KEYSTONE_FAKE_CLAUDE_DELAY_MS=1",
        );
        let (agent, woken) = agent(
            Programs {
                claude: Some(claude),
                codex: Some(dir.path().join("no-such-codex")),
            },
            REQUEST_TIMEOUT,
        );
        agent.ask(ask(1, Provider::Claude));
        let results = next(&agent, &woken);
        assert!(
            matches!(results.as_slice(), [(1, Outcome::Failed(problem))] if problem.contains("could not be used")),
            "{results:?}"
        );
        agent.ask(ask(2, Provider::Codex));
        let results = next(&agent, &woken);
        assert!(
            matches!(results.as_slice(), [(2, Outcome::Failed(problem))] if problem.contains("Could not start Codex")),
            "{results:?}"
        );
    }

    #[test]
    fn a_replaced_or_cancelled_request_reports_nothing_and_a_slow_one_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let reply =
            format!("KEYSTONE_FAKE_CODEX_REPLY='{CHANGE}' KEYSTONE_FAKE_CODEX_DELAY_MS=1500");
        let codex = wrapper(dir.path(), "keystone-fake-codex", &reply);
        let programs = Programs {
            claude: None,
            codex: Some(codex),
        };
        let (agent, woken) = agent(programs.clone(), REQUEST_TIMEOUT);
        agent.ask(ask(1, Provider::Codex));
        agent.ask(ask(2, Provider::Codex));
        agent.ask(ask(3, Provider::Codex));
        agent.cancel(3);
        agent.ask(ask(4, Provider::Codex));
        let results = next(&agent, &woken);
        assert!(
            matches!(results.as_slice(), [(4, Outcome::Proposal(_))]),
            "{results:?}"
        );

        let (agent, woken) = agent_with_timeout(programs);
        agent.ask(ask(5, Provider::Codex));
        let results = next(&agent, &woken);
        assert!(
            matches!(results.as_slice(), [(5, Outcome::Failed(problem))] if problem.contains("No answer")),
            "{results:?}"
        );
    }

    fn agent_with_timeout(programs: Programs) -> (Agent, mpsc::Receiver<()>) {
        agent(programs, Duration::from_millis(300))
    }
}
