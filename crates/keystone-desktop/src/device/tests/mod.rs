use super::capture::{ACTIVE, Capture, INACTIVE, STALE};
use super::frames::for_model;
use super::life::Lifecycle;
use super::pane::{fitted, fraction, next_tick};
use super::*;
use crate::conversation::Row;
use crate::{AppState, ComposerDraft};
use gpui::{Bounds, TestAppContext, px};
use image::{DynamicImage, ImageFormat};
use keystone_agent::device::{Device, Frame, HostId, Point, Power, Support};
use keystone_core::ApprovalMode;
use keystone_core::{Provider, Store, WorkingDirectory};
use std::cell::Cell;
use std::collections::HashSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const PHONE: &str = "ios:FAKE-PHONE";
const SLOW: &str = "ios:FAKE-SLOW";
const STUCK: &str = "ios:FAKE-STUCK";
const RUNNING: &str = "android:Already_Running";

/// Scripted devices: everything they are asked is logged, and nothing real runs.
struct FakeHost {
    devices: Mutex<Vec<Device>>,
    log: Mutex<Vec<String>>,
    /// Holds the slow device's boot until set.
    release: AtomicBool,
    /// Makes every shutdown fail while set.
    refuse_shutdown: AtomicBool,
    /// Makes every listing panic while set.
    broken_listing: AtomicBool,
    /// Makes every start panic while set.
    broken_start: AtomicBool,
    /// Calls that wait, once logged, until they are let go.
    holding: Mutex<HashSet<&'static str>>,
}

impl FakeHost {
    fn new() -> Arc<Self> {
        let device = |id: &str, power| Device {
            id: id.parse().unwrap(),
            name: format!("Fake {id}"),
            os: "iOS 27.0".into(),
            power,
        };
        Arc::new(Self {
            devices: Mutex::new(vec![
                device(PHONE, Power::Shutdown),
                device(SLOW, Power::Shutdown),
                device(RUNNING, Power::Booted),
                device(STUCK, Power::Shutdown),
            ]),
            log: Mutex::default(),
            release: AtomicBool::new(false),
            refuse_shutdown: AtomicBool::new(false),
            broken_listing: AtomicBool::new(false),
            broken_start: AtomicBool::new(false),
            holding: Mutex::default(),
        })
    }

    fn hold(&self, call: &'static str) {
        self.holding.lock().unwrap().insert(call);
    }

    fn let_go(&self, call: &'static str) {
        self.holding.lock().unwrap().remove(call);
    }

    /// Logs `entry`, then waits while `call` is held.
    fn called(&self, call: &'static str, entry: String) {
        self.log.lock().unwrap().push(entry);
        let start = Instant::now();
        while self.holding.lock().unwrap().contains(call) {
            assert!(start.elapsed() < Duration::from_secs(30), "{call} held");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// What the host was asked besides listing, which Keystone repeats after opens
    /// and closes.
    /// What the devices were asked, leaving out listings and the host's stop.
    fn work(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|l| l != "list" && l != "stop")
            .collect()
    }

    fn calls(&self, prefix: &str) -> usize {
        self.log().iter().filter(|l| l.starts_with(prefix)).count()
    }

    fn power(&self, id: &DeviceId) -> Option<Power> {
        let devices = self.devices.lock().unwrap();
        devices.iter().find(|d| &d.id == id).map(|d| d.power)
    }

    fn set_power(&self, id: &DeviceId, power: Power) {
        for device in self.devices.lock().unwrap().iter_mut() {
            if &device.id == id {
                device.power = power;
            }
        }
    }
}

impl DeviceHost for FakeHost {
    fn id(&self) -> HostId {
        HostId::Local
    }

    fn stop(&self) {
        self.log.lock().unwrap().push("stop".into());
    }

    fn inventory(&self, _: Deadline) -> Inventory {
        let devices = self.devices.lock().unwrap().clone();
        self.called("inventory", "list".into());
        assert!(
            !self.broken_listing.load(Ordering::Acquire),
            "a broken listing"
        );
        Inventory {
            devices,
            platforms: vec![keystone_agent::device::PlatformStatus {
                platform: Platform::Ios,
                support: Support::Available,
                input: Input::Available { tool: "fake" },
            }],
        }
    }

    fn start(&self, device: &DeviceId, _: Deadline) -> Result<Start, DeviceError> {
        self.called("start", format!("start {device}"));
        assert!(!self.broken_start.load(Ordering::Acquire), "a broken start");
        Ok(
            match self.power(device).ok_or(DeviceError::UnknownDevice)? {
                Power::Shutdown => {
                    self.set_power(device, Power::Booting);
                    Start::Started
                }
                Power::Booting => Start::Booting,
                Power::Booted => Start::Booted,
            },
        )
    }

    fn wait(&self, device: &DeviceId, until: Deadline) -> Result<BootState, DeviceError> {
        self.log.lock().unwrap().push(format!("wait {device}"));
        match device.to_string().as_str() {
            STUCK => return Ok(BootState::Booting),
            SLOW => {
                while !self.release.load(Ordering::Acquire) {
                    if until.passed() {
                        return Ok(BootState::Booting);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            _ => {}
        }
        if self.power(device) == Some(Power::Shutdown) {
            return Err(DeviceError::NotBooted);
        }
        self.set_power(device, Power::Booted);
        Ok(BootState::Booted)
    }

    fn screenshot(&self, device: &DeviceId, _: Deadline) -> Result<Frame, DeviceError> {
        self.called("screenshot", format!("screenshot {device}"));
        if self.power(device) != Some(Power::Booted) {
            return Err(DeviceError::NotBooted);
        }
        let image = image::RgbaImage::from_pixel(603, 1311, image::Rgba([40, 90, 200, 255]));
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        Ok(Frame {
            png,
            width: 603,
            height: 1311,
        })
    }

    fn act(&self, device: &DeviceId, action: &Action, _: Deadline) -> Result<(), DeviceError> {
        let described = match action {
            Action::Tap(p) => format!("tap {:.3},{:.3}", p.x, p.y),
            Action::Launch { app } => format!("launch {app}"),
            other => format!("{other:?}"),
        };
        self.log
            .lock()
            .unwrap()
            .push(format!("act {device} {described}"));
        Ok(())
    }

    fn shutdown(&self, device: &DeviceId, _: Deadline) -> Result<(), DeviceError> {
        self.called("shutdown", format!("shutdown {device}"));
        if self.refuse_shutdown.load(Ordering::Acquire) {
            return Err(DeviceError::Failed("simctl refused".into()));
        }
        self.set_power(device, Power::Shutdown);
        Ok(())
    }
}

fn scripted(name: &str) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("keystone-fake-{name}"));
    assert!(binary.is_file(), "build scripted provider binaries first");
    binary
}

/// An app with Claude sessions 1 and 2 in `directory`, in `mode`, over `host`.
fn setup(
    directory: &Path,
    mode: ApprovalMode,
    host: &Arc<FakeHost>,
    cx: &mut TestAppContext,
) -> Entity<AppState> {
    let mut store = Store::open(directory.join("workspace.json")).unwrap();
    let project = WorkingDirectory::new(directory).unwrap();
    for id in [1, 2] {
        store.create(Provider::Claude, &project, id).unwrap();
        store.set_approval_mode(id, mode).unwrap();
    }
    cx.executor().allow_parking();
    let host = host.clone();
    cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.test_programs = Some((scripted("codex"), scripted("claude")));
        state.devices = Devices::new(host);
        state
    })
}

fn wait_until(
    state: &Entity<AppState>,
    cx: &mut TestAppContext,
    what: &str,
    predicate: impl Fn(&AppState, &gpui::App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if state.read_with(cx, |state, cx| predicate(state, cx)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Like [`wait_until`], moving the fake clock on so capture timers fire.
fn advance_until(
    state: &Entity<AppState>,
    cx: &mut TestAppContext,
    what: &str,
    predicate: impl Fn(&AppState) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if state.read_with(cx, |state, _| predicate(state)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        cx.executor().advance_clock(ACTIVE);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn transcript(state: &AppState, session: u64) -> String {
    state
        .conversations()
        .get(&session)
        .into_iter()
        .flat_map(|c| &c.rows)
        .filter_map(|row| match row {
            Row::Item(item) => Some(item.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn send(state: &Entity<AppState>, session: u64, text: &str, cx: &mut TestAppContext) {
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session,
                text: text.into(),
                attachments: Vec::new(),
            },
            cx
        ))
    });
}

/// The latest scripted device reply in `session`, once its turn ended.
fn reply(state: &Entity<AppState>, session: u64, cx: &mut TestAppContext) -> String {
    wait_until(state, cx, "the scripted device reply", |state, _| {
        state
            .conversations()
            .get(&session)
            .is_some_and(|c| !c.running)
            && transcript(state, session).contains("Scripted device")
    });
    state.read_with(cx, |state, _| {
        let text = transcript(state, session);
        text.lines()
            .rfind(|line| line.contains("Scripted device"))
            .unwrap()
            .to_owned()
    })
}

/// Sends `text` to `session` and waits for the scripted reply that starts `Scripted
/// device`.
fn ask(state: &Entity<AppState>, session: u64, text: &str, cx: &mut TestAppContext) -> String {
    let before = state.read_with(cx, |state, _| {
        transcript(state, session)
            .matches("Scripted device")
            .count()
    });
    send(state, session, text, cx);
    wait_until(state, cx, "a new scripted device reply", |state, _| {
        transcript(state, session)
            .matches("Scripted device")
            .count()
            > before
    });
    reply(state, session, cx)
}

fn id(text: &str) -> DeviceId {
    text.parse().unwrap()
}

fn life(state: &AppState, device: &str) -> Option<Life> {
    state.devices.record(&id(device)).map(|r| r.life().clone())
}

fn beside(state: &AppState, session: u64) -> Option<DeviceId> {
    state.devices.beside(session).map(|b| b.device.clone())
}

/// Opens `device` beside each of `sessions` for the user and waits until it runs.
fn open_for_user(
    state: &Entity<AppState>,
    sessions: &[u64],
    device: &str,
    cx: &mut TestAppContext,
) {
    for &session in sessions {
        state.update(cx, |state, cx| state.open_device(session, id(device), cx));
    }
    wait_until(state, cx, "the device runs", |state, _| {
        life(state, device) == Some(Life::Booted)
    });
}

fn lifecycle(state: &AppState, device: &str) -> Option<Lifecycle> {
    state
        .devices
        .record(&id(device))
        .map(|r| r.lifecycle.clone())
}

mod lifecycle;
mod quit;
mod surface;
mod tools;
