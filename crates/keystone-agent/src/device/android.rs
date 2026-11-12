//! Android Emulators through the SDK's `emulator` and `adb`. Devices are named by AVD,
//! and each running emulator's serial is found again from `adb devices`.

use super::local::{LocalHost, run};
use super::{
    Action, BootState, Deadline, Device, DeviceError, DeviceId, Frame, Input, Platform,
    PlatformStatus, Point, Power, Start, Support, parse,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const NO_ADB: &str =
    "adb was not found. Install Android Studio's platform-tools, or set KEYSTONE_ADB_PATH.";
const NO_EMULATOR: &str = "The Android emulator was not found. Install it with Android Studio, or set KEYSTONE_EMULATOR_PATH.";
/// How long an emulator this host started counts as booting before adb lists it.
const LAUNCH_GRACE: Duration = Duration::from_secs(120);
/// How often the emulator this host started is checked for having exited.
const REAP_PAUSE: Duration = Duration::from_millis(250);

/// An emulator this host started and adb may not list yet.
#[derive(Clone)]
pub(crate) struct Launch {
    at: Instant,
    /// Shared with the thread that notes its exit. Both only `try_wait`, so the process
    /// is never reaped between checking it runs and signalling it.
    child: Arc<Mutex<Child>>,
    /// Why the emulator exited, once it has.
    exit: Arc<OnceLock<String>>,
}

/// The running emulators adb lists, by the AVD each runs.
struct Running {
    by_avd: HashMap<String, (String, bool)>,
    /// Why an emulator adb lists could not be asked its AVD, if one could not.
    unnamed: Option<DeviceError>,
}

impl Running {
    /// `avd`'s serial and whether adb has it online, or `None` when adb lists no
    /// emulator running it. An emulator that could not be asked might be it, so then
    /// the answer is that error, never that it is not running.
    fn find(&self, avd: &str) -> Result<Option<(String, bool)>, DeviceError> {
        match (self.by_avd.get(avd), &self.unnamed) {
            (Some(found), _) => Ok(Some(found.clone())),
            (None, Some(error)) => Err(error.clone()),
            (None, None) => Ok(None),
        }
    }
}

fn adb(host: &LocalHost) -> Result<PathBuf, DeviceError> {
    host.tools()
        .adb
        .ok_or_else(|| DeviceError::Unsupported(NO_ADB.into()))
}

fn emulator(host: &LocalHost) -> Result<PathBuf, DeviceError> {
    host.tools()
        .emulator
        .ok_or_else(|| DeviceError::Unsupported(NO_EMULATOR.into()))
}

fn avds(host: &LocalHost, until: Deadline) -> Result<Vec<String>, DeviceError> {
    adb(host)?;
    Ok(parse::avds(&run(
        &host.children,
        &emulator(host)?,
        &["-list-avds"],
        until,
    )?))
}

fn avd_at(
    host: &LocalHost,
    adb: &Path,
    serial: &str,
    until: Deadline,
) -> Result<String, DeviceError> {
    let output = run(
        &host.children,
        adb,
        &["-s", serial, "emu", "avd", "name"],
        until,
    )?;
    parse::avd_name(&output).ok_or_else(|| {
        DeviceError::Failed(format!(
            "The emulator at {serial} did not say which AVD it runs"
        ))
    })
}

/// The emulators adb lists, each named by asking its console which AVD it runs. Nothing
/// of it is kept: an adb server that restarts hands serials and transport ids out again
/// from the start, so a name learned for one can belong to another emulator later.
fn running(host: &LocalHost, until: Deadline) -> Result<Running, DeviceError> {
    let adb = adb(host)?;
    let listed = parse::adb_emulators(&run(&host.children, &adb, &["devices", "-l"], until)?);
    let mut found = Running {
        by_avd: HashMap::new(),
        unnamed: None,
    };
    for emulator in listed {
        match avd_at(host, &adb, &emulator.serial, until) {
            Ok(name) => {
                found
                    .by_avd
                    .insert(name, (emulator.serial, emulator.online));
            }
            // adb keeps listing an emulator that died as offline, and its console port
            // is closed. Nothing runs there.
            Err(DeviceError::Failed(output))
                if !emulator.online && output.contains("Connection refused") => {}
            Err(DeviceError::Failed(output)) if !emulator.online => {
                found.unnamed = Some(DeviceError::Failed(format!(
                    "adb lists {} as offline and it didn't say which AVD it runs: {output}. If no emulator is running, `adb kill-server` clears the entry.",
                    emulator.serial
                )));
            }
            Err(error) => found.unnamed = Some(error),
        }
    }
    Ok(found)
}

/// What the emulator at a serial did with a command guarded by its AVD.
enum Guarded {
    /// It ran the command, which printed this. `named` says it named the AVD asked for;
    /// otherwise its image names none and the guard was told to trust that.
    Ran { printed: Vec<u8>, named: bool },
    /// It names another AVD, so nothing ran.
    Other(String),
    /// Its image names no AVD, so nothing ran and it has to be asked another way.
    Unnamed,
}

/// Runs `command` in the shell of the emulator at `serial` only if that emulator says
/// it runs `avd`, in one adb call, so an emulator that took the serial never gets it.
/// AVD names are checked to be plain names before they get here.
fn guarded(
    host: &LocalHost,
    serial: &str,
    avd: &str,
    command: &str,
    unnamed: Unnamed,
    until: Deadline,
) -> Result<Guarded, DeviceError> {
    let or_unnamed = match unnamed {
        Unnamed::Runs => "[ -z \"$a$k\" ] || ",
        Unnamed::Waits => "",
    };
    let script = format!(
        "a=$(getprop ro.boot.qemu.avd_name); k=$(getprop ro.kernel.qemu.avd_name); \
         echo \"$a\"; echo \"$k\"; \
         if {or_unnamed}[ \"$a\" = '{avd}' ] || [ \"$k\" = '{avd}' ]; then {command}; fi"
    );
    let output = run(
        &host.children,
        &adb(host)?,
        &["-s", serial, "exec-out", &script],
        until,
    )?;
    let ([boot, kernel], printed) = parse::named(&output).ok_or_else(|| {
        DeviceError::Failed(format!(
            "The emulator at {serial} didn't say which AVD it runs"
        ))
    })?;
    let named = boot == avd || kernel == avd;
    Ok(
        if named || (boot.is_empty() && kernel.is_empty() && unnamed == Unnamed::Runs) {
            Guarded::Ran { printed, named }
        } else if boot.is_empty() && kernel.is_empty() {
            Guarded::Unnamed
        } else {
            Guarded::Other(if boot.is_empty() { kernel } else { boot })
        },
    )
}

/// What a guard does at an emulator whose image names no AVD.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unnamed {
    /// Runs the command: its console named the AVD just before.
    Runs,
    /// Runs nothing.
    Waits,
}

/// Why the emulator at `serial`, which its console says runs the AVD asked for, names
/// `name` instead: another AVD took the serial, or its image names its AVD by a name
/// that is no AVD's, which nothing Keystone does can match.
fn named_otherwise(host: &LocalHost, serial: &str, name: &str, until: Deadline) -> DeviceError {
    match avds(host, until) {
        Ok(avds) if avds.iter().any(|known| known == name) => DeviceError::NotBooted,
        _ => DeviceError::Failed(format!(
            "The emulator at {serial} calls itself {name}, which Keystone can't match to an AVD."
        )),
    }
}

/// Runs `command` on the emulator running `avd`, which says so in the same call: at
/// the serial its last command used, else where adb lists it now. An image that names
/// no AVD can't say so in the call, so its command runs where adb's listing, which asked
/// each emulator's console just before, found it.
fn on_avd(
    host: &LocalHost,
    avd: &str,
    command: &str,
    until: Deadline,
) -> Result<Vec<u8>, DeviceError> {
    let plain = |serial: &str| {
        run(
            &host.children,
            &adb(host)?,
            &["-s", serial, "exec-out", command],
            until,
        )
    };
    let blind = host.blind.lock().unwrap().contains(avd);
    let pinned = host.pinned.lock().unwrap().get(avd).cloned();
    if let Some(serial) = pinned.filter(|_| !blind) {
        match guarded(host, &serial, avd, command, Unnamed::Waits, until) {
            Ok(Guarded::Ran { printed, .. }) => return Ok(printed),
            Err(DeviceError::TimedOut) => return Err(DeviceError::TimedOut),
            _ => {
                host.pinned.lock().unwrap().remove(avd);
            }
        }
    }
    let serial = lookup(host, avd, until)?;
    // An image seen naming no AVD may name it now, as one early in its boot does, so it
    // is asked each time and leaves the three-call path the first time it says.
    let unnamed = if blind { Unnamed::Runs } else { Unnamed::Waits };
    match guarded(host, &serial, avd, command, unnamed, until)? {
        Guarded::Ran { printed, named } => {
            if named {
                host.blind.lock().unwrap().remove(avd);
                host.pinned.lock().unwrap().insert(avd.to_owned(), serial);
            }
            Ok(printed)
        }
        Guarded::Unnamed => {
            host.blind.lock().unwrap().insert(avd.to_owned());
            plain(&serial)
        }
        // Its console named `avd` a moment ago, and now it names something else.
        Guarded::Other(name) => Err(named_otherwise(host, &serial, &name, until)),
    }
}

/// Whether `avd`, which adb lists at `serial`, has finished booting, as the emulator
/// there says when it names `avd`.
/// An emulator whose image names its AVD by a name that is no AVD's is an error, so a
/// boot wait ends with why rather than waiting for a name that never comes.
fn booted(host: &LocalHost, avd: &str, serial: &str, until: Deadline) -> Result<bool, DeviceError> {
    let done = |printed: &[u8]| String::from_utf8_lossy(printed).trim() == "1";
    Ok(
        match guarded(
            host,
            serial,
            avd,
            "getprop sys.boot_completed",
            Unnamed::Runs,
            until,
        ) {
            Ok(Guarded::Ran { printed, .. }) => done(&printed),
            Ok(Guarded::Other(name)) => match named_otherwise(host, serial, &name, until) {
                DeviceError::NotBooted => false,
                error => return Err(error),
            },
            Ok(Guarded::Unnamed) | Err(_) => false,
        },
    )
}

/// Whether this host started `avd` lately and it has not exited.
fn launching(host: &LocalHost, avd: &str) -> bool {
    host.launched
        .lock()
        .unwrap()
        .get(avd)
        .is_some_and(|launch| launch.at.elapsed() < LAUNCH_GRACE && launch.exit.get().is_none())
}

/// Why the emulator this host started for `avd` exited, if it has.
fn exited(host: &LocalHost, avd: &str) -> Option<String> {
    let launched = host.launched.lock().unwrap();
    launched.get(avd)?.exit.get().cloned()
}

pub(super) fn inventory(host: &LocalHost, until: Deadline) -> (Vec<Device>, PlatformStatus) {
    let input = match host.tools().adb {
        Some(_) => Input::Available { tool: "adb" },
        None => Input::Unavailable {
            reason: NO_ADB.into(),
        },
    };
    let listed = avds(host, until).and_then(|avds| {
        let running = running(host, until)?;
        // An AVD no listed emulator was named for might run in the one that couldn't
        // say, so none of them is reported off.
        match &running.unnamed {
            Some(error)
                if avds
                    .iter()
                    .any(|avd| !running.by_avd.contains_key(avd) && !launching(host, avd)) =>
            {
                Err(error.clone())
            }
            _ => Ok((avds, running)),
        }
    });
    let (devices, support) = match listed {
        Ok((avds, running)) => {
            let devices = avds
                .into_iter()
                .map(|avd| {
                    let power = match running.by_avd.get(&avd) {
                        Some((serial, true))
                            if booted(host, &avd, serial, until).unwrap_or(false) =>
                        {
                            Power::Booted
                        }
                        Some(_) => Power::Booting,
                        None if launching(host, &avd) => Power::Booting,
                        None => Power::Shutdown,
                    };
                    Device {
                        id: DeviceId {
                            platform: Platform::Android,
                            key: avd.clone(),
                        },
                        name: avd.replace('_', " "),
                        os: "Android".into(),
                        power,
                    }
                })
                .collect();
            (devices, Support::Available)
        }
        Err(error @ DeviceError::Unsupported(_)) => (
            Vec::new(),
            Support::Unavailable {
                reason: error.to_string(),
            },
        ),
        Err(error) => (
            Vec::new(),
            Support::Unavailable {
                reason: format!("Couldn't list Android Emulators: {error}"),
            },
        ),
    };
    (
        devices,
        PlatformStatus {
            platform: Platform::Android,
            support,
            input,
        },
    )
}

/// Starts `avd` in its own session, so it outlives the call and Keystone. Its output
/// goes to a file, whose last line says why it exited if it does.
fn launch(host: &LocalHost, avd: &str) -> Result<(), DeviceError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let log_path = std::env::temp_dir().join(format!(
        "keystone-emulator-{}-{}-{avd}.log",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let log = std::fs::File::create(&log_path)
        .map_err(|e| DeviceError::Failed(format!("Couldn't start the emulator: {e}")))?;
    let mut command = crate::git::program(&emulator(host)?, &std::env::temp_dir());
    command
        .args(["-avd", avd, "-no-window", "-no-audio", "-no-boot-anim"])
        .stdout(Stdio::from(log.try_clone().map_err(|e| {
            DeviceError::Failed(format!("Couldn't start the emulator: {e}"))
        })?))
        .stderr(Stdio::from(log));
    let child = command
        .spawn()
        .map_err(|e| DeviceError::Failed(format!("Couldn't start the emulator: {e}")))?;
    let launch = Launch {
        at: Instant::now(),
        child: Arc::new(Mutex::new(child)),
        exit: Arc::new(OnceLock::new()),
    };
    host.launched
        .lock()
        .unwrap()
        .insert(avd.to_owned(), launch.clone());
    let Launch { child, exit, .. } = launch;
    std::thread::spawn(move || {
        let status = loop {
            match child.lock().unwrap().try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(error) => break Err(error),
            }
            std::thread::sleep(REAP_PAUSE);
        };
        let output = std::fs::read_to_string(&log_path).unwrap_or_default();
        let _ = std::fs::remove_file(&log_path);
        let last = output.lines().map(str::trim).rfind(|line| !line.is_empty());
        let _ = exit.set(match (last, status) {
            (Some(line), _) => format!("The emulator exited: {line}"),
            (None, Ok(status)) => format!("The emulator exited ({status})"),
            (None, Err(error)) => format!("The emulator exited: {error}"),
        });
    });
    Ok(())
}

pub(super) fn start(host: &LocalHost, avd: &str, until: Deadline) -> Result<Start, DeviceError> {
    if !avds(host, until)?.iter().any(|known| known == avd) {
        return Err(DeviceError::UnknownDevice);
    }
    let running = running(host, until)?;
    match running.by_avd.get(avd) {
        Some((serial, true)) => Ok(if booted(host, avd, serial, until)? {
            Start::Booted
        } else {
            Start::Booting
        }),
        Some(_) => Ok(Start::Booting),
        None if launching(host, avd) => Ok(Start::Booting),
        // An emulator that couldn't say might run it, and one launched beside it would
        // be taken for Keystone's.
        None => match running.unnamed {
            Some(error) => Err(error),
            None => {
                launch(host, avd)?;
                Ok(Start::Started)
            }
        },
    }
}

pub(super) fn wait(host: &LocalHost, avd: &str, until: Deadline) -> Result<BootState, DeviceError> {
    loop {
        let running = match running(host, until) {
            Err(_) if until.passed() => return Ok(BootState::Booting),
            running => running?,
        };
        match running.find(avd) {
            Ok(Some((serial, true))) if booted(host, avd, &serial, until)? => {
                host.launched.lock().unwrap().remove(avd);
                return Ok(BootState::Booted);
            }
            Err(_) if let Some(reason) = exited(host, avd) => {
                return Err(DeviceError::Failed(reason));
            }
            // Booting, or an emulator that might be it couldn't say yet.
            Ok(Some(_)) | Err(_) => {}
            Ok(None) if launching(host, avd) => {}
            Ok(None) => {
                return Err(exited(host, avd).map_or(DeviceError::NotBooted, DeviceError::Failed));
            }
        }
        if until.remaining() < Duration::from_millis(500) {
            return Ok(BootState::Booting);
        }
        std::thread::sleep(until.remaining().min(Duration::from_secs(1)));
    }
}

/// The serial adb lists `avd` at, each emulator's console asked just now. `NotBooted`
/// only when adb lists no emulator running it.
fn lookup(host: &LocalHost, avd: &str, until: Deadline) -> Result<String, DeviceError> {
    running(host, until)?
        .find(avd)?
        .map(|(serial, _)| serial)
        .ok_or(DeviceError::NotBooted)
}

/// The screen of `avd`, taken by the emulator that says in the same call that it runs
/// `avd`, so a serial another emulator took never gives its screen.
pub(super) fn screenshot(
    host: &LocalHost,
    avd: &str,
    until: Deadline,
) -> Result<Frame, DeviceError> {
    let png = on_avd(host, avd, "screencap -p", until)?;
    frame(host, avd, png)
}

fn frame(host: &LocalHost, avd: &str, png: Vec<u8>) -> Result<Frame, DeviceError> {
    let (width, height) = parse::png_size(&png)
        .ok_or_else(|| DeviceError::Failed("adb returned a screenshot that isn't a PNG".into()))?;
    host.screens
        .lock()
        .unwrap()
        .insert(avd.to_owned(), (width, height));
    Ok(Frame { png, width, height })
}

fn pixels(point: Point, (width, height): (u32, u32)) -> [String; 2] {
    [
        (point.x * f64::from(width - 1)).round(),
        (point.y * f64::from(height - 1)).round(),
    ]
    .map(|v| format!("{}", v as i64))
}

/// The screen in `input`'s pixels: the latest screenshot's size, which turns with the
/// screen, else `wm size`.
fn screen(host: &LocalHost, avd: &str, until: Deadline) -> Result<(u32, u32), DeviceError> {
    if let Some(&size) = host.screens.lock().unwrap().get(avd) {
        return Ok(size);
    }
    parse::wm_size(&on_avd(host, avd, "wm size", until)?)
        .ok_or_else(|| DeviceError::Failed("adb did not report the screen size".into()))
}

/// Acts on `avd` through [`on_avd`], so input reaches only the emulator that says in
/// the same call that it runs `avd`. Arguments are checked before anything runs.
pub(super) fn act(
    host: &LocalHost,
    avd: &str,
    action: &Action,
    until: Deadline,
) -> Result<(), DeviceError> {
    let command = match action {
        Action::Launch { app } => {
            parse::app_id(Platform::Android, app).map_err(DeviceError::Invalid)?;
            format!("monkey -p {app} -c android.intent.category.LAUNCHER 1")
        }
        Action::Type(text) => {
            let escaped = parse::android_text(text).map_err(DeviceError::Invalid)?;
            format!("input text {escaped}")
        }
        Action::Tap(point) => {
            let [x, y] = pixels(*point, screen(host, avd, until)?);
            format!("input tap {x} {y}")
        }
        Action::Swipe { from, to } => {
            let size = screen(host, avd, until)?;
            let [x, y] = pixels(*from, size);
            let [to_x, to_y] = pixels(*to, size);
            format!("input swipe {x} {y} {to_x} {to_y} 300")
        }
    };
    let printed = String::from_utf8_lossy(&on_avd(host, avd, &command, until)?).into_owned();
    if let Action::Launch { app } = action
        && (printed.contains("No activities found") || printed.contains("monkey aborted"))
    {
        return Err(DeviceError::Failed(format!(
            "No launchable app {app} on this emulator"
        )));
    }
    Ok(())
}

pub(super) fn shutdown(host: &LocalHost, avd: &str, until: Deadline) -> Result<(), DeviceError> {
    host.screens.lock().unwrap().remove(avd);
    host.pinned.lock().unwrap().remove(avd);
    host.blind.lock().unwrap().remove(avd);
    let launched = host.launched.lock().unwrap().get(avd).cloned();
    // The lookup asks each emulator's console, so one that took a freed serial and port
    // answers with its own AVD and is never shut down for `avd`.
    match (lookup(host, avd, until), launched) {
        (Ok(serial), _) => {
            run(
                &host.children,
                &adb(host)?,
                &["-s", &serial, "emu", "kill"],
                until,
            )?;
            host.launched.lock().unwrap().remove(avd);
            Ok(())
        }
        // adb can't name it yet, but this host started it, so its process goes.
        (Err(_), Some(launch)) if launch.exit.get().is_none() => {
            stop(&launch, until)?;
            host.launched.lock().unwrap().remove(avd);
            Ok(())
        }
        (Err(DeviceError::NotBooted), _) => Ok(()),
        (Err(error), _) => Err(error),
    }
}

/// Stops the emulator `launch` started, with whatever it started, and returns once it
/// has exited: asked first, then killed when a second is left.
fn stop(launch: &Launch, until: Deadline) -> Result<(), DeviceError> {
    let signal = |signal| {
        let mut child = launch.child.lock().unwrap();
        if matches!(child.try_wait(), Ok(None)) {
            // The emulator leads its own process group, so qemu goes with it.
            // SAFETY: kill has no memory effects, and the child is not reaped yet.
            unsafe { libc::kill(-(child.id() as libc::pid_t), signal) };
        }
    };
    signal(libc::SIGTERM);
    let mut killed = false;
    loop {
        if !matches!(launch.child.lock().unwrap().try_wait(), Ok(None)) {
            return Ok(());
        }
        if until.passed() {
            return Err(DeviceError::Failed(
                "The emulator Keystone started did not exit in time".into(),
            ));
        }
        if !killed && until.remaining() <= Duration::from_secs(1) {
            signal(libc::SIGKILL);
            killed = true;
        }
        std::thread::sleep(until.remaining().min(Duration::from_millis(50)));
    }
}
