use super::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const UDID: &str = "2DDB4799-A985-431E-B16B-09FA92673ACA";

/// A PNG header naming `width` by `height`, enough for the parsers.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.extend([8, 6, 0, 0, 0]);
    bytes
}

struct Fakes {
    dir: tempfile::TempDir,
}

impl Fakes {
    fn new() -> Self {
        let fakes = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::write(fakes.path("list.json"), simctl_list("Shutdown")).unwrap();
        std::fs::write(fakes.path("booting.json"), simctl_list("Booting")).unwrap();
        std::fs::write(fakes.path("frame.png"), png(1206, 2622)).unwrap();
        fakes
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path(name);
        let dir = self.dir.path().display();
        keystone_core::test_support::write_executable(
            &path,
            format!(
                "#!/bin/sh\nD='{dir}'\necho \"{name} $*\" >> \"$D/log\"\n{}\n",
                body
            ),
        );
        path
    }

    fn xcrun(&self) -> PathBuf {
        self.script(
            "xcrun",
            r#"case "$1 $2" in
  "simctl list") cat "$D/list.json" ;;
  "simctl boot") cp "$D/booting.json" "$D/list.json" ;;
  "simctl bootstatus") while [ ! -f "$D/ready" ]; do sleep 0.05; done ;;
  "simctl io")
    if [ -f "$D/hang" ]; then sleep 30; fi
    if [ -f "$D/off" ]; then echo "Timeout waiting for screen surfaces" >&2; exit 60; fi
    cp "$D/frame.png" "$6"; if [ -f "$D/fail" ]; then echo "Capture failed" >&2; exit 1; fi ;;
esac"#,
        )
    }

    fn axe(&self) -> PathBuf {
        self.script(
            "axe",
            r#"if [ -f "$D/axe-slow" ]; then sleep 0.4; fi
if [ "$1" = tap ] && [ ! -f "$D/axe-ready" ]; then
  [ -f "$D/axe-flaky" ] || touch "$D/axe-ready"
  echo "Timed out creating the simulator remote automation session" >&2; exit 1
fi
if [ "$1" = describe-ui ]; then
  if [ -f "$D/describing" ]; then echo "two AXe sessions at once" >&2; exit 1; fi
  touch "$D/describing"; sleep 0.3; rm "$D/describing"
  if [ -f "$D/describe-fails" ]; then echo "AXe could not describe the screen" >&2; exit 1; fi
  if [ -f "$D/landscape" ]; then echo '[{"frame":{"x":0,"y":0,"width":874,"height":402}}]'
  else echo '[{"frame":{"x":0,"y":0,"width":402,"height":874},"children":[]}]'; fi
fi
exit 0"#,
        )
    }

    /// adb with one emulator, `emulator-5554`, running Pixel_8, or Tablet_API_35 over a
    /// new connection once `swapped` exists, or over the same serial and transport id
    /// once `reused` exists, as after an adb server restart. Tablet_API_35's screen is
    /// `tablet.png`. `liar` makes the image name its AVD sdk_gphone64_arm64, which is no
    /// AVD's. `exec-fails` fails every shell command, and `garbage` makes the
    /// screenshot something other than a PNG. `unnamed` makes its console
    /// unreachable. `stale` adds an offline `emulator-5556` whose console fails with that
    /// file's text. `noprops` makes the image set no AVD property. `hang-shot` makes a
    /// screenshot hang, writing its pid to `shot.pid`. `own-listed` adds the emulator
    /// Keystone launched, running Tablet_API_35 at the port it was given, and
    /// `hang-devices` makes `devices -l` hang. Commands run through a stand-in device
    /// shell, and whatever reaches a device goes to `device.log` under its AVD.
    fn adb(&self) -> PathBuf {
        self.script(
            "adb",
            r#"if [ -f "$D/swapped" ] || [ -f "$D/reused" ]; then cur=Tablet_API_35; else cur=Pixel_8; fi
serial=; if [ "$1" = -s ]; then serial=$2; shift 2; fi
own=; if [ -f "$D/own-listed" ] && [ -f "$D/port" ]; then own=emulator-$(cat "$D/port"); fi
if [ -n "$own" ] && [ "$serial" = "$own" ]; then
  case "$*" in
    "emu avd name") printf 'Tablet_API_35\r\nOK\r\n'; exit 0 ;;
    "emu kill") echo "Tablet_API_35 killed at $own" >> "$D/device.log"; echo "OK: killing emulator, bye bye"; exit 0 ;;
  esac
  cur=Tablet_API_35; serial=emulator-5554
fi
case "$serial:$*" in
  ":devices -l")
    if [ -f "$D/hang-devices" ]; then exec sleep 30; fi
    if [ -n "$own" ]; then printf '%s          device product:sdk_gphone64_arm64 transport_id:7\n' "$own"; fi
    if [ -f "$D/swapped" ]; then t=2; else t=1; fi
    printf 'List of devices attached\nemulator-5554          device product:sdk_gphone64_arm64 model:sdk_gphone64_arm64 device:emu64a transport_id:%s\n' "$t"
    if [ -f "$D/stale" ]; then printf 'emulator-5556          offline transport_id:9\n'; fi
    echo; exit 0 ;;
  "emulator-5556:emu avd name") cat "$D/stale" >&2; exit 1 ;;
  emulator-5556:*) echo "adb: device offline" >&2; exit 1 ;;
  "emulator-5554:emu avd name")
    if [ -f "$D/unnamed" ]; then echo "error: could not connect to TCP port 5554: Operation timed out" >&2; exit 1; fi
    printf '%s\r\nOK\r\n' "$cur"; exit 0 ;;
  "emulator-5554:emu kill") echo "$cur killed" >> "$D/device.log"; echo "OK: killing emulator, bye bye"; exit 0 ;;
esac
[ "$serial" = emulator-5554 ] || exit 0
case "$1" in exec-out|shell) shift ;; *) exit 0 ;; esac
if [ -f "$D/exec-fails" ]; then echo "error: closed" >&2; exit 1; fi
getprop() {
  case "$1" in
    ro.boot.qemu.avd_name)
      if [ -f "$D/noprops" ]; then echo; elif [ -f "$D/liar" ]; then echo sdk_gphone64_arm64; else echo "$cur"; fi ;;
    ro.kernel.qemu.avd_name) echo ;;
    sys.boot_completed) echo 1 ;;
  esac
}
screencap() {
  if [ -f "$D/hang-shot" ]; then echo $$ > "$D/shot.pid"; exec sleep 30; fi
  if [ -f "$D/garbage" ]; then echo not a png; return; fi
  if [ "$cur" = Pixel_8 ]; then cat "$D/frame.png"; else cat "$D/tablet.png"; fi
}
input() { echo "$cur input $*" >> "$D/device.log"; }
monkey() { echo "$cur monkey $*" >> "$D/device.log"; echo "Events injected: 1"; }
wm() { echo 'Physical size: 1080x2400'; }
eval "$*""#,
        )
    }

    fn device_log(&self) -> String {
        std::fs::read_to_string(self.path("device.log")).unwrap_or_default()
    }

    /// Lists two AVDs. Launching one writes the console port it was given to `port`,
    /// then fails at once, or runs until stopped once `emulator-runs` exists, writing
    /// its pid to `emulator.pid`, or runs until killed, asking nothing to SIGTERM, once
    /// `emulator-stubborn` exists.
    fn emulator(&self) -> PathBuf {
        self.script(
            "emulator",
            r#"if [ "$1" = -list-avds ]; then printf 'Pixel_8\nTablet_API_35\n'; exit 0; fi
prev=; for a; do if [ "$prev" = -port ]; then echo "$a" > "$D/port"; fi; prev=$a; done
if [ -f "$D/emulator-stubborn" ]; then echo $$ > "$D/emulator.pid"; trap '' TERM; while :; do sleep 0.05; done; fi
if [ -f "$D/emulator-runs" ]; then echo $$ > "$D/emulator.pid"; exec sleep 30; fi
echo "ERROR   | Unknown AVD name [$2], use -list-avds to see valid list."; exit 1"#,
        )
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.path("log")).unwrap_or_default()
    }
}

fn simctl_list(state: &str) -> String {
    serde_json::json!({"devices": {
        "com.apple.CoreSimulator.SimRuntime.iOS-27-0": [
            {"udid": UDID, "name": "iPhone 18 Pro", "state": state, "isAvailable": true},
            {"udid": "GONE-1", "name": "Old iPhone", "state": "Shutdown", "isAvailable": false}
        ],
        "com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4": [
            {"udid": "NEWER-1", "name": "iPhone 19", "state": "Booted", "isAvailable": true}
        ],
        "com.apple.CoreSimulator.SimRuntime.watchOS-12-0": [
            {"udid": "WATCH-1", "name": "Apple Watch", "state": "Shutdown", "isAvailable": true}
        ]
    }})
    .to_string()
}

fn soon() -> Deadline {
    Deadline::after(Duration::from_secs(10))
}

fn ios(id: &str) -> DeviceId {
    format!("ios:{id}").parse().unwrap()
}

#[test]
fn simctl_listing_keeps_available_ios_devices_with_stable_ids_newest_first() {
    let devices = parse::simctl_devices(simctl_list("Shutdown").as_bytes()).unwrap();
    let summary: Vec<_> = devices
        .iter()
        .map(|d| (d.id.to_string(), d.name.as_str(), d.os.as_str(), d.power))
        .collect();
    assert_eq!(
        summary,
        [
            (
                "ios:NEWER-1".to_owned(),
                "iPhone 19",
                "iOS 27.2",
                Power::Booted
            ),
            (
                format!("ios:{UDID}"),
                "iPhone 18 Pro",
                "iOS 27.0",
                Power::Shutdown
            ),
        ]
    );
    assert!(parse::simctl_devices(b"not json").is_err());
}

#[test]
fn device_ids_round_trip_and_refuse_anything_else() {
    let id: DeviceId = "android:Pixel_8".parse().unwrap();
    assert_eq!(id.platform, Platform::Android);
    assert_eq!(id.to_string(), "android:Pixel_8");
    for bad in [
        "Pixel_8",
        "web:x",
        "ios:",
        "ios:a b",
        "android:../x/y",
        "android:-help",
        "ios:--set",
    ] {
        assert!(bad.parse::<DeviceId>().is_err(), "{bad}");
    }
    assert!("android:Pixel-8".parse::<DeviceId>().is_ok());
}

#[test]
fn no_app_id_reaches_a_tool_as_an_option() {
    for platform in [Platform::Ios, Platform::Android] {
        assert!(parse::app_id(platform, "com.example.app").is_ok());
        for bad in ["-help", "--user", "-"] {
            assert!(parse::app_id(platform, bad).is_err(), "{platform:?} {bad}");
        }
    }
    assert!(parse::app_id(Platform::Ios, "com.example-app").is_ok());
}

#[test]
fn without_xcrun_ios_says_why_and_android_is_still_listed() {
    let fakes = Fakes::new();
    let host = LocalHost::new(Tools {
        xcrun: None,
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        axe: None,
    });
    let inventory = host.inventory(soon());
    let ios = inventory.platform(Platform::Ios).unwrap();
    assert!(
        matches!(&ios.support, Support::Unavailable { reason } if reason.contains("macOS") || reason.contains("Xcode"))
    );
    let android: Vec<_> = inventory
        .devices
        .iter()
        .map(|d| (d.id.to_string(), d.name.as_str(), d.power))
        .collect();
    assert_eq!(
        android,
        [
            ("android:Pixel_8".to_owned(), "Pixel 8", Power::Booted),
            (
                "android:Tablet_API_35".to_owned(),
                "Tablet API 35",
                Power::Shutdown
            ),
        ],
        "a running emulator keeps its AVD id, never its serial"
    );
    assert_eq!(
        inventory.platform(Platform::Android).unwrap().input,
        Input::Available { tool: "adb" }
    );
}

#[test]
fn without_the_android_sdk_android_says_what_to_install() {
    let host = LocalHost::new(Tools::default());
    let inventory = host.inventory(soon());
    let android = inventory.platform(Platform::Android).unwrap();
    assert!(
        matches!(&android.support, Support::Unavailable { reason } if reason.contains("KEYSTONE_ADB_PATH"))
    );
    assert!(inventory.devices.is_empty());
}

#[test]
fn start_boots_once_and_wait_reports_booting_until_the_simulator_is_ready() {
    let fakes = Fakes::new();
    let host = LocalHost::new(Tools {
        xcrun: Some(fakes.xcrun()),
        ..Tools::default()
    });
    assert_eq!(host.start(&ios(UDID), soon()), Ok(Start::Started));
    assert_eq!(
        host.start(&ios(UDID), soon()),
        Ok(Start::Booting),
        "a device already booting is not booted again"
    );
    assert_eq!(
        host.wait(&ios(UDID), Deadline::after(Duration::from_millis(600))),
        Ok(BootState::Booting)
    );
    std::fs::write(fakes.path("list.json"), simctl_list("Booted")).unwrap();
    std::fs::write(fakes.path("ready"), "").unwrap();
    assert_eq!(host.wait(&ios(UDID), soon()), Ok(BootState::Booted));
    assert_eq!(host.start(&ios(UDID), soon()), Ok(Start::Booted));
    assert_eq!(fakes.log().matches("simctl boot ").count(), 1);
    assert_eq!(
        host.start(&ios("NOPE-1"), soon()),
        Err(DeviceError::UnknownDevice)
    );
}

#[test]
fn a_shut_down_simulator_screenshot_is_not_booted_rather_than_a_failure() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("off"), "").unwrap();
    let host = LocalHost::new(Tools {
        xcrun: Some(fakes.xcrun()),
        ..Tools::default()
    });
    assert_eq!(
        host.screenshot(&ios(UDID), soon()).unwrap_err(),
        DeviceError::NotBooted
    );
    std::fs::write(fakes.path("list.json"), simctl_list("Booted")).unwrap();
    assert!(
        matches!(
            host.screenshot(&ios(UDID), soon()).unwrap_err(),
            DeviceError::Failed(output) if output.contains("Timeout waiting")
        ),
        "a running simulator's failure keeps simctl's own words"
    );
    std::fs::write(fakes.path("list.json"), simctl_list("Shutdown")).unwrap();
    std::fs::write(fakes.path("hang"), "").unwrap();
    let started = Instant::now();
    assert_eq!(
        host.screenshot(&ios(UDID), Deadline::after(Duration::from_secs(8)))
            .unwrap_err(),
        DeviceError::NotBooted,
        "a capture that outlasts its time still leaves time to ask simctl"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
}

fn captured_paths(log: &str) -> Vec<PathBuf> {
    log.lines()
        .filter(|line| line.contains("simctl io"))
        .filter_map(|line| line.split(' ').next_back().map(PathBuf::from))
        .collect()
}

#[test]
fn a_screenshot_returns_the_png_and_leaves_no_file_behind() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("list.json"), simctl_list("Booted")).unwrap();
    let host = LocalHost::new(Tools {
        xcrun: Some(fakes.xcrun()),
        ..Tools::default()
    });
    let frame = host.screenshot(&ios(UDID), soon()).unwrap();
    assert_eq!((frame.width, frame.height), (1206, 2622));
    assert_eq!(frame.png, png(1206, 2622));
    std::fs::write(fakes.path("fail"), "").unwrap();
    let failed = host.screenshot(&ios(UDID), soon()).unwrap_err();
    assert_eq!(failed, DeviceError::Failed("Capture failed".into()));
    let paths = captured_paths(&fakes.log());
    assert_eq!(paths.len(), 2);
    assert!(paths.iter().all(|path| !path.exists()), "{paths:?}");
}

#[test]
fn ios_input_needs_axe_and_taps_in_points() {
    let fakes = Fakes::new();
    let without = LocalHost::new(Tools {
        xcrun: Some(fakes.xcrun()),
        ..Tools::default()
    });
    let center = Action::Tap(Point::new(0.5, 0.5).unwrap());
    assert!(matches!(
        without.act(&ios(UDID), &center, soon()),
        Err(DeviceError::InputUnavailable(hint)) if hint.contains("brew install")
    ));
    assert!(!fakes.log().contains("axe"), "nothing runs without AXe");
    let with = LocalHost::new(Tools {
        xcrun: Some(fakes.xcrun()),
        axe: Some(fakes.axe()),
        ..Tools::default()
    });
    with.act(&ios(UDID), &center, soon()).unwrap();
    with.act(
        &ios(UDID),
        &Action::Swipe {
            from: Point::new(0.5, 0.8).unwrap(),
            to: Point::new(0.5, 0.2).unwrap(),
        },
        soon(),
    )
    .unwrap();
    with.act(&ios(UDID), &Action::Type("hello world".into()), soon())
        .unwrap();
    with.act(
        &ios(UDID),
        &Action::Launch {
            app: "com.apple.Preferences".into(),
        },
        soon(),
    )
    .unwrap();
    let log = fakes.log();
    assert!(
        log.contains(&format!("axe tap -x 201 -y 437 --udid {UDID}")),
        "{log}"
    );
    assert!(log.contains(&format!(
        "axe swipe --start-x 201 --start-y 699 --end-x 201 --end-y 175 --udid {UDID}"
    )));
    assert!(log.contains(&format!("axe type hello world --udid {UDID}")));
    assert!(log.contains(&format!("xcrun simctl launch {UDID} com.apple.Preferences")));
    assert_eq!(
        log.matches("describe-ui").count(),
        1,
        "the screen size is learned once"
    );
    assert!(matches!(
        with.act(
            &ios(UDID),
            &Action::Launch {
                app: "x; rm -rf ~".into()
            },
            soon()
        ),
        Err(DeviceError::Invalid(_))
    ));
}

#[test]
fn android_input_maps_fractions_to_pixels_and_escapes_text() {
    let fakes = Fakes::new();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    host.act(&pixel, &Action::Tap(Point::new(0.5, 0.5).unwrap()), soon())
        .unwrap();
    host.act(&pixel, &Action::Type("it's 5 $".into()), soon())
        .unwrap();
    host.act(
        &pixel,
        &Action::Launch {
            app: "com.android.settings".into(),
        },
        soon(),
    )
    .unwrap();
    assert_eq!(
        fakes.device_log(),
        "Pixel_8 input tap 540 1200\n\
         Pixel_8 input text it's%s5%s$\n\
         Pixel_8 monkey -p com.android.settings -c android.intent.category.LAUNCHER 1\n",
        "the device's shell takes the escaped text back to what was typed"
    );
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(
        host.screenshot(&tablet, soon()).unwrap_err(),
        DeviceError::NotBooted
    );
}

#[test]
fn android_text_refuses_what_input_text_cannot_carry() {
    assert_eq!(parse::android_text("a b").unwrap(), "a%sb");
    assert_eq!(parse::android_text("50%").unwrap(), "50%");
    assert!(parse::android_text("100%s").is_err());
    assert!(parse::android_text("café").is_err());
    assert!(parse::android_text("line\nbreak").is_err());
}

#[test]
fn parsers_read_what_the_tools_print() {
    assert_eq!(parse::png_size(&png(722, 1568)), Some((722, 1568)));
    assert_eq!(parse::png_size(b"GIF89a"), None);
    assert_eq!(
        parse::wm_size(b"Physical size: 1080x2400\nOverride size: 720x1600\n"),
        Some((720, 1600))
    );
    assert_eq!(
        parse::adb_emulators(
            b"List of devices attached\nemulator-5556          offline transport_id:7\nR58M  device usb:1-1 transport_id:8\nemulator-5554\tdevice\n"
        ),
        [
            parse::Listed {
                serial: "emulator-5556".into(),
                online: false,
            },
            parse::Listed {
                serial: "emulator-5554".into(),
                online: true,
            },
        ]
    );
    assert_eq!(parse::avd_name(b"KO: unknown command\r\n"), None);
    assert_eq!(
        parse::avds(b"INFO    | Storing crashdata in: /tmp/x\nPixel_8\n"),
        ["Pixel_8"]
    );
    assert_eq!(
        parse::axe_screen(br#"[{"frame":{"x":0,"y":0,"width":402,"height":874}}]"#),
        Some((402.0, 874.0))
    );
}

#[test]
fn points_off_the_screen_are_refused() {
    assert!(Point::new(1.2, 0.5).is_none());
    assert!(Point::new(0.5, -0.1).is_none());
    assert!(Point::new(f64::NAN, 0.5).is_none());
}

fn eventually(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_screenshot_learns_the_point_size_so_taps_take_one_axe_call_and_rotation_relearns_it() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("list.json"), simctl_list("Booted")).unwrap();
    std::fs::write(fakes.path("axe-ready"), "").unwrap();
    let host = LocalHost::new(Tools {
        xcrun: Some(fakes.xcrun()),
        axe: Some(fakes.axe()),
        ..Tools::default()
    });
    let center = Action::Tap(Point::new(0.5, 0.5).unwrap());
    let learned = |count| fakes.log().matches("axe describe-ui").count() == count;
    host.screenshot(&ios(UDID), soon()).unwrap();
    eventually("the point size is learned", || {
        matches!(host.points.get(UDID), Some(ios::Points::Known(..)))
    });
    host.act(&ios(UDID), &center, soon()).unwrap();
    assert!(learned(1), "{}", fakes.log());
    assert!(
        fakes
            .log()
            .contains(&format!("axe tap -x 201 -y 437 --udid {UDID}"))
    );
    std::fs::write(fakes.path("frame.png"), png(2622, 1206)).unwrap();
    std::fs::write(fakes.path("landscape"), "").unwrap();
    host.screenshot(&ios(UDID), soon()).unwrap();
    eventually(
        "a turned screen is learned again",
        || matches!(host.points.get(UDID), Some(ios::Points::Known(w, _)) if w > 800.),
    );
    host.act(&ios(UDID), &center, soon()).unwrap();
    assert!(
        fakes
            .log()
            .contains(&format!("axe tap -x 437 -y 201 --udid {UDID}")),
        "{}",
        fakes.log()
    );
    host.shutdown(&ios(UDID), soon()).unwrap();
    host.act(&ios(UDID), &center, soon()).unwrap();
    assert!(learned(3), "a shut-down simulator's size is learned afresh");
}

#[test]
fn an_axe_session_timeout_is_retried_once_when_time_allows() {
    let fakes = Fakes::new();
    let host = LocalHost::new(Tools {
        xcrun: Some(fakes.xcrun()),
        axe: Some(fakes.axe()),
        ..Tools::default()
    });
    let center = Action::Tap(Point::new(0.5, 0.5).unwrap());
    host.act(&ios(UDID), &center, soon()).unwrap();
    assert_eq!(fakes.log().matches("axe tap").count(), 2);
    std::fs::remove_file(fakes.path("axe-ready")).unwrap();
    std::fs::write(fakes.path("axe-flaky"), "").unwrap();
    std::fs::write(fakes.path("axe-slow"), "").unwrap();
    let failed = host
        .act(
            &ios(UDID),
            &center,
            Deadline::after(Duration::from_millis(700)),
        )
        .unwrap_err();
    assert!(
        matches!(&failed, DeviceError::Failed(output) if output.contains("automation session")),
        "{failed:?}"
    );
    assert_eq!(
        fakes.log().matches("axe tap").count(),
        3,
        "no second try without the time for one"
    );
}

#[test]
fn an_emulator_serial_taken_by_another_avd_is_never_used_for_the_first() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("frame.png"), png(1080, 2400)).unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    host.screenshot(&pixel, soon()).unwrap();
    std::fs::write(fakes.path("swapped"), "").unwrap();
    assert_eq!(
        host.screenshot(&pixel, soon()).unwrap_err(),
        DeviceError::NotBooted
    );
    assert_eq!(
        host.act(&pixel, &Action::Tap(Point::new(0.5, 0.5).unwrap()), soon()),
        Err(DeviceError::NotBooted)
    );
    let log = fakes.log();
    assert!(!log.contains("input tap"), "{log}");
}

#[test]
fn a_running_emulators_screenshot_is_one_adb_call_that_checks_its_avd() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("frame.png"), png(1080, 2400)).unwrap();
    std::fs::write(
        fakes.path("stale"),
        "error: could not connect to TCP port 5556: Connection refused",
    )
    .unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    host.screenshot(&pixel, soon()).unwrap();
    let before = fakes.log().lines().count();
    let frame = host.screenshot(&pixel, soon()).unwrap();
    assert_eq!((frame.width, frame.height), (1080, 2400));
    let calls: Vec<String> = fakes
        .log()
        .lines()
        .skip(before)
        .map(str::to_owned)
        .collect();
    assert_eq!(calls.len(), 1, "{calls:?}");
    std::fs::write(fakes.path("swapped"), "").unwrap();
    assert_eq!(
        host.screenshot(&pixel, soon()).unwrap_err(),
        DeviceError::NotBooted,
        "a serial another emulator took never gives its screen"
    );
}

#[test]
fn android_taps_follow_the_latest_screenshot_when_the_screen_turns() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("frame.png"), png(2400, 1080)).unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    host.screenshot(&pixel, soon()).unwrap();
    host.act(&pixel, &Action::Tap(Point::new(0.5, 0.5).unwrap()), soon())
        .unwrap();
    assert_eq!(fakes.device_log(), "Pixel_8 input tap 1200 540\n");
    let log = fakes.log();
    assert!(
        !log.contains("wm size"),
        "the screenshot's size wins: {log}"
    );
}

#[test]
fn an_emulator_that_exits_while_booting_fails_the_wait_with_its_reason() {
    let fakes = Fakes::new();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    let failed = host.wait(&tablet, soon()).unwrap_err();
    assert!(
        matches!(&failed, DeviceError::Failed(reason) if reason.contains("Unknown AVD name")),
        "{failed:?}"
    );
    assert_eq!(
        host.start(&tablet, soon()),
        Ok(Start::Started),
        "an emulator that exited is not counted as booting"
    );
}

#[test]
fn a_stalled_ios_listing_still_lists_android() {
    let fakes = Fakes::new();
    let host = LocalHost::new(Tools {
        xcrun: Some(fakes.script("xcrun", "sleep 5")),
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        axe: None,
    });
    let started = Instant::now();
    let inventory = host.inventory(Deadline::after(Duration::from_secs(2)));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(matches!(
        inventory.platform(Platform::Ios).unwrap().support,
        Support::Unavailable { .. }
    ));
    assert_eq!(
        inventory
            .devices
            .iter()
            .filter(|d| d.id.platform == Platform::Android)
            .count(),
        2
    );
}

#[test]
fn an_emulator_that_cannot_say_its_avd_is_never_reported_stopped() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("frame.png"), png(1080, 2400)).unwrap();
    std::fs::write(fakes.path("unnamed"), "").unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    let failed = host.screenshot(&pixel, soon()).unwrap_err();
    assert!(
        matches!(&failed, DeviceError::Failed(output) if output.contains("could not connect")),
        "{failed:?}"
    );
    assert_eq!(
        host.wait(&pixel, Deadline::after(Duration::from_millis(600))),
        Ok(BootState::Booting),
        "the wait goes on while the emulator can't say"
    );
    std::fs::remove_file(fakes.path("unnamed")).unwrap();
    host.screenshot(&pixel, soon()).unwrap();
    let asked = fakes.log().matches("emu avd name").count();
    std::fs::write(fakes.path("unnamed"), "").unwrap();
    host.screenshot(&pixel, soon()).unwrap();
    host.act(&pixel, &Action::Tap(Point::new(0.5, 0.5).unwrap()), soon())
        .unwrap();
    assert_eq!(
        fakes.log().matches("emu avd name").count(),
        asked,
        "once the emulator named its AVD in a command, later ones need no console"
    );
}

#[test]
fn an_emulator_that_cannot_say_its_avd_is_never_launched_beside_nor_listed_off() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("unnamed"), "").unwrap();
    std::fs::write(fakes.path("emulator-runs"), "").unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    let refused = host.start(&tablet, soon()).unwrap_err();
    assert!(
        matches!(&refused, DeviceError::Failed(output) if output.contains("could not connect")),
        "{refused:?}"
    );
    assert!(
        !fakes.log().contains("-avd Tablet_API_35"),
        "no second emulator: {}",
        fakes.log()
    );
    let inventory = host.inventory(soon());
    assert!(
        inventory
            .devices
            .iter()
            .all(|d| d.id.platform != Platform::Android),
        "{inventory:?}"
    );
    assert!(
        matches!(
            &inventory.platform(Platform::Android).unwrap().support,
            Support::Unavailable { reason } if reason.contains("could not connect")
        ),
        "{inventory:?}"
    );
}

#[test]
fn a_launched_emulator_that_exits_fails_the_wait_while_another_cannot_say_its_avd() {
    let fakes = Fakes::new();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    std::fs::write(fakes.path("swapped"), "").unwrap();
    std::fs::write(fakes.path("unnamed"), "").unwrap();
    let failed = host.wait(&tablet, soon()).unwrap_err();
    assert!(
        matches!(&failed, DeviceError::Failed(reason) if reason.contains("Unknown AVD name")),
        "{failed:?}"
    );
}

#[test]
fn a_refusing_offline_entry_is_no_emulator_and_a_silent_one_says_how_to_clear_it() {
    let fakes = Fakes::new();
    std::fs::write(
        fakes.path("stale"),
        "error: could not connect to TCP port 5556: Connection refused",
    )
    .unwrap();
    std::fs::write(fakes.path("emulator-runs"), "").unwrap();
    let tools = Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    };
    let host = LocalHost::new(tools.clone());
    let inventory = host.inventory(soon());
    assert_eq!(
        inventory.platform(Platform::Android).unwrap().support,
        Support::Available,
        "{inventory:?}"
    );
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    host.shutdown(&tablet, soon()).unwrap();

    std::fs::write(
        fakes.path("stale"),
        "error: could not connect to TCP port 5556: Operation timed out",
    )
    .unwrap();
    let refused = LocalHost::new(tools).start(&tablet, soon()).unwrap_err();
    assert!(
        matches!(&refused, DeviceError::Failed(reason)
            if reason.contains("emulator-5556") && reason.contains("adb kill-server")),
        "{refused:?}"
    );
}

#[test]
fn closing_an_emulator_adb_does_not_list_yet_stops_the_one_keystone_started() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("emulator-runs"), "").unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    eventually("the emulator runs", || fakes.path("emulator.pid").exists());
    let pid: libc::pid_t = std::fs::read_to_string(fakes.path("emulator.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let started = Instant::now();
    host.shutdown(&tablet, Deadline::after(Duration::from_secs(5)))
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));
    // SAFETY: signal 0 only asks whether the process exists.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "the emulator exited before the shutdown answered"
    );
    assert_eq!(
        host.start(&tablet, soon()),
        Ok(Start::Started),
        "nothing counts as booting it any more"
    );
    host.shutdown(&tablet, soon()).unwrap();
}

/// A host whose launch of Tablet_API_35 exited as another emulator took that AVD, as
/// when the user launches it at the same moment.
fn taken_over(fakes: &Fakes) -> LocalHost {
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    eventually("Keystone's emulator exits", || {
        android::exited(&host, "Tablet_API_35").is_some()
    });
    std::fs::write(fakes.path("swapped"), "").unwrap();
    host
}

#[test]
fn a_boot_wait_never_takes_an_emulator_that_another_launch_runs() {
    let fakes = Fakes::new();
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    let host = taken_over(&fakes);
    assert_eq!(host.wait(&tablet, soon()), Ok(BootState::Elsewhere));
    assert_eq!(
        host.wait(&tablet, soon()),
        Ok(BootState::Booted),
        "the next wait follows its boot"
    );

    let fakes = Fakes::new();
    let host = taken_over(&fakes);
    host.shutdown(&tablet, soon()).unwrap();
    assert!(
        !fakes.device_log().contains("killed"),
        "a shutdown of what Keystone launched leaves another's: {}",
        fakes.device_log()
    );
}

#[test]
fn the_hosts_stop_kills_an_emulator_a_shutdown_asked_to_exit() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("emulator-stubborn"), "").unwrap();
    let host = std::sync::Arc::new(LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    }));
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    eventually("the emulator runs", || fakes.path("emulator.pid").exists());
    let pid: libc::pid_t = std::fs::read_to_string(fakes.path("emulator.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let shutting = host.clone();
    let shutdown = std::thread::spawn(move || {
        shutting.shutdown(&tablet, Deadline::after(Duration::from_secs(20)))
    });
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: signal 0 only asks whether the process exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "it shrugged off SIGTERM");
    host.stop();
    let started = Instant::now();
    assert_eq!(shutdown.join().unwrap(), Ok(()));
    assert!(started.elapsed() < Duration::from_secs(5));
    // SAFETY: as above.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "killed and reaped");
}

/// The pid the stand-in emulator wrote once it runs.
fn emulator_pid(fakes: &Fakes) -> libc::pid_t {
    eventually("the emulator runs", || fakes.path("emulator.pid").exists());
    std::fs::read_to_string(fakes.path("emulator.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn the_hosts_stop_kills_an_emulator_whose_shutdown_still_waits_on_adb() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("emulator-stubborn"), "").unwrap();
    let host = std::sync::Arc::new(LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    }));
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    let pid = emulator_pid(&fakes);
    std::fs::write(fakes.path("hang-devices"), "").unwrap();
    let shutting = host.clone();
    let shutdown = std::thread::spawn(move || {
        shutting.shutdown(&tablet, Deadline::after(Duration::from_secs(20)))
    });
    eventually("the shutdown asks adb", || {
        fakes.log().matches("adb devices -l").count() == 2
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(alive(pid), "the shutdown never got past adb");
    host.stop();
    eventually("the emulator is killed and reaped", || !alive(pid));
    assert_eq!(shutdown.join().unwrap(), Ok(()));
}

#[test]
fn a_boot_wait_never_claims_the_users_emulator_while_keystones_still_runs() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("emulator-runs"), "").unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    let pid = emulator_pid(&fakes);
    // The user's emulator runs the AVD, booted, at a serial of its own, while
    // Keystone's has yet to find the AVD locked.
    std::fs::write(fakes.path("swapped"), "").unwrap();
    assert_eq!(
        host.wait(&tablet, Deadline::after(Duration::from_millis(1500))),
        Ok(BootState::Booting),
        "never Booted on another's boot"
    );
    host.shutdown(&tablet, soon()).unwrap();
    assert!(
        !fakes.device_log().contains("killed"),
        "the user's emulator keeps running: {}",
        fakes.device_log()
    );
    eventually("Keystone's own emulator stops", || !alive(pid));

    let fakes = Fakes::new();
    std::fs::write(fakes.path("emulator-runs"), "").unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    let pid = emulator_pid(&fakes);
    std::fs::write(fakes.path("swapped"), "").unwrap();
    // SAFETY: kill has no memory effects. The stand-in leads its own process group.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    eventually("Keystone's emulator exits", || {
        android::exited(&host, "Tablet_API_35").is_some()
    });
    assert_eq!(host.wait(&tablet, soon()), Ok(BootState::Elsewhere));
}

#[test]
fn keystones_emulator_is_the_one_at_the_port_it_was_given() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("emulator-runs"), "").unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    assert_eq!(host.start(&tablet, soon()), Ok(Start::Started));
    let pid = emulator_pid(&fakes);
    let port: u16 = std::fs::read_to_string(fakes.path("port"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        (5556..=5584).contains(&port) && port % 2 == 0,
        "a free even console port adb looks at, not the listed 5554: {port}"
    );
    std::fs::write(fakes.path("own-listed"), "").unwrap();
    assert_eq!(host.wait(&tablet, soon()), Ok(BootState::Booted));
    host.shutdown(&tablet, soon()).unwrap();
    assert_eq!(
        fakes.device_log().trim(),
        format!("Tablet_API_35 killed at emulator-{port}")
    );
    // SAFETY: as above. The stand-in's emu kill only logs.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
}

#[test]
fn a_tap_waits_for_the_point_size_a_screenshot_is_learning() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("list.json"), simctl_list("Booted")).unwrap();
    std::fs::write(fakes.path("axe-ready"), "").unwrap();
    let host = LocalHost::new(Tools {
        xcrun: Some(fakes.xcrun()),
        axe: Some(fakes.axe()),
        ..Tools::default()
    });
    host.screenshot(&ios(UDID), soon()).unwrap();
    host.act(
        &ios(UDID),
        &Action::Tap(Point::new(0.5, 0.5).unwrap()),
        soon(),
    )
    .unwrap();
    let log = fakes.log();
    assert_eq!(log.matches("axe describe-ui").count(), 1, "{log}");
    assert!(
        log.contains(&format!("axe tap -x 201 -y 437 --udid {UDID}")),
        "{log}"
    );
}

#[test]
fn a_failed_learn_waits_before_the_next_and_a_size_that_does_not_turn_is_asked_per_tap() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("list.json"), simctl_list("Booted")).unwrap();
    std::fs::write(fakes.path("axe-ready"), "").unwrap();
    std::fs::write(fakes.path("describe-fails"), "").unwrap();
    let host = LocalHost::new(Tools {
        xcrun: Some(fakes.xcrun()),
        axe: Some(fakes.axe()),
        ..Tools::default()
    });
    let described = || fakes.log().matches("axe describe-ui").count();
    host.screenshot(&ios(UDID), soon()).unwrap();
    eventually("the learn fails", || {
        matches!(host.points.get(UDID), Some(ios::Points::Failed { .. }))
    });
    host.screenshot(&ios(UDID), soon()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(described(), 1, "no second try straight after a failure");
    std::fs::remove_file(fakes.path("describe-fails")).unwrap();
    host.shutdown(&ios(UDID), soon()).unwrap();
    std::fs::write(fakes.path("frame.png"), png(2622, 1206)).unwrap();
    host.screenshot(&ios(UDID), soon()).unwrap();
    eventually("a portrait size for a landscape screen", || {
        host.points.get(UDID) == Some(ios::Points::Unreliable)
    });
    let center = Action::Tap(Point::new(0.5, 0.5).unwrap());
    host.act(&ios(UDID), &center, soon()).unwrap();
    host.act(&ios(UDID), &center, soon()).unwrap();
    host.screenshot(&ios(UDID), soon()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        described(),
        4,
        "each tap asks AXe, and a screenshot no longer starts a learn"
    );
}

#[test]
fn stop_ends_a_command_still_running_and_refuses_the_next() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("frame.png"), png(1080, 2400)).unwrap();
    std::fs::write(fakes.path("hang-shot"), "").unwrap();
    let host = std::sync::Arc::new(LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    }));
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    let shooting = {
        let (host, pixel) = (host.clone(), pixel.clone());
        std::thread::spawn(move || host.screenshot(&pixel, soon()))
    };
    eventually("the screenshot hangs", || fakes.path("shot.pid").exists());
    let pid: libc::pid_t = std::fs::read_to_string(fakes.path("shot.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    host.stop();
    // Killed is a zombie until its reaper waits, or gone.
    let running = || {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            let state = stat.rsplit(')').next().unwrap().split_whitespace().next();
            !matches!(state, Some("Z" | "X"))
        })
    };
    let killed = Instant::now();
    while running() {
        assert!(
            killed.elapsed() < Duration::from_secs(2),
            "stop kills the hung command"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(shooting.join().unwrap().is_err());
    std::fs::remove_file(fakes.path("hang-shot")).unwrap();
    assert!(
        host.screenshot(&pixel, soon()).is_err(),
        "a stopped host runs nothing more"
    );
}

#[test]
fn an_emulators_answer_names_its_avd_on_two_lines_before_what_it_printed() {
    let shot = png(4, 8);
    let output = |head: &str| [head.as_bytes(), &shot].concat();
    let names = |boot: &str, kernel: &str| [boot.to_owned(), kernel.to_owned()];
    assert_eq!(
        parse::named(&output("Pixel_8\n\n")),
        Some((names("Pixel_8", ""), shot.clone()))
    );
    assert_eq!(
        parse::named(&output("\nOld_API_28\n")),
        Some((names("", "Old_API_28"), shot.clone())),
        "an older image sets only the kernel property"
    );
    assert_eq!(parse::named(&output("\n\n")), Some((names("", ""), shot)));
    assert_eq!(parse::named(b"Pixel_8"), None, "no second line");
}

/// Pixel_8 runs at emulator-5554, transport 1, and Keystone has taken a screenshot of
/// it. Then the adb server restarts and Tablet_API_35 starts on the freed port, which
/// the new server gives the same serial and transport id.
fn pixel_then_reused(fakes: &Fakes) -> LocalHost {
    std::fs::write(fakes.path("frame.png"), png(1080, 2400)).unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    host.screenshot(&pixel, soon()).unwrap();
    host.act(&pixel, &Action::Tap(Point::new(0.5, 0.5).unwrap()), soon())
        .unwrap();
    assert_eq!(fakes.device_log(), "Pixel_8 input tap 540 1200\n");
    std::fs::write(fakes.path("reused"), "").unwrap();
    host
}

#[test]
fn after_an_adb_restart_another_avd_on_the_same_serial_and_transport_is_never_shown_as_it() {
    let fakes = Fakes::new();
    let host = pixel_then_reused(&fakes);
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    assert_eq!(
        host.screenshot(&pixel, soon()).unwrap_err(),
        DeviceError::NotBooted,
        "the tablet's screen is never Pixel_8's"
    );
}

#[test]
fn after_an_adb_restart_no_input_reaches_another_avd_on_the_same_serial() {
    let fakes = Fakes::new();
    let host = pixel_then_reused(&fakes);
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    for action in [
        Action::Tap(Point::new(0.5, 0.5).unwrap()),
        Action::Swipe {
            from: Point::new(0.5, 0.8).unwrap(),
            to: Point::new(0.5, 0.2).unwrap(),
        },
        Action::Type("hello".into()),
        Action::Launch {
            app: "com.android.settings".into(),
        },
    ] {
        assert_eq!(
            host.act(&pixel, &action, soon()),
            Err(DeviceError::NotBooted),
            "{action:?}"
        );
    }
    assert!(
        !fakes.device_log().contains("Tablet_API_35"),
        "{}",
        fakes.device_log()
    );
}

#[test]
fn after_an_adb_restart_close_never_kills_another_avd_on_the_same_serial() {
    let fakes = Fakes::new();
    let host = pixel_then_reused(&fakes);
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    assert_eq!(
        host.shutdown(&pixel, soon()),
        Ok(()),
        "Pixel_8 isn't running"
    );
    assert!(
        !fakes.device_log().contains("killed"),
        "{}",
        fakes.device_log()
    );
}

#[test]
fn an_android_action_with_arguments_it_refuses_runs_nothing_first() {
    let fakes = Fakes::new();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    for action in [
        Action::Launch {
            app: "-help".into(),
        },
        Action::Type("héllo".into()),
    ] {
        assert!(
            matches!(
                host.act(&pixel, &action, soon()),
                Err(DeviceError::Invalid(_))
            ),
            "{action:?}"
        );
    }
    assert_eq!(fakes.log(), "", "no adb ran before the refusal");
}

#[test]
fn each_listing_finds_every_avd_where_it_runs_now_whatever_an_older_server_said() {
    let fakes = Fakes::new();
    let host = pixel_then_reused(&fakes);
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    let tablet = "android:Tablet_API_35".parse::<DeviceId>().unwrap();
    let power = |host: &LocalHost, id: &DeviceId| {
        host.inventory(soon()).device(id).map(|device| device.power)
    };
    assert_eq!(power(&host, &pixel), Some(Power::Shutdown));
    assert_eq!(power(&host, &tablet), Some(Power::Booted));
    std::fs::remove_file(fakes.path("reused")).unwrap();
    assert_eq!(power(&host, &pixel), Some(Power::Booted));
    assert_eq!(
        host.start(&pixel, soon()),
        Ok(Start::Booted),
        "found running, not launched beside itself"
    );
    assert!(!fakes.log().contains("emulator -avd"), "{}", fakes.log());
}

/// adb calls per frame, and that no frame of Tablet_API_35 is ever Pixel_8's and no
/// input reaches it, in each case the final check measured.
#[test]
fn calls_per_frame_and_nothing_of_another_avd_in_every_case() {
    let cases = [
        ("steady", None, true),
        ("a swap with a new transport id", Some("swapped"), false),
        ("a swap with the same transport id", Some("reused"), false),
        ("the shell command failing", Some("exec-fails"), false),
        ("garbage after valid names", Some("garbage"), false),
        ("no AVD properties set", Some("noprops"), true),
    ];
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    let mut table = String::new();
    for (case, flag, frames) in cases {
        let fakes = Fakes::new();
        let shot = png(1080, 2400);
        std::fs::write(fakes.path("frame.png"), &shot).unwrap();
        std::fs::write(fakes.path("tablet.png"), png(1600, 2560)).unwrap();
        let host = LocalHost::new(Tools {
            adb: Some(fakes.adb()),
            emulator: Some(fakes.emulator()),
            ..Tools::default()
        });
        if flag == Some("noprops") {
            std::fs::write(fakes.path("noprops"), "").unwrap();
        }
        host.screenshot(&pixel, soon()).unwrap();
        if let Some(flag) = flag.filter(|flag| *flag != "noprops") {
            std::fs::write(fakes.path(flag), "").unwrap();
        }
        let calls = || fakes.log().lines().count();
        let (before, tries) = (calls(), 5);
        let mut shown = 0;
        for _ in 0..tries {
            if let Ok(frame) = host.screenshot(&pixel, soon()) {
                assert_eq!(frame.png, shot, "{case}: only Pixel_8's own screen");
                shown += 1;
            }
        }
        let per_frame = (calls() - before) as f64 / f64::from(tries);
        let _ = host.act(&pixel, &Action::Tap(Point::new(0.5, 0.5).unwrap()), soon());
        assert_eq!(shown == tries, frames, "{case}: {shown} of {tries} frames");
        assert!(
            !fakes.device_log().contains("Tablet_API_35"),
            "{case}: {}",
            fakes.device_log()
        );
        table.push_str(&format!(
            "{case}: {per_frame:.1} adb calls per capture, {shown}/{tries} frames\n"
        ));
    }
    println!("{table}");
}

#[test]
fn an_image_that_names_its_avd_by_no_avds_name_says_so_and_no_one_waits() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("frame.png"), png(1080, 2400)).unwrap();
    std::fs::write(fakes.path("liar"), "").unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    let says = DeviceError::Failed(
        "The emulator at emulator-5554 calls itself sdk_gphone64_arm64, which Keystone can't match to an AVD.".into(),
    );
    assert_eq!(host.screenshot(&pixel, soon()).unwrap_err(), says);
    assert_eq!(host.start(&pixel, soon()), Err(says.clone()));
    let started = Instant::now();
    assert_eq!(host.wait(&pixel, soon()), Err(says));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the wait doesn't wait"
    );
    assert!(
        matches!(
            host.act(&pixel, &Action::Tap(Point::new(0.5, 0.5).unwrap()), soon()),
            Err(DeviceError::Failed(_))
        ),
        "nothing is tapped"
    );
    assert_eq!(fakes.device_log(), "");
}

#[test]
fn an_image_that_names_its_avd_later_leaves_the_three_call_path_at_once() {
    let fakes = Fakes::new();
    std::fs::write(fakes.path("frame.png"), png(1080, 2400)).unwrap();
    std::fs::write(fakes.path("noprops"), "").unwrap();
    let host = LocalHost::new(Tools {
        adb: Some(fakes.adb()),
        emulator: Some(fakes.emulator()),
        ..Tools::default()
    });
    let pixel = "android:Pixel_8".parse::<DeviceId>().unwrap();
    let calls = || fakes.log().lines().count();
    host.screenshot(&pixel, soon()).unwrap();
    let before = calls();
    host.screenshot(&pixel, soon()).unwrap();
    assert_eq!(
        calls() - before,
        3,
        "named by no property: a lookup, then the capture"
    );
    std::fs::remove_file(fakes.path("noprops")).unwrap();
    host.screenshot(&pixel, soon()).unwrap();
    let before = calls();
    host.screenshot(&pixel, soon()).unwrap();
    assert_eq!(calls() - before, 1, "it names its AVD now, so one call");
}

#[test]
fn starts_on_separate_lanes_at_once_take_separate_ports() {
    // Each lane chooses a port from what the host has launched. Until a launch is
    // recorded another lane can't see its port, so the choice and the record are one
    // step under the host's lock.
    for _ in 0..10 {
        let fakes = Fakes::new();
        let adb = fakes.script(
            "adb",
            r#"case "$*" in "devices -l") printf 'List of devices attached\n\n'; exit 0 ;; esac; exit 1"#,
        );
        let emulator = fakes.script(
            "emulator",
            r#"if [ "$1" = -list-avds ]; then printf 'A1\nA2\nA3\nA4\n'; exit 0; fi
prev=; for a; do if [ "$prev" = -port ]; then echo "$2 $a" >> "$D/ports"; fi; prev=$a; done
exec sleep 5"#,
        );
        let host = std::sync::Arc::new(LocalHost::new(Tools {
            adb: Some(adb),
            emulator: Some(emulator),
            ..Tools::default()
        }));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let starts: Vec<_> = ["A1", "A2", "A3", "A4"]
            .into_iter()
            .map(|avd| {
                let (host, barrier) = (host.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let id: DeviceId = format!("android:{avd}").parse().unwrap();
                    barrier.wait();
                    host.start(&id, soon())
                })
            })
            .collect();
        for start in starts {
            assert_eq!(start.join().unwrap(), Ok(Start::Started));
        }
        eventually("every emulator was given its port", || {
            std::fs::read_to_string(fakes.path("ports"))
                .unwrap_or_default()
                .lines()
                .count()
                == 4
        });
        let ports = std::fs::read_to_string(fakes.path("ports")).unwrap();
        let mut seen: Vec<&str> = ports
            .lines()
            .map(|line| line.split(' ').nth(1).unwrap())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 4, "two starts took one port:\n{ports}");
        host.stop();
    }
}
