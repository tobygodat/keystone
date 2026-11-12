//! iOS Simulators and Android Emulators behind one host interface. [`LocalHost`] runs
//! them on this machine through `xcrun simctl`, `adb`, `emulator` and AXe; a remote host
//! implements the same [`DeviceHost`] calls over its own transport and reuses the
//! parsers. See `docs/design/devices.md`.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

mod android;
mod ios;
mod local;
mod parse;

pub use local::{LocalHost, Tools};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub enum Platform {
    Ios,
    Android,
}

impl Platform {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ios => "iOS",
            Self::Android => "Android",
        }
    }
}

/// `ios:<udid>` or `android:<avd>`, the same across boots. An emulator's serial stays
/// inside the host.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct DeviceId {
    pub platform: Platform,
    pub key: String,
}

impl FromStr for DeviceId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let (platform, key) = text
            .split_once(':')
            .ok_or("A device id looks like ios:<udid> or android:<avd>")?;
        let platform = match platform {
            "ios" => Platform::Ios,
            "android" => Platform::Android,
            _ => return Err("A device id starts with ios: or android:".into()),
        };
        // A key that starts with - would reach adb, emulator or simctl as an option.
        if key.is_empty()
            || key.starts_with('-')
            || key.len() > 128
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err("Unknown device id; take one from device_list".into());
        }
        Ok(Self {
            platform,
            key: key.to_owned(),
        })
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.platform.slug(), self.key)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Power {
    Shutdown,
    Booting,
    Booted,
}

impl Power {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Booting => "booting",
            Self::Booted => "booted",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub os: String,
    pub power: Power,
}

/// Whether a platform's devices can be tapped, swiped and typed into.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Input {
    Available { tool: &'static str },
    Unavailable { reason: String },
}

/// Whether a platform's devices can be listed and run here at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Support {
    Available,
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformStatus {
    pub platform: Platform,
    pub support: Support,
    pub input: Input,
}

/// Everything one listing found. One platform failing never hides the other.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Inventory {
    pub devices: Vec<Device>,
    pub platforms: Vec<PlatformStatus>,
}

impl Inventory {
    pub fn device(&self, id: &DeviceId) -> Option<&Device> {
        self.devices.iter().find(|d| &d.id == id)
    }

    pub fn platform(&self, platform: Platform) -> Option<&PlatformStatus> {
        self.platforms.iter().find(|p| p.platform == platform)
    }
}

/// A point as a fraction of the screen, 0 to 1 on each axis, so callers and remote
/// hosts never deal in points or pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    /// `None` unless both coordinates lie on the screen.
    pub fn new(x: f64, y: f64) -> Option<Self> {
        ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some(Self { x, y })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// A bundle id (iOS) or package name (Android).
    Launch {
        app: String,
    },
    Tap(Point),
    Swipe {
        from: Point,
        to: Point,
    },
    Type(String),
}

/// One captured screen: PNG bytes and their size in pixels.
#[derive(Clone)]
pub struct Frame {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Frame({}x{}, {} bytes)",
            self.width,
            self.height,
            self.png.len()
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Deadline(pub Instant);

impl Deadline {
    pub fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    pub fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    pub fn passed(self) -> bool {
        self.remaining().is_zero()
    }
}

/// What [`DeviceHost::start`] found or did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Start {
    /// It was already booted.
    Booted,
    /// It was already booting, by the user, another app or an earlier call.
    Booting,
    /// This call started the boot.
    Started,
}

/// Where a device stood when [`DeviceHost::wait`] returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootState {
    Booted,
    /// The deadline came first; the boot carries on without the caller.
    Booting,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceError {
    UnknownDevice,
    NotBooted,
    /// The platform's tools are missing; says which and how to get them.
    Unsupported(String),
    /// No input tool for this platform; names it and how to install it.
    InputUnavailable(String),
    Invalid(String),
    Failed(String),
    TimedOut,
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDevice => f.write_str("No such device here; take an id from device_list"),
            Self::NotBooted => f.write_str("The device is not running; open it first"),
            Self::Unsupported(reason)
            | Self::InputUnavailable(reason)
            | Self::Invalid(reason)
            | Self::Failed(reason) => f.write_str(reason),
            Self::TimedOut => f.write_str("The device did not answer in time"),
        }
    }
}

impl std::error::Error for DeviceError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HostId {
    Local,
}

impl HostId {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Local => "local",
        }
    }
}

/// What runs devices. Each call blocks until done or `until`, so callers run them on
/// worker threads.
pub trait DeviceHost: Send + Sync + 'static {
    fn id(&self) -> HostId;
    fn inventory(&self, until: Deadline) -> Inventory;
    /// Starts `device` booting unless it already runs, without waiting for the boot.
    fn start(&self, device: &DeviceId, until: Deadline) -> Result<Start, DeviceError>;
    /// Waits for `device` to finish booting, or reports it still booting at `until`. A
    /// device that is not running nor starting is [`DeviceError::NotBooted`].
    fn wait(&self, device: &DeviceId, until: Deadline) -> Result<BootState, DeviceError>;
    fn screenshot(&self, device: &DeviceId, until: Deadline) -> Result<Frame, DeviceError>;
    fn act(&self, device: &DeviceId, action: &Action, until: Deadline) -> Result<(), DeviceError>;
    fn shutdown(&self, device: &DeviceId, until: Deadline) -> Result<(), DeviceError>;
    /// Ends every command still running, with whatever each started, and refuses new
    /// ones. Quit calls it last, so nothing Keystone ran outlives it.
    fn stop(&self);
}

#[cfg(test)]
mod tests;
