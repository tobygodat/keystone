//! The Device surface: a picker, or the device open beside the session with its latest
//! frame, which a click taps. Each window's surface is a cached view of its own that
//! redraws when its device's screen or the app's state changes, so a new frame redraws
//! only the windows showing that device.

use super::capture::STALE;
use super::{DEVICE_WIDTH, Fetch, Life};
use crate::Keystone;
use crate::app_state::AppState;
use gpui::{
    AnyElement, AnyView, App, Bounds, Context, Entity, EntityId, FocusHandle, IntoElement,
    MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Render, StyleRefinement, Subscription, Task,
    Window, canvas, div, img, prelude::*, px,
};
use keystone_agent::device::{Action, DeviceId, Input, Point, Power, Support};
use keystone_core::Component;
use keystone_ui::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// What a window's Device surface showed, kept by the window.
pub(crate) struct Pane {
    view: Option<Entity<Panel>>,
    pub(crate) drawn: Option<u64>,
    /// The reveal this window last acted on. `None` until its first frame takes the
    /// current one, so a new window never acts on a reveal from before it opened.
    revealed: Option<u64>,
    pub(crate) focus: FocusHandle,
}

impl Pane {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            view: None,
            drawn: None,
            revealed: None,
            focus: cx.focus_handle(),
        }
    }
}

struct Panel {
    state: Entity<AppState>,
    session: Option<u64>,
    focus: FocusHandle,
    /// A cached view redraws only when notified, so the panel follows the app state and
    /// the screen of the device it shows.
    _state_changed: Subscription,
    /// Follows the `Screen` entity itself: a device closed and opened again gets a new
    /// one under the same id.
    screen_changed: Option<(EntityId, Subscription)>,
    /// Redraws the panel when its frame's age label is next due to change.
    tick: Option<Task<()>>,
    /// Where a press on the frame started, as a fraction of the screen.
    pressed: Option<Point>,
    /// The frame's area in the last frame drawn, which the image is fitted to.
    area: Option<Bounds<Pixels>>,
}

fn age(at: Instant) -> String {
    let seconds = at.elapsed().as_secs();
    if seconds < 60 {
        format!("{seconds} s ago")
    } else {
        format!("{} min ago", seconds / 60)
    }
}

/// How long until a frame `age` old next needs its panel redrawn: when it turns stale,
/// or at its next whole second while its age label shows.
pub(super) fn next_tick(age: Duration, labelled: bool) -> Duration {
    match STALE.checked_sub(age) {
        Some(left) if !labelled => left,
        _ => Duration::from_secs(1) - Duration::from_nanos(u64::from(age.subsec_nanos())),
    }
}

/// Where an image of `size` lands when fitted into `bounds`, keeping its shape.
pub(super) fn fitted(bounds: Bounds<Pixels>, (width, height): (u32, u32)) -> Bounds<Pixels> {
    let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let aspect = width as f32 / height as f32;
    let (w, h) = if bw / bh > aspect {
        (bh * aspect, bh)
    } else {
        (bw, bw / aspect)
    };
    Bounds {
        origin: gpui::point(
            bounds.origin.x + px((bw - w) / 2.),
            bounds.origin.y + px((bh - h) / 2.),
        ),
        size: gpui::size(px(w), px(h)),
    }
}

/// `position` as a fraction of `image`, or `None` outside it.
pub(super) fn fraction(image: Bounds<Pixels>, position: gpui::Point<Pixels>) -> Option<Point> {
    Point::new(
        f64::from(f32::from(position.x - image.origin.x) / f32::from(image.size.width)),
        f64::from(f32::from(position.y - image.origin.y) / f32::from(image.size.height)),
    )
}

impl Keystone {
    pub(crate) fn focus_device(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .shown_composition(cx)
            .views()
            .any(|(_, component)| component == Component::Device)
        {
            window.focus(&self.device.focus, cx);
        }
    }

    /// Reports this frame's Device surface to the app, and brings the surface out
    /// once when an agent opens a device beside the session this window shows.
    pub(crate) fn note_device(&mut self, window: &Window, cx: &mut Context<Self>) {
        // A session on another machine shows the panel's refusal, which lists nothing here.
        let shown = (self.device.drawn.take())
            .filter(|session| !self.covered(cx) && self.device_elsewhere(*session, cx).is_none());
        let active = window.is_window_active();
        let id = self.window_id;
        if self.state.read(cx).device_drawn_changes(id, shown, active) {
            self.state
                .update(cx, |state, cx| state.device_drawn(id, shown, active, cx));
        }
        let super::Reveal { count, session } = self.state.read(cx).devices.reveal;
        let seen = *self.device.revealed.get_or_insert(count);
        if count > seen {
            self.device.revealed = Some(count);
            let hidden = !self
                .shown_composition(cx)
                .views()
                .any(|(_, component)| component == Component::Device);
            if hidden && self.selected(cx) == Some(session) {
                let this = cx.weak_entity();
                window.defer(cx, move |window, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.show_surface(Component::Device, DEVICE_WIDTH, window, cx)
                    });
                });
            }
        }
    }

    fn device_elsewhere(&self, session: u64, cx: &App) -> Option<String> {
        self.state.read(cx).device_elsewhere(session)
    }

    pub(crate) fn device_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let session = self.selected(cx);
        self.device.drawn = session;
        let (state, focus) = (self.state.clone(), self.device.focus.clone());
        let view = self
            .device
            .view
            .get_or_insert_with(|| {
                cx.new(|cx| Panel {
                    _state_changed: cx.observe(&state, |_, _, cx| cx.notify()),
                    screen_changed: None,
                    tick: None,
                    state,
                    session,
                    focus,
                    pressed: None,
                    area: None,
                })
            })
            .clone();
        view.update(cx, |panel, _| panel.session = session);
        // The session is part of the id, so a cached view drawn for another session is
        // never reused for this one.
        let pane = div()
            .id(gpui::SharedString::from(format!(
                "device-pane-{}",
                session.unwrap_or(0)
            )))
            .size_full()
            .min_h_0();
        if keystone_ui::automation::enabled() {
            pane.child(view).into_any_element()
        } else {
            pane.child(AnyView::from(view).cached(StyleRefinement::default().size_full().min_h_0()))
                .into_any_element()
        }
    }
}

impl Render for Panel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let beside = self
            .session
            .and_then(|session| self.state.read(cx).devices.beside(session).cloned());
        self.follow_screen(beside.as_ref().map(|b| &b.device), cx);
        self.tick = None;
        let elsewhere = self
            .session
            .and_then(|session| self.state.read(cx).device_elsewhere(session));
        let body = match (self.session, elsewhere) {
            // As Files and Changes say it beside such a session, so it wraps the same.
            (Some(_), Some(machine)) => div()
                .flex_1()
                .px(px(14.))
                .py_2()
                .automation_id("device-elsewhere")
                .child(
                    Label::new(super::elsewhere(&machine))
                        .size(TextSize::Ui)
                        .color(Color::Muted),
                )
                .into_any_element(),
            (Some(session), None) => match beside {
                Some(beside) => self.device_screen(session, beside.device, cx),
                None => div()
                    .id("device-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.picker(session, cx))
                    .into_any_element(),
            },
            (None, _) => note("Select a session to open a device beside it.", Color::Muted),
        };
        v_flex()
            .id("device")
            .track_focus(&self.focus)
            .size_full()
            .min_h_0()
            .child(body)
    }
}

/// The word for a device's life, and the color of the dot before it.
fn phase(life: Option<&Life>) -> (&'static str, Color) {
    match life {
        Some(Life::Starting) => ("Opening…", Color::Faint),
        Some(Life::Booting { .. }) => ("Booting…", Color::Faint),
        Some(Life::Booted) => ("Running", Color::Success),
        Some(Life::Stalled) => ("Still booting", Color::Warning),
        Some(Life::Off) | None => ("Not running", Color::Warning),
        Some(Life::ShuttingDown { .. }) => ("Shutting down…", Color::Faint),
        Some(Life::Failed(_)) => ("Failed", Color::Danger),
    }
}

/// A picker row's word for a device: what Keystone holds for one it opened, else
/// what the latest listing found, which can be older.
pub(super) fn row_state(life: Option<&Life>, power: Power) -> &'static str {
    match life {
        Some(Life::Starting) => "Opening",
        Some(Life::Booting { .. }) => "Booting",
        Some(Life::Booted) => "Running",
        Some(Life::Stalled) => "Still booting",
        Some(Life::Off) => "Off",
        Some(Life::ShuttingDown { .. }) => "Shutting down",
        Some(Life::Failed(_)) => "Failed",
        None => match power {
            Power::Booting => "Booting",
            Power::Booted => "Running",
            Power::Shutdown => "Off",
        },
    }
}

/// Muted text after a dot in a status color. The color marks the dot only.
fn marked(text: String, color: Color, cx: &App) -> gpui::Div {
    h_flex()
        .gap(px(5.))
        .child(
            div()
                .flex_none()
                .size(px(6.))
                .rounded_full()
                .bg(color.to_hsla(cx)),
        )
        .child(Label::new(text).size(TextSize::Sm).color(Color::Muted))
}

impl Panel {
    fn follow_screen(&mut self, device: Option<&DeviceId>, cx: &mut Context<Self>) {
        let screen = device
            .and_then(|device| Some(self.state.read(cx).devices.record(device)?.screen.clone()));
        if self.screen_changed.as_ref().map(|(id, _)| *id) == screen.as_ref().map(Entity::entity_id)
        {
            return;
        }
        self.screen_changed = screen.map(|screen| {
            (
                screen.entity_id(),
                cx.observe(&screen, |_, _, cx| cx.notify()),
            )
        });
    }

    /// Redraws this panel alone when a frame taken `at` turns five seconds old, and
    /// each second while its age label shows, so the label stays true while no new
    /// frame comes.
    fn tick_after(&mut self, at: Instant, labelled: bool, cx: &mut Context<Self>) {
        let wait = next_tick(at.elapsed(), labelled);
        self.tick = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    fn refresh(&self, cx: &mut Context<Self>) -> IconButton {
        let listing = self.state.read(cx).devices.list != Fetch::Idle;
        IconButton::new("device-refresh", IconName::Refresh)
            .size(ButtonSize::Compact)
            .disabled(listing)
            .debug_selector(|| "device-refresh".into())
            .tooltip(Tooltip::text("Refresh devices"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.state.update(cx, |state, cx| state.list_devices(cx));
            }))
    }

    fn picker(&self, session: u64, cx: &mut Context<Self>) -> AnyElement {
        let header = h_flex()
            .px(px(12.))
            .py_2()
            .gap_2()
            .child(
                div().flex_1().child(
                    Label::new("Devices")
                        .size(TextSize::Ui)
                        .color(Color::Strong),
                ),
            )
            .child(self.refresh(cx));
        let devices = &self.state.read(cx).devices;
        let mut notes = Vec::new();
        let mut listed = Vec::new();
        if let Some(error) = &devices.list_error {
            notes.push(note_owned(error.clone(), Color::Danger));
        }
        match &devices.inventory {
            None if devices.list != Fetch::Idle => {
                notes.push(note("Looking for simulators and emulators…", Color::Muted))
            }
            None => {}
            Some(inventory) => {
                for status in &inventory.platforms {
                    if let Support::Unavailable { reason } = &status.support {
                        notes.push(note_owned(
                            format!("{}: {reason}", status.platform.label()),
                            Color::Muted,
                        ));
                    }
                }
                if inventory.devices.is_empty()
                    && inventory
                        .platforms
                        .iter()
                        .any(|p| p.support == Support::Available)
                {
                    notes.push(note("No simulators or emulators found.", Color::Muted));
                }
                for device in &inventory.devices {
                    let record = devices.record(&device.id);
                    let state = row_state(record.map(|r| r.life()), device.power);
                    let refused = record
                        .and_then(|r| r.lifecycle.refused.as_deref())
                        .map(|why| format!("{}. Refresh to try again.", why.trim_end_matches('.')));
                    listed.push((
                        device.name.clone(),
                        format!("{} · {state}", device.os),
                        refused,
                        device.id.clone(),
                    ));
                }
            }
        }
        let rows = listed
            .into_iter()
            .enumerate()
            .map(|(index, (name, detail, refused, id))| {
                let button_id = format!("device-open-{index}");
                h_flex()
                    .px(px(12.))
                    .py_1()
                    .gap_2()
                    .automation_id(format!("device-row-{index}"))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(name).size(TextSize::Ui).truncate())
                            .child(Label::new(detail).size(TextSize::Sm).color(Color::Muted))
                            .children(refused.map(|refused| {
                                div()
                                    .automation_id(format!("device-refused-{index}"))
                                    .child(
                                        Label::new(refused).size(TextSize::Sm).color(Color::Danger),
                                    )
                            })),
                    )
                    .child(
                        Button::new(gpui::SharedString::from(button_id.clone()), "Open")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .debug_selector(move || button_id.clone())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = id.clone();
                                this.state
                                    .update(cx, |state, cx| state.open_device(session, id, cx));
                            })),
                    )
            });
        v_flex()
            .automation_id("device-picker")
            .child(header)
            .children(notes)
            .children(rows.collect::<Vec<_>>())
            .into_any_element()
    }

    fn device_screen(
        &mut self,
        session: u64,
        device: DeviceId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let close = IconButton::new("device-close", IconName::Close)
            .size(ButtonSize::Compact)
            .debug_selector(|| "device-close".into())
            .tooltip(Tooltip::text("Close device"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state
                    .update(cx, |state, cx| state.close_device(session, cx));
            }));
        let refresh = self.refresh(cx);
        let devices = &self.state.read(cx).devices;
        let record = devices.record(&device);
        let life = record.map(|r| r.life().clone());
        let listed = devices
            .inventory
            .as_ref()
            .and_then(|inventory| inventory.device(&device));
        let name = listed.map_or_else(|| device.to_string(), |d| d.name.clone());
        let os = listed.map(|d| d.os.clone());
        let (word, color) = phase(life.as_ref());
        let input = devices.input(device.platform).cloned();
        let (picture, error) = record.map_or((None, None), |record| {
            let screen = record.screen.read(cx);
            (screen.picture.clone(), screen.error.clone())
        });
        let refused = record.and_then(|r| r.lifecycle.refused.clone());
        let header = h_flex()
            .px(px(12.))
            .py_2()
            .gap_2()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div().automation_id("device-name").child(
                            Label::new(name)
                                .size(TextSize::Ui)
                                .color(Color::Strong)
                                .truncate(),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .children(os.map(|os| {
                                Label::new(format!("{os} ·"))
                                    .size(TextSize::Sm)
                                    .color(Color::Muted)
                            }))
                            .child(div().automation_id("device-phase").child(marked(
                                word.into(),
                                color,
                                cx,
                            ))),
                    ),
            )
            .child(refresh)
            .child(close);
        let mut notes = Vec::new();
        match &life {
            Some(Life::Failed(reason)) => notes.push(note_owned(reason.clone(), Color::Danger)),
            Some(Life::Stalled) => notes.push(note(
                "It was still booting after 3 minutes, so Keystone stopped waiting. Refresh to check it again.",
                Color::Muted,
            )),
            _ => {}
        }
        let labelled = picture.as_ref().is_some_and(|picture| {
            error.is_some() || life != Some(Life::Booted) || picture.at.elapsed() > STALE
        });
        if let Some(picture) = picture.as_ref().filter(|_| labelled) {
            notes.push(
                div()
                    .debug_selector(|| "device-stale".into())
                    .child(
                        div()
                            .px(px(12.))
                            .py_1()
                            .automation_id("device-note")
                            .child(marked(
                                format!("Last frame {}", age(picture.at)),
                                Color::Warning,
                                cx,
                            )),
                    )
                    .into_any_element(),
            );
        }
        if let Some(picture) = &picture {
            self.tick_after(picture.at, labelled, cx);
        }
        if let Some(error) = error {
            notes.push(note_owned(error, Color::Danger));
        }
        if let Some(refused) = refused {
            notes.push(
                div()
                    .automation_id("device-refused")
                    .child(note_owned(refused, Color::Danger))
                    .into_any_element(),
            );
        }
        let accepts_input = !matches!(input, Some(Input::Unavailable { .. }));
        if let Some(Input::Unavailable { reason }) = &input {
            notes.push(note_owned(reason.clone(), Color::Muted));
        }
        let screen = match picture {
            Some(picture) => self.frame(session, picture, accepts_input, cx),
            None => note(
                match life {
                    Some(Life::Starting) => "Opening the device…",
                    Some(Life::Booting { .. }) => "Waiting for the device to boot…",
                    Some(Life::Booted) => "Waiting for the first frame…",
                    _ => "",
                },
                Color::Muted,
            ),
        };
        v_flex()
            .size_full()
            .min_h_0()
            .child(header)
            .children(notes)
            .child(div().relative().flex_1().min_h_0().m_2().child(screen))
            .into_any_element()
    }

    /// The latest frame, fitted to the pane. A click taps the device there and a drag
    /// swipes, when the platform takes input.
    fn frame(
        &self,
        session: u64,
        picture: super::Picture,
        accepts_input: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let area = Rc::new(Cell::new(self.area));
        let size = (picture.width, picture.height);
        let (measured, this) = (area.clone(), cx.weak_entity());
        // Fitted and centred in this frame's area, so a pane that moved draws its first
        // frame in place.
        let image = img(picture.image)
            .debug_selector(|| "device-image".into())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .object_fit(gpui::ObjectFit::Contain);
        let mut frame = div()
            .id("device-frame")
            .debug_selector(|| "device-frame".into())
            .automation_id("device-frame")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                canvas(
                    move |bounds, window, cx| {
                        if measured.replace(Some(bounds)) != Some(bounds) {
                            // Kept for the next frame's clicks; the image needs no redraw.
                            window.defer(cx, move |_, cx| {
                                let _ = this.update(cx, |this, _| this.area = Some(bounds));
                            });
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(image);
        if accepts_input {
            let (down_area, up_area) = (area.clone(), area.clone());
            frame = frame
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, _| {
                        this.pressed = down_area
                            .get()
                            .and_then(|b| fraction(fitted(b, size), event.position));
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                        this.release(session, up_area.get(), size, event.position, cx)
                    }),
                )
                // A drag let go past the frame swipes to its edge.
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                        this.release(session, area.get(), size, event.position, cx)
                    }),
                );
        }
        frame.into_any_element()
    }

    /// Ends a press on the frame begun at `pressed`: a tap where it began, or a swipe to
    /// where it was let go, held to the screen's edge.
    fn release(
        &mut self,
        session: u64,
        area: Option<Bounds<Pixels>>,
        size: (u32, u32),
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.pressed.take() else {
            return;
        };
        let to = area.map_or(from, |b| clamped(fitted(b, size), position));
        let moved =
            ((to.x - from.x) * f64::from(size.0)).hypot((to.y - from.y) * f64::from(size.1));
        let action = if moved < f64::from(size.0) * 0.02 {
            Action::Tap(from)
        } else {
            Action::Swipe { from, to }
        };
        self.state
            .update(cx, |state, cx| state.device_input(session, action, cx));
    }
}

/// `position` as a point on `image`, held to its edges.
pub(super) fn clamped(image: Bounds<Pixels>, position: gpui::Point<Pixels>) -> Point {
    let along = |offset: Pixels, length: Pixels| {
        f64::from(f32::from(offset) / f32::from(length)).clamp(0.0, 1.0)
    };
    Point::new(
        along(position.x - image.origin.x, image.size.width),
        along(position.y - image.origin.y, image.size.height),
    )
    .expect("clamped onto the screen")
}

fn note(text: &'static str, color: Color) -> AnyElement {
    note_owned(text.to_owned(), color)
}

fn note_owned(text: String, color: Color) -> AnyElement {
    div()
        .px(px(12.))
        .py_1()
        .automation_id("device-note")
        .child(Label::new(text).size(TextSize::Sm).color(color))
        .into_any_element()
}
