//! Captures for the Device surface: each shown, running device is captured on its
//! lane, faster while an active window shows it, and each frame redraws only the panes
//! showing that device.

use super::frames::{self, Picture};
use super::{Done, Fetch, Lane, Life, Record};
use crate::app_state::AppState;
use gpui::{Context, Task};
use keystone_agent::device::{Deadline, DeviceError, DeviceId};
use keystone_core::WindowId;
use std::time::Duration;

/// The pause between one capture ending and the next starting, while an active window
/// shows the device and while only inactive ones do.
pub(super) const ACTIVE: Duration = Duration::from_millis(250);
pub(super) const INACTIVE: Duration = Duration::from_secs(2);
/// A frame older than this is labelled with its age.
pub(super) const STALE: Duration = Duration::from_secs(5);
const CAPTURE_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Focus {
    Active,
    Inactive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Shown {
    pub(super) session: u64,
    pub(super) focus: Focus,
}

pub(super) enum Capture {
    Idle,
    Running,
    Waiting { _timer: Task<()> },
}

impl super::Devices {
    /// How soon to capture `device` again, or `None` when no window shows it.
    pub(super) fn pace(&self, device: &DeviceId) -> Option<Duration> {
        let mut showing = self
            .shown
            .values()
            .filter(|shown| self.beside.get(&shown.session).map(|b| &b.device) == Some(device))
            .peekable();
        showing.peek()?;
        Some(if showing.any(|shown| shown.focus == Focus::Active) {
            ACTIVE
        } else {
            INACTIVE
        })
    }
}

impl AppState {
    /// Takes a capture's frame or failure into the device's screen and paces the next.
    pub(super) fn captured(
        &mut self,
        device: &DeviceId,
        result: Result<Option<Picture>, DeviceError>,
        cx: &mut Context<Self>,
    ) {
        let pace = self.devices.pace(device);
        let Some(record) = self.devices.records.get_mut(device) else {
            return;
        };
        let changed = record.lifecycle.seen(result.as_ref().map(drop));
        show(record, result, cx);
        record.capture = match pace.filter(|_| record.life() == &Life::Booted) {
            Some(pause) => {
                let again = device.clone();
                Capture::Waiting {
                    _timer: cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(pause).await;
                        let _ = this.update(cx, |state, cx| state.capture(&again, cx));
                    }),
                }
            }
            None => Capture::Idle,
        };
        if changed {
            self.changed(device, cx);
        }
    }

    /// Takes a screenshot or an action's outcome into the device's state: a frame shows
    /// the device runs, and a host that says it doesn't stops the captures.
    pub(super) fn took(
        &mut self,
        device: &DeviceId,
        result: Result<Option<Picture>, DeviceError>,
        cx: &mut Context<Self>,
    ) {
        let Some(record) = self.devices.records.get_mut(device) else {
            return;
        };
        let changed = record.lifecycle.seen(result.as_ref().map(drop));
        show(record, result, cx);
        if changed {
            self.changed(device, cx);
        }
    }

    /// Starts each shown device's capture loop that is not running, and stops those of
    /// devices no window shows.
    pub(super) fn pace_captures(&mut self, cx: &mut Context<Self>) {
        let devices: Vec<_> = self.devices.records.keys().cloned().collect();
        for device in devices {
            let wanted = self.devices.pace(&device).is_some();
            let record = self.devices.records.get_mut(&device).unwrap();
            match (&record.capture, wanted && record.life() == &Life::Booted) {
                (Capture::Idle, true) => self.capture(&device, cx),
                (Capture::Waiting { .. }, false) => record.capture = Capture::Idle,
                _ => {}
            }
        }
    }

    /// Captures `device` now unless a capture is already running.
    pub(super) fn capture_soon(&mut self, device: &DeviceId, cx: &mut Context<Self>) {
        if let Some(record) = self.devices.records.get(device)
            && !matches!(record.capture, Capture::Running)
            && record.life() == &Life::Booted
            && self.devices.pace(device).is_some()
        {
            self.capture(device, cx);
        }
    }

    pub(super) fn capture(&mut self, device: &DeviceId, cx: &mut Context<Self>) {
        let Some(record) = self.devices.records.get_mut(device) else {
            return;
        };
        if record.life() != &Life::Booted {
            record.capture = Capture::Idle;
            return;
        }
        record.capture = Capture::Running;
        #[cfg(test)]
        {
            self.devices.captures += 1;
        }
        let (asked, failed) = (device.clone(), device.clone());
        let on_panic = |error| Done::Captured {
            device: failed,
            result: Err(error),
        };
        self.queue(Lane::Device(device), None, cx, on_panic, move |host| {
            Done::Captured {
                result: host
                    .screenshot(&asked, Deadline::after(CAPTURE_LIMIT))
                    .map(|frame| frames::for_panel(&frame)),
                device: asked,
            }
        });
    }

    /// Notes which session `window` showed in its Device surface this frame.
    pub(crate) fn device_drawn(
        &mut self,
        window: WindowId,
        shown: Option<u64>,
        active: bool,
        cx: &mut Context<Self>,
    ) {
        let focus = if active {
            Focus::Active
        } else {
            Focus::Inactive
        };
        match shown {
            Some(session) => {
                self.devices.shown.insert(window, Shown { session, focus });
                if self.devices.inventory.is_none() && !self.devices.beside.contains_key(&session) {
                    self.list_devices(cx);
                }
            }
            None => {
                self.devices.shown.remove(&window);
            }
        }
        self.pace_captures(cx);
    }

    /// Whether `window`'s last frame changes anything [`AppState::device_drawn`] keeps.
    pub(crate) fn device_drawn_changes(
        &self,
        window: WindowId,
        shown: Option<u64>,
        active: bool,
    ) -> bool {
        let focus = if active {
            Focus::Active
        } else {
            Focus::Inactive
        };
        let now = shown.map(|session| Shown { session, focus });
        self.devices.shown.get(&window).copied() != now
            || shown.is_some_and(|s| {
                self.devices.inventory.is_none()
                    && self.devices.list == Fetch::Idle
                    && !self.devices.beside.contains_key(&s)
            })
    }
}

/// Puts a frame or a failure on the device's screen, redrawing only its panes.
pub(super) fn show(
    record: &mut Record,
    result: Result<Option<Picture>, DeviceError>,
    cx: &mut Context<AppState>,
) {
    let old = record.screen.update(cx, |screen, cx| {
        cx.notify();
        match result {
            Ok(picture) => {
                screen.error = None;
                picture.and_then(|picture| screen.picture.replace(picture))
            }
            // The phase says it isn't running, in the panel's own words.
            Err(DeviceError::NotBooted) => {
                screen.error = None;
                None
            }
            Err(error) => {
                screen.error = Some(error.to_string());
                None
            }
        }
    });
    if let Some(old) = old {
        cx.drop_image(old.image, None);
    }
}
