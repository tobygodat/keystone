//! What every driver shares: the commands Keystone sends and the outputs a driver
//! produces. Drivers are state machines with no IO; see `docs/design/providers.md`.

use crate::item::{Event, RequestId};
use keystone_core::ApprovalMode;

/// The approval mode a session's next turns run under. Each driver maps the mode to its
/// provider's own settings; see "Approval modes" in `docs/design/providers.md`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ApprovalState {
    pub mode: ApprovalMode,
    /// Whether an earlier process may have sent the conversation an override that the
    /// provider keeps across a resume. Saved before any override is sent.
    pub overridden: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Open the session's conversation in `cwd`, resuming `resume` when given. `model`
    /// is the provider-native model the session asked for; `None` leaves the choice to
    /// the provider.
    Open {
        session: u64,
        cwd: String,
        resume: Option<String>,
        model: Option<String>,
    },
    /// Start a turn, or steer the running one.
    Send {
        session: u64,
        text: String,
    },
    Interrupt {
        session: u64,
    },
    Answer {
        session: u64,
        request: RequestId,
        allow: bool,
    },
    /// Sets the approval mode for the session's next turns, including a message that
    /// waits to start one. `overridden` is read only before the session opens.
    Configure {
        session: u64,
        approval: ApprovalState,
    },
}

/// Lines to write to the provider and events for each session.
#[derive(Debug, Default)]
pub struct Outputs {
    pub writes: Vec<String>,
    pub events: Vec<(u64, Event)>,
}

impl Outputs {
    pub(crate) fn event(&mut self, session: u64, event: Event) {
        self.events.push((session, event));
    }
}
