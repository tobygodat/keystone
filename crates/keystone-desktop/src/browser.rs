//! The Browser surface: the project's browser page beside the session, which the user
//! browses and agents drive through the `browser_*` tools. The page runs in headless
//! Chrome (see `docs/design/browser-preview.md`); this surface draws its screencast
//! frames and sends the user's pointer and keys back.

use crate::Keystone;
use crate::app_state::AppState;
use crate::presentation::SessionPlace;
use gpui::{
    AnyElement, App, AppContext, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, RenderImage, ScrollWheelEvent, Subscription, Task, Window,
    canvas, div, img, prelude::*, px,
};
use keystone_agent::browser::{
    self, Act, Browsers, DevServer, History, Mouse, Page, Viewport, modifiers,
};
use keystone_agent::mcp::{Image, Reply};
use keystone_core::{ApprovalMode, Axis, Component, Place, Size};
use keystone_ui::prelude::*;
use serde_json::{Value, json};
use std::cell::Cell;
use std::num::NonZeroU16;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

gpui::actions!(browser, [ToggleBrowser, PageTab, PageShiftTab]);

pub(crate) const BROWSER_WIDTH: Size = Size::Px(NonZeroU16::new(640).unwrap());
const CONTEXT: &str = "BrowserPage";

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("tab", PageTab, Some(CONTEXT)),
        gpui::KeyBinding::new("shift-tab", PageShiftTab, Some(CONTEXT)),
    ]);
}

/// A page has a new frame or address.
pub(crate) struct BrowserChanged;

/// Runs `job` on a thread of its own: starting Chrome or waiting for a page's load can
/// take seconds, which would hold up GPUI's shared background threads.
pub(crate) fn off_thread<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
) -> futures::channel::oneshot::Receiver<T> {
    let (done, result) = futures::channel::oneshot::channel();
    let _ = std::thread::Builder::new()
        .name("keystone-browser".into())
        .spawn(move || {
            let _ = done.send(job());
        });
    result
}

/// One window's Browser surface. It shows the page of the folder the window's selected
/// session works in.
pub(crate) struct BrowserView {
    state: Entity<AppState>,
    folder: Option<String>,
    /// The machine the selected session runs on, when it isn't this one. Its pages
    /// and dev servers are there, so this view shows none.
    elsewhere: Option<String>,
    address: Entity<TextInput>,
    focus: FocusHandle,
    /// The frame drawn, by its sequence number.
    /// The frame drawn: its page's ID, its sequence number and its image.
    shown: Option<(u64, u64, Arc<RenderImage>)>,
    /// Images no longer drawn, dropped from the GPU at the next render, which has the
    /// window that holds them.
    stale: Vec<Arc<RenderImage>>,
    decoding: Option<Task<()>>,
    /// Where the page was last drawn, to map the pointer onto it.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The page and size this view last asked for. A view resizes the page only when its
    /// own size changes, so two windows showing one page don't take turns resizing it.
    sized: Rc<Cell<Option<(u64, Viewport)>>>,
    servers: Vec<DevServer>,
    finding: Option<Task<()>>,
    working: Option<Task<()>>,
    error: Option<String>,
    /// The page's count of loaded documents when `error` was shown. A document that
    /// loads after it, whoever led there, clears the error.
    error_at: u64,
    menu: bool,
    _subscriptions: Vec<Subscription>,
}

impl BrowserView {
    pub(crate) fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let address = cx.new(|cx| {
            TextInput::new("Address or localhost:port", InputMode::SingleLine, cx)
                .text_style(TextSize::Ui, Color::Default)
        });
        let changed = cx.subscribe(&state, |_, _, _: &BrowserChanged, cx| cx.notify());
        Self {
            state,
            folder: None,
            elsewhere: None,
            address,
            focus: cx.focus_handle(),
            shown: None,
            stale: Vec::new(),
            decoding: None,
            bounds: Rc::default(),
            sized: Rc::default(),
            servers: Vec::new(),
            finding: None,
            working: None,
            error: None,
            error_at: 0,
            menu: false,
            _subscriptions: vec![changed],
        }
    }

    /// Shows `folder`'s page; a change of folder finds its dev servers again. With
    /// `elsewhere`, the selected session runs on that machine and nothing is shown.
    pub(crate) fn set_folder(
        &mut self,
        folder: Option<String>,
        elsewhere: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.elsewhere != elsewhere {
            self.elsewhere = elsewhere;
            cx.notify();
        }
        if self.folder == folder {
            return;
        }
        self.folder = folder;
        self.decoding = None;
        self.stale
            .extend(self.shown.take().map(|(_, _, image)| image));
        self.error = None;
        self.working = None;
        let url = self.page(cx).map(|page| page.url()).unwrap_or_default();
        self.show_address(url, cx);
        self.find_servers(cx);
        // Back on the page it was on before the last quit, loading it again.
        if let Some(folder) = self.folder.clone()
            && let Some(url) = self
                .state
                .update(cx, |state, _| state.browser_to_restore(&folder))
        {
            self.show_address(url.clone(), cx);
            self.run(move |page| page.restore(&url), cx);
        }
    }

    /// The folder whose page this view shows.
    #[cfg(test)]
    pub(crate) fn folder(&self) -> Option<&str> {
        self.folder.as_deref()
    }

    fn browsers(&self, cx: &App) -> Arc<Browsers> {
        self.state.read(cx).browsers.clone()
    }

    fn page(&self, cx: &App) -> Option<Arc<Page>> {
        self.browsers(cx).existing(self.folder.as_deref()?)
    }

    fn show_address(&mut self, url: String, cx: &mut Context<Self>) {
        let url = if url == "about:blank" {
            String::new()
        } else {
            url
        };
        if self.address.read(cx).value() != url {
            self.address
                .update(cx, |input, cx| input.set_value(url, cx));
        }
    }

    fn find_servers(&mut self, cx: &mut Context<Self>) {
        let Some(folder) = self.folder.clone() else {
            self.servers.clear();
            return;
        };
        self.finding = Some(cx.spawn(async move |this, cx| {
            let servers = off_thread(move || browser::servers::find(Path::new(&folder)))
                .await
                .unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.servers = servers;
                this.finding = None;
                cx.notify();
            });
        }));
    }

    /// Runs `job` on the folder's page, starting the browser and the page if needed.
    fn run(
        &mut self,
        job: impl FnOnce(&Page) -> Result<(), String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(folder) = self.folder.clone() else {
            return;
        };
        let browsers = self.browsers(cx);
        self.error = None;
        self.working = Some(cx.spawn(async move |this, cx| {
            let result = off_thread(move || {
                let page = browsers.page(&folder)?;
                let _operation = page.operation();
                page.set_note(None);
                page.user_drives();
                job(&page)
            })
            .await
            .unwrap_or_else(|_| Err(browser::cdp::CLOSED.into()));
            let _ = this.update(cx, |this, cx| {
                this.working = None;
                this.show_error(result.err(), cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn open(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        match browser::normalize_url(&url) {
            Ok(url) => {
                self.show_address(url.clone(), cx);
                window.focus(&self.focus, cx);
                self.run(move |page| page.navigate(&url), cx);
            }
            Err(error) => {
                self.show_error(Some(error), cx);
                cx.notify();
            }
        }
    }

    fn show_error(&mut self, error: Option<String>, cx: &App) {
        self.error = error;
        self.error_at = self.page(cx).map_or(0, |page| page.documents());
    }

    fn go(&mut self, history: History, cx: &mut Context<Self>) {
        if self.page(cx).is_some() {
            self.run(move |page| page.go(history), cx);
        }
    }

    /// Decodes the page's newest frame off the UI thread, one at a time.
    fn decode(&mut self, page: &Page, cx: &mut Context<Self>) {
        let Some(frame) = page.frame() else {
            return;
        };
        let id = page.id();
        if self.decoding.is_some()
            || self
                .shown
                .as_ref()
                .is_some_and(|(page, seq, _)| *page == id && *seq >= frame.seq)
        {
            return;
        }
        self.decoding = Some(cx.spawn(async move |this, cx| {
            let seq = frame.seq;
            let image = cx
                .background_spawn(async move {
                    let mut buffer =
                        image::load_from_memory_with_format(&frame.jpeg, image::ImageFormat::Jpeg)
                            .ok()?
                            .into_rgba8();
                    for pixel in buffer.as_chunks_mut::<4>().0 {
                        pixel.swap(0, 2);
                    }
                    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.decoding = None;
                if let Some(image) = image {
                    this.stale
                        .extend(this.shown.replace((id, seq, image)).map(|(_, _, old)| old));
                }
                cx.notify();
            });
        }));
    }

    /// The pointer's place on the page, in CSS pixels.
    fn on_page(&self, position: Point<Pixels>, page: &Page) -> Option<(f64, f64)> {
        let bounds = self.bounds.get()?;
        let viewport = page.viewport();
        let x = f64::from(f32::from(position.x - bounds.origin.x))
            / f64::from(f32::from(bounds.size.width));
        let y = f64::from(f32::from(position.y - bounds.origin.y))
            / f64::from(f32::from(bounds.size.height));
        Some((
            x * f64::from(viewport.width),
            y * f64::from(viewport.height),
        ))
    }

    fn pointer(
        &mut self,
        kind: Mouse,
        position: Point<Pixels>,
        count: u8,
        held: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        if let Some(page) = self.page(cx)
            && let Some((x, y)) = self.on_page(position, &page)
        {
            if kind == Mouse::Pressed {
                page.set_note(None);
                page.user_drives();
            }
            page.mouse(kind, x, y, count, bits(held));
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(page) = self.page(cx) else {
            return;
        };
        let keystroke = &event.keystroke;
        page.set_note(None);
        page.user_drives();
        // Paste, copy, cut and select all take the platform's chord, Command on macOS and
        // Control elsewhere, and use the system clipboard rather than Chrome's own.
        let held = keystroke.modifiers;
        let chord = held.secondary() && !held.alt && !held.shift && !held.function;
        if chord && matches!(keystroke.key.as_str(), "v" | "a" | "c" | "x") {
            match keystroke.key.as_str() {
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        page.insert_text(&text);
                    }
                }
                "a" => page.command("selectAll"),
                _ => {
                    let cut = keystroke.key == "x";
                    cx.spawn(async move |_, cx| {
                        let selected = off_thread(move || {
                            let text = page.evaluate("String(getSelection())").ok()?;
                            if cut {
                                page.command("delete");
                            }
                            text.as_str().map(str::to_owned)
                        })
                        .await
                        .ok()
                        .flatten();
                        if let Some(text) = selected.filter(|text| !text.is_empty()) {
                            cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
                        }
                    })
                    .detach();
                }
            }
            cx.stop_propagation();
            return;
        }
        // Other Command chords stay the app's on macOS, as Super ones do elsewhere.
        if held.platform {
            return;
        }
        let text = match keystroke.key.as_str() {
            "enter" => Some("\r".to_owned()),
            _ if keystroke.modifiers.control => None,
            _ => keystroke
                .key_char
                .clone()
                .filter(|text| !text.chars().any(char::is_control)),
        };
        if browser::page::key_definition(&keystroke.key).is_some() {
            page.key(&keystroke.key, text.as_deref(), bits(&keystroke.modifiers));
        } else if let Some(text) = text {
            page.insert_text(&text);
        } else {
            return;
        }
        cx.stop_propagation();
    }

    fn toolbar(&mut self, page: Option<&Page>, cx: &mut Context<Self>) -> impl IntoElement {
        let has_page = page.is_some();
        let loading = page.is_some_and(Page::loading) || self.working.is_some();
        let mut servers = PopoverMenu::new(
            "browser-servers-menu",
            IconButton::new("browser-servers", IconName::Server)
                .size(ButtonSize::Compact)
                .selected(self.menu)
                .debug_selector(|| "browser-servers".into())
                .when(!self.menu, |button| {
                    button.tooltip(Tooltip::text("Dev servers"))
                }),
        )
        .anchor(gpui::Anchor::TopRight)
        .open(self.menu)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.menu = *open;
            if *open {
                this.find_servers(cx);
            }
            cx.notify();
        }));
        servers = servers.child(MenuHeader::new(if self.finding.is_some() {
            "Looking for dev servers…"
        } else {
            "Dev servers in this project"
        }));
        if self.servers.is_empty() {
            servers = servers.child(MenuHeader::new("None running"));
        }
        for (index, server) in self.servers.iter().enumerate() {
            let url = server.url.clone();
            servers = servers.child(
                MenuItem::new(
                    ("browser-server", index),
                    format!("localhost:{}", server.port),
                )
                .detail(server.process.clone())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.menu = false;
                    this.open(url.clone(), window, cx);
                })),
            );
        }
        h_flex()
            .flex_none()
            .gap_1()
            .px(px(8.))
            .py(px(6.))
            .child(
                IconButton::new("browser-back", IconName::Left)
                    .size(ButtonSize::Compact)
                    .disabled(!has_page)
                    .debug_selector(|| "browser-back".into())
                    .tooltip(Tooltip::text("Back"))
                    .on_click(cx.listener(|this, _, _, cx| this.go(History::Back, cx))),
            )
            .child(
                IconButton::new("browser-forward", IconName::Right)
                    .size(ButtonSize::Compact)
                    .disabled(!has_page)
                    .debug_selector(|| "browser-forward".into())
                    .tooltip(Tooltip::text("Forward"))
                    .on_click(cx.listener(|this, _, _, cx| this.go(History::Forward, cx))),
            )
            .child(
                IconButton::new("browser-reload", IconName::Refresh)
                    .size(ButtonSize::Compact)
                    .disabled(!has_page || loading)
                    .debug_selector(|| "browser-reload".into())
                    .tooltip(Tooltip::text(if loading { "Loading…" } else { "Reload" }))
                    .on_click(cx.listener(|this, _, _, cx| this.go(History::Reload, cx))),
            )
            .child(
                div()
                    .automation_id("browser-address")
                    .flex_1()
                    .min_w_0()
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(radius::CONTROL)
                    .border_1()
                    .border_color(cx.theme().colors.border)
                    .on_action(cx.listener(
                        |this, _: &keystone_ui::text_input::Newline, window, cx| {
                            let url = this.address.read(cx).value().to_owned();
                            this.open(url, window, cx);
                        },
                    ))
                    .child(self.address.clone()),
            )
            .child(servers)
    }

    /// The surface with no page to show: what to open, under the error of the last
    /// attempt when there is one, so the project's dev servers stay a click away.
    fn empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut body = v_flex()
            .automation_id(if self.elsewhere.is_some() {
                "browser-elsewhere"
            } else {
                "browser-empty"
            })
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_2()
            .px(px(24.))
            .when_some(self.error.clone(), |body, error| {
                body.child(
                    div()
                        .automation_id("browser-error")
                        .pb_2()
                        .child(Label::new(error).color(Color::Danger)),
                )
            });
        if let Some(machine) = &self.elsewhere {
            return body
                .child(
                    Label::new(format!(
                        "The browser isn't available for sessions on {machine} yet."
                    ))
                    .color(Color::Muted),
                )
                .into_any_element();
        }
        if self.folder.is_none() {
            return body
                .child(Label::new("Select a session to browse its project.").color(Color::Muted))
                .into_any_element();
        }
        if self.servers.is_empty() {
            body = body.child(
                Label::new(if self.finding.is_some() {
                    "Looking for dev servers…"
                } else {
                    "No dev server is running in this project."
                })
                .color(Color::Muted),
            );
        } else {
            body = body.child(Label::new("Open a dev server").color(Color::Muted));
            for (index, server) in self.servers.iter().enumerate() {
                let url = server.url.clone();
                body = body.child(
                    Button::new(
                        ("browser-open-server", index),
                        format!("localhost:{} · {}", server.port, server.process),
                    )
                    .style(ButtonStyle::Subtle)
                    .debug_selector(move || format!("browser-open-server-{index}"))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open(url.clone(), window, cx)),
                    ),
                );
            }
        }
        body.child(
            Label::new("Or type an address above. Agents in this project can open and drive this page too.")
                .size(TextSize::Sm)
                .color(Color::Muted),
        )
        .into_any_element()
    }

    fn frame(
        &mut self,
        page: Arc<Page>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.decode(&page, cx);
        let bounds = self.bounds.clone();
        let sized = self.sized.clone();
        let resized = page.clone();
        let scale = f64::from(window.scale_factor());
        let mut surface = div()
            .id("browser-page")
            .automation_id("browser-page")
            .track_focus(&self.focus)
            .key_context(CONTEXT)
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .bg(cx.theme().colors.background)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    let count = event.click_count.clamp(1, 3) as u8;
                    this.pointer(Mouse::Pressed, event.position, count, &event.modifiers, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    let count = event.click_count.clamp(1, 3) as u8;
                    this.pointer(Mouse::Released, event.position, count, &event.modifiers, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let held = u8::from(event.pressed_button == Some(MouseButton::Left));
                this.pointer(Mouse::Moved, event.position, held, &event.modifiers, cx);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                if let Some(page) = this.page(cx)
                    && let Some((x, y)) = this.on_page(event.position, &page)
                {
                    let delta = event.delta.pixel_delta(px(20.));
                    page.wheel(
                        x,
                        y,
                        -f64::from(f32::from(delta.x)),
                        -f64::from(f32::from(delta.y)),
                    );
                }
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.key_down(event, cx)))
            .on_action(cx.listener(|this, _: &PageTab, _, cx| {
                if let Some(page) = this.page(cx) {
                    page.key("tab", None, 0);
                }
            }))
            .on_action(cx.listener(|this, _: &PageShiftTab, _, cx| {
                if let Some(page) = this.page(cx) {
                    page.key("tab", None, modifiers::SHIFT);
                }
            }))
            .child(
                canvas(
                    move |drawn, _, _| {
                        bounds.set(Some(drawn));
                        let wanted = Viewport {
                            width: f32::from(drawn.size.width).round() as u32,
                            height: f32::from(drawn.size.height).round() as u32,
                            scale,
                        };
                        if sized.get() != Some((resized.id(), wanted)) {
                            sized.set(Some((resized.id(), wanted)));
                            resized.resize(wanted);
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        if let Some((_, _, image)) = &self.shown {
            surface = surface.child(img(image.clone()).absolute().size_full());
        }
        surface.into_any_element()
    }
}

/// CDP modifier bits for GPUI's modifiers. The platform key is Meta.
fn bits(held: &Modifiers) -> u8 {
    let mut bits = 0;
    if held.alt {
        bits |= modifiers::ALT;
    }
    if held.control {
        bits |= modifiers::CONTROL;
    }
    if held.platform {
        bits |= modifiers::META;
    }
    if held.shift {
        bits |= modifiers::SHIFT;
    }
    bits
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.stale.drain(..) {
            cx.drop_image(image, Some(window));
        }
        let page = self.page(cx);
        if let Some(page) = &page
            && !self.address.focus_handle(cx).contains_focused(window, cx)
        {
            self.show_address(page.url(), cx);
        }
        if page
            .as_ref()
            .is_some_and(|page| page.documents() > self.error_at)
        {
            self.error = None;
        }
        let note = page.as_ref().and_then(|page| page.note());
        let blank = page.as_ref().is_none_or(|page| page.url() == "about:blank");
        let body = match page {
            Some(page) if !blank && self.error.is_none() => self.frame(page, window, cx),
            _ => self.empty(cx),
        };
        let colors = cx.theme().colors;
        v_flex()
            .size_full()
            .min_h_0()
            .child(self.toolbar(self.page(cx).as_deref(), cx))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    // Over the page's bottom edge, so it never moves the page under
                    // the pointer. It takes no input: a press there reaches the page.
                    .children(note.map(|note| {
                        h_flex()
                            .absolute()
                            .bottom(px(8.))
                            .left(px(8.))
                            .right(px(8.))
                            .child(
                                div()
                                    .automation_id("browser-agent-note")
                                    .min_w_0()
                                    .max_w_full()
                                    .px(px(8.))
                                    .py(px(3.))
                                    .rounded(radius::CONTROL)
                                    .bg(colors.overlay)
                                    .border_1()
                                    .border_color(colors.border)
                                    .child(
                                        Label::new(note)
                                            .size(TextSize::Sm)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                            )
                    })),
            )
    }
}

impl AppState {
    /// Checks a browser tool call on the UI thread and returns the work to run off it.
    pub(crate) fn browser_job(
        &mut self,
        caller: keystone_agent::mcp::Caller,
        tool: &str,
        arguments: Value,
        deadline: std::time::Instant,
        cx: &mut Context<Self>,
    ) -> Result<Box<dyn FnOnce() -> Result<Reply, String> + Send>, String> {
        let (project, ceiling) = self.caller(caller)?;
        let acts = matches!(tool, "browser_act" | "browser_evaluate");
        if acts && (ceiling == ApprovalMode::ReadOnly || self.reviews(caller.session)) {
            return Err(
                "Read-only sessions and second opinions may look at the page but not act on it"
                    .into(),
            );
        }
        let title = self
            .session(caller.session)
            .map_or_else(String::new, |session| session.title().to_owned());
        // Whatever an agent makes the page do happens where the user can see it.
        if matches!(
            tool,
            "browser_open" | "browser_navigate" | "browser_act" | "browser_evaluate"
        ) {
            self.reveal_browser_for(caller.session, cx);
        }
        let folder = project.as_str().to_owned();
        let browsers = self.browsers.clone();
        let tool = tool.to_owned();
        Ok(Box::new(move || {
            let mut reply = run_tool(&browsers, &folder, &title, &tool, &arguments, deadline)?;
            // A navigation an agent's action led to, stopped since its last call.
            if let Some(url) = browsers
                .existing(&folder)
                .and_then(|page| page.take_refused())
                && reply.value.is_object()
            {
                reply.value["refused_navigation"] =
                    json!({"url": url, "message": browser::consent::refused_hint(&url)});
            }
            Ok(reply)
        }))
    }
}

fn run_tool(
    browsers: &Browsers,
    folder: &str,
    title: &str,
    tool: &str,
    arguments: &Value,
    deadline: std::time::Instant,
) -> Result<Reply, String> {
    let servers = || {
        json!(browser::servers::find(Path::new(folder))
            .into_iter()
            .map(|server| json!({"url": server.url, "port": server.port, "process": server.process}))
            .collect::<Vec<_>>())
    };
    let note = |page: &Page, did: String| page.set_note(Some(format!("“{title}” {did}")));
    if tool == "browser_status" {
        let mut status = match browsers.existing(folder) {
            Some(page) => page.status()?,
            None => json!({"url": null, "open": false}),
        };
        status["dev_servers"] = servers();
        return Ok(status.into());
    }
    let page = browsers.page(folder)?;
    let _operation = page.operation();
    // The caller was told this call timed out; it must not happen behind its back.
    if std::time::Instant::now() >= deadline {
        return Err(
            "The page was busy past this call's deadline, so nothing was done. Retry it.".into(),
        );
    }
    // Where the page goes next is this agent's doing, until the user acts in it.
    if matches!(
        tool,
        "browser_open" | "browser_navigate" | "browser_act" | "browser_evaluate"
    ) {
        page.agent_drives(arguments["url"].as_str());
    }
    match tool {
        "browser_open" => {
            if let Some(url) = arguments["url"].as_str() {
                page.navigate(url)?;
                note(&page, format!("opened {}", page.url()));
            }
            let mut status = page.status()?;
            status["dev_servers"] = servers();
            Ok(status.into())
        }
        "browser_navigate" => {
            match (arguments["url"].as_str(), arguments["history"].as_str()) {
                (Some(url), None) => page.navigate(url)?,
                (None, Some(history)) => page.go(match history {
                    "back" => History::Back,
                    "forward" => History::Forward,
                    _ => History::Reload,
                })?,
                _ => return Err("Give either url or history".into()),
            }
            note(&page, format!("went to {}", page.url()));
            page.status().map(Reply::from)
        }
        "browser_snapshot" => {
            note(&page, "read the page".into());
            page.snapshot().map(Reply::from)
        }
        "browser_act" => {
            let act = Act::parse(arguments)?;
            let result = page.act(&act)?;
            note(&page, describe(&act));
            Ok(result.into())
        }
        "browser_screenshot" => {
            let data = page.screenshot()?;
            note(&page, "took a screenshot".into());
            Ok(Reply {
                value: json!({"url": page.url()}),
                images: vec![Image {
                    mime: "image/jpeg",
                    data,
                }],
            })
        }
        "browser_evaluate" => {
            let value = page.evaluate(arguments["expression"].as_str().unwrap_or_default())?;
            note(&page, "ran a script".into());
            Ok(json!({"value": value}).into())
        }
        _ => Err("Unknown tool".into()),
    }
}

fn describe(act: &Act) -> String {
    let target = |target: &browser::Target| match target {
        browser::Target::Ref(reference) => reference.clone(),
        browser::Target::Point(x, y) => format!("{x:.0}, {y:.0}"),
    };
    match act {
        Act::Click { target: at, .. } => format!("clicked {}", target(at)),
        Act::Hover { target: at } => format!("hovered {}", target(at)),
        Act::Type { reference, .. } => {
            format!("typed into {}", reference.as_deref().unwrap_or("the page"))
        }
        Act::Press { key, .. } => format!("pressed {key}"),
        Act::Scroll { .. } => "scrolled".into(),
        Act::Select { reference, value } => format!("chose {value} in {reference}"),
    }
}

impl Keystone {
    pub(crate) fn toggle_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_surface(Component::Browser, BROWSER_WIDTH, window, cx);
    }

    /// Opens the Browser surface beside the conversation without moving focus, when an
    /// agent in a session this window shows drives the page. One open behind another
    /// tab gets a dot until it is shown.
    pub(crate) fn reveal_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .shown_composition(cx)
            .views()
            .any(|(_, component)| component == Component::Browser)
        {
            if !self.browser_drawn {
                self.browser_unseen = true;
                cx.notify();
            }
            return;
        }
        let at = Place::Beside {
            target: self.side_surface_target(cx),
            axis: Axis::Row,
            after: true,
        };
        self.open_surface(Component::Browser, at, BROWSER_WIDTH, window, cx);
    }

    /// The folder the Browser surface shows: the selected session's, else the project's,
    /// or the machine the selected session runs on, when it isn't this one.
    fn browser_folder(&self, cx: &App) -> Result<Option<String>, String> {
        let project = || {
            self.presentation
                .project
                .as_ref()
                .ok()
                .and_then(|project| project.path().to_str().map(str::to_owned))
        };
        match self.selected(cx).and_then(|id| self.session_place(id, cx)) {
            Some(SessionPlace::Local(directory)) => Ok(Some(directory.as_str().to_owned())),
            Some(SessionPlace::Remote(machine)) => Err(machine),
            Some(SessionPlace::Unusable) | None => Ok(project()),
        }
    }

    pub(crate) fn browser_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (folder, elsewhere) = match self.browser_folder(cx) {
            Ok(folder) => (folder, None),
            Err(machine) => (None, Some(machine)),
        };
        self.browser_drawn = true;
        self.browser_unseen = false;
        self.browser
            .update(cx, |view, cx| view.set_folder(folder, elsewhere, cx));
        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(self.browser.clone())
            .into_any_element()
    }

    /// Focuses the page when one shows, else the address field.
    pub(crate) fn focus_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.browser.read(cx);
        let focus = match view.page(cx) {
            Some(page) if page.url() != "about:blank" => view.focus.clone(),
            _ => view.address.focus_handle(cx),
        };
        window.focus(&focus, cx);
    }

    pub(crate) fn browser_contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.browser
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
            || self
                .browser
                .read(cx)
                .address
                .focus_handle(cx)
                .contains_focused(window, cx)
    }
}
