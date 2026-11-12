use super::{
    Action, BootState, Deadline, DeviceError, DeviceHost, DeviceId, Frame, HostId, Inventory,
    Platform, Start, android, ios,
};
use crate::git::{self, Output, Until};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// The programs [`LocalHost`] runs. `None` when not found.
#[derive(Clone, Debug, Default)]
pub struct Tools {
    pub xcrun: Option<PathBuf>,
    pub adb: Option<PathBuf>,
    pub emulator: Option<PathBuf>,
    pub axe: Option<PathBuf>,
}

impl Tools {
    /// Each from its `KEYSTONE_<NAME>_PATH` override, else `PATH`, common install
    /// directories and the Android SDK's usual homes.
    pub fn find() -> Self {
        let sdk =
            |relative: &str| {
                let home = std::env::var_os("HOME").map(PathBuf::from);
                ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
                    .into_iter()
                    .filter_map(|name| std::env::var_os(name).map(PathBuf::from))
                    .chain(home.iter().flat_map(|home| {
                        [home.join("Library/Android/sdk"), home.join("Android/Sdk")]
                    }))
                    .map(|root| root.join(relative))
                    .find(|path| path.is_file())
            };
        Self {
            xcrun: overridden("KEYSTONE_XCRUN_PATH").unwrap_or_else(|| {
                cfg!(target_os = "macos")
                    .then(|| crate::find::program("xcrun"))
                    .flatten()
            }),
            adb: overridden("KEYSTONE_ADB_PATH").unwrap_or_else(|| {
                crate::find::program("adb").or_else(|| sdk("platform-tools/adb"))
            }),
            emulator: overridden("KEYSTONE_EMULATOR_PATH").unwrap_or_else(|| {
                crate::find::program("emulator").or_else(|| sdk("emulator/emulator"))
            }),
            axe: overridden("KEYSTONE_AXE_PATH").unwrap_or_else(|| crate::find::program("axe")),
        }
    }
}

/// `Some` when `name` is set: the file it names, or `None` when there is none, so an
/// override never falls back to another copy.
fn overridden(name: &str) -> Option<Option<PathBuf>> {
    let path = PathBuf::from(std::env::var_os(name).filter(|v| !v.is_empty())?);
    Some(path.is_file().then_some(path))
}

/// Simulators and emulators on this machine.
pub struct LocalHost {
    tools: RwLock<Tools>,
    /// Whether each listing looks for the tools again, so installing AXe or the SDK
    /// takes effect on the next Refresh.
    refind: bool,
    /// Emulators this host started, by AVD name, until adb reports them.
    pub(super) launched: Mutex<HashMap<String, android::Launch>>,
    /// Each emulator's latest screenshot size in pixels, by AVD name, which `input`
    /// taps in whatever the rotation.
    pub(super) screens: Mutex<HashMap<String, (u32, u32)>>,
    /// Each simulator's screen size in points, which AXe taps in.
    pub(super) points: Arc<ios::PointSizes>,
    /// The serial each AVD's latest guarded command ran at, where the emulator named
    /// that AVD, by AVD name.
    pub(super) pinned: Mutex<HashMap<String, String>>,
    /// AVDs whose image names no AVD, so a command can't check it in the same call.
    pub(super) blind: Mutex<std::collections::HashSet<String>>,
    /// Every command this host runs, so Quit can end the ones still running.
    pub(super) children: git::Children,
}

impl Default for LocalHost {
    fn default() -> Self {
        Self {
            refind: true,
            ..Self::new(Tools::find())
        }
    }
}

impl LocalHost {
    /// A host over exactly `tools`, never looking for others.
    pub fn new(tools: Tools) -> Self {
        Self {
            tools: RwLock::new(tools),
            refind: false,
            launched: Mutex::default(),
            screens: Mutex::default(),
            points: Arc::default(),
            pinned: Mutex::default(),
            blind: Mutex::default(),
            children: git::Children::default(),
        }
    }

    pub(super) fn tools(&self) -> Tools {
        self.tools.read().unwrap().clone()
    }
}

/// Runs `program` with `args` until it exits or `until` passes. Stdout is the result;
/// a failure reports the program's own words.
pub(super) fn run(
    children: &git::Children,
    program: &Path,
    args: &[&str],
    until: Deadline,
) -> Result<Vec<u8>, DeviceError> {
    if until.passed() {
        return Err(DeviceError::TimedOut);
    }
    let mut command = git::program(program, &std::env::temp_dir());
    command.args(args);
    git::run_until(
        command,
        None,
        Output::Separate,
        Until::Held(until.remaining(), children),
    )
    .map_err(|error| match error {
        _ if until.passed() => DeviceError::TimedOut,
        git::Error::Spawn(reason) => DeviceError::Failed(format!(
            "Couldn't run {}: {reason}",
            program.file_name().unwrap_or_default().to_string_lossy()
        )),
        error => DeviceError::Failed(error.to_string()),
    })
}

impl DeviceHost for LocalHost {
    fn id(&self) -> HostId {
        HostId::Local
    }

    fn inventory(&self, until: Deadline) -> Inventory {
        if self.refind {
            *self.tools.write().unwrap() = Tools::find();
        }
        let ((ios_devices, ios_status), (android_devices, android_status)) =
            std::thread::scope(|scope| {
                let ios = scope.spawn(|| ios::inventory(self, until));
                let android = android::inventory(self, until);
                (ios.join().expect("iOS listing panicked"), android)
            });
        Inventory {
            devices: ios_devices.into_iter().chain(android_devices).collect(),
            platforms: vec![ios_status, android_status],
        }
    }

    fn start(&self, device: &DeviceId, until: Deadline) -> Result<Start, DeviceError> {
        match device.platform {
            Platform::Ios => ios::start(self, &device.key, until),
            Platform::Android => android::start(self, &device.key, until),
        }
    }

    fn wait(&self, device: &DeviceId, until: Deadline) -> Result<BootState, DeviceError> {
        match device.platform {
            Platform::Ios => ios::wait(self, &device.key, until),
            Platform::Android => android::wait(self, &device.key, until),
        }
    }

    fn screenshot(&self, device: &DeviceId, until: Deadline) -> Result<Frame, DeviceError> {
        match device.platform {
            Platform::Ios => ios::screenshot(self, &device.key, until),
            Platform::Android => android::screenshot(self, &device.key, until),
        }
    }

    fn act(&self, device: &DeviceId, action: &Action, until: Deadline) -> Result<(), DeviceError> {
        match device.platform {
            Platform::Ios => ios::act(self, &device.key, action, until),
            Platform::Android => android::act(self, &device.key, action, until),
        }
    }

    fn shutdown(&self, device: &DeviceId, until: Deadline) -> Result<(), DeviceError> {
        match device.platform {
            Platform::Ios => ios::shutdown(self, &device.key, until),
            Platform::Android => android::shutdown(self, &device.key, until),
        }
    }

    fn stop(&self) {
        self.children.stop();
    }
}
