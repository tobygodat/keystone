//! iOS Simulators through `xcrun simctl`, with input through AXe when it is installed.

use super::local::{LocalHost, run};
use super::{
    Action, BootState, Deadline, Device, DeviceError, Frame, Input, Platform, PlatformStatus,
    Power, Start, Support, parse,
};
use crate::git::Children;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const AXE_HINT: &str =
    "Install AXe to tap, swipe and type on iOS Simulators: brew install cameroncooke/axe/axe";
/// What AXe prints when the simulator was too slow to give it an automation session.
const AXE_SESSION_TIMEOUT: &str = "Timed out creating the simulator remote automation session";
/// How long one `simctl bootstatus` runs before Keystone checks the simulator is still on.
const STATUS_STEP: Duration = Duration::from_secs(5);
/// How long learning a simulator's point size may take off the tap path, how long to
/// wait after a failed try before the next, doubling each time, and how many tries a
/// screenshot starts before only taps ask.
const LEARN_LIMIT: Duration = Duration::from_secs(30);
const LEARN_RETRY: Duration = Duration::from_secs(10);
const LEARN_TRIES: u32 = 3;
/// The end of a screenshot's time kept for asking simctl the simulator's state when the
/// capture fails: a shut-down simulator makes `simctl io` wait a minute, never fail.
const STATE_CHECK: Duration = Duration::from_secs(3);

/// What is known of a simulator's screen size in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Points {
    /// An AXe session is asking now; anyone else who needs the size waits for it.
    Learning {
        tries: u32,
    },
    Known(f64, f64),
    Failed {
        at: Instant,
        tries: u32,
    },
    /// AXe gave a size that didn't turn with the screen, so each tap asks it afresh.
    Unreliable,
}

/// Each simulator's point size by UDID, and a signal for when a learn ends.
#[derive(Default)]
pub(crate) struct PointSizes {
    sizes: Mutex<HashMap<String, Points>>,
    learned: Condvar,
}

impl PointSizes {
    #[cfg(test)]
    pub(crate) fn get(&self, udid: &str) -> Option<Points> {
        self.lock().get(udid).copied()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Points>> {
        self.sizes.lock().unwrap()
    }

    /// Ends the learn under way for `udid`, unless a shutdown forgot it meanwhile.
    fn learned(&self, udid: &str, points: Points) {
        let mut sizes = self.lock();
        if matches!(sizes.get(udid), Some(Points::Learning { .. })) {
            sizes.insert(udid.to_owned(), points);
        }
        self.learned.notify_all();
    }

    fn forget(&self, udid: &str) {
        self.lock().remove(udid);
        self.learned.notify_all();
    }
}

/// Whether a size in points has the shape of a screenshot in pixels.
fn upright((width, height): (f64, f64)) -> bool {
    height >= width
}

fn unsupported() -> DeviceError {
    DeviceError::Unsupported(if cfg!(target_os = "macos") {
        "Xcode's xcrun was not found. Install Xcode to run iOS Simulators, or set KEYSTONE_XCRUN_PATH.".into()
    } else {
        "iOS Simulators need macOS with Xcode.".into()
    })
}

fn xcrun(host: &LocalHost) -> Result<PathBuf, DeviceError> {
    host.tools().xcrun.ok_or_else(unsupported)
}

fn axe(host: &LocalHost) -> Result<PathBuf, DeviceError> {
    host.tools()
        .axe
        .ok_or_else(|| DeviceError::InputUnavailable(AXE_HINT.into()))
}

fn simctl(host: &LocalHost, args: &[&str], until: Deadline) -> Result<Vec<u8>, DeviceError> {
    let mut all = vec!["simctl"];
    all.extend_from_slice(args);
    run(&host.children, &xcrun(host)?, &all, until)
}

/// Runs AXe, once more when the simulator was too slow to give it a session and the
/// first try left as much time again before `until`.
fn run_axe(
    children: &Children,
    axe: &Path,
    args: &[&str],
    until: Deadline,
) -> Result<Vec<u8>, DeviceError> {
    let started = Instant::now();
    match run(children, axe, args, until) {
        Err(DeviceError::Failed(output))
            if output.contains(AXE_SESSION_TIMEOUT) && until.remaining() >= started.elapsed() =>
        {
            run(children, axe, args, until)
        }
        result => result,
    }
}

fn devices(host: &LocalHost, until: Deadline) -> Result<Vec<Device>, DeviceError> {
    let listed = simctl(host, &["list", "devices", "available", "-j"], until)?;
    parse::simctl_devices(&listed).map_err(DeviceError::Failed)
}

fn power(host: &LocalHost, udid: &str, until: Deadline) -> Result<Power, DeviceError> {
    devices(host, until)?
        .into_iter()
        .find(|d| d.id.key == udid)
        .map(|d| d.power)
        .ok_or(DeviceError::UnknownDevice)
}

pub(super) fn inventory(host: &LocalHost, until: Deadline) -> (Vec<Device>, PlatformStatus) {
    let input = match host.tools().axe {
        Some(_) => Input::Available { tool: "axe" },
        None => Input::Unavailable {
            reason: AXE_HINT.into(),
        },
    };
    let (devices, support) = match devices(host, until) {
        Ok(devices) => (devices, Support::Available),
        Err(error @ DeviceError::Unsupported(_)) => (
            Vec::new(),
            Support::Unavailable {
                reason: error.to_string(),
            },
        ),
        Err(error) => (
            Vec::new(),
            Support::Unavailable {
                reason: format!("Couldn't list iOS Simulators: {error}"),
            },
        ),
    };
    (
        devices,
        PlatformStatus {
            platform: Platform::Ios,
            support,
            input,
        },
    )
}

pub(super) fn start(host: &LocalHost, udid: &str, until: Deadline) -> Result<Start, DeviceError> {
    match power(host, udid, until)? {
        Power::Booted => Ok(Start::Booted),
        Power::Booting => Ok(Start::Booting),
        Power::Shutdown => match simctl(host, &["boot", udid], until) {
            Ok(_) | Err(DeviceError::TimedOut) => Ok(Start::Started),
            Err(DeviceError::Failed(output)) if output.contains("current state: Booted") => {
                Ok(Start::Booted)
            }
            Err(DeviceError::Failed(output)) if output.contains("current state: Booting") => {
                Ok(Start::Booting)
            }
            Err(error) => Err(error),
        },
    }
}

/// `bootstatus` in short steps, so a simulator shut down meanwhile ends the wait.
pub(super) fn wait(
    host: &LocalHost,
    udid: &str,
    until: Deadline,
) -> Result<BootState, DeviceError> {
    loop {
        let step = Deadline(until.0.min(Instant::now() + STATUS_STEP));
        match simctl(host, &["bootstatus", udid], step) {
            Ok(_) => return Ok(BootState::Booted),
            Err(DeviceError::TimedOut) => {}
            Err(error) => return Err(error),
        }
        match power(host, udid, until) {
            _ if until.passed() => return Ok(BootState::Booting),
            Ok(Power::Shutdown) => return Err(DeviceError::NotBooted),
            Ok(_) | Err(DeviceError::TimedOut) => {}
            Err(error) => return Err(error),
        }
    }
}

/// A file of its own for each capture, since `simctl io` can't write to a pipe.
fn capture_path() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "keystone-device-{}-{}.png",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(super) fn screenshot(
    host: &LocalHost,
    udid: &str,
    until: Deadline,
) -> Result<Frame, DeviceError> {
    let path = capture_path();
    let target = path.to_string_lossy().into_owned();
    let capture_until = if until.remaining() > STATE_CHECK * 2 {
        Deadline(until.0 - STATE_CHECK)
    } else {
        until
    };
    let taken = simctl(
        host,
        &["io", udid, "screenshot", "--type=png", &target],
        capture_until,
    )
    .and_then(|_| {
        std::fs::read(&path)
            .map_err(|e| DeviceError::Failed(format!("Couldn't read the screenshot: {e}")))
    });
    let _ = std::fs::remove_file(&path);
    let png = match taken {
        Ok(png) => png,
        Err(DeviceError::Failed(output))
            if output.contains("No devices are booted")
                || output.contains("current state: Shutdown") =>
        {
            return Err(DeviceError::NotBooted);
        }
        Err(error) => {
            return Err(match power(host, udid, until) {
                Ok(Power::Shutdown | Power::Booting) => DeviceError::NotBooted,
                _ => error,
            });
        }
    };
    let (width, height) = parse::png_size(&png)
        .ok_or_else(|| DeviceError::Failed("simctl wrote a screenshot that isn't a PNG".into()))?;
    learn_points(host, udid, (width, height));
    Ok(Frame { png, width, height })
}

/// Learns the simulator's point size on a thread of its own, so a later tap takes one
/// AXe call. Learns again when the screen turns between portrait and landscape, after a
/// failure once its pause is over, and never while another learn runs.
fn learn_points(host: &LocalHost, udid: &str, (width, height): (u32, u32)) {
    let Some(axe) = host.tools().axe else {
        return;
    };
    let shot = upright((f64::from(width), f64::from(height)));
    let mut sizes = host.points.lock();
    let tries = match sizes.get(udid) {
        None => 0,
        Some(Points::Known(w, h)) if upright((*w, *h)) != shot => 0,
        Some(Points::Failed { at, tries })
            if *tries < LEARN_TRIES && at.elapsed() > LEARN_RETRY * 2u32.pow(*tries - 1) =>
        {
            *tries
        }
        Some(_) => return,
    };
    sizes.insert(udid.to_owned(), Points::Learning { tries });
    drop(sizes);
    let (sizes, udid, children) = (host.points.clone(), udid.to_owned(), host.children.clone());
    std::thread::spawn(move || {
        let points = match describe(&children, &axe, &udid, Deadline::after(LEARN_LIMIT)) {
            Ok(size) if upright(size) == shot => Points::Known(size.0, size.1),
            Ok(_) => Points::Unreliable,
            Err(_) => Points::Failed {
                at: Instant::now(),
                tries: tries + 1,
            },
        };
        sizes.learned(&udid, points);
    });
}

fn describe(
    children: &Children,
    axe: &Path,
    udid: &str,
    until: Deadline,
) -> Result<(f64, f64), DeviceError> {
    let described = run_axe(children, axe, &["describe-ui", "--udid", udid], until)?;
    parse::axe_screen(&described)
        .ok_or_else(|| DeviceError::Failed("AXe did not report the simulator's screen size".into()))
}

/// The simulator's screen in points, which AXe taps in. A learn under way is waited
/// for rather than run beside, since a second AXe session at once is what makes the
/// simulator too slow to give either one.
fn points(
    host: &LocalHost,
    axe: &Path,
    udid: &str,
    until: Deadline,
) -> Result<(f64, f64), DeviceError> {
    let mut sizes = host.points.lock();
    let tries = loop {
        match sizes.get(udid).copied() {
            Some(Points::Known(width, height)) => return Ok((width, height)),
            Some(Points::Learning { .. }) if until.passed() => return Err(DeviceError::TimedOut),
            Some(Points::Learning { .. }) => {
                sizes = host
                    .points
                    .learned
                    .wait_timeout(sizes, until.remaining())
                    .unwrap()
                    .0;
            }
            Some(Points::Unreliable) => {
                drop(sizes);
                return describe(&host.children, axe, udid, until);
            }
            Some(Points::Failed { tries, .. }) => break tries,
            None => break 0,
        }
    };
    sizes.insert(udid.to_owned(), Points::Learning { tries });
    drop(sizes);
    let size = describe(&host.children, axe, udid, until);
    host.points.learned(
        udid,
        match size {
            Ok((width, height)) => Points::Known(width, height),
            Err(_) => Points::Failed {
                at: Instant::now(),
                tries: tries + 1,
            },
        },
    );
    size
}

fn coordinate(value: f64) -> String {
    format!("{}", value.round() as i64)
}

pub(super) fn act(
    host: &LocalHost,
    udid: &str,
    action: &Action,
    until: Deadline,
) -> Result<(), DeviceError> {
    match action {
        Action::Launch { app } => {
            parse::app_id(Platform::Ios, app).map_err(DeviceError::Invalid)?;
            simctl(host, &["launch", udid, app], until).map(drop)
        }
        Action::Tap(point) => {
            let axe = axe(host)?;
            let (width, height) = points(host, &axe, udid, until)?;
            let (x, y) = (coordinate(point.x * width), coordinate(point.y * height));
            run_axe(
                &host.children,
                &axe,
                &["tap", "-x", &x, "-y", &y, "--udid", udid],
                until,
            )
            .map(drop)
        }
        Action::Swipe { from, to } => {
            let axe = axe(host)?;
            let (width, height) = points(host, &axe, udid, until)?;
            let [x, y, to_x, to_y] =
                [from.x * width, from.y * height, to.x * width, to.y * height].map(coordinate);
            run_axe(
                &host.children,
                &axe,
                &[
                    "swipe",
                    "--start-x",
                    &x,
                    "--start-y",
                    &y,
                    "--end-x",
                    &to_x,
                    "--end-y",
                    &to_y,
                    "--udid",
                    udid,
                ],
                until,
            )
            .map(drop)
        }
        Action::Type(text) => {
            let axe = axe(host)?;
            if text.starts_with('-') || !text.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
                return Err(DeviceError::Invalid(
                    "AXe types printable ASCII only, not starting with -".into(),
                ));
            }
            run_axe(&host.children, &axe, &["type", text, "--udid", udid], until).map(drop)
        }
    }
}

pub(super) fn shutdown(host: &LocalHost, udid: &str, until: Deadline) -> Result<(), DeviceError> {
    host.points.forget(udid);
    match simctl(host, &["shutdown", udid], until) {
        Err(DeviceError::Failed(output)) if output.contains("current state: Shutdown") => Ok(()),
        result => result.map(drop),
    }
}
