//! The app's end of one machine: a transport command (`ssh … keystone-host attach`, or
//! `keystone-host attach` itself in tests), the handshake, reconnecting, calls that are
//! sent again until answered, and the cursors that make each event arrive once. Nothing
//! the app calls waits on the transport: frames go into a queue that one writer thread
//! per transport empties.

use super::Route;
use super::frame::{self, Call, Frame, Op, Outcome, PROTOCOL, SessionState};
use crate::{Catalog, Event, Wake};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// How long a transport may go without a frame before it is ended. The link pings at a
/// third of that.
const SILENCE: Duration = Duration::from_secs(15);
const BACKOFF: [u64; 6] = [1, 2, 4, 8, 16, 30];
/// How long dropping a link lets its transport end by itself.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

pub(crate) const NOT_CONNECTED: &str = "The machine isn't connected, so the message wasn't sent";
const GAP: &str = "Part of this session's transcript was lost: the machine couldn't keep all of it until Keystone received it";
const INSTALL: &str = "cargo install --locked --path crates/keystone-agent --bin keystone-host";

/// Where a link stands. Every state change wakes each of its connections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The first attempt since the link was made.
    Connecting,
    /// The host answered and its replay is done.
    Connected,
    /// The link dropped, and attempts back off: 1, 2, 4, 8, 16, then every 30 s.
    Reconnecting,
    /// Retrying wouldn't help, for this reason, until [`Link::retry`].
    Failed(String),
}

/// The `ssh` command that links `destination`. It never prompts and fails fast, and
/// the remote login shell gives `keystone-host` the user's own `PATH`.
pub fn ssh(destination: &str) -> Result<Vec<OsString>, String> {
    if destination.is_empty()
        || destination.starts_with('-')
        || destination
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("{destination:?} isn't an SSH destination"));
    }
    Ok([
        "ssh",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=3",
        "--",
        destination,
        "sh -lc \"exec keystone-host attach\"",
    ]
    .map(OsString::from)
    .into())
}

/// One connection to one machine, shared by every [`super::RemoteConnection`] for it.
/// Dropping it ends the transport.
pub struct Link {
    shared: Arc<Shared>,
}

pub(crate) struct Shared {
    argv: Vec<OsString>,
    client: String,
    state: Mutex<State>,
    changed: Condvar,
}

/// Where a call's error reply shows: in `session` of `route`.
pub(crate) struct Then {
    pub route: Route,
    pub session: u64,
    pub kind: Kind,
}

pub(crate) enum Kind {
    /// As a local send that fails after it was accepted: a warning while a turn runs,
    /// else a failure.
    Send,
    /// As a failed catalog.
    Catalog,
    /// As a warning.
    Other,
}

struct Pending {
    line: Vec<u8>,
    then: Then,
}

#[derive(Default)]
struct RouteState {
    /// Events not drained yet, each with its place in the sequence when it has one.
    inbox: Vec<(Option<u64>, u64, Event)>,
    /// The registered connection's wake, with the token of its registration.
    wake: Option<(u64, Wake)>,
    sessions: HashMap<u64, SessionState>,
}

struct State {
    status: Status,
    closed: bool,
    /// Whether a host ever welcomed this link, after which `hello` carries `after`.
    linked: bool,
    /// The highest place received. Anything at or below it is a repeat.
    received: u64,
    sent_ack: (u64, u64),
    next_call: u64,
    calls: BTreeMap<u64, Pending>,
    routes: HashMap<Route, RouteState>,
    registrations: u64,
    heard: Instant,
    silence: Duration,
    retry: bool,
    transport: Option<u32>,
    /// The current transport's writer queue, from its `welcome` until it ends.
    out: Option<Sender<Vec<u8>>>,
}

impl Link {
    /// Starts linking through `argv`, a command whose stdin and stdout reach
    /// `keystone-host attach`. `client` is the workspace's own random ID.
    pub fn new(argv: Vec<OsString>, client: String) -> Self {
        let shared = Arc::new(Shared {
            argv,
            client,
            state: Mutex::new(State {
                status: Status::Connecting,
                closed: false,
                linked: false,
                received: 0,
                sent_ack: (0, 0),
                next_call: 1,
                calls: BTreeMap::new(),
                routes: HashMap::new(),
                registrations: 0,
                heard: Instant::now(),
                silence: SILENCE,
                retry: false,
                transport: None,
                out: None,
            }),
            changed: Condvar::new(),
        });
        {
            let shared = shared.clone();
            thread::Builder::new()
                .name("remote-link".into())
                .spawn(move || shared.supervise())
                .expect("spawn the link thread");
        }
        Self { shared }
    }

    pub fn status(&self) -> Status {
        self.shared.state().status.clone()
    }

    /// Tries again at once after [`Status::Failed`].
    pub fn retry(&self) {
        self.shared.state().retry = true;
        self.shared.changed.notify_all();
    }

    /// Tells the host the app is quitting, after acknowledging everything drained. With
    /// `keep_running`, the host keeps this workspace's provider processes.
    pub fn quit(&self, keep_running: bool) {
        let mut state = self.shared.state();
        state.ack(true);
        state.write(&Frame::Quit { keep_running });
    }

    /// The transport process while it runs.
    #[doc(hidden)]
    pub fn transport_pid(&self) -> Option<u32> {
        self.shared.state().transport
    }

    /// Ends a transport after `silence` with no frame instead of 15 s, and pings at a
    /// third of that.
    #[doc(hidden)]
    pub fn set_silence(&self, silence: Duration) {
        self.shared.state().silence = silence;
    }

    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }
}

impl Drop for Link {
    /// Returns at once. The writer closes the transport's stdin once what was queued is
    /// written, so `attach` ends, and the transport gets [`CLOSE_WAIT`] to go before it
    /// is killed.
    fn drop(&mut self) {
        let mut state = self.shared.state();
        state.closed = true;
        state.out = None;
        drop(state);
        self.shared.changed.notify_all();
    }
}

/// How one transport ended.
enum Ended {
    Closed,
    Failed(String),
    Dropped { welcomed: bool },
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn set_status(&self, status: Status) {
        let wakes: Vec<Wake> = {
            let mut state = self.state();
            if state.status == status {
                return;
            }
            state.status = status;
            state.wakes()
        };
        self.changed.notify_all();
        for wake in wakes {
            wake();
        }
    }

    fn supervise(self: Arc<Self>) {
        let mut attempt = 0;
        loop {
            match self.connect_once() {
                Ended::Closed => return,
                Ended::Failed(reason) => {
                    self.set_status(Status::Failed(reason));
                    let mut state = self.state();
                    state.retry = false;
                    while !state.retry && !state.closed {
                        state = self.changed.wait(state).unwrap();
                    }
                    if state.closed {
                        return;
                    }
                    attempt = 0;
                    drop(state);
                    self.set_status(Status::Reconnecting);
                }
                Ended::Dropped { welcomed } => {
                    if welcomed {
                        attempt = 0;
                    }
                    self.set_status(Status::Reconnecting);
                    let delay = backoff(attempt);
                    attempt += 1;
                    let state = self.state();
                    let (state, _) = self
                        .changed
                        .wait_timeout_while(state, delay, |state| !state.closed)
                        .unwrap();
                    if state.closed {
                        return;
                    }
                }
            }
        }
    }

    /// Runs one transport from spawn to exit.
    fn connect_once(self: &Arc<Self>) -> Ended {
        if self.state().closed {
            return Ended::Closed;
        }
        let Some((program, args)) = self.argv.split_first() else {
            return Ended::Failed("No command reaches this machine".into());
        };
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                return Ended::Failed(format!(
                    "Could not run {}: {error}",
                    program.to_string_lossy()
                ));
            }
        };
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let tail = thread::spawn(move || {
            let mut text = Vec::new();
            let _ = stderr.take(64 * 1024).read_to_end(&mut text);
            String::from_utf8_lossy(&text).into_owned()
        });
        let after = {
            let mut state = self.state();
            state.transport = Some(child.id());
            state.heard = Instant::now();
            state.linked.then_some(state.received)
        };
        let hello = Frame::Hello {
            protocol: PROTOCOL,
            client: self.client.clone(),
            after,
        };
        let (out, queue) = mpsc::channel();
        let _ = out.send(frame::encode(&hello).expect("hello fits a frame"));
        thread::Builder::new()
            .name("remote-link-write".into())
            .spawn(move || write(stdin, queue))
            .expect("spawn the link writer");
        let child = Arc::new(Mutex::new(child));
        let watch = self.watch(child.clone());
        let ended = self.read(stdout, out);
        self.state().out = None;
        watch.done();
        let status = finish(&child);
        self.state().transport = None;
        let stderr = tail.join().unwrap_or_default();
        match ended {
            Ended::Dropped { welcomed } => classify(status, &stderr).map_or(
                if self.state().closed {
                    Ended::Closed
                } else {
                    Ended::Dropped { welcomed }
                },
                Ended::Failed,
            ),
            ended => ended,
        }
    }

    /// Pings, and kills the transport after [`SILENCE`] with no frame, or [`CLOSE_WAIT`]
    /// after the link was dropped. Neither waits on the writer, which a transport that
    /// stops reading can hold forever.
    fn watch(self: &Arc<Self>, child: Arc<Mutex<Child>>) -> Watch {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let shared = self.clone();
        let flag = done.clone();
        let handle = thread::spawn(move || {
            let mut pinged = Instant::now();
            let mut closed_at = None;
            loop {
                {
                    let (lock, cond) = &*flag;
                    let guard = lock.lock().unwrap();
                    let (guard, _) = cond
                        .wait_timeout_while(guard, Duration::from_millis(100), |done| !*done)
                        .unwrap();
                    if *guard {
                        return;
                    }
                }
                let now = Instant::now();
                let state = shared.state();
                if state.closed {
                    closed_at.get_or_insert(now);
                }
                if now.duration_since(state.heard) >= state.silence
                    || closed_at.is_some_and(|at| now.duration_since(at) >= CLOSE_WAIT)
                {
                    drop(state);
                    let _ = child.lock().unwrap().kill();
                    return;
                }
                if now.duration_since(pinged) >= state.silence / 3 {
                    pinged = now;
                    state.write(&Frame::Ping);
                }
            }
        });
        Watch { done, handle }
    }

    /// Reads frames until the transport ends or breaks the protocol. `out` becomes the
    /// link's queue once the host welcomes it.
    fn read(&self, stdout: impl Read, out: Sender<Vec<u8>>) -> Ended {
        let mut reader = BufReader::with_capacity(64 * 1024, stdout);
        let mut line = Vec::new();
        let mut out = Some(out);
        let mut welcomed = false;
        loop {
            let frame = match frame::read(&mut reader, &mut line) {
                Ok(Some(frame)) => frame,
                Ok(None) => return Ended::Dropped { welcomed },
                Err(_) => return Ended::Dropped { welcomed },
            };
            self.state().heard = Instant::now();
            match frame {
                Frame::Welcome(welcome) if !welcomed => {
                    if welcome.protocol != PROTOCOL {
                        return Ended::Failed(format!(
                            "The machine's keystone-host {} speaks protocol {}, and Keystone needs {PROTOCOL}. Install the matching version there: {INSTALL}",
                            welcome.version, welcome.protocol
                        ));
                    }
                    welcomed = true;
                    self.resend(
                        out.take().expect("the queue until welcomed"),
                        welcome.latest,
                    );
                }
                Frame::Refused { reason } if !welcomed => return Ended::Failed(reason),
                Frame::Event(routed) if welcomed => {
                    let route = Route {
                        provider: routed.provider,
                        account: routed.account,
                    };
                    self.route(route, routed.seq, vec![(routed.session, routed.event)]);
                }
                Frame::Gap {
                    provider,
                    account,
                    to,
                    sessions,
                    ..
                } if welcomed => {
                    let warnings = sessions
                        .into_iter()
                        .map(|session| {
                            let message = GAP.to_owned();
                            (session, Event::Warning { message })
                        })
                        .collect();
                    self.route(Route { provider, account }, to, warnings);
                }
                Frame::Warning {
                    provider,
                    account,
                    session,
                    message,
                } if welcomed => self.push(
                    &Route { provider, account },
                    session,
                    Event::Warning { message },
                ),
                Frame::Synced { .. } if welcomed => self.set_status(Status::Connected),
                Frame::Sessions { sessions } if welcomed => self.restore(sessions),
                Frame::Reply { id, outcome } if welcomed => self.answer(id, outcome),
                Frame::Ping => self.state().write(&Frame::Pong),
                Frame::Pong => {}
                _ => return Ended::Dropped { welcomed },
            }
        }
    }

    /// Makes `out` the link's queue after queueing again, in order, every call that has
    /// no reply, then the current acknowledgement. Under the one lock that numbers
    /// calls, so no newer call goes out before them.
    fn resend(&self, out: Sender<Vec<u8>>, latest: u64) {
        let mut state = self.state();
        state.linked = true;
        // A host whose journal is behind what was received lost it, and numbers its
        // events from there again.
        state.received = state.received.min(latest);
        for call in state.calls.values() {
            let _ = out.send(call.line.clone());
        }
        state.out = Some(out);
        state.sent_ack = (0, 0);
        state.ack(false);
    }

    /// Adds the events at place `seq` to the route's inbox, unless they were received
    /// before.
    fn route(&self, route: Route, seq: u64, events: Vec<(u64, Event)>) {
        let wake = {
            let mut state = self.state();
            if seq <= state.received {
                return;
            }
            state.received = seq;
            let target = state.routes.entry(route.clone()).or_default();
            let was_empty = target.inbox.is_empty();
            for (session, event) in events {
                target
                    .sessions
                    .entry(session)
                    .or_insert_with(|| empty_session(&route, session))
                    .apply(&event);
                target.inbox.push((Some(seq), session, event));
            }
            let wake = target.wake.as_ref().map(|(_, wake)| wake.clone());
            if wake.is_none() {
                state.ack(false);
            }
            wake.filter(|_| was_empty)
        };
        if let Some(wake) = wake {
            wake();
        }
    }

    fn restore(&self, sessions: Vec<SessionState>) {
        let wakes: Vec<Wake> = {
            let mut state = self.state();
            for session in sessions {
                let route = Route {
                    provider: session.provider,
                    account: session.account.clone(),
                };
                let target = state.routes.entry(route).or_default();
                target.sessions.insert(session.session, session);
            }
            state.wakes()
        };
        for wake in wakes {
            wake();
        }
    }

    fn answer(&self, id: u64, outcome: Outcome) {
        let wake = {
            let mut state = self.state();
            let Some(pending) = state.calls.remove(&id) else {
                return;
            };
            let wake = match outcome {
                Outcome::Ok(()) => None,
                Outcome::Err(message) => state.failed(pending.then, message),
            };
            state.ack(false);
            wake
        };
        if let Some(wake) = wake {
            wake();
        }
    }

    /// Numbers a call and queues it, to go out now if the link is up, else when it
    /// reconnects. With `connected`, refuses at once while the machine isn't connected.
    /// A call too large to send shows in its session as an error reply would.
    pub(crate) fn call(&self, op: Op, then: Then, connected: bool) -> Result<(), &'static str> {
        let wake = {
            let mut state = self.state();
            if connected && state.status != Status::Connected {
                return Err(NOT_CONNECTED);
            }
            let id = state.next_call;
            let call = Frame::Call(Call {
                id,
                provider: then.route.provider,
                account: then.route.account.clone(),
                op,
            });
            match frame::encode(&call) {
                Ok(line) => {
                    state.next_call += 1;
                    state.write_line(line.clone());
                    state.calls.insert(id, Pending { line, then });
                    None
                }
                Err(message) => state.failed(then, message),
            }
        };
        if let Some(wake) = wake {
            wake();
        }
        Ok(())
    }

    /// Adds an event of the app's own, outside the host's sequence.
    pub(crate) fn push(&self, route: &Route, session: u64, event: Event) {
        let wake = self.state().push(route.clone(), session, event);
        if let Some(wake) = wake {
            wake();
        }
    }

    pub(crate) fn drain(&self, route: &Route) -> Vec<(u64, Event)> {
        let mut state = self.state();
        let Some(target) = state.routes.get_mut(route) else {
            return Vec::new();
        };
        let drained = std::mem::take(&mut target.inbox);
        if drained.iter().any(|(seq, _, _)| seq.is_some()) {
            state.ack(false);
        }
        drained
            .into_iter()
            .map(|(_, session, event)| (session, event))
            .collect()
    }

    /// Makes `wake` the route's, replacing any earlier registration, and returns the
    /// token that [`Shared::unregister`] takes.
    pub(crate) fn register(&self, route: &Route, wake: Wake) -> u64 {
        let (token, waiting) = {
            let mut state = self.state();
            state.registrations += 1;
            let token = state.registrations;
            let target = state.routes.entry(route.clone()).or_default();
            target.wake = Some((token, wake.clone()));
            (token, !target.inbox.is_empty())
        };
        if waiting {
            wake();
        }
        token
    }

    /// Forgets the route's wake if `token` still names its registration, so a
    /// connection dropped after its replacement registered leaves the replacement's.
    pub(crate) fn unregister(&self, route: &Route, token: u64) {
        let mut state = self.state();
        if let Some(target) = state.routes.get_mut(route)
            && target.wake.as_ref().is_some_and(|(held, _)| *held == token)
        {
            target.wake = None;
            state.ack(false);
        }
    }

    /// The app answered `request`. Claude reports no event for that.
    pub(crate) fn answered(&self, route: &Route, session: u64, request: &crate::RequestId) {
        if let Some(state) = self
            .state()
            .routes
            .get_mut(route)
            .and_then(|target| target.sessions.get_mut(&session))
        {
            state.answered(request);
        }
    }

    pub(crate) fn session(&self, route: &Route, session: u64) -> Option<SessionState> {
        self.state()
            .routes
            .get(route)
            .and_then(|target| target.sessions.get(&session).cloned())
    }
}

impl State {
    /// Every registered connection's wake.
    fn wakes(&self) -> Vec<Wake> {
        self.routes
            .values()
            .filter_map(|route| route.wake.as_ref().map(|(_, wake)| wake.clone()))
            .collect()
    }

    /// Queues `line` for the transport while the link is up.
    fn write_line(&self, line: Vec<u8>) {
        if let Some(out) = &self.out {
            let _ = out.send(line);
        }
    }

    fn write(&self, frame: &Frame) {
        if let Ok(line) = frame::encode(frame) {
            self.write_line(line);
        }
    }

    /// Queues an acknowledgement when it moved, or with `force`. The event cursor is the
    /// highest place below which every event of a route with a connection was drained.
    /// A route with no connection holds nothing back: its events wait here for one to
    /// register, and a route nobody drains can't stop the host trimming its journal.
    fn ack(&mut self, force: bool) {
        if self.out.is_none() {
            return;
        }
        let seq = self
            .routes
            .values()
            .filter(|route| route.wake.is_some())
            .flat_map(|route| route.inbox.iter().filter_map(|(seq, _, _)| *seq))
            .min()
            .map_or(self.received, |first| self.received.min(first - 1));
        let calls = self
            .calls
            .keys()
            .next()
            .map_or(self.next_call - 1, |first| first - 1);
        if (seq, calls) == self.sent_ack && !force {
            return;
        }
        self.sent_ack = (seq, calls);
        self.write(&Frame::Ack { seq, calls });
    }

    /// Adds an event outside the host's sequence, and returns the wake to run.
    fn push(&mut self, route: Route, session: u64, event: Event) -> Option<Wake> {
        let target = self.routes.entry(route).or_default();
        target.inbox.push((None, session, event));
        target.wake.as_ref().map(|(_, wake)| wake.clone())
    }

    /// Shows a call's error in its session, and returns the wake to run.
    fn failed(&mut self, then: Then, message: String) -> Option<Wake> {
        let running = self
            .routes
            .get(&then.route)
            .and_then(|target| target.sessions.get(&then.session))
            .is_some_and(|session| session.running);
        let event = match then.kind {
            Kind::Catalog => Event::Catalog(Catalog::Failed(message)),
            Kind::Send if !running => Event::Failed { message },
            Kind::Send | Kind::Other => Event::Warning { message },
        };
        self.push(then.route, then.session, event)
    }
}

/// Writes what the link queues for one transport, in order, until the queue closes or
/// the transport stops taking input. Dropping `stdin` then ends the transport's input.
fn write(mut stdin: ChildStdin, queue: Receiver<Vec<u8>>) {
    for line in queue {
        if stdin.write_all(&line).is_err() {
            return;
        }
    }
}

struct Watch {
    done: Arc<(Mutex<bool>, Condvar)>,
    handle: thread::JoinHandle<()>,
}

impl Watch {
    fn done(self) {
        *self.done.0.lock().unwrap() = true;
        self.done.1.notify_all();
        let _ = self.handle.join();
    }
}

fn empty_session(route: &Route, session: u64) -> SessionState {
    SessionState {
        provider: route.provider,
        account: route.account.clone(),
        session,
        running: false,
        approvals: Vec::new(),
    }
}

fn backoff(attempt: usize) -> Duration {
    let base = BACKOFF[attempt.min(BACKOFF.len() - 1)] * 1000;
    let mut bytes = [0u8; 8];
    let _ = getrandom::getrandom(&mut bytes);
    let jitter = u64::from_le_bytes(bytes) % (base / 5 + 1);
    Duration::from_millis(base + jitter)
}

/// Waits for the transport to exit on its own for a moment, then ends it.
fn finish(child: &Mutex<Child>) -> Option<ExitStatus> {
    let mut child = child.lock().unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    child.wait().ok()
}

/// A transport exit that retrying won't fix, as the reason to show.
fn classify(status: Option<ExitStatus>, stderr: &str) -> Option<String> {
    match status?.code()? {
        127 => Some(format!(
            "keystone-host isn't installed on that machine. Install it there from a Keystone checkout: {INSTALL}"
        )),
        255 if stderr.contains("Host key verification failed") => Some(
            "Keystone never accepts a host key. Run ssh with this destination once in a terminal to accept it.".into(),
        ),
        255 if stderr.contains("Permission denied") => Some(
            "SSH refused the login. Keystone signs in with keys and the SSH agent only, so add a key for this machine.".into(),
        ),
        _ => None,
    }
}
