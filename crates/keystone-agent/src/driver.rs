//! What every driver shares: the commands Keystone sends and the outputs a driver
//! produces. Drivers are state machines with no IO; see `docs/design/providers.md`.

use crate::item::{Event, RequestId};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Open the session's conversation in `cwd`, resuming `resume` when given.
    Open {
        session: u64,
        cwd: String,
        resume: Option<String>,
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
