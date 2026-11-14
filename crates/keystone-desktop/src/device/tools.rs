//! The orchestrator's `device_*` tools: who may call which, their arguments, and the
//! JSON they answer with.

use super::frames::{self, ModelShot, Picture};
use super::{Beside, Done, Life, REPLY_MARGIN};
use crate::app_state::AppState;
use crate::app_state::orchestrator::Request;
use gpui::Context;
use keystone_agent::device::{
    Action, Deadline, DeviceError, DeviceId, Input, Inventory, Point, Support,
};
use keystone_agent::mcp::{Caller, HandlerError, Reply};
use keystone_core::ApprovalMode;
use serde_json::{Value, json};

/// A device request, once authorized on the UI thread.
pub(crate) enum Call {
    List,
    Open(DeviceId),
    Screenshot(DeviceId),
    Act(DeviceId, Action),
    Close,
}

fn argument<'a>(arguments: &'a Value, key: &str) -> Result<&'a Value, String> {
    arguments
        .get(key)
        .ok_or_else(|| format!("{key} is required"))
}

fn number(arguments: &Value, key: &str) -> Result<f64, String> {
    argument(arguments, key)?
        .as_f64()
        .ok_or_else(|| format!("{key} must be a number"))
}

fn text(arguments: &Value, key: &str) -> Result<String, String> {
    argument(arguments, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} must be text"))
}

/// A point in the session's latest screenshot image as a fraction of the screen.
fn point(beside: &Beside, x: f64, y: f64) -> Result<Point, String> {
    let (width, height) = beside.shot.ok_or(
        "Take a device_screenshot first: tap and swipe coordinates are pixels of its image",
    )?;
    Point::new(x / f64::from(width), y / f64::from(height)).ok_or_else(|| {
        format!("x and y must lie within the latest device_screenshot image, {width}x{height}")
    })
}

pub(super) fn opened(device: &DeviceId, state: &str) -> Result<Reply, String> {
    Ok(json!({"device": device.to_string(), "state": state}).into())
}

pub(super) fn closed(device: &DeviceId, shut_down: bool) -> Result<Reply, String> {
    Ok(json!({"device": device.to_string(), "shut_down": shut_down}).into())
}

/// A close whose budget ran out first: it goes through once the device's start or
/// shutdown answers. A shutdown may yet fail, so that one is named as under way.
pub(super) fn closing(device: &DeviceId, life: &Life) -> Result<Reply, String> {
    let state = match life {
        Life::ShuttingDown { .. } => "shutting_down",
        _ => "closing",
    };
    Ok(json!({"device": device.to_string(), "state": state}).into())
}

impl AppState {
    /// The session a second opinion, or a handoff or fork of one, reviews.
    pub(super) fn reviewed(&self, id: u64) -> Option<u64> {
        let mut current = self.session(id);
        while let Some(session) = current {
            if let Some(source) = session.second_opinion_on() {
                return Some(source);
            }
            current = session
                .handed_off_from()
                .or(session.forked_from())
                .and_then(|from| self.session(from));
        }
        None
    }

    /// What `request` asks for, if its caller may: listing and screenshots for every
    /// caller with a running turn, control only for sessions that may write.
    pub(crate) fn device_call(
        &self,
        caller: Caller,
        tool: &str,
        arguments: &Value,
    ) -> Result<Call, String> {
        if let Some(machine) = self.device_elsewhere(caller.session) {
            return Err(super::elsewhere(&machine));
        }
        let (_, mode) = self.caller(caller)?;
        let session = caller.session;
        let reviewing = self.reviews(session);
        if !matches!(tool, "device_list" | "device_screenshot") {
            if reviewing {
                return Err("A second opinion only reads: device_list and device_screenshot work, nothing that controls a device".into());
            }
            if mode == ApprovalMode::ReadOnly {
                return Err("A read-only session can't control devices: device_list and device_screenshot still work".into());
            }
        }
        let target = if reviewing {
            self.reviewed(session).unwrap_or(session)
        } else {
            session
        };
        let beside = || {
            self.devices
                .beside
                .get(&target)
                .ok_or("No device is open beside this session; call device_open first")
        };
        Ok(match tool {
            "device_list" => Call::List,
            "device_open" => Call::Open(text(arguments, "device")?.parse()?),
            "device_screenshot" => Call::Screenshot(beside()?.device.clone()),
            "device_launch" => Call::Act(
                beside()?.device.clone(),
                Action::Launch {
                    app: text(arguments, "app")?,
                },
            ),
            "device_tap" => {
                let beside = beside()?;
                let at = point(beside, number(arguments, "x")?, number(arguments, "y")?)?;
                Call::Act(beside.device.clone(), Action::Tap(at))
            }
            "device_swipe" => {
                let beside = beside()?;
                let from = point(beside, number(arguments, "x")?, number(arguments, "y")?)?;
                let to = point(
                    beside,
                    number(arguments, "to_x")?,
                    number(arguments, "to_y")?,
                )?;
                Call::Act(beside.device.clone(), Action::Swipe { from, to })
            }
            "device_type" => {
                let typed = text(arguments, "text")?;
                if typed.is_empty() || typed.chars().count() > 1024 {
                    return Err("text must be 1 to 1024 characters".into());
                }
                Call::Act(beside()?.device.clone(), Action::Type(typed))
            }
            "device_close" => {
                beside()?;
                Call::Close
            }
            _ => return Err("Unknown device tool".into()),
        })
    }

    pub(crate) fn device_request(&mut self, request: Request, cx: &mut Context<Self>) {
        let call = match self.device_call(request.caller, &request.tool, &request.arguments) {
            Ok(call) => call,
            Err(error) => {
                let _ = request.reply.send(Err(HandlerError::from(error)));
                return;
            }
        };
        let session = request.caller.session;
        let until = Deadline(
            request
                .deadline
                .checked_sub(REPLY_MARGIN)
                .unwrap_or(request.deadline),
        );
        let request = Some(Box::new(request));
        match call {
            Call::List => self.list(request, until, cx),
            // Nothing beside the session changes for an id no listing names.
            Call::Open(device) if self.devices.listed(&device) => {
                self.open(session, device, request, cx)
            }
            Call::Open(device) => {
                if let Some(request) = request {
                    self.devices.unlisted.push((session, device, request));
                }
                self.list_devices(cx);
            }
            Call::Screenshot(device) => {
                self.queue(&device, request, cx, Done::Shot, move |host, device| {
                    host.screenshot(device, until)
                        .map(|frame| frames::for_both(&frame))
                });
            }
            Call::Act(device, action) => self.act(device, action, until, request, cx),
            Call::Close => self.close(session, request, cx),
        }
    }

    pub(super) fn shot(
        &mut self,
        device: &DeviceId,
        result: Result<(Option<Picture>, ModelShot), DeviceError>,
        request: Option<&Request>,
        cx: &mut Context<Self>,
    ) -> Result<Reply, String> {
        let (picture, model) = match result {
            // A frame that can't be decoded is no frame, so the panel says why.
            Ok((None, Err(reason))) => (Err(DeviceError::Failed(reason.clone())), Err(reason)),
            Ok((picture, model)) => (Ok(picture), model),
            Err(error) => (Err(error.clone()), Err(error.to_string())),
        };
        self.took(device, picture, cx);
        let (image, (width, height)) = model?;
        if let Some(request) = request
            && let Some(beside) = self.devices.beside.get_mut(&request.caller.session)
            && &beside.device == device
        {
            beside.shot = Some((width, height));
        }
        Ok(Reply {
            value: json!({"device": device.to_string(), "width": width, "height": height}),
            images: vec![image],
        })
    }

    pub(super) fn act(
        &mut self,
        device: DeviceId,
        action: Action,
        until: Deadline,
        request: Option<Box<Request>>,
        cx: &mut Context<Self>,
    ) {
        self.queue(&device, request, cx, Done::Acted, move |host, device| {
            host.act(device, &action, until)
        });
    }

    pub(super) fn acted(
        &mut self,
        device: &DeviceId,
        result: Result<(), DeviceError>,
        cx: &mut Context<Self>,
    ) -> Result<Reply, String> {
        let reply = match &result {
            Ok(()) => Ok(json!({"device": device.to_string(), "done": true}).into()),
            Err(error) => Err(error.to_string()),
        };
        let done = result.is_ok();
        self.took(device, result.map(|()| None), cx);
        if done {
            self.capture_soon(device, cx);
        }
        reply
    }
}

pub(super) fn list_json(host: &str, inventory: &Inventory, open: Option<&DeviceId>) -> Value {
    json!({
        "host": host,
        "devices": inventory.devices.iter().map(|device| json!({
            "id": device.id.to_string(),
            "name": device.name,
            "platform": device.id.platform.slug(),
            "os": device.os,
            "state": device.power.slug(),
        })).collect::<Vec<_>>(),
        "platforms": inventory.platforms.iter().map(|status| {
            let mut platform = json!({"platform": status.platform.slug()});
            match &status.support {
                Support::Available => platform["available"] = json!(true),
                Support::Unavailable { reason } => {
                    platform["available"] = json!(false);
                    platform["reason"] = json!(reason);
                }
            }
            match &status.input {
                Input::Available { tool } => platform["input"] = json!(tool),
                Input::Unavailable { reason } => {
                    platform["input"] = json!("unavailable");
                    platform["input_reason"] = json!(reason);
                }
            }
            platform
        }).collect::<Vec<_>>(),
        "open": open.map(ToString::to_string),
    })
}
