//! [`RemoteConnection`]: the [`Connection`] for one machine, provider and account,
//! which sends each call to that machine's `keystone-host` over the shared [`Link`].

use super::Route;
use super::frame::Op;
use super::link::{Kind, Link, Shared, Then};
use crate::connection::command_session;
use crate::idle::IdleCandidate;
use crate::{Approval, ApprovalState, Command, Connection, Event, Image, Wake};
use keystone_core::{ForkPoint, ModelChoice, Provider};
use std::sync::Arc;

const NO_IMAGES: &str = "Images can't be sent to a session on another machine yet";

pub struct RemoteConnection {
    link: Arc<Shared>,
    route: Route,
    registration: u64,
}

impl RemoteConnection {
    /// Events for `provider` and `account` on the link's machine arrive here, and
    /// `wake` runs when they do or the link's state changes. A connection made for the
    /// same provider and account replaces this one's wake.
    pub fn new(link: &Link, provider: Provider, account: Option<String>, wake: Wake) -> Self {
        let link = link.shared();
        let route = Route { provider, account };
        let registration = link.register(&route, wake);
        Self {
            link,
            route,
            registration,
        }
    }

    /// The approvals the session's provider waits on, as far as the host has said.
    pub fn waiting(&self, session: u64) -> Vec<Approval> {
        self.link
            .session(&self.route, session)
            .map(|state| state.approvals)
            .unwrap_or_default()
    }

    /// Sends `op` now or once the link reconnects. An error reply shows in the session.
    fn report(&self, session: u64, op: Op, kind: Kind) {
        let then = self.then(session, kind);
        let _ = self.link.call(op, then, false);
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
        if !images.is_empty() {
            return Err(NO_IMAGES);
        }
        let op = Op::Send {
            session,
            cwd: cwd.to_owned(),
            resume: resume.map(str::to_owned),
            choice: choice.clone(),
            approval,
            text,
        };
        self.link.call(op, self.then(session, Kind::Send), true)
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

    fn drain(&self) -> Vec<(u64, Event)> {
        self.link.drain(&self.route)
    }
}
