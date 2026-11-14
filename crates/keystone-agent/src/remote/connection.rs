//! [`RemoteConnection`]: the [`Connection`] for one machine, provider and account,
//! which sends each call to that machine's `keystone-host` over the shared [`Link`].

use super::frame::{Attached, Login, Op};
use super::link::{Kind, Link, Shared, Then};
use super::{Place, Route};
use crate::connection::command_session;
use crate::idle::IdleCandidate;
use crate::{Approval, ApprovalState, Command, Connection, Event, Image, Wake};
use keystone_core::{ForkPoint, ModelChoice, Provider};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const NO_IMAGES: &str = "An image can go to a session on another machine only with a message";
const UNREAD: &str =
    "An attached image couldn't be read from Keystone's data folder, so the message wasn't sent";

pub struct RemoteConnection {
    link: Arc<Shared>,
    route: Route,
    login: Option<Login>,
    registration: u64,
}

impl RemoteConnection {
    /// Events for `provider` and `account` on the link's machine arrive here, and
    /// `wake` runs when they do or the link's state changes. A connection made for the
    /// same provider and account replaces this one's wake. `account` is `None` for the
    /// machine's default login; another is named by its ID and has a `login`.
    pub fn new(
        link: &Link,
        provider: Provider,
        account: Option<(String, Login)>,
        wake: Wake,
    ) -> Self {
        let link = link.shared();
        let (account, login) = account.unzip();
        let route = Route { provider, account };
        let registration = link.register(&route, wake);
        Self {
            link,
            route,
            login,
            registration,
        }
    }

    /// Reads Codex's stored history of `thread`, which arrives as
    /// [`Event::HistoryRead`] with `ticket`, as [`crate::CodexConnection::read_history`].
    pub fn read_history(&self, session: u64, thread: String, ticket: u64) {
        let op = Op::ReadHistory {
            session,
            thread,
            ticket,
        };
        self.report(session, op, Kind::Other);
    }

    /// Lists the Codex conversations that ran in `cwd` on the machine, which arrive as
    /// [`Event::ThreadsListed`] with `ticket`, as [`crate::CodexConnection::list_threads`].
    pub fn list_threads(&self, cwd: &str, ticket: u64) {
        let op = Op::ListThreads {
            cwd: cwd.to_owned(),
            ticket,
        };
        self.report(0, op, Kind::Other);
    }

    /// Reads the newest turns of `thread` for an import, as
    /// [`crate::CodexConnection::read_import`].
    pub fn read_import(&self, thread: String, ticket: u64) {
        self.report(0, Op::ReadImport { thread, ticket }, Kind::Other);
    }

    /// The approvals the session's provider waits on, as far as the host has said.
    pub fn waiting(&self, session: u64) -> Vec<Approval> {
        self.link
            .session(&self.route, session)
            .map(|state| state.approvals)
            .unwrap_or_default()
    }

    /// The events that arrived, each with its place in the machine's sequence, or
    /// `None` for one the app made itself, such as a call's error. The acknowledgement
    /// stays below each placed event until [`Link::saved`] says its session saved it.
    pub fn drain_placed(&self) -> Vec<(Option<Place>, u64, Event)> {
        self.link.drain(&self.route)
    }

    /// Whether events wait to be drained.
    pub fn has_events(&self) -> bool {
        self.link.has_events(&self.route)
    }

    /// Sends `op` now or once the link reconnects. An error reply shows in the session.
    fn report(&self, session: u64, op: Op, kind: Kind) {
        let then = self.then(session, kind);
        let _ = self.link.call(op, then, self.login.as_ref(), false);
    }

    fn then(&self, session: u64, kind: Kind) -> Then {
        Then {
            route: self.route.clone(),
            session,
            kind,
        }
    }
}

impl Drop for RemoteConnection {
    fn drop(&mut self) {
        self.link.unregister(&self.route, self.registration);
    }
}

impl Connection for RemoteConnection {
    fn is_running(&self, session: u64) -> bool {
        self.link
            .session(&self.route, session)
            .is_some_and(|state| state.running)
    }

    /// Returns once the call is queued, without waiting for the host. While the machine
    /// isn't connected it refuses at once, so the draft stays. A refusal from the host,
    /// such as a steer Claude can't take now, or an answer saying the message may not
    /// have run, arrives later in the session: as a warning while a turn runs, else as
    /// a failure, the way a local send reports a message that fails after it was taken.
    /// Each image goes by its hash, after its bytes when this link hasn't sent them.
    fn send(
        &self,
        session: u64,
        cwd: &str,
        resume: Option<&str>,
        choice: &ModelChoice,
        approval: ApprovalState,
        text: String,
        images: Vec<Image>,
    ) -> Result<(), &'static str> {
        let mut attached = Vec::with_capacity(images.len());
        for image in images {
            let bytes = match &image.base64 {
                Some(data) => {
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &**data)
                        .map_err(|_| UNREAD)?
                }
                None => std::fs::read(&image.path).map_err(|_| UNREAD)?,
            };
            let sha256 = Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let image = Attached {
                media_type: image.media_type,
                sha256,
            };
            attached.push((image, bytes));
        }
        let op = Op::Send {
            session,
            cwd: cwd.to_owned(),
            resume: resume.map(str::to_owned),
            choice: choice.clone(),
            approval,
            text,
            images: attached.iter().map(|(image, _)| image.clone()).collect(),
        };
        let then = self.then(session, Kind::Send);
        if attached.is_empty() {
            self.link.call(op, then, self.login.as_ref(), true)
        } else {
            self.link
                .send_images(attached, op, then, self.login.as_ref())
        }
    }

    fn request_catalog(&self, session: u64, cwd: &str) {
        let op = Op::RequestCatalog {
            session,
            cwd: cwd.to_owned(),
        };
        self.report(session, op, Kind::Catalog);
    }

    fn set_choice(&self, session: u64, choice: ModelChoice) {
        self.report(session, Op::SetChoice { session, choice }, Kind::Other);
    }

    fn set_approval(&self, session: u64, approval: ApprovalState) {
        self.report(session, Op::SetApproval { session, approval }, Kind::Other);
    }

    /// Forks are off for sessions on another machine.
    fn set_fork(&self, _session: u64, _point: ForkPoint) {}

    /// Never runs `job`: its work, such as a checkpoint, reads the session's folder,
    /// which is on the other machine.
    fn run_in_order(&self, _session: u64, _job: Box<dyn FnOnce() + Send>) {}

    fn hold_follow_ups(&self, session: u64) {
        self.report(session, Op::HoldFollowUps { session }, Kind::Other);
    }

    fn release_follow_ups(&self, session: u64) {
        self.report(session, Op::ReleaseFollowUps { session }, Kind::Other);
    }

    fn command(&self, command: Command) {
        let session = command_session(&command);
        if let Command::Send { images, .. } = &command
            && !images.is_empty()
        {
            let message = NO_IMAGES.to_owned();
            return self
                .link
                .push(&self.route, session, Event::Warning { message });
        }
        if let Command::Answer { request, .. } = &command {
            self.link.answered(&self.route, session, request);
        }
        self.report(session, Op::Command(command), Kind::Other);
    }

    fn evict_idle(&self, session: u64, candidate: IdleCandidate) {
        self.report(session, Op::EvictIdle { session, candidate }, Kind::Other);
    }

    fn decline_idle(&self, session: u64, candidate: IdleCandidate, reason: String) {
        let op = Op::DeclineIdle {
            session,
            candidate,
            reason,
        };
        self.report(session, op, Kind::Other);
    }

    /// The events without their places. Each stays unsaved until [`Link::saved`]
    /// covers it, so the host keeps it; the app drains with [`Self::drain_placed`].
    fn drain(&self) -> Vec<(u64, Event)> {
        self.drain_placed()
            .into_iter()
            .map(|(_, session, event)| (session, event))
            .collect()
    }
}
