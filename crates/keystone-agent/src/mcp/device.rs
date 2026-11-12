//! The device panel's tools. Each acts on the device open beside the caller's session,
//! which the Device surface shows.

use std::time::Duration;

use serde_json::{Value, json};

use super::protocol::tool;

/// Every device tool's name. Claude runs them without asking: the device is a sandbox
/// the user watches.
pub const TOOLS: [&str; 8] = [
    "device_list",
    "device_open",
    "device_screenshot",
    "device_launch",
    "device_tap",
    "device_swipe",
    "device_type",
    "device_close",
];

/// How long a device tool may take. Booting waits longest, under the 60 seconds Codex
/// gives a tool call.
pub fn budget(tool: &str) -> Duration {
    Duration::from_secs(if tool == "device_open" { 50 } else { 20 })
}

pub(super) fn tools() -> Vec<Value> {
    let pixel = json!({"type":"number","minimum":0,"description":"Pixels in the latest device_screenshot image."});
    vec![
        tool(
            "device_list",
            "List the iOS Simulators and Android Emulators on this machine, whether each platform can run and take input here, and the device open beside this session.",
            json!({}),
            json!([]),
        ),
        tool(
            "device_open",
            "Boot a device if needed and show it beside this session, replacing any device open there. Waits up to 50 seconds; state booting means it is still starting and shutting_down that Keystone is still shutting it down, so call again to keep waiting.",
            json!({"device":{"type":"string","minLength":1,"maxLength":160,"description":"A device id from device_list, such as ios:<udid> or android:<avd>."}}),
            json!(["device"]),
        ),
        tool(
            "device_screenshot",
            "Capture the screen of the device open beside this session as an image. device_tap and device_swipe take pixel coordinates in the latest image this returned.",
            json!({}),
            json!([]),
        ),
        tool(
            "device_launch",
            "Launch an installed app on the device open beside this session, by bundle id on iOS or package name on Android.",
            json!({"app":{"type":"string","minLength":1,"maxLength":256}}),
            json!(["app"]),
        ),
        tool(
            "device_tap",
            "Tap the device open beside this session at a point in the latest device_screenshot image.",
            json!({"x":pixel,"y":pixel}),
            json!(["x", "y"]),
        ),
        tool(
            "device_swipe",
            "Swipe on the device open beside this session from (x, y) to (to_x, to_y), in pixels of the latest device_screenshot image.",
            json!({"x":pixel,"y":pixel,"to_x":pixel,"to_y":pixel}),
            json!(["x", "y", "to_x", "to_y"]),
        ),
        tool(
            "device_type",
            "Type text into the focused field of the device open beside this session.",
            json!({"text":{"type":"string","minLength":1,"maxLength":1024}}),
            json!(["text"]),
        ),
        tool(
            "device_close",
            "Remove the device from beside this session. Keystone shuts it down only if Keystone booted it and no other session has it open.",
            json!({}),
            json!([]),
        ),
    ]
}
