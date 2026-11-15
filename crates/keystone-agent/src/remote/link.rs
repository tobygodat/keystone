//! The app's end of one machine: a transport command (`ssh … keystone-host attach`, or
//! `keystone-host attach` itself in tests), the handshake, reconnecting, calls that are
//! sent again until answered, and the cursors that make each event arrive once. Nothing
//! the app calls waits on the transport: frames go into a queue that one writer thread
//! per transport empties.

use super::frame::{
    self, Attached, CHUNK, Call, Frame, Login, Op, Outcome, PROTOCOL, SessionState, Welcome,
};
use super::{Place, Route};
use crate::{Catalog, Event, Wake};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
/// How long an ended transport's stderr may stay open before its reason is read.
const STDERR_WAIT: Duration = Duration::from_secs(1);
/// How long dropping a link lets its transport end by itself.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

pub const NOT_CONNECTED: &str = "The machine isn't connected, so the message wasn't sent";
const GAP: &str = "Part of this session's transcript was lost: the machine couldn't keep all of it until Keystone received it";
const INSTALL: &str = "cargo install --locked --path crates/keystone-agent --bin keystone-host";
const NO_ANSWER: &str = "The machine didn't answer. Check that it is on and reachable over SSH.";
/// Why a message the app sent before it last quit counts as not sent: the host never
/// received it.
const UNRECORDED: &str = "The machine has no record of this message, so it may already have run";

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
    keystone_core::check_destination(destination)?;
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
    /// What the app calls the machine, which the host names in what it says.
    name: Option<String>,
    state: Mutex<State>,
    changed: Condvar,
}

/// How a call failed, as its reply says.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum How {
    /// Refused before it ran.
    Refused,
    /// Refused before it ran, for the account's login.
    Login,
    /// The host lost track of it, so it may or may not have run.
    MayHaveRun,
    /// The host has no record of the message at all: it never arrived, or its record
    /// is gone with a replaced journal or past the newest records kept, so it may
    /// already have run.
    Unrecorded,
}

/// Where a call's error reply shows: in `session` of `route`.
pub(crate) struct Then {
    pub route: Route,
    pub session: u64,
    pub kind: Kind,
}

pub(crate) enum Kind {
    /// The message the app saved as `send`, carrying the images with these hashes.
    /// Its answer names it: [`Event::Taken`], or [`Event::NotSent`] since nothing
    /// reached the provider and the app can offer it again. A refusal uploads the images
    /// again with the next message, since the host may have refused it for want of one.
    Send { send: String, images: Vec<String> },
    /// What became of the message the app saved as this, in an earlier run. Answered as
    /// that message's own call would have been.
    Recall(String),
    /// As a failed catalog.
    Catalog,
    /// Part of the image with this hash: as a warning, and the image is uploaded
    /// again with the next message that carries it.
    Upload(String),
    /// As a warning.
    Other,
}

struct Pending {
    line: Vec<u8>,
    then: Then,
}

/// An event's number in the sequence and the stamp it was given with.
type At = (u64, u64);

/// The places `hello` shows the host: the highest in each journal, as protocol 6 sent
/// them, and every place with its stamp.
type Seen = (Vec<(String, u64)>, Vec<(String, u64, u64)>);

#[derive(Default)]
struct RouteState {
    /// Events not drained yet, each with its place in the sequence and its stamp when
    /// it has one.
    inbox: Vec<(Option<At>, u64, Event)>,
    /// The registered connection's wake, with the token of its registration.
    wake: Option<(u64, Wake)>,
    sessions: HashMap<u64, SessionState>,
}

struct State {
    status: Status,
    closed: bool,
    /// Whether a host ever welcomed this link, after which `hello` carries `after`.
    linked: bool,
    /// The journal `received`, `unsaved` and every number in an inbox count in: the one
    /// the host last welcomed this link with. `None` until the first welcome.
    journal: Option<std::sync::Arc<str>>,
    /// The highest place received. Anything at or below it is a repeat.
    received: u64,
    /// The stamp `received` came with.
    received_stamp: u64,
    /// Events a connection drained whose session hasn't reported them saved, as
    /// `(place, session)`. The acknowledgement stays below the first of them.
    unsaved: BTreeSet<(u64, u64)>,
    /// Each session's saved cursor: every row its events up to this place added is
    /// in the app's log. Given at launch from the log and raised by [`Link::saved`].
    /// After a welcome it holds only places in that journal ([`State::adopt`]). A
    /// replayed event at or below it is dropped, since its rows are saved already.
    saved: HashMap<u64, Place>,
    /// How many times a welcome named a journal other than the one this link's places
    /// counted in, and whether the host's report of its sessions has followed the last.
    journal_changes: u64,
    journal_reported: bool,
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
    /// What the host said about itself in its last `welcome`.
    welcome: Option<Welcome>,
    /// Why the last transport ended before its host answered, while no later one has.
    trouble: Option<String>,
    /// Set when the watchdog ended the current transport for its silence.
    silenced: bool,
    /// How many `sessions` reports the host has sent this link.
    reports: u64,
    /// The images, by hash, this link has sent to the host since its last welcome, so
    /// a message that carries one again names it without sending its bytes again.
    uploaded: BTreeSet<String>,
    /// Set once [`Link::start`] started the link's thread.
    started: bool,
}

impl Link {
    /// Starts linking through `argv`, a command whose stdin and stdout reach
    /// `keystone-host attach`. `client` is the workspace's own random ID.
    pub fn new(argv: Vec<OsString>, client: String) -> Self {
        Self::with_cursors(argv, client, HashMap::new())
    }

    /// As [`Link::new`], for a machine whose sessions' saved cursors are `saved`, by
    /// session, as the log holds them at launch. A cursor in a journal other than the
    /// one the host welcomes the link with counts for nothing.
    pub fn with_cursors(argv: Vec<OsString>, client: String, saved: HashMap<u64, Place>) -> Self {
        let link = Self::prepared(argv, client, saved, None);
        link.start();
        link
    }

    /// As [`Link::with_cursors`], but reads nothing until [`Link::start`], so every
    /// connection can register first and no event arrives on a route with none.
    /// `name` is what the app calls the machine, which the host uses when it names it.
    pub fn prepared(
        argv: Vec<OsString>,
        client: String,
        saved: HashMap<u64, Place>,
        name: Option<String>,
    ) -> Self {
        let shared = Arc::new(Shared {
            argv,
            client,
            name,
            state: Mutex::new(State {
                status: Status::Connecting,
                closed: false,
                linked: false,
                journal: None,
                received: 0,
                received_stamp: 0,
                unsaved: BTreeSet::new(),
                saved,
                journal_changes: 0,
                journal_reported: true,
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
                welcome: None,
                trouble: None,
                silenced: false,
                reports: 0,
                uploaded: BTreeSet::new(),
                started: false,
            }),
            changed: Condvar::new(),
        });
        Self { shared }
    }

    /// Starts reaching the machine. Only the first call does anything.
    pub fn start(&self) {
        if std::mem::replace(&mut self.shared.state().started, true) {
            return;
        }
        let shared = self.shared.clone();
        thread::Builder::new()
            .name("remote-link".into())
            .spawn(move || shared.supervise())
            .expect("spawn the link thread");
    }

    pub fn status(&self) -> Status {
        self.shared.state().status.clone()
    }

    /// Tries again at once after [`Status::Failed`].
    pub fn retry(&self) {
        self.shared.state().retry = true;
        self.shared.changed.notify_all();
    }

    /// The app saved every row `session`'s events up to `place` added, with the
    /// session's cursor. Only this moves the acknowledgement past a drained event. A
    /// place the link handed out before it last took on a journal moves nothing: it is
    /// the very `Arc` the link held then, so even the same journal taken on again
    /// after it went back doesn't match it.
    pub fn saved(&self, session: u64, place: &Place) {
        let mut state = self.shared.state();
        if !state
            .journal
            .as_ref()
            .is_some_and(|journal| std::sync::Arc::ptr_eq(journal, &place.journal))
        {
            return;
        }
        let seq = place.seq;
        let cursor = state.saved.entry(session).or_insert_with(|| place.clone());
        if seq > cursor.seq {
            *cursor = place.clone();
        }
        state
            .unsaved
            .retain(|&(at, held)| held != session || at > seq);
        state.ack(false);
    }

    /// How many times the host welcomed this link with a journal other than the one
    /// its places counted in, and whether its report of every session has come since
    /// the last. A turn that was running in the old journal has no end there to come.
    pub fn journal_changes(&self) -> (u64, bool) {
        let state = self.shared.state();
        (state.journal_changes, state.journal_reported)
    }

    /// Tells the host the app is quitting, after acknowledging everything saved. With
    /// `keep_running`, the host keeps this workspace's provider processes.
    pub fn quit(&self, keep_running: bool) {
        let mut state = self.shared.state();
        state.ack(true);
        state.write(&Frame::Quit { keep_running });
    }

    /// What the host said about itself when it last answered.
    pub fn welcome(&self) -> Option<Welcome> {
        self.shared.state().welcome.clone()
    }

    /// Why the last attempt ended before the host answered, such as SSH's own error,
    /// while no attempt since has reached it.
    pub fn trouble(&self) -> Option<String> {
        self.shared.state().trouble.clone()
    }

    /// How many times the host has reported every session's state, which it does
    /// after the replay of an app's first link. Each report replaces what
    /// [`super::RemoteConnection::is_running`] and [`super::RemoteConnection::waiting`]
    /// knew.
    pub fn reports(&self) -> u64 {
        self.shared.state().reports
    }

    /// Drops the link and waits up to `within` for its transport to end, so what was
    /// queued, such as [`Link::quit`], reaches the host before the app exits.
    pub fn close(self, within: Duration) {
        let shared = self.shared.clone();
        drop(self);
        let state = shared.state();
        let _ = shared
            .changed
            .wait_timeout_while(state, within, |state| state.transport.is_some());
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
        // What the transport says on stderr, read as it comes. A process the transport
        // left behind, such as a `ProxyCommand`, can hold stderr open long after it
        // exits, so the reason is taken after at most [`STDERR_WAIT`].
        let tailed = Arc::new(Mutex::new(Vec::new()));
        let (tail_done, tail) = mpsc::channel::<()>();
        {
            let tailed = tailed.clone();
            thread::spawn(move || {
                let mut stderr = stderr.take(64 * 1024);
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = stderr.read(&mut buf) {
                    tailed.lock().unwrap().extend_from_slice(&buf[..n]);
                }
                let _ = tail_done.send(());
            });
        }
        let (after, journal, (seen, places)) = {
            let mut state = self.state();
            state.transport = Some(child.id());
            state.heard = Instant::now();
            state.silenced = false;
            let journal = state.journal.as_deref().map(str::to_owned);
            (
                state.linked.then_some(state.received),
                journal,
                state.seen(),
            )
        };
        let hello = Frame::Hello {
            protocol: PROTOCOL,
            client: self.client.clone(),
            after,
            journal,
            seen,
            places,
            name: self.name.clone(),
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
        self.changed.notify_all();
        let _ = tail.recv_timeout(STDERR_WAIT);
        let stderr = String::from_utf8_lossy(&tailed.lock().unwrap()).into_owned();
        if let Ended::Dropped { welcomed: false } = ended {
            let mut state = self.state();
            state.trouble = Some(if state.silenced {
                NO_ANSWER.to_owned()
            } else {
                stderr
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map_or_else(|| NO_ANSWER.to_owned(), str::to_owned)
            });
        }
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
                let silent = now.duration_since(state.heard) >= state.silence;
                if silent || closed_at.is_some_and(|at| now.duration_since(at) >= CLOSE_WAIT) {
                    let mut state = state;
                    state.silenced = silent;
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
                    {
                        let mut state = self.state();
                        state.welcome = Some(welcome.clone());
                        state.trouble = None;
                    }
                    self.resend(out.take().expect("the queue until welcomed"), &welcome);
                }
                Frame::Refused { reason } if !welcomed => {
                    return Ended::Failed(refusal(reason));
                }
                Frame::Event(routed) if welcomed => {
                    let route = Route {
                        provider: routed.provider,
                        account: routed.account,
                    };
                    let at = (routed.seq, routed.stamp);
                    self.route(route, at, vec![(routed.session, routed.event)]);
                }
                Frame::Gap {
                    provider,
                    account,
                    to,
                    stamp,
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
                    self.route(Route { provider, account }, (to, stamp), warnings);
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
    /// calls, so no newer call goes out before them. First puts the link's places in
    /// the welcomed journal ([`State::adopt`]).
    fn resend(&self, out: Sender<Vec<u8>>, welcome: &Welcome) {
        let wakes = {
            let mut state = self.state();
            let changed = state.adopt(&welcome.journal, welcome.latest);
            // A daemon that started again may have dropped an image, so each is sent
            // again before a message names it.
            state.uploaded.clear();
            state.linked = true;
            for call in state.calls.values() {
                let _ = out.send(call.line.clone());
            }
            state.out = Some(out);
            state.sent_ack = (0, 0);
            state.ack(false);
            if changed { state.wakes() } else { Vec::new() }
        };
        for wake in wakes {
            wake();
        }
    }

    /// Adds the events at place `seq`, given with `stamp`, to the route's inbox, unless
    /// they were received before or their session saved them in an earlier run.
    fn route(&self, route: Route, (seq, stamp): At, events: Vec<(u64, Event)>) {
        let wake = {
            let mut state = self.state();
            if seq <= state.received {
                return;
            }
            state.received = seq;
            state.received_stamp = stamp;
            let State { saved, routes, .. } = &mut *state;
            let target = routes.entry(route.clone()).or_default();
            let was_empty = target.inbox.is_empty();
            for (session, event) in events {
                target
                    .sessions
                    .entry(session)
                    .or_insert_with(|| empty_session(&route, session))
                    .apply(&event);
                // `saved` holds places in the welcomed journal only, as `seq` is.
                if saved.get(&session).is_some_and(|cursor| seq <= cursor.seq) {
                    continue;
                }
                target.inbox.push((Some((seq, stamp)), session, event));
            }
            let wake = target.wake.as_ref().map(|(_, wake)| wake.clone());
            let waiting = !target.inbox.is_empty();
            if wake.is_none() || !waiting {
                state.ack(false);
            }
            wake.filter(|_| was_empty && waiting)
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
            state.reports += 1;
            state.journal_reported = true;
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
                Outcome::Ok(()) => state.taken(pending.then),
                Outcome::Err(message) => state.failed(pending.then, message),
                Outcome::Login(message) => state.failed_as(pending.then, message, How::Login),
                Outcome::Unknown(message) => {
                    state.failed_as(pending.then, message, How::MayHaveRun)
                }
                Outcome::Unrecorded => {
                    state.failed_as(pending.then, UNRECORDED.into(), How::Unrecorded)
                }
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
    pub(crate) fn call(
        &self,
        op: Op,
        then: Then,
        login: Option<&Login>,
        connected: bool,
    ) -> Result<(), &'static str> {
        let wake = {
            let mut state = self.state();
            if connected && state.status != Status::Connected {
                return Err(NOT_CONNECTED);
            }
            state.queue(op, then, login)
        };
        if let Some(wake) = wake {
            wake();
        }
        Ok(())
    }

    /// Queues `send` behind the bytes of each of `images` the host hasn't been sent,
    /// in parts of at most [`CHUNK`], all at once, so nothing goes while the machine
    /// isn't connected. The host takes calls in order, so it has every image before
    /// the send that names it.
    pub(crate) fn send_images(
        &self,
        images: Vec<(Attached, Vec<u8>)>,
        send: Op,
        then: Then,
        login: Option<&Login>,
    ) -> Result<(), &'static str> {
        let wakes = {
            let mut state = self.state();
            if state.status != Status::Connected {
                return Err(NOT_CONNECTED);
            }
            let mut wakes = Vec::new();
            for (attached, bytes) in images {
                if !state.uploaded.insert(attached.sha256.clone()) {
                    continue;
                }
                let size = bytes.len() as u64;
                let parts = bytes.chunks(CHUNK).map(Some).collect::<Vec<_>>();
                // An empty image is still one part.
                let parts = if parts.is_empty() { vec![None] } else { parts };
                let mut offset = 0;
                for part in parts {
                    let part = part.unwrap_or_default();
                    let op = Op::PutAttachment {
                        sha256: attached.sha256.clone(),
                        media_type: attached.media_type.clone(),
                        size,
                        offset,
                        data: base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            part,
                        ),
                    };
                    offset += part.len() as u64;
                    let then = Then {
                        route: then.route.clone(),
                        session: then.session,
                        kind: Kind::Upload(attached.sha256.clone()),
                    };
                    wakes.extend(state.queue(op, then, login));
                }
            }
            wakes.extend(state.queue(send, then, login));
            wakes
        };
        for wake in wakes {
            wake();
        }
        Ok(())
    }

    /// Drops the calls still waiting for an answer that carry the message `send` or ask
    /// after it, so no reconnect sends them again: the user gave the message up. One
    /// the host already has is answered by its own record, as its journal says.
    pub(crate) fn abandon(&self, send: &str) {
        let mut state = self.state();
        state.calls.retain(|_, pending| match &pending.then.kind {
            Kind::Send { send: s, .. } | Kind::Recall(s) => s != send,
            Kind::Catalog | Kind::Upload(_) | Kind::Other => true,
        });
        state.ack(false);
    }

    /// Adds an event of the app's own, outside the host's sequence.
    pub(crate) fn push(&self, route: &Route, session: u64, event: Event) {
        let wake = self.state().push(route.clone(), session, event);
        if let Some(wake) = wake {
            wake();
        }
    }

    /// Hands over the route's events with their places. Each one with a place stays
    /// unsaved, holding the acknowledgement back, until [`Link::saved`] covers it.
    pub(crate) fn drain(&self, route: &Route) -> Vec<(Option<Place>, u64, Event)> {
        let mut state = self.state();
        let Some(target) = state.routes.get_mut(route) else {
            return Vec::new();
        };
        let drained = std::mem::take(&mut target.inbox);
        let places: Vec<_> = drained
            .iter()
            .filter_map(|(at, session, _)| at.map(|(seq, _)| (seq, *session)))
            .collect();
        state.unsaved.extend(places);
        let journal = state.journal.clone();
        drained
            .into_iter()
            .map(|(at, session, event)| {
                let place = at
                    .zip(journal.clone())
                    .map(|((seq, stamp), journal)| Place {
                        journal,
                        seq,
                        stamp,
                    });
                (place, session, event)
            })
            .collect()
    }

    pub(crate) fn has_events(&self, route: &Route) -> bool {
        self.state()
            .routes
            .get(route)
            .is_some_and(|target| !target.inbox.is_empty())
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
    /// Every place the link has, with its stamp: its saved cursors, and what it
    /// received in the journal it took on. `hello` sends them, so a host whose journal
    /// went back from one of them can tell, even once it has grown past it again, and
    /// take a new identity. Also the highest of them in each journal without stamps, as
    /// protocol 6 sent them, so an older host still parses `hello` and refuses it.
    fn seen(&self) -> Seen {
        let received = self
            .journal
            .iter()
            .filter(|_| self.received > 0)
            .map(|journal| (journal.to_string(), self.received, self.received_stamp));
        let saved = self
            .saved
            .values()
            .map(|place| (place.journal.to_string(), place.seq, place.stamp));
        let places: BTreeSet<_> = received.chain(saved).collect();
        let mut highest: BTreeMap<String, u64> = BTreeMap::new();
        for (journal, seq, _) in &places {
            let high = highest.entry(journal.clone()).or_default();
            *high = (*high).max(*seq);
        }
        (highest.into_iter().collect(), places.into_iter().collect())
    }

    /// The one rule for places: a number counts only in the journal that numbered it.
    /// Run on every welcome, at launch, on a reconnect, and when the daemon was
    /// replaced while the app ran. Welcomed by another journal than the one this
    /// link's places count in, it forgets them all: what was received, what waits to be
    /// saved, each session's saved cursor from that journal, the inbox's numbers (those
    /// events still reach their sessions, with no place) and what the host said of each
    /// session. It returns whether it did. A host whose journal went back below a place
    /// the link has takes a new identity when `hello` shows it one ([`State::seen`]),
    /// so that is another journal too. Were the same journal ever to answer behind a
    /// place anyway, it is taken as another all the same, with no saved cursor kept:
    /// Keystone can't tell which of its numbers it already has.
    fn adopt(&mut self, journal: &str, latest: u64) -> bool {
        let same = self.journal.as_deref() == Some(journal);
        let behind = (same && self.received > latest)
            || self
                .saved
                .values()
                .any(|place| &*place.journal == journal && place.seq > latest);
        if same && !behind {
            return false;
        }
        let stale = behind
            || self.journal.is_some()
            || self.saved.values().any(|place| &*place.journal != journal);
        let journal: std::sync::Arc<str> = journal.into();
        if behind {
            self.saved.clear();
        }
        self.saved.retain(|_, place| place.journal == journal);
        self.journal = Some(journal);
        self.received = 0;
        self.received_stamp = 0;
        self.unsaved.clear();
        for route in self.routes.values_mut() {
            for (seq, _, _) in &mut route.inbox {
                *seq = None;
            }
            route.sessions.clear();
        }
        if stale {
            self.journal_changes += 1;
            self.journal_reported = false;
        }
        stale
    }

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
    /// highest place at or below which every event of a route with a connection is
    /// saved: drained, and covered by its session's saved cursor ([`Link::saved`]). A
    /// route with no connection holds nothing back: its events wait here for one to
    /// register, and a route nobody drains can't stop the host trimming its journal.
    fn ack(&mut self, force: bool) {
        if self.out.is_none() {
            return;
        }
        let undrained = self
            .routes
            .values()
            .filter(|route| route.wake.is_some())
            .flat_map(|route| {
                route
                    .inbox
                    .iter()
                    .filter_map(|(at, _, _)| at.map(|(seq, _)| seq))
            });
        let unsaved = self.unsaved.first().map(|(seq, _)| *seq);
        let seq = undrained
            .chain(unsaved)
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

    /// Numbers a call and queues it. One too large to send shows in its session as an
    /// error reply would.
    fn queue(&mut self, op: Op, then: Then, login: Option<&Login>) -> Option<Wake> {
        let id = self.next_call;
        let call = Frame::Call(Call {
            id,
            provider: then.route.provider,
            account: then.route.account.clone(),
            login: login.cloned(),
            op,
        });
        match frame::encode(&call) {
            Ok(line) => {
                self.next_call += 1;
                self.write_line(line.clone());
                self.calls.insert(id, Pending { line, then });
                None
            }
            Err(message) => self.failed(then, message),
        }
    }

    /// Says that the host took the message a call carried, and returns the wake to run.
    fn taken(&mut self, then: Then) -> Option<Wake> {
        match then.kind {
            Kind::Send { send, .. } | Kind::Recall(send) => {
                self.push(then.route, then.session, Event::Taken { send })
            }
            Kind::Catalog | Kind::Upload(_) | Kind::Other => None,
        }
    }

    /// Shows the error of a call that didn't run in its session, and returns the wake
    /// to run.
    fn failed(&mut self, then: Then, message: String) -> Option<Wake> {
        self.failed_as(then, message, How::Refused)
    }

    /// Shows a call's error in its session, as `how` it failed, and returns the wake to
    /// run.
    fn failed_as(&mut self, then: Then, message: String, how: How) -> Option<Wake> {
        match &then.kind {
            Kind::Upload(sha256) => {
                self.uploaded.remove(sha256);
            }
            Kind::Send { images, .. } => {
                for sha256 in images {
                    self.uploaded.remove(sha256);
                }
            }
            Kind::Catalog | Kind::Recall(_) | Kind::Other => {}
        }
        let running = self
            .routes
            .get(&then.route)
            .and_then(|target| target.sessions.get(&then.session))
            .is_some_and(|session| session.running);
        let event = match then.kind {
            Kind::Catalog => Event::Catalog(Catalog::Failed(message)),
            // A call the host lost track of may have run, so it isn't offered again.
            // A message the host lost track of may have run, so it is taken, and it
            // ends a turn that hadn't started; it is never offered again as unsent.
            Kind::Send { send, .. } | Kind::Recall(send) if how == How::MayHaveRun => {
                let wake = self.push(then.route.clone(), then.session, Event::Taken { send });
                let event = if running {
                    Event::Warning { message }
                } else {
                    Event::Failed { message }
                };
                return self.push(then.route, then.session, event).or(wake);
            }
            Kind::Send { send, .. } | Kind::Recall(send) => Event::NotSent {
                send,
                message,
                login: how == How::Login,
                unrecorded: how == How::Unrecorded,
            },
            Kind::Upload(_) | Kind::Other => Event::Warning { message },
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

/// What a host's refusal says, with the install command added when the host refused
/// for its protocol: a host that speaks another protocol refuses `hello` before it
/// welcomes anything, so the welcome path never gets to give the command, and an older
/// host's refusal can't carry it itself.
fn refusal(reason: String) -> String {
    if reason.starts_with("protocol ") {
        format!("{reason}. Install the matching version there: {INSTALL}")
    } else {
        reason
    }
}

/// A transport exit that retrying won't fix, as the reason to show.
fn classify(status: Option<ExitStatus>, stderr: &str) -> Option<String> {
    match status?.code()? {
        127 => Some(format!(
            "keystone-host isn't installed on that machine. Install it there from a Keystone checkout: {INSTALL}"
        )),
        255 if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") => Some(
            "The machine's host key changed since you last connected, so SSH refused it. Keystone won't connect until you have checked why and fixed known_hosts yourself.".into(),
        ),
        255 if stderr.contains("Host key verification failed") => Some(
            "Keystone never accepts a host key. Run ssh with this destination once in a terminal to accept it.".into(),
        ),
        255 if stderr.contains("Permission denied") => Some(
            "SSH refused the login. Keystone signs in with keys and the SSH agent only, so add a key for this machine.".into(),
        ),
        _ => None,
    }
}
