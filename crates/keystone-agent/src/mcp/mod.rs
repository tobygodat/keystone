use std::collections::BTreeMap;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use keystone_core::ApprovalMode;
use serde_json::Value;

mod auth;
pub mod browser;
pub mod device;
mod http;
mod protocol;

pub use auth::{Credentials, Issuer, Lease};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Caller {
    pub session: u64,
    pub generation: u64,
}

#[derive(Debug)]
pub enum HandlerError {
    Deadline,
    Rejected(String),
}

impl From<String> for HandlerError {
    fn from(message: String) -> Self {
        Self::Rejected(message)
    }
}

impl From<&str> for HandlerError {
    fn from(message: &str) -> Self {
        Self::Rejected(message.into())
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deadline => {
                f.write_str("Keystone did not return a result in time; the request may still have taken effect. Retry writes with the same client_request_id")
            }
            Self::Rejected(message) => f.write_str(message),
        }
    }
}

/// An image a tool returns beside its JSON result.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub mime: &'static str,
    pub data: Vec<u8>,
}

/// A tool's result: JSON for the caller, and images shown to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub value: Value,
    pub images: Vec<Image>,
}

impl From<Value> for Reply {
    fn from(value: Value) -> Self {
        Self {
            value,
            images: Vec::new(),
        }
    }
}

pub type Handler =
    Arc<dyn Fn(Caller, &str, Value, Duration) -> Result<Reply, HandlerError> + Send + Sync>;

/// The session tools, which Claude runs without asking.
const SESSION_TOOLS: [&str; 6] = [
    "capabilities",
    "session_launch",
    "session_send",
    "session_read",
    "session_wait",
    "session_interrupt",
];

/// Whether Claude runs the `keystone` tool `name` with `input` without asking.
pub fn auto_allowed(name: &str, input: &Value) -> bool {
    SESSION_TOOLS.contains(&name)
        || device::TOOLS.contains(&name)
        || browser::auto_allowed(name, input)
}

pub fn mode_allowed(target: ApprovalMode, ceiling: ApprovalMode) -> bool {
    if target == ApprovalMode::Provider || ceiling == ApprovalMode::Provider {
        return true;
    }
    fn rank(mode: ApprovalMode) -> u8 {
        match mode {
            ApprovalMode::Provider => 0,
            ApprovalMode::ReadOnly => 1,
            ApprovalMode::Ask => 2,
            ApprovalMode::Auto => 3,
            ApprovalMode::FullAccess => 4,
        }
    }
    rank(target) <= rank(ceiling)
}

const MAX_CLIENTS: usize = 32;

struct State {
    stopped: AtomicBool,
    registry: Mutex<Vec<auth::Entry>>,
    scopes: Mutex<BTreeMap<u64, auth::Scope>>,
    clients: Mutex<BTreeMap<u64, TcpStream>>,
    wake: Condvar,
    pause: Mutex<()>,
}

impl State {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn sleep(&self, duration: Duration) {
        let guard = self.pause.lock().unwrap();
        let _ = self
            .wake
            .wait_timeout_while(guard, duration, |_| !self.stopped())
            .unwrap();
    }
}

pub struct Server {
    issuer: Issuer,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(handler: Handler) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let authority = listener.local_addr()?.to_string();
        let state = Arc::new(State {
            stopped: AtomicBool::new(false),
            registry: Mutex::new(Vec::new()),
            scopes: Mutex::new(BTreeMap::new()),
            clients: Mutex::new(BTreeMap::new()),
            wake: Condvar::new(),
            pause: Mutex::new(()),
        });
        let issuer = Issuer::new(state.clone(), format!("http://{authority}/mcp"));
        let accept = thread::Builder::new()
            .name("keystone-mcp".into())
            .spawn(move || accept_loop(listener, authority, state, handler))?;
        Ok(Self {
            issuer,
            accept: Some(accept),
        })
    }

    pub fn issuer(&self) -> Issuer {
        self.issuer.clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let state = &self.issuer.state;
        {
            let _guard = state.pause.lock().unwrap();
            state.stopped.store(true, Ordering::Release);
            state.wake.notify_all();
        }
        state.registry.lock().unwrap().clear();
        for client in state.clients.lock().unwrap().values() {
            let _ = client.shutdown(Shutdown::Both);
        }
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

struct Client {
    id: u64,
    state: Arc<State>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.state.clients.lock().unwrap().remove(&self.id);
    }
}

fn accept_loop(listener: TcpListener, authority: String, state: Arc<State>, handler: Handler) {
    let next = AtomicU64::new(1);
    while !state.stopped() {
        let (mut stream, peer) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                state.sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        // macOS hands out an accepted socket nonblocking, like the listener; Linux never does.
        if !peer.ip().is_loopback() || stream.set_nonblocking(false).is_err() {
            continue;
        }
        let id = next.fetch_add(1, Ordering::Relaxed);
        {
            let mut clients = state.clients.lock().unwrap();
            if state.stopped() {
                break;
            }
            if clients.len() >= MAX_CLIENTS {
                drop(clients);
                let _ = thread::Builder::new()
                    .name("keystone-mcp-busy".into())
                    .spawn(move || {
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                        http::respond(
                            &mut stream,
                            503,
                            protocol::error(Value::Null, -32000, "Server busy"),
                        );
                        http::close_early(&mut stream);
                    });
                continue;
            }
            let Ok(copy) = stream.try_clone() else {
                continue;
            };
            clients.insert(id, copy);
        }
        let client = Client {
            id,
            state: state.clone(),
        };
        let handler = handler.clone();
        let authority = authority.clone();
        let _ = thread::Builder::new()
            .name("keystone-mcp-client".into())
            .spawn(move || {
                let _client = client;
                http::serve(stream, &authority, &_client.state, &handler);
            });
    }
}

#[cfg(test)]
mod tests;
