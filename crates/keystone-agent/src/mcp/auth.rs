use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use keystone_core::ApprovalMode;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::{Caller, State};

static GENERATION: AtomicU64 = AtomicU64::new(1);

pub(super) struct Entry {
    caller: Caller,
    digest: [u8; 32],
}

pub(super) struct Scope {
    mode: ApprovalMode,
    running: bool,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            mode: ApprovalMode::Provider,
            running: false,
        }
    }
}

#[derive(Clone)]
pub struct Issuer {
    pub(super) state: Arc<State>,
    url: String,
}

impl Issuer {
    pub(super) fn new(state: Arc<State>, url: String) -> Self {
        Self { state, url }
    }

    pub fn issue(&self, session: u64) -> io::Result<Credentials> {
        let mut random = [0; 32];
        getrandom::getrandom(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let generation = GENERATION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| io::Error::other("MCP generation exhausted"))?;
        let caller = Caller {
            session,
            generation,
        };
        let mut registry = self.state.registry.lock().unwrap();
        if self.state.stopped() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "MCP server stopped",
            ));
        }
        self.state
            .scopes
            .lock()
            .unwrap()
            .entry(session)
            .or_default()
            .running = false;
        registry.retain(|entry| entry.caller.session != session);
        registry.push(Entry {
            caller,
            digest: digest(&token),
        });
        Ok(Credentials {
            url: self.url.clone(),
            token,
            lease: Lease {
                caller,
                state: Arc::downgrade(&self.state),
            },
        })
    }

    pub fn is_live(&self, caller: Caller) -> bool {
        let registry = self.state.registry.lock().unwrap();
        !self.state.stopped() && registry.iter().any(|entry| entry.caller == caller)
    }

    pub fn set_mode(&self, session: u64, mode: ApprovalMode) {
        self.state
            .scopes
            .lock()
            .unwrap()
            .entry(session)
            .or_default()
            .mode = mode;
    }

    pub fn set_running(&self, session: u64, running: bool) {
        self.state
            .scopes
            .lock()
            .unwrap()
            .entry(session)
            .or_default()
            .running = running;
    }

    pub fn authorize(&self, caller: Caller, target_mode: ApprovalMode) -> bool {
        self.permitted(caller, target_mode, true)
    }

    pub(crate) fn permits_launch(&self, caller: Caller, target_mode: ApprovalMode) -> bool {
        self.permitted(caller, target_mode, false)
    }

    fn permitted(&self, caller: Caller, target_mode: ApprovalMode, require_running: bool) -> bool {
        let registry = self.state.registry.lock().unwrap();
        if self.state.stopped() || !registry.iter().any(|entry| entry.caller == caller) {
            return false;
        }
        self.state
            .scopes
            .lock()
            .unwrap()
            .get(&caller.session)
            .is_some_and(|scope| {
                (!require_running || scope.running) && super::mode_allowed(target_mode, scope.mode)
            })
    }
}

pub struct Credentials {
    url: String,
    token: String,
    lease: Lease,
}

impl Credentials {
    pub fn caller(&self) -> Caller {
        self.lease.caller
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn into_parts(self) -> (String, String, Lease) {
        (self.url, self.token, self.lease)
    }
}

pub struct Lease {
    caller: Caller,
    state: Weak<State>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .registry
                .lock()
                .unwrap()
                .retain(|entry| entry.caller != self.caller);
            state.wake.notify_all();
        }
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

pub(super) fn authenticate(state: &State, token: &str) -> Option<Caller> {
    let candidate = digest(token);
    let registry = state.registry.lock().unwrap();
    if state.stopped() {
        return None;
    }
    let mut caller = None;
    for entry in registry.iter() {
        if bool::from(candidate.ct_eq(&entry.digest)) {
            caller = Some(entry.caller);
        }
    }
    caller
}

pub(super) fn is_live(state: &State, caller: Caller) -> bool {
    let registry = state.registry.lock().unwrap();
    !state.stopped() && registry.iter().any(|entry| entry.caller == caller)
}
