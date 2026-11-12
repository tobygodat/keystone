//! The Device surface and the orchestrator's `device_*` tools: a simulator or emulator
//! open beside a session, which the user and the session's agent both drive. What is
//! open beside which session lives for this app run only. See `docs/design/devices.md`.
//!
//! [`Devices`] keeps what Keystone knows of each device in one [`Record`]: its
//! [`Lifecycle`], which `life.rs` changes with each answer from the host, and the
//! requests waiting on it. Every change happens on the UI thread. Host calls run on one
//! lane per device, and the wait for a boot on a thread of its own, so the lane stays
//! free for screenshots, input and Close.

use crate::app_state::AppState;
use crate::app_state::orchestrator::Request;
use crate::presentation::SessionPlace;
use futures::channel::oneshot;
use gpui::{AppContext, Context, Entity, Task};
use keystone_agent::device::{
    Action, BootState, Deadline, DeviceError, DeviceHost, DeviceId, Input, Inventory, Platform,
    Start,
};
use keystone_agent::mcp::{HandlerError, Reply};
use keystone_agent::worker::Worker;
use keystone_core::WindowId;
use std::collections::HashMap;
use std::num::NonZeroU16;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

mod capture;
mod frames;
mod life;
mod pane;
mod tools;

use capture::{Capture, Shown};
use frames::{ModelShot, Picture};
pub(crate) use life::Life;
use life::{Lifecycle, Opened, Settle};
pub(crate) use pane::Pane;
use tools::{closed, closing, list_json, opened};

gpui::actions!(device, [ToggleDevice]);

pub(crate) const DEVICE_WIDTH: keystone_core::Size =
    keystone_core::Size::Px(NonZeroU16::new(420).unwrap());

const LIST_LIMIT: Duration = Duration::from_secs(15);
/// Finding a device and asking it to boot, which returns before the boot ends.
const START_LIMIT: Duration = Duration::from_secs(30);
/// How long Keystone waits on a boot before it calls the device stalled.
const BOOT_LIMIT: Duration = Duration::from_secs(180);
/// The user's tap or swipe.
const INPUT_LIMIT: Duration = Duration::from_secs(20);
/// A shutdown once the last session lets go of a device Keystone booted.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(20);
/// How long Quit waits for the devices Keystone booted to shut down.
const QUIT_LIMIT: Duration = Duration::from_secs(3);
/// A tool's host call ends this long before the tool's deadline, so its answer still
/// goes out in time.
const REPLY_MARGIN: Duration = Duration::from_secs(1);
const LEFT: &str = "The device is no longer open beside this session";
const PANICKED: &str = "Keystone's call to the device failed unexpectedly";

/// App-level device state, so every window draws the same and closing one stops nothing.
pub(crate) struct Devices {
    pub(crate) host: Arc<dyn DeviceHost>,
    lanes: HashMap<DeviceId, Worker>,
    listing: Option<Worker>,
    /// Set at Quit, so jobs still queued return without calling the host.
    stopped: Arc<AtomicBool>,
    inventory: Option<Inventory>,
    list: Fetch,
    /// Why the latest listing failed, until one succeeds.
    list_error: Option<String>,
    /// The device open beside each session.
    beside: HashMap<u64, Beside>,
    /// Every device a session holds, is opening or closing, or that Keystone booted and
    /// could not shut down.
    records: HashMap<DeviceId, Record>,
    /// The session whose Device surface each window drew in its last frame.
    shown: HashMap<WindowId, Shown>,
    /// The latest agent open, so each window showing its session brings its Device
    /// surface out once.
    reveal: Reveal,
    /// Agents' opens of a device the latest listing didn't name, until the next one.
    unlisted: Vec<(u64, DeviceId, Box<Request>)>,
    /// Numbers opens, boot waits and waiting requests.
    next: u64,
    #[cfg(test)]
    pub(crate) captures: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fetch {
    Idle,
    /// With `again` when something asked for a listing while this one ran, so what it
    /// found may be older than what was asked.
    Running {
        again: bool,
    },
}

/// How many times an agent opened a device, and beside which session it last did.
#[derive(Clone, Copy, Debug, Default)]
struct Reveal {
    count: u64,
    session: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Beside {
    pub(crate) device: DeviceId,
    /// Which open put the device here, so an older open never answers for a newer one.
    generation: u64,
    /// The size of the latest screenshot image this session's agent got, which its
    /// tap and swipe coordinates are in.
    shot: Option<(u32, u32)>,
}

/// What Keystone knows of one device.
pub(crate) struct Record {
    pub(crate) lifecycle: Lifecycle,
    /// Whether the latest start booted the device, sent by its job as soon as the host
    /// answers, so Quit knows before the UI thread hears it.
    start: Option<mpsc::Receiver<bool>>,
    /// Taken by whichever of the shutdown job and Quit shuts the device down first, so
    /// the other doesn't.
    shutting: Arc<AtomicBool>,
    capture: Capture,
    waiting: Vec<Waiter>,
    /// The latest frame and error, in an entity of its own so that a new frame redraws
    /// only the panes showing this device.
    pub(crate) screen: Entity<Screen>,
}

impl Record {
    pub(crate) fn life(&self) -> &Life {
        &self.lifecycle.life
    }
}

#[derive(Default)]
pub(crate) struct Screen {
    pub(crate) picture: Option<Picture>,
    /// Why the last capture or action failed, until one succeeds.
    pub(crate) error: Option<String>,
}

/// A request waiting for a device to boot or to close.
struct Waiter {
    ticket: u64,
    session: u64,
    wants: Wants,
    /// The agent's request; `None` once answered, or for the user, who watches the panel.
    request: Option<Box<Request>>,
    /// Answers the request with what is known when its deadline nears.
    _expiry: Option<Task<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Wants {
    /// The device booted beside the session, through open `generation`.
    Open { generation: u64 },
    /// The session lets go of the device, which shuts down if Keystone booted it and no
    /// other session holds it.
    Close,
}

/// What a host call answered, with what it was asked.
enum Done {
    Listed(Result<Inventory, String>),
    Started {
        device: DeviceId,
        ask: u64,
        result: Result<Start, DeviceError>,
    },
    /// A frame for the panel.
    Captured {
        device: DeviceId,
        result: Result<Option<Picture>, DeviceError>,
    },
    /// A screenshot an agent asked for, which the panel shows too.
    Shot {
        device: DeviceId,
        result: Result<(Option<Picture>, ModelShot), DeviceError>,
    },
    Acted {
        device: DeviceId,
        result: Result<(), DeviceError>,
    },
    ShutDown {
        device: DeviceId,
        result: Result<(), DeviceError>,
    },
}

/// Where a host call runs.
enum Lane<'a> {
    List,
    Device(&'a DeviceId),
}

impl Devices {
    pub(crate) fn new(host: Arc<dyn DeviceHost>) -> Self {
        Self {
            host,
            lanes: HashMap::new(),
            listing: None,
            stopped: Arc::default(),
            inventory: None,
            list: Fetch::Idle,
            list_error: None,
            beside: HashMap::new(),
            records: HashMap::new(),
            shown: HashMap::new(),
            reveal: Reveal::default(),
            unlisted: Vec::new(),
            next: 0,
            #[cfg(test)]
            captures: 0,
        }
    }

    /// Whether the latest listing, if it succeeded, names `device`.
    fn listed(&self, device: &DeviceId) -> bool {
        self.list_error.is_none()
            && self
                .inventory
                .as_ref()
                .is_some_and(|inventory| inventory.device(device).is_some())
    }

    /// Why `device` can't be opened, naming the devices that can.
    fn missing(&self, device: &DeviceId) -> String {
        if let Some(error) = &self.list_error {
            return format!("Couldn't list devices to find {device}: {error}");
        }
        let ids: Vec<String> = self
            .inventory
            .iter()
            .flat_map(|inventory| &inventory.devices)
            .map(|listed| listed.id.to_string())
            .collect();
        if ids.is_empty() {
            format!("No device {device} here, and none is available; device_list says why")
        } else {
            format!("No device {device} here. Available: {}", ids.join(", "))
        }
    }

    fn number(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Runs `job` on `lane`. A job that panics answers what `on_panic` makes of the
    /// failure, and its lane runs on.
    fn run(
        &mut self,
        lane: Lane,
        on_panic: impl FnOnce(DeviceError) -> Done + Send + 'static,
        job: impl FnOnce(&dyn DeviceHost) -> Done + Send + 'static,
    ) -> oneshot::Receiver<Done> {
        let (sender, receiver) = oneshot::channel();
        let (host, stopped) = (self.host.clone(), self.stopped.clone());
        let job = move || {
            if !stopped.load(Ordering::Acquire) {
                let done = std::panic::catch_unwind(AssertUnwindSafe(|| job(&*host)))
                    .unwrap_or_else(|_| on_panic(DeviceError::Failed(PANICKED.into())));
                let _ = sender.send(done);
            }
        };
        match lane {
            Lane::List => self
                .listing
                .get_or_insert_with(|| Worker::new("keystone-device-list"))
                .run(job),
            Lane::Device(device) => self
                .lanes
                .entry(device.clone())
                .or_insert_with(|| Worker::new("keystone-device"))
                .run(job),
        }
        receiver
    }

    /// Stops capturing and shuts down the devices Keystone booted, waiting for them at
    /// most [`QUIT_LIMIT`] in all, whatever their lanes are doing. A device whose start
    /// is still to answer is shut down once that start ends, if it booted the device.
    /// Jobs still queued return without calling the host.
    pub(crate) fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let until = Deadline::after(QUIT_LIMIT);
        let mut quitting = Vec::new();
        for (device, record) in self.records.drain() {
            let lifecycle = &record.lifecycle;
            let shut_down = lifecycle.owned()
                && !(matches!(lifecycle.life, Life::ShuttingDown { .. })
                    && record.shutting.swap(true, Ordering::AcqRel));
            let start = record.start.filter(|_| lifecycle.asked.is_some());
            if shut_down || start.is_some() {
                quitting.push((device, shut_down, start));
            }
        }
        self.beside.clear();
        for worker in self
            .lanes
            .drain()
            .map(|(_, w)| w)
            .chain(self.listing.take())
        {
            worker.detach();
        }
        let host = &*self.host;
        std::thread::scope(|scope| {
            for (device, shut_down, start) in quitting {
                scope.spawn(move || {
                    if shut_down {
                        let _ = host.shutdown(&device, until);
                    }
                    if start.is_some_and(|start| start.recv_timeout(until.remaining()) == Ok(true))
                    {
                        let _ = host.shutdown(&device, until);
                    }
                });
            }
        });
        // A capture or call still hung on a lane would outlive the app otherwise.
        host.stop();
    }

    pub(crate) fn window_closed(&mut self, window: WindowId) {
        self.shown.remove(&window);
    }

    pub(crate) fn beside(&self, session: u64) -> Option<&Beside> {
        self.beside.get(&session)
    }

    pub(crate) fn record(&self, device: &DeviceId) -> Option<&Record> {
        self.records.get(device)
    }

    /// Whether a session other than a closing one holds `device`.
    fn held(&self, device: &DeviceId, record: &Record) -> bool {
        self.beside.iter().any(|(session, beside)| {
            &beside.device == device
                && !record
                    .waiting
                    .iter()
                    .any(|w| w.wants == Wants::Close && w.session == *session)
        })
    }

    fn input(&self, platform: Platform) -> Option<&Input> {
        Some(&self.inventory.as_ref()?.platform(platform)?.input)
    }
}

/// What a session on `machine` is told about devices.
pub(crate) fn elsewhere(machine: &str) -> String {
    format!("Devices aren't available for sessions on {machine} yet.")
}

impl AppState {
    /// The machine `session` runs on when that isn't this one. A device belongs to a
    /// machine, and Keystone drives only this machine's so far.
    pub(crate) fn device_elsewhere(&self, session: u64) -> Option<String> {
        match self.place(session)? {
            SessionPlace::Remote(machine) => Some(machine),
            SessionPlace::Local(_) | SessionPlace::Unusable => None,
        }
    }

    fn queue(
        &mut self,
        lane: Lane,
        request: Option<Box<Request>>,
        cx: &mut Context<Self>,
        on_panic: impl FnOnce(DeviceError) -> Done + Send + 'static,
        job: impl FnOnce(&dyn DeviceHost) -> Done + Send + 'static,
    ) {
        let receiver = self.devices.run(lane, on_panic, job);
        cx.spawn(async move |this, cx| {
            let Ok(done) = receiver.await else {
                return;
            };
            let _ = this.update(cx, |state, cx| state.apply(done, request, cx));
        })
        .detach();
    }

    /// Answers an agent's request, after checking its caller again.
    fn reply_to(&self, request: Box<Request>, reply: Result<Reply, String>) {
        let reply = self
            .caller(request.caller)
            .and(reply)
            .map_err(HandlerError::from);
        let _ = request.reply.send(reply);
    }

    fn apply(&mut self, done: Done, request: Option<Box<Request>>, cx: &mut Context<Self>) {
        let reply = match done {
            Done::Listed(listing) => Some(self.listed(listing, request.as_deref(), cx)),
            Done::Started {
                device,
                ask,
                result,
            } => {
                self.start_answered(&device, ask, result, cx);
                None
            }
            Done::Captured { device, result } => {
                self.captured(&device, result, cx);
                None
            }
            Done::Shot { device, result } => {
                Some(self.shot(&device, result, request.as_deref(), cx))
            }
            Done::Acted { device, result } => Some(self.acted(&device, result, cx)),
            Done::ShutDown { device, result } => {
                self.shut_down(&device, result, cx);
                None
            }
        };
        if let (Some(request), Some(reply)) = (request, reply) {
            self.reply_to(request, reply);
        }
    }

    /// Opens `device` beside `session`, which holds it from now on, and starts it
    /// unless a start or boot is under way. An agent's request is answered once the
    /// device has booted or failed, or as still booting when its deadline nears.
    fn open(
        &mut self,
        session: u64,
        device: DeviceId,
        request: Option<Box<Request>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(machine) = self.device_elsewhere(session) {
            if let Some(request) = request {
                self.reply_to(request, Err(elsewhere(&machine)));
            }
            return;
        }
        let generation = match self.devices.beside.get(&session) {
            Some(beside) if beside.device == device => beside.generation,
            _ => {
                let generation = self.devices.number();
                let left = self.devices.beside.insert(
                    session,
                    Beside {
                        device: device.clone(),
                        generation,
                        shot: None,
                    },
                );
                if request.is_some() {
                    self.devices.reveal = Reveal {
                        count: self.devices.reveal.count + 1,
                        session,
                    };
                }
                if let Some(left) = left {
                    self.changed(&left.device, cx);
                }
                generation
            }
        };
        let record = self
            .devices
            .records
            .entry(device.clone())
            .or_insert_with(|| Record {
                lifecycle: Lifecycle::default(),
                start: None,
                shutting: Arc::default(),
                capture: Capture::Idle,
                waiting: Vec::new(),
                screen: cx.new(|_| Screen::default()),
            });
        let mut kept = Vec::new();
        record.waiting.retain_mut(|waiter| {
            let reopened = waiter.wants == Wants::Close && waiter.session == session;
            if reopened {
                kept.extend(waiter.request.take());
            }
            !reopened
        });
        for request in kept {
            self.reply_to(
                request,
                Err("A later device_open kept the device beside this session".into()),
            );
        }
        self.start(&device, cx);
        if let Some(request) = request {
            self.wait_for(&device, session, Wants::Open { generation }, request, cx);
        }
        self.changed(&device, cx);
    }

    /// Asks the host to start `device` unless a start, boot or shutdown under way
    /// answers the open.
    fn start(&mut self, device: &DeviceId, cx: &mut Context<Self>) {
        let ask = self.devices.number();
        let Some(record) = self.devices.records.get_mut(device) else {
            return;
        };
        if !record.lifecycle.open(ask) {
            return;
        }
        let (started, start) = mpsc::channel();
        record.start = Some(start);
        let (asked, failed) = (device.clone(), device.clone());
        let on_panic = move |error| Done::Started {
            device: failed,
            ask,
            result: Err(error),
        };
        self.queue(Lane::Device(device), None, cx, on_panic, move |host| {
            let result = host.start(&asked, Deadline::after(START_LIMIT));
            let _ = started.send(result == Ok(Start::Started));
            Done::Started {
                result,
                device: asked,
                ask,
            }
        });
    }

    fn start_answered(
        &mut self,
        device: &DeviceId,
        ask: u64,
        result: Result<Start, DeviceError>,
        cx: &mut Context<Self>,
    ) {
        let watch = self.devices.number();
        let Some(record) = self.devices.records.get_mut(device) else {
            return;
        };
        if record.lifecycle.started(ask, result, watch)
            && let Err(reason) = self.watch_boot(device, watch, cx)
            && let Some(record) = self.devices.records.get_mut(device)
        {
            record
                .lifecycle
                .waited(watch, Err(DeviceError::Failed(reason)));
        }
        self.list_devices(cx);
        self.changed(device, cx);
    }

    /// Waits for `device` to boot on a thread of its own, so the wait holds up nothing
    /// on the device's lane.
    fn watch_boot(
        &mut self,
        device: &DeviceId,
        watch: u64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (sender, receiver) = oneshot::channel();
        let (host, asked) = (self.devices.host.clone(), device.clone());
        std::thread::Builder::new()
            .name("keystone-device-boot".into())
            .spawn(move || {
                let waited = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    host.wait(&asked, Deadline::after(BOOT_LIMIT))
                }));
                let _ = sender.send(waited.unwrap_or_else(|_| {
                    Err(DeviceError::Failed("Waiting for the boot failed".into()))
                }));
            })
            .map_err(|e| format!("Couldn't wait for the device to boot: {e}"))?;
        let device = device.clone();
        cx.spawn(async move |this, cx| {
            let Ok(result) = receiver.await else {
                return;
            };
            let _ = this.update(cx, |state, cx| state.waited(&device, watch, result, cx));
        })
        .detach();
        Ok(())
    }

    fn waited(
        &mut self,
        device: &DeviceId,
        watch: u64,
        result: Result<BootState, DeviceError>,
        cx: &mut Context<Self>,
    ) {
        if let Some(record) = self.devices.records.get_mut(device)
            && record.lifecycle.waited(watch, result)
        {
            self.changed(device, cx);
        }
    }

    /// Lets go of the device beside `session`. The request is answered once Keystone
    /// has shut the device down, or at once when the device stays.
    fn close(&mut self, session: u64, request: Option<Box<Request>>, cx: &mut Context<Self>) {
        let Some(device) = self.devices.beside.get(&session).map(|b| b.device.clone()) else {
            return;
        };
        let Some(record) = self.devices.records.get_mut(&device) else {
            return;
        };
        record.lifecycle.close();
        let closing = record
            .waiting
            .iter()
            .any(|w| w.wants == Wants::Close && w.session == session);
        match request {
            Some(request) => self.wait_for(&device, session, Wants::Close, request, cx),
            None if closing => return,
            None => {
                let ticket = self.devices.number();
                if let Some(record) = self.devices.records.get_mut(&device) {
                    record.waiting.push(Waiter {
                        ticket,
                        session,
                        wants: Wants::Close,
                        request: None,
                        _expiry: None,
                    });
                }
            }
        }
        self.changed(&device, cx);
    }

    /// Parks an agent's request on `device` until it can be answered or its deadline
    /// nears.
    fn wait_for(
        &mut self,
        device: &DeviceId,
        session: u64,
        wants: Wants,
        request: Box<Request>,
        cx: &mut Context<Self>,
    ) {
        let ticket = self.devices.number();
        let patience = request
            .deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(REPLY_MARGIN);
        let expired = device.clone();
        let expiry = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(patience).await;
            let _ = this.update(cx, |state, cx| state.expire(&expired, ticket, cx));
        });
        match self.devices.records.get_mut(device) {
            Some(record) => record.waiting.push(Waiter {
                ticket,
                session,
                wants,
                request: Some(request),
                _expiry: Some(expiry),
            }),
            None => self.reply_to(request, Err(LEFT.into())),
        }
    }

    /// Answers a waiting request with what is known as its deadline nears. A close
    /// carries on without its request, and says so.
    fn expire(&mut self, device: &DeviceId, ticket: u64, cx: &mut Context<Self>) {
        let Some(record) = self.devices.records.get_mut(device) else {
            return;
        };
        let Some(index) = record.waiting.iter().position(|w| w.ticket == ticket) else {
            return;
        };
        let waiter = &mut record.waiting[index];
        let (wants, session, request) = (waiter.wants, waiter.session, waiter.request.take());
        let (answer, life) = (record.lifecycle.opened(), record.lifecycle.life.clone());
        if !matches!(wants, Wants::Close) {
            record.waiting.remove(index);
        }
        let Some(request) = request else {
            return;
        };
        let reply = match wants {
            Wants::Open { generation } => self
                .open_answer(device, session, generation, answer)
                .unwrap_or_else(|| {
                    let state = match life {
                        Life::Booted => "booted",
                        Life::ShuttingDown { .. } => "shutting_down",
                        _ => "booting",
                    };
                    opened(device, state)
                }),
            Wants::Close => closing(device, &life),
        };
        self.reply_to(request, reply);
        cx.notify();
    }

    /// The answer to open `generation` by `session`, once `answer` settles it.
    fn open_answer(
        &self,
        device: &DeviceId,
        session: u64,
        generation: u64,
        answer: Option<Opened>,
    ) -> Option<Result<Reply, String>> {
        let current = self
            .devices
            .beside
            .get(&session)
            .is_some_and(|b| &b.device == device && b.generation == generation);
        if !current {
            return Some(Err(LEFT.into()));
        }
        Some(match answer? {
            Opened::Booted => opened(device, "booted"),
            Opened::Booting => opened(device, "booting"),
            Opened::Stopped => Err("The device stopped before it finished booting".into()),
            Opened::Failed(reason) => Err(reason),
        })
    }

    /// After any change to `device`: answers the opens it settles, decides what
    /// becomes of the device, and redraws.
    fn changed(&mut self, device: &DeviceId, cx: &mut Context<Self>) {
        if let Some(record) = self.devices.records.get_mut(device) {
            let answer = record.lifecycle.opened();
            let waiting = std::mem::take(&mut record.waiting);
            let mut answers = Vec::new();
            let mut kept = Vec::new();
            for mut waiter in waiting {
                let answer = match waiter.wants {
                    Wants::Open { generation } => {
                        self.open_answer(device, waiter.session, generation, answer.clone())
                    }
                    Wants::Close => None,
                };
                match answer {
                    Some(reply) => answers.extend(waiter.request.take().map(|r| (r, reply))),
                    None => kept.push(waiter),
                }
            }
            if let Some(record) = self.devices.records.get_mut(device) {
                record.waiting = kept;
                record.screen.update(cx, |_, cx| cx.notify());
            }
            for (request, reply) in answers {
                self.reply_to(request, reply);
            }
        }
        self.settle(device, cx);
        self.pace_captures(cx);
        cx.notify();
    }

    /// Decides what becomes of `device`: a close lets go at once while another session
    /// holds it, a device Keystone booted shuts down once no session holds it, and one
    /// nobody holds and Keystone need not shut down is forgotten.
    fn settle(&mut self, device: &DeviceId, cx: &mut Context<Self>) {
        let Some(record) = self.devices.records.get(device) else {
            return;
        };
        let held = self.devices.held(device, record);
        let record = self.devices.records.get_mut(device).unwrap();
        let forget = match record.lifecycle.settle(held) {
            Settle::Wait => return,
            Settle::ShutDown => {
                record.screen.update(cx, |_, cx| cx.notify());
                let shutting = Arc::new(AtomicBool::new(false));
                record.shutting = shutting.clone();
                let (asked, failed) = (device.clone(), device.clone());
                let on_panic = |error| Done::ShutDown {
                    device: failed,
                    result: Err(error),
                };
                self.queue(Lane::Device(device), None, cx, on_panic, move |host| {
                    // Quit took it, and shuts the device down itself.
                    let result = if shutting.swap(true, Ordering::AcqRel) {
                        Ok(())
                    } else {
                        host.shutdown(&asked, Deadline::after(SHUTDOWN_LIMIT))
                    };
                    Done::ShutDown {
                        result,
                        device: asked,
                    }
                });
                return;
            }
            Settle::LetGo { forget } => forget,
        };
        let beside = &mut self.devices.beside;
        let mut answers = Vec::new();
        record.waiting.retain_mut(|waiter| {
            if waiter.wants != Wants::Close {
                return true;
            }
            if beside.get(&waiter.session).map(|b| &b.device) == Some(device) {
                beside.remove(&waiter.session);
            }
            answers.extend(waiter.request.take());
            false
        });
        let dropped = forget
            .then(|| self.devices.records.remove(device))
            .flatten();
        if !answers.is_empty() || dropped.is_some() {
            self.list_devices(cx);
        }
        for request in answers {
            self.reply_to(request, closed(device, false));
        }
        if let Some(record) = dropped {
            for waiter in record.waiting {
                if let Some(request) = waiter.request {
                    self.reply_to(request, Err(LEFT.into()));
                }
            }
            if let Some(picture) = record.screen.update(cx, |screen, _| screen.picture.take()) {
                cx.drop_image(picture.image, None);
            }
        }
    }

    /// Takes Keystone's shutdown's answer. A failed one keeps the device as it was,
    /// beside the sessions that closed it, and says why.
    fn shut_down(
        &mut self,
        device: &DeviceId,
        result: Result<(), DeviceError>,
        cx: &mut Context<Self>,
    ) {
        let Some(record) = self.devices.records.get_mut(device) else {
            return;
        };
        let Some(outcome) = record.lifecycle.shut_down(result) else {
            return;
        };
        let closers: Vec<_> = record
            .waiting
            .iter_mut()
            .filter(|w| w.wants == Wants::Close)
            .map(|w| (w.session, w.request.take()))
            .collect();
        record.waiting.retain(|w| w.wants != Wants::Close);
        match outcome {
            Ok(()) => {
                for (session, request) in closers {
                    if self.devices.beside.get(&session).map(|b| &b.device) == Some(device) {
                        self.devices.beside.remove(&session);
                    }
                    if let Some(request) = request {
                        self.reply_to(request, closed(device, true));
                    }
                }
                // A session opened it again while it shut down.
                if self.devices.beside.values().any(|b| &b.device == device) {
                    self.start(device, cx);
                }
                self.list_devices(cx);
            }
            Err(message) => {
                for request in closers.into_iter().filter_map(|(_, request)| request) {
                    self.reply_to(request, Err(message.clone()));
                }
            }
        }
        self.changed(device, cx);
    }

    fn list(&mut self, request: Option<Box<Request>>, until: Deadline, cx: &mut Context<Self>) {
        let again = self.devices.list == Fetch::Running { again: true };
        self.devices.list = Fetch::Running { again };
        let on_panic = |error: DeviceError| Done::Listed(Err(error.to_string()));
        self.queue(Lane::List, request, cx, on_panic, move |host| {
            Done::Listed(Ok(host.inventory(until)))
        });
        cx.notify();
    }

    /// Lists devices for the picker, or once more after the listing already running.
    pub(crate) fn list_devices(&mut self, cx: &mut Context<Self>) {
        match self.devices.list {
            Fetch::Idle => self.list(None, Deadline::after(LIST_LIMIT), cx),
            Fetch::Running { .. } => self.devices.list = Fetch::Running { again: true },
        }
    }

    /// Takes a listing, and the host's word on each device Keystone holds: one booted
    /// outside Keystone runs again, and a shutdown that failed is tried again. A device
    /// that stopped is left to its captures, which a listing taken before a boot could
    /// contradict.
    fn listed(
        &mut self,
        listing: Result<Inventory, String>,
        request: Option<&Request>,
        cx: &mut Context<Self>,
    ) -> Result<Reply, String> {
        let again = self.devices.list == Fetch::Running { again: true };
        self.devices.list = Fetch::Idle;
        let reply = self.take_listing(listing, request, cx);
        if again && self.devices.list == Fetch::Idle {
            self.list_devices(cx);
        }
        if self.devices.list == Fetch::Idle {
            self.open_unlisted(cx);
        }
        cx.notify();
        reply
    }

    /// Opens each device an agent asked for that this listing names, and answers the
    /// rest with what there is, leaving the device open beside its session as it was.
    fn open_unlisted(&mut self, cx: &mut Context<Self>) {
        for (session, device, request) in std::mem::take(&mut self.devices.unlisted) {
            if self.devices.listed(&device) {
                self.open(session, device, Some(request), cx);
            } else {
                let missing = self.devices.missing(&device);
                self.reply_to(request, Err(missing));
            }
        }
    }

    fn take_listing(
        &mut self,
        listing: Result<Inventory, String>,
        request: Option<&Request>,
        cx: &mut Context<Self>,
    ) -> Result<Reply, String> {
        self.devices.list_error = listing.as_ref().err().cloned();
        let inventory = listing?;
        let held: Vec<DeviceId> = (self.devices.records.iter())
            .filter(|(device, record)| self.devices.held(device, record))
            .map(|(device, _)| device.clone())
            .collect();
        let mut changed = Vec::new();
        for (device, record) in &mut self.devices.records {
            if record.lifecycle.listed(
                inventory.device(device).map(|listed| listed.power),
                held.contains(device),
            ) {
                changed.push(device.clone());
            }
        }
        let value = request.map(|request| {
            let session = request.caller.session;
            let target = self.reviewed(session).unwrap_or(session);
            list_json(
                self.devices.host.id().slug(),
                &inventory,
                self.devices.beside.get(&target).map(|b| &b.device),
            )
        });
        self.devices.inventory = Some(inventory);
        for device in changed {
            self.changed(&device, cx);
        }
        self.pace_captures(cx);
        Ok(value.unwrap_or_default().into())
    }

    /// Opens `device` beside `session` for the user.
    pub(crate) fn open_device(&mut self, session: u64, device: DeviceId, cx: &mut Context<Self>) {
        self.open(session, device, None, cx);
    }

    pub(crate) fn close_device(&mut self, session: u64, cx: &mut Context<Self>) {
        self.close(session, None, cx);
    }

    /// The user's tap or swipe on the panel, through the same lane as the tools.
    pub(crate) fn device_input(&mut self, session: u64, action: Action, cx: &mut Context<Self>) {
        let Some(device) = self.devices.beside.get(&session).map(|b| b.device.clone()) else {
            return;
        };
        if self.devices.records.get(&device).map(Record::life) == Some(&Life::Booted) {
            self.act(device, action, Deadline::after(INPUT_LIMIT), None, cx);
        }
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod tests;
