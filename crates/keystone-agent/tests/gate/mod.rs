//! Holds provider starts, so tests can check what still works while one is stuck.

// Each test binary uses only some of these.
#![allow(dead_code)]

use keystone_agent::StartHook;
use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The longest a start is held. A connection that waits on a held start then fails
/// its test instead of hanging it.
const HOLD_LIMIT: Duration = Duration::from_secs(8);

#[derive(Default)]
pub struct Gate {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// The sessions whose starts are held, or every session when `None`.
    held: Option<HashSet<u64>>,
    released: HashSet<u64>,
    reached: HashSet<u64>,
}

impl Gate {
    /// Holds the starts of `sessions`, or of every session when `None`.
    pub fn new(sessions: Option<&[u64]>) -> Arc<Self> {
        let gate = Arc::new(Self::default());
        gate.state.lock().unwrap().held = sessions.map(|s| s.iter().copied().collect());
        gate
    }

    pub fn hook(self: &Arc<Self>) -> StartHook {
        let gate = self.clone();
        Arc::new(move |session| gate.wait(session))
    }

    fn wait(&self, session: u64) {
        let mut state = self.state.lock().unwrap();
        state.reached.insert(session);
        self.changed.notify_all();
        let held = |s: &mut State| {
            !s.released.contains(&session) && s.held.as_ref().is_none_or(|h| h.contains(&session))
        };
        let _ = self.changed.wait_timeout_while(state, HOLD_LIMIT, held);
    }

    /// Waits until `session`'s start is being held.
    pub fn reached(&self, session: u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = self.state.lock().unwrap();
        while !state.reached.contains(&session) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "session {session} never started");
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
    }

    pub fn release(&self, session: u64) {
        self.state.lock().unwrap().released.insert(session);
        self.changed.notify_all();
    }
}

/// Waits until `pid` has exited.
pub fn gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(keystone_agent::host::process_state(pid), None | Some('Z')) {
            return;
        }
        assert!(Instant::now() < deadline, "process {pid} still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}
