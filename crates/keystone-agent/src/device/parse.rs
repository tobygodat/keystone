//! Pure parsers for what the device tools print, shared by any host that runs them.

use super::{Device, DeviceId, Platform, Power};
use serde_json::Value;

/// The iOS Simulators in `simctl list devices available -j`, newest OS first.
pub(crate) fn simctl_devices(json: &[u8]) -> Result<Vec<Device>, String> {
    let value: Value = serde_json::from_slice(json)
        .map_err(|_| "simctl printed a device list Keystone can't read".to_owned())?;
    let runtimes = value["devices"]
        .as_object()
        .ok_or("simctl printed a device list Keystone can't read")?;
    let mut found = Vec::new();
    for (runtime, devices) in runtimes {
        let Some(os) = runtime_os(runtime) else {
            continue;
        };
        for device in devices.as_array().into_iter().flatten() {
            let (Some(udid), Some(name)) = (device["udid"].as_str(), device["name"].as_str())
            else {
                continue;
            };
            if device["isAvailable"] == false {
                continue;
            }
            let Ok(id) = format!("ios:{udid}").parse::<DeviceId>() else {
                continue;
            };
            let power = match device["state"].as_str() {
                Some("Booted") => Power::Booted,
                Some("Booting") => Power::Booting,
                _ => Power::Shutdown,
            };
            found.push((
                version(&os),
                Device {
                    id,
                    name: name.to_owned(),
                    os: os.clone(),
                    power,
                },
            ));
        }
    }
    found.sort_by(|(a, x), (b, y)| b.cmp(a).then_with(|| x.name.cmp(&y.name)));
    Ok(found.into_iter().map(|(_, device)| device).collect())
}

/// `iOS 27.0` from `com.apple.CoreSimulator.SimRuntime.iOS-27-0`; `None` for watchOS,
/// tvOS and visionOS runtimes.
fn runtime_os(runtime: &str) -> Option<String> {
    let name = runtime.rsplit('.').next()?;
    let mut parts = name.split('-');
    if parts.next()? != "iOS" {
        return None;
    }
    let numbers: Vec<_> = parts
        .take_while(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        .take(2)
        .collect();
    Some(if numbers.is_empty() {
        "iOS".to_owned()
    } else {
        format!("iOS {}", numbers.join("."))
    })
}

fn version(os: &str) -> Vec<u32> {
    os.split([' ', '.'])
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// AVD names from `emulator -list-avds`, one per line. Lines that are not names, such
/// as the emulator's own `INFO` chatter, are skipped.
pub(crate) fn avds(output: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && line
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        .map(str::to_owned)
        .collect()
}

/// One emulator in `adb devices -l`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Listed {
    pub(crate) serial: String,
    pub(crate) online: bool,
}

/// The emulators in `adb devices -l`. Physical devices are left out.
pub(crate) fn adb_emulators(output: &[u8]) -> Vec<Listed> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let serial = fields.next()?;
            let state = fields.next()?;
            let port = serial.strip_prefix("emulator-")?;
            (!port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())).then(|| Listed {
                serial: serial.to_owned(),
                online: state == "device",
            })
        })
        .collect()
}

/// The AVD name `adb -s <serial> emu avd name` prints before its `OK`.
pub(crate) fn avd_name(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let name = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    (name != "OK" && !name.starts_with("KO")).then(|| name.to_owned())
}

/// The screen size in pixels from `adb shell wm size`, an override first.
pub(crate) fn wm_size(output: &[u8]) -> Option<(u32, u32)> {
    let text = String::from_utf8_lossy(output);
    let size = |prefix: &str| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(prefix))
            .and_then(|size| size.trim().split_once('x'))
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            .filter(|&(w, h): &(u32, u32)| w > 0 && h > 0)
    };
    size("Override size:").or_else(|| size("Physical size:"))
}

/// The two AVD names an emulator printed, from `ro.boot.qemu.avd_name` and then
/// `ro.kernel.qemu.avd_name`, each on a line of its own and either empty, and the bytes
/// its command printed after them. `None` when the two lines aren't there.
pub(crate) fn named(output: &[u8]) -> Option<([String; 2], Vec<u8>)> {
    let mut rest = output;
    let mut names = [String::new(), String::new()];
    for name in &mut names {
        let end = rest.iter().position(|&b| b == b'\n')?;
        *name = String::from_utf8_lossy(&rest[..end]).trim().to_owned();
        rest = &rest[end + 1..];
    }
    Some((names, rest.to_vec()))
}

/// A PNG's width and height from its header.
pub(crate) fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

/// The screen's size in points: the frame of the first element `axe describe-ui`
/// prints, which is the application or the springboard filling the screen.
pub(crate) fn axe_screen(json: &[u8]) -> Option<(f64, f64)> {
    let value: Value = serde_json::from_slice(json).ok()?;
    let root = match &value {
        Value::Array(elements) => elements.first()?,
        object => object,
    };
    let frame = &root["frame"];
    let width = frame["width"].as_f64()?;
    let height = frame["height"].as_f64()?;
    (width >= 1.0 && height >= 1.0).then_some((width, height))
}

/// `text` as one argument for `adb shell input text`, which runs through the device's
/// shell and reads `%s` as a space. Text it cannot carry is refused, never altered.
pub(crate) fn android_text(text: &str) -> Result<String, String> {
    if text.contains("%s") {
        return Err("Android can't type the characters %s; type them separately".into());
    }
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        match c {
            ' ' => out.push_str("%s"),
            'a'..='z'
            | 'A'..='Z'
            | '0'..='9'
            | '.'
            | ','
            | '-'
            | '_'
            | '/'
            | ':'
            | '@'
            | '+'
            | '='
            | '%' => out.push(c),
            '!' | '"' | '#' | '$' | '&' | '\'' | '(' | ')' | '*' | ';' | '<' | '>' | '?' | '['
            | '\\' | ']' | '^' | '`' | '{' | '|' | '}' | '~' => {
                out.push('\\');
                out.push(c);
            }
            _ => {
                return Err(format!(
                    "Android's input text carries only printable ASCII; it can't type {c:?}"
                ));
            }
        }
    }
    Ok(out)
}

/// What an app id may contain on `platform`, so it never reaches a shell as anything
/// but a name.
pub(crate) fn app_id(platform: Platform, app: &str) -> Result<(), String> {
    let allowed = |b: u8| match platform {
        Platform::Ios => b.is_ascii_alphanumeric() || b"-.".contains(&b),
        Platform::Android => b.is_ascii_alphanumeric() || b"._".contains(&b),
    };
    if app.is_empty() || app.starts_with('-') || app.len() > 256 || !app.bytes().all(allowed) {
        return Err(match platform {
            Platform::Ios => "app must be a bundle id, such as com.apple.Preferences".into(),
            Platform::Android => "app must be a package name, such as com.android.settings".into(),
        });
    }
    Ok(())
}
