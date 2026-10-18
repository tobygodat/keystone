mod conversation;
mod customization;
mod customization_agent;
mod relative_time;
mod reveal;
mod settings;
mod transcript;
mod view_state;

use conversation::{Conversation, Row};
use customization::Customization;
use futures::StreamExt;
use gpui::{
    Anchor, AnyElement, AnyView, App, AvailableSpace, BackgroundExecutor, Bounds, BoxShadow,
    Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, ListOffset, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Render, ScrollHandle, StyleRefinement, Subscription,
    WeakEntity, Window, WindowBounds, WindowOptions, canvas, div, list, point, prelude::*, px,
    size,
};
use keystone_agent::log::{Logs, Report};
use keystone_agent::{
    ApprovalState, ClaudeConnection, CodexConnection, Command, Connection, Event, ItemKind,
    RequestId, TurnStatus, Wake,
};
use keystone_core::{
    Appearance, ApprovalMode, Attention, Density, Durability, Provider, SidebarSide, Store,
    ThemeMode,
};
use keystone_ui::markdown::Document;
use keystone_ui::text_input::{Changed, SelectAll};
use keystone_ui::{automation, prelude::*};
use relative_time::{LocalTime, relative};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use view_state::{ViewId, ViewKey, ViewStates};

gpui::actions!(
    keystone,
    [
        Quit,
        NewClaude,
        NewCodex,
        ToggleSettle,
        FocusComposer,
        FocusSearch,
        SendMessage,
        RenameSession
    ]
);

/// The transcript and the composer share one centred column.
const CONTENT_WIDTH: f32 = 640.;

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude Code",
        Provider::Codex => "Codex",
    }
}

fn provider_color(provider: Provider, cx: &App) -> Color {
    let colors = &cx.theme().colors;
    Color::Custom(match provider {
        Provider::Claude => colors.provider_claude,
        Provider::Codex => colors.provider_codex,
    })
}

/// Keystone's mark: two muted side stones and the keystone in the accent colour.
fn arch_mark(size: Pixels) -> impl IntoElement {
    div()
        .relative()
        .flex_shrink_0()
        .size(size)
        .child(Icon::new(IconName::ArchStones).size(size))
        .child(
            div().absolute().top_0().left_0().child(
                Icon::new(IconName::ArchKeystone)
                    .size(size)
                    .color(Color::Accent),
            ),
        )
}

fn ranges(mut rows: Vec<usize>) -> Vec<Range<usize>> {
    rows.sort_unstable();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for index in rows {
        match ranges.last_mut() {
            Some(range) if range.end == index => range.end += 1,
            _ => ranges.push(index..index + 1),
        }
    }
    ranges
}

/// The wall clock in unix milliseconds. The store takes it as an argument, so its
/// tests pass fixed times.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Whether `event` ends a turn, and if so whether the turn failed. An interruption is a
/// normal end only when the user ended that turn, which the provider reports with it.
fn turn_failed(event: &Event) -> Option<bool> {
    match event {
        Event::TurnCompleted {
            status, stopped, ..
        } => Some(match status {
            TurnStatus::Completed => false,
            TurnStatus::Interrupted => !stopped,
            TurnStatus::Failed => true,
        }),
        Event::Failed { .. } => Some(true),
        _ => None,
    }
}

/// How long closing waits for saved histories to finish loading.
const LOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

fn durability_warning(durability: Durability) -> Option<String> {
    match durability {
        Durability::Synced => None,
        Durability::DirectorySyncPending(error) => Some(format!(
            "Change saved, but crash durability is unconfirmed. Retry saving before leaving. {error}"
        )),
    }
}

/// The open rename dialog. A failed save keeps it open with `error`.
struct Renaming {
    id: u64,
    focus: DialogFocus,
    error: Option<String>,
}

struct ComposerDraft {
    session: u64,
    text: String,
}

struct Keystone {
    store: Store,
    error: Option<String>,
    composer: Entity<TextInput>,
    search: Entity<TextInput>,
    session_sidebar: Entity<SessionSidebar>,
    _subscriptions: Vec<Subscription>,
    draft_save_task: Option<gpui::Task<()>>,
    focus: FocusHandle,
    new_menu: bool,
    settings_menu: bool,
    approval_menu: bool,
    /// Full access chosen from the menu for this session, waiting in the confirmation
    /// dialog. Nothing is saved or sent until Confirm.
    confirm_full_access: Option<(u64, DialogFocus)>,
    renaming: Option<Renaming>,
    rename_input: Entity<TextInput>,
    /// Set by a press on the top bar. The first move then hands the window to the
    /// compositor, so a plain click never starts a move.
    window_drag: bool,
    /// Whether the sidebar's Settled section is expanded. It starts collapsed.
    settled_open: bool,
    sidebar: bool,
    fixture: bool,
    /// The shared Codex app-server connection, created on the first message.
    codex: Option<CodexConnection>,
    /// Claude Code processes, one per session, created on the first message.
    claude: Option<ClaudeConnection>,
    conversations: HashMap<u64, Conversation>,
    /// Each session's log of completed items, read and written off the UI thread.
    logs: Logs,
    views: ViewStates,
    /// The directory agents work in.
    project_dir: PathBuf,
    wake: Wake,
    /// Scrolls the sample transcript, which is not a list.
    transcript_scroll: ScrollHandle,
    /// Each reply row's Markdown and how much of it is shown. The open session's
    /// streaming replies catch up with each burst over a few frames.
    replies: RefCell<reveal::Replies>,
    reveal_task: Option<gpui::Task<()>>,
    clock: BackgroundExecutor,
    /// The sample transcript, parsed once.
    fixture_document: Arc<Document>,
    customization: Customization,
    customize_prompt: Entity<TextInput>,
    customize_provider: Provider,
    customize_agent: Option<customization_agent::Agent>,
    sidebar_before_customize: Option<bool>,
    sidebar_drag: Option<customization::SidebarDrag>,
}

impl Keystone {
    fn new(store: Store, fixture: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            TextInput::new("Write a draft…", InputMode::Multiline, cx)
                .text_style(TextSize::Body, Color::Strong)
        });
        let search = cx.new(|cx| TextInput::new("Search", InputMode::SingleLine, cx));
        let rename_input = cx.new(|cx| TextInput::new("Session title", InputMode::SingleLine, cx));
        let customize_prompt = cx.new(|cx| {
            TextInput::new(
                "e.g. Put the sidebar on the right and make it a bit wider",
                InputMode::SingleLine,
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe(&composer, |this, _, _: &Changed, cx| {
                this.draft_save_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                    // The sidebar shows each draft's first line, so it refreshes once the
                    // debounced save lands rather than on every keystroke.
                    let _ = this.update(cx, |this, cx| {
                        this.save_draft(cx);
                        this.refresh(cx);
                    });
                }));
                cx.notify();
            }),
            cx.subscribe(&search, |this, _, _: &Changed, cx| this.refresh(cx)),
            cx.observe_window_appearance(window, |this, window, cx| this.apply_theme(window, cx)),
        ];
        let app = cx.weak_entity();
        let session_sidebar = cx.new(|_| SessionSidebar {
            app,
            hovered: None,
            shows_time: false,
        });
        // Reader threads wake this task when provider events arrive. Each wake-up drains
        // everything that arrived, so a burst of streamed text costs one update.
        let (wake_tx, mut wake_rx) = futures::channel::mpsc::unbounded::<()>();
        cx.spawn(async move |this, cx| {
            while wake_rx.next().await.is_some() {
                if this.update(cx, |this, cx| this.pump(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // Relative times age, so the sidebar redraws once a minute while it shows one.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(60))
                    .await;
                let aged = this.update(cx, |this, cx| {
                    if this.sidebar && this.session_sidebar.read(cx).shows_time {
                        this.refresh(cx);
                    }
                });
                if aged.is_err() {
                    break;
                }
            }
        })
        .detach();
        let wake: Wake = Arc::new(move || {
            let _ = wake_tx.unbounded_send(());
        });
        let sessions_dir = store
            .path()
            .parent()
            .unwrap_or(Path::new("."))
            .join("sessions");
        let project_dir = std::env::var_os("KEYSTONE_PROJECT_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let mut result = Self {
            store,
            error: None,
            composer,
            search,
            session_sidebar,
            _subscriptions: subscriptions,
            draft_save_task: None,
            focus: cx.focus_handle(),
            new_menu: false,
            settings_menu: false,
            approval_menu: false,
            confirm_full_access: None,
            renaming: None,
            rename_input,
            window_drag: false,
            settled_open: false,
            sidebar: true,
            fixture,
            codex: None,
            claude: None,
            conversations: HashMap::new(),
            logs: Logs::new(sessions_dir, wake.clone()),
            views: ViewStates::default(),
            project_dir,
            wake,
            transcript_scroll: ScrollHandle::new(),
            replies: RefCell::default(),
            reveal_task: None,
            clock: cx.background_executor().clone(),
            fixture_document: Arc::new(Document::parse(if fixture {
                include_str!("fixture.md")
            } else {
                ""
            })),
            customization: Customization::default(),
            customize_prompt,
            customize_provider: Provider::Claude,
            customize_agent: None,
            sidebar_before_customize: None,
            sidebar_drag: None,
        };
        result.sync_draft(cx);
        result.apply_theme(window, cx);
        result
    }

    fn apply_theme(&self, window: &Window, cx: &mut App) {
        keystone_ui::set_theme(
            settings::theme_for(
                self.store.workspace().preferences().appearance(),
                window,
                cx,
            ),
            cx,
        );
    }

    fn update_appearance(
        &mut self,
        change: impl FnOnce(&mut Appearance),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut appearance = self.store.workspace().preferences().appearance().clone();
        change(&mut appearance);
        self.error = match self.store.set_appearance(appearance) {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(format!("Settings not saved. {error}")),
        };
        self.apply_theme(window, cx);
        self.refresh(cx);
    }

    /// Notifies this view and the cached sidebar. Use it for any change the sidebar shows.
    /// Composer typing and draft saves notify only this view, so they reuse the sidebar.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        self.session_sidebar.update(cx, |_, cx| cx.notify());
    }

    fn sync_draft(&mut self, cx: &mut Context<Self>) {
        let value = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| Some(s.id()) == self.store.workspace().selected())
            .map(|s| s.draft().to_owned())
            .unwrap_or_default();
        self.composer
            .update(cx, |input, cx| input.set_value(value, cx));
    }

    fn save_draft(&mut self, cx: &mut Context<Self>) -> bool {
        let session_id = self.store.workspace().selected();
        let result = if let Some(id) = session_id {
            let draft = self.composer.read(cx).value().to_owned();
            self.store
                .set_draft(id, draft)
                .map(|applied| applied.durability)
        } else {
            self.store
                .sync_pending()
                .map(|()| Durability::Synced)
                .map_err(Into::into)
        };
        match result {
            Ok(durability) => {
                self.error = durability_warning(durability);
                cx.notify();
                self.error.is_none()
            }
            Err(error) => {
                self.error = Some(format!(
                    "Draft not saved. Stay in this session and retry after fixing storage. {error}"
                ));
                cx.notify();
                false
            }
        }
    }

    /// Saves the draft, writes the session logs, and marks the open session viewed.
    /// Returns false, with the error shown, when the draft or the session cannot be
    /// saved, so the app stays open. The logs are written either way.
    fn save_before_exit(&mut self, cx: &mut Context<Self>) -> bool {
        let saved = self.save_draft(cx);
        // Before `leave`: the last events may end a turn the user is watching.
        self.flush_logs(cx);
        if !saved {
            return false;
        }
        match self.store.leave(now_ms()) {
            Ok(applied) => {
                self.error = durability_warning(applied.durability);
                self.error.is_none()
            }
            Err(error) => {
                self.error = Some(format!("Could not save before closing. {error}"));
                cx.notify();
                false
            }
        }
    }

    fn create(&mut self, provider: Provider, cx: &mut Context<Self>) {
        if !self.save_draft(cx) {
            return;
        }
        if let Some(left) = self.store.workspace().selected() {
            let rows = self.replies.get_mut().settle(left);
            self.invalidate(left, ranges(rows), false);
        }
        self.error = match self.store.create(provider, now_ms()) {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(error.to_string()),
        };
        self.new_menu = false;
        self.sync_draft(cx);
        self.refresh(cx);
    }

    fn toggle_settle(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| Some(s.id()) == self.store.workspace().selected())
            .map(|s| (s.id(), s.settled()));
        if let Some((id, settled)) = selected {
            self.settle(id, !settled, cx);
        }
    }

    /// Settles or un-settles one session. Selection does not change.
    fn settle(&mut self, id: u64, settled: bool, cx: &mut Context<Self>) {
        if !self.save_draft(cx) {
            return;
        }
        self.error = match self.store.set_settled(id, settled) {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(error.to_string()),
        };
        self.refresh(cx);
    }

    fn transcript(&self) -> gpui::Div {
        v_flex()
            .gap_5()
            .text_size(TextSize::Body.px())
            .line_height(px(22.))
            .child(
                Label::new("Interface preview · sample transcript")
                    .size(TextSize::Ui)
                    .color(Color::Muted),
            )
            .child(Markdown::new("fixture", self.fixture_document.clone()))
    }

    fn select(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.save_draft(cx) {
            return;
        }
        if let Some(left) = self.store.workspace().selected() {
            let rows = self.replies.get_mut().settle(left);
            self.invalidate(left, ranges(rows), false);
        }
        self.replies.get_mut().forget(id);
        self.error = match self.store.select(id, now_ms()) {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(error.to_string()),
        };
        self.sync_draft(cx);
        self.views.scroll_to_end(ViewKey {
            view: ViewId::MAIN,
            session: id,
        });
        self.refresh(cx);
    }

    fn start(&mut self, provider: Provider, window: &mut Window, cx: &mut Context<Self>) {
        self.create(provider, cx);
        if self.error.is_none() && self.store.workspace().selected().is_some() {
            self.composer.focus_handle(cx).focus(window, cx);
        }
    }

    /// Lets a press on `bar` move the window, for window managers that draw no title
    /// bar. The press arms the move before any control sees it, and a control wrapped in
    /// [`Keystone::own_press`] disarms it, so a press that drifts on a control still clicks.
    fn drag_window(&self, bar: gpui::Div, cx: &mut Context<Self>) -> gpui::Div {
        bar.capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
            this.window_drag = event.button == MouseButton::Left;
        }))
        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, _| {
            if std::mem::take(&mut this.window_drag) && event.dragging() {
                window.start_window_move();
            }
        }))
    }

    /// Keeps a press on `control` from moving the window around it.
    fn own_press(&self, control: impl IntoElement, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.window_drag = false),
            )
            .child(control)
    }

    /// The main column's bar: the sidebar toggle, the breadcrumb, and the window controls.
    fn top_bar(&self, session: Option<(String, bool)>, cx: &mut Context<Self>) -> impl IntoElement {
        let hover = cx.theme().colors.element_hover;
        let bar = h_flex()
            .flex_shrink_0()
            .h(cx.theme().density.control(px(44.)))
            .px(px(6.))
            .gap(px(10.))
            .child(
                self.own_press(
                    IconButton::new("sidebar-toggle", IconName::Panel)
                        .debug_selector(|| "sidebar-toggle".into())
                        .tooltip(Tooltip::text(if self.sidebar {
                            "Hide sidebar"
                        } else {
                            "Show sidebar"
                        }))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar = !this.sidebar;
                            this.refresh(cx);
                        })),
                    cx,
                ),
            )
            .child(Label::new("keystone").color(Color::Muted))
            .when_some(session, |d, (title, settled)| {
                d.child(Label::new("/").color(Color::Faint))
                    .child(
                        div()
                            .id("rename-session")
                            .automation_id("rename-session")
                            .min_w_0()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, _| this.window_drag = false),
                            )
                            .child(
                                Label::new(title)
                                    .weight(FontWeight::MEDIUM)
                                    .color(Color::Strong)
                                    .truncate(),
                            )
                            .tooltip(Tooltip::with_key("Rename session", "F2").builder())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_rename(window, cx)),
                            ),
                    )
                    .when(settled, |d| {
                        d.child(Label::new("Settled").color(Color::Muted))
                    })
            })
            .child(div().flex_1())
            .child(self.customize_toggle(cx))
            .child(
                self.own_press(
                    h_flex()
                        .gap(px(10.))
                        .child(
                            IconButton::new("minimize", IconName::Minus)
                                .debug_selector(|| "minimize".into())
                                .tooltip(Tooltip::text("Minimize"))
                                .on_click(|_, window, _| window.minimize_window()),
                        )
                        .child(
                            IconButton::new("maximize", IconName::Square)
                                .debug_selector(|| "maximize".into())
                                .tooltip(Tooltip::text("Maximize"))
                                .on_click(|_, window, _| window.zoom_window()),
                        )
                        .child(
                            IconButton::new("close", IconName::Close)
                                .debug_selector(|| "close".into())
                                .tooltip(Tooltip::with_key("Close window", "Ctrl+Q"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if this.save_before_exit(cx) {
                                        window.remove_window();
                                    }
                                })),
                        ),
                    cx,
                ),
            );
        self.drag_window(bar, cx)
    }

    /// Opens the rename dialog for the selected session, filled with its title.
    fn open_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let (id, title) = (session.id(), session.title().to_owned());
        let field = self.rename_input.focus_handle(cx);
        self.renaming = Some(Renaming {
            id,
            focus: DialogFocus::with_body(field, window, cx),
            error: None,
        });
        self.rename_input.update(cx, |input, cx| {
            input.set_value(title, cx);
            input.select_all(&SelectAll, window, cx);
        });
        cx.notify();
    }

    fn close_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = None;
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Saves the typed title. A blank one keeps the dialog open; a failed save keeps
    /// it open with the typed title and the error, so the user can retry.
    fn confirm_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.renaming.as_ref().map(|renaming| renaming.id) else {
            return;
        };
        let title = self.rename_input.read(cx).value().to_owned();
        match self.store.rename(id, &title) {
            Ok(applied) => {
                self.error = durability_warning(applied.durability);
                self.close_rename(window, cx);
            }
            Err(error) => {
                if let Some(renaming) = &mut self.renaming {
                    renaming.error = Some(format!("Title not saved. {error}"));
                }
                self.rename_input.focus_handle(cx).focus(window, cx);
            }
        }
        self.refresh(cx);
    }

    fn rename_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let renaming = self.renaming.as_ref()?;
        let colors = cx.theme().colors;
        let cancel = cx.listener(|this, _: &(), window, cx| this.close_rename(window, cx));
        let confirm =
            Rc::new(cx.listener(|this, _: &(), window, cx| this.confirm_rename(window, cx)));
        let submit = confirm.clone();
        let field = div()
            .px(px(10.))
            .rounded(px(6.))
            .border_1()
            .border_color(colors.border_focused)
            .bg(colors.background)
            .automation_id("rename-field")
            .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter" {
                    submit(&(), window, cx);
                    cx.stop_propagation();
                }
            })
            .child(self.rename_input.clone());
        let body = v_flex()
            .gap(spacing::SM)
            .child(field)
            .children(renaming.error.clone().map(|error| {
                div()
                    .automation_id("rename-error")
                    .child(Label::new(error).size(TextSize::Sm).color(Color::Danger))
            }));
        Some(
            ConfirmDialog::new(
                "rename-dialog",
                &renaming.focus,
                "Rename session",
                "Shown in the sidebar and above the conversation.",
            )
            .icon(IconName::Pencil)
            .tone(Color::Accent)
            .confirm_label("Rename")
            .body(body)
            .on_cancel(move |window, cx| cancel(&(), window, cx))
            .on_confirm(move |window, cx| confirm(&(), window, cx)),
        )
    }

    fn sidebar_column(&self, width: f32, side: SidebarSide, cx: &mut Context<Self>) -> gpui::Div {
        let (colors, density) = (cx.theme().colors, cx.theme().density);
        let list = AnyView::from(self.session_sidebar.clone());
        let new_session = self.new_session_menu(cx).into_any_element();
        v_flex()
            .relative()
            .w(px(width))
            .flex_shrink_0()
            .h_full()
            .overflow_hidden()
            .bg(colors.sidebar)
            .map(|d| match side {
                SidebarSide::Left => d.hairline_r(cx),
                SidebarSide::Right => d.hairline_l(cx),
            })
            .child(
                self.drag_window(
                    h_flex()
                        .flex_shrink_0()
                        .h(density.control(px(44.)))
                        .pl(px(13.))
                        .pr_2()
                        .gap(px(10.))
                        .child(arch_mark(px(20.)))
                        .child(
                            Label::new("keystone")
                                .size(TextSize::Body)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            h_flex()
                                .h(px(18.))
                                .px(px(6.))
                                .rounded(px(5.))
                                .bg(colors.element_hover)
                                .child(
                                    Label::new("preview").size(TextSize::Xs).color(Color::Muted),
                                ),
                        )
                        .child(div().flex_1())
                        .child(self.own_press(new_session, cx)),
                    cx,
                ),
            )
            .child(
                h_flex()
                    .id("search")
                    .flex_shrink_0()
                    .h(density.control(px(36.)))
                    .pl(px(14.))
                    .pr_2()
                    .gap_2()
                    .cursor(gpui::CursorStyle::IBeam)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.search.focus_handle(cx).focus(window, cx)
                    }))
                    .child(Icon::new(IconName::Search).size(px(15.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .automation_id("search-field")
                            .child(self.search.clone()),
                    )
                    .child(Label::new("Ctrl F").size(TextSize::Sm).color(Color::Faint)),
            )
            // A reused cached view skips prepaint, so it would report no element
            // geometry. Verification trades that speed for names.
            .map(|d| {
                if automation::enabled() {
                    d.child(div().flex_1().min_h_0().w_full().child(list))
                } else {
                    d.child(list.cached(StyleRefinement::default().flex_1().min_h_0().w_full()))
                }
            })
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h(density.control(px(40.)))
                    .pl(px(6.))
                    .pr(px(14.))
                    .gap_0p5()
                    .hairline_t(cx)
                    .child(self.settings_menu(cx))
                    .child(
                        div().flex_1().child(
                            Label::new("Local workspace")
                                .size(TextSize::Ui)
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        Label::new("Claude Code + Codex")
                            .size(TextSize::Sm)
                            .color(Color::Faint),
                    ),
            )
    }

    fn new_session_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        PopoverMenu::new(
            "new-session-menu",
            IconButton::new("new-session", IconName::Pencil)
                .debug_selector(|| "new-session".into())
                .selected(self.new_menu)
                .tooltip(Tooltip::text("New session")),
        )
        .anchor(Anchor::TopRight)
        .open(self.new_menu)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.new_menu = *open;
            cx.notify();
        }))
        .child(
            MenuItem::new("new-claude", "New Claude session")
                .icon(IconName::Chat)
                .kbd("Ctrl+N")
                .debug_selector(|| "new-claude".into())
                .on_click(
                    cx.listener(|this, _, window, cx| this.start(Provider::Claude, window, cx)),
                ),
        )
        .child(
            MenuItem::new("new-codex", "New Codex session")
                .icon(IconName::Chat)
                .kbd("Ctrl+Shift+N")
                .debug_selector(|| "new-codex".into())
                .on_click(
                    cx.listener(|this, _, window, cx| this.start(Provider::Codex, window, cx)),
                ),
        )
    }

    fn settings_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = self.store.workspace().preferences().appearance().clone();
        let fonts = if self.settings_menu {
            settings::font_choices(&appearance, cx)
        } else {
            Vec::new()
        };
        let themes = [
            (ThemeMode::System, "System", "theme-system"),
            (ThemeMode::Light, "Light", "theme-light"),
            (ThemeMode::Dark, "Dark", "theme-dark"),
        ];
        let densities = [
            (Density::Compact, "Compact", "density-compact"),
            (Density::Default, "Default", "density-default"),
            (Density::Spacious, "Spacious", "density-spacious"),
        ];
        let font_items = std::iter::once(None)
            .chain(fonts.into_iter().map(Some))
            .enumerate()
            .map(|(index, font)| {
                let checked = font == appearance.ui_font;
                let label = font.clone().unwrap_or_else(|| "Default".into());
                MenuItem::new(("font", index), label)
                    .checked(checked)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let font = font.clone();
                        this.update_appearance(|a| a.ui_font = font, window, cx)
                    }))
            })
            .collect::<Vec<_>>();
        PopoverMenu::new(
            "settings-menu",
            IconButton::new("settings", IconName::Settings)
                .size(ButtonSize::Large)
                .debug_selector(|| "settings".into())
                .selected(self.settings_menu)
                .tooltip(Tooltip::text("Settings")),
        )
        .anchor(Anchor::BottomLeft)
        .open(self.settings_menu)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.settings_menu = *open;
            cx.notify();
        }))
        .child(MenuHeader::new("Theme"))
        .children(themes.map(|(mode, label, selector)| {
            MenuItem::new(selector, label)
                .checked(appearance.theme == mode)
                .debug_selector(|| selector.into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_appearance(|a| a.theme = mode, window, cx)
                }))
        }))
        .child(MenuSeparator)
        .child(MenuHeader::new("Density"))
        .children(densities.map(|(density, label, selector)| {
            MenuItem::new(selector, label)
                .checked(appearance.density == density)
                .debug_selector(|| selector.into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_appearance(|a| a.density = density, window, cx)
                }))
        }))
        .child(MenuSeparator)
        .child(MenuHeader::new("Font"))
        .children(font_items)
    }

    fn composer(&self, current: Option<(u64, Provider)>, cx: &mut Context<Self>) -> gpui::Div {
        let colors = cx.theme().colors;
        let conversation = current.and_then(|(id, _)| self.conversations.get(&id));
        let running = conversation.is_some_and(|c| c.running);
        let waiting = conversation.is_some_and(|c| !c.approvals.is_empty());
        let mode = self.selected_session().map(|s| s.approval_mode());
        let can_send = current.is_some() && !self.composer.read(cx).value().trim().is_empty();
        let field = div()
            .id("composer-field")
            .automation_id("composer-field")
            .min_h(px(44.))
            .pr(px(6.));
        let (field, toolbar) = match current {
            None => (
                field
                    .text_size(TextSize::Body.px())
                    .line_height(px(20.))
                    .text_color(colors.text_placeholder)
                    .child("Choose a provider below to start a session…"),
                h_flex()
                    .gap_2()
                    .child(self.welcome_pill(Provider::Claude, cx))
                    .child(self.welcome_pill(Provider::Codex, cx))
                    .child(div().flex_1())
                    .child(
                        Label::new("Ctrl N · Ctrl Shift N")
                            .size(TextSize::Sm)
                            .color(Color::Muted),
                    ),
            ),
            Some((session, provider)) => {
                let status = if waiting {
                    "Waiting for your approval"
                } else if running {
                    "Ctrl+Enter steers this turn"
                } else {
                    "Ctrl+Enter to send · Enter for a new line"
                };
                let project = self.project_dir.file_name().map_or_else(
                    || self.project_dir.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let project = Button::new("project", project)
                    .size(ButtonSize::Compact)
                    .icon(IconName::Folder)
                    .debug_selector(|| "project".into())
                    .tooltip(Tooltip::text(format!(
                        "Open project folder · {}",
                        self.project_dir.display()
                    )))
                    .on_click(cx.listener(|this, _, _, cx| cx.open_with_system(&this.project_dir)));
                let toolbar = h_flex()
                    .gap(px(14.))
                    .child(
                        h_flex()
                            .flex_shrink(1.)
                            .min_w_0()
                            .gap(px(7.))
                            .child(
                                Icon::new(IconName::Diamond)
                                    .size(px(12.))
                                    .color(provider_color(provider, cx)),
                            )
                            .child(
                                Label::new(provider_name(provider))
                                    .size(TextSize::Ui)
                                    .weight(FontWeight::MEDIUM),
                            )
                            .child(
                                Label::new(status)
                                    .size(TextSize::Ui)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .child(div().flex_shrink_0().w_px().h(px(14.)).bg(colors.border))
                    .child(project)
                    .when_some(mode, |d, mode| {
                        d.child(self.approval_menu(mode, running, cx))
                    })
                    .child(div().flex_1())
                    .when(running, |d| {
                        d.child(
                            Button::new("stop", "Stop")
                                .style(ButtonStyle::Subtle)
                                .icon(IconName::Square)
                                .debug_selector(|| "stop".into())
                                .tooltip(Tooltip::text("Stop this turn"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.stop_turn(session, window, cx)
                                })),
                        )
                    })
                    .child(
                        div()
                            .id("send")
                            .automation_id("send")
                            .flex_shrink_0()
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(if can_send {
                                colors.element_inverse
                            } else {
                                colors.element_disabled
                            })
                            .child(Icon::new(IconName::Up).size(px(15.)).color(if can_send {
                                Color::Custom(colors.text_inverse)
                            } else {
                                Color::Faint
                            }))
                            .tooltip(
                                Tooltip::with_key(
                                    if running { "Steer" } else { "Send" },
                                    "Ctrl+Enter",
                                )
                                .builder(),
                            )
                            .when(can_send, |d| {
                                d.cursor_pointer()
                                    .hover(|s| s.opacity(0.85))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(draft) = this
                                            .composer_draft(cx)
                                            .filter(|draft| draft.session == session)
                                        {
                                            this.send_message(draft, window, cx)
                                        }
                                    }))
                            })
                            .when(!can_send, |d| d.cursor_not_allowed()),
                    );
                (
                    field
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.composer.focus_handle(cx).focus(window, cx)
                        }))
                        .child(self.composer.clone()),
                    toolbar,
                )
            }
        };
        v_flex()
            // Not `w_full().max_w(..)`: taffy sizes the field from a pass that measures the
            // draft at the uncapped width, so a wrapped draft overflowed a one-row field.
            .w(px(CONTENT_WIDTH))
            .max_w_full()
            .flex_shrink_0()
            .bg(colors.composer)
            .border_1()
            .border_color(colors.composer_border)
            .rounded(radius::COMPOSER)
            .shadow(vec![BoxShadow {
                color: colors.composer_shadow,
                offset: point(px(0.), px(10.)),
                blur_radius: px(30.),
                spread_radius: px(0.),
                inset: false,
            }])
            .pt(px(16.))
            .pl(px(18.))
            .pr(px(12.))
            .pb(px(12.))
            .gap(px(14.))
            .child(field)
            .child(toolbar)
    }

    fn welcome_pill(&self, provider: Provider, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let (id, tooltip, key) = match provider {
            Provider::Claude => ("welcome-claude", "New Claude session", "Ctrl+N"),
            Provider::Codex => ("welcome-codex", "New Codex session", "Ctrl+Shift+N"),
        };
        h_flex()
            .id(id)
            .automation_id(id)
            .h(px(30.))
            .px(px(11.))
            .gap(px(7.))
            .rounded(px(15.))
            .bg(colors.element_filled)
            .cursor_pointer()
            .hover(move |s| s.bg(colors.element_filled_hover))
            .child(
                Icon::new(IconName::Diamond)
                    .size(px(12.))
                    .color(provider_color(provider, cx)),
            )
            .child(
                Label::new(provider_name(provider))
                    .size(TextSize::Ui)
                    .weight(FontWeight::MEDIUM),
            )
            .tooltip(Tooltip::with_key(tooltip, key).builder())
            .on_click(cx.listener(move |this, _, window, cx| this.start(provider, window, cx)))
    }

    fn selected_session(&self) -> Option<&keystone_core::Session> {
        let workspace = self.store.workspace();
        let id = workspace.selected()?;
        workspace.sessions().iter().find(|s| s.id() == id)
    }

    /// The session's approval mode, as a menu of every mode with what it allows.
    fn approval_menu(
        &self,
        mode: ApprovalMode,
        running: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tooltip = if running {
            "Approval mode · a change applies from the next turn"
        } else {
            "Approval mode · applies from the next turn"
        };
        PopoverMenu::new(
            "approval-menu",
            Button::new("approval-mode", mode.label())
                .size(ButtonSize::Compact)
                .icon(IconName::Shield)
                .selected(self.approval_menu)
                .when(mode == ApprovalMode::FullAccess, |b| {
                    b.color(Color::Warning)
                })
                .debug_selector(|| "approval-mode".into())
                .tooltip(Tooltip::text(tooltip)),
        )
        .anchor(Anchor::BottomLeft)
        .open(self.approval_menu)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.approval_menu = *open;
            cx.notify();
        }))
        .child(MenuHeader::new("Approvals"))
        .children(ApprovalMode::ALL.map(|choice| {
            MenuItem::new(("approval", choice as usize), choice.label())
                .detail(choice.description())
                .checked(choice == mode)
                .debug_selector(|| format!("approval-{}", choice.slug()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.choose_approval_mode(choice, window, cx)
                }))
        }))
    }

    /// A choice from the approval menu. The current mode changes nothing, and Full
    /// access asks first; any other mode is saved and applied at once.
    fn choose_approval_mode(
        &mut self,
        mode: ApprovalMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.approval_menu = false;
        let Some(session) = self.selected_session() else {
            return;
        };
        let (id, current) = (session.id(), session.approval_mode());
        if mode == current {
            self.composer.focus_handle(cx).focus(window, cx);
        } else if mode == ApprovalMode::FullAccess {
            self.confirm_full_access = Some((id, DialogFocus::new(window, cx)));
        } else {
            self.set_approval_mode(id, mode, window, cx);
        }
        cx.notify();
    }

    /// The confirmation for Full access, while it is open.
    fn full_access_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (id, focus) = self.confirm_full_access.as_ref()?;
        let provider = provider_name(self.provider(*id)?);
        let cancel = cx.listener(|this, _: &(), window, cx| {
            this.confirm_full_access = None;
            this.composer.focus_handle(cx).focus(window, cx);
            cx.notify();
        });
        let confirm = cx.listener(|this, _: &(), window, cx| {
            if let Some((id, _)) = this.confirm_full_access.take() {
                this.set_approval_mode(id, ApprovalMode::FullAccess, window, cx);
            }
        });
        Some(
            ConfirmDialog::new(
                "full-access-dialog",
                focus,
                "Turn on Full access?",
                format!(
                    "{provider} will run commands and edit files without asking, \
                     until you choose another approval mode."
                ),
            )
            .tone(Color::Warning)
            .on_cancel(move |window, cx| cancel(&(), window, cx))
            .on_confirm(move |window, cx| confirm(&(), window, cx)),
        )
    }

    /// Saves the session's mode, then hands it to its provider at once, so a
    /// message already waiting to start a turn, or a steer that becomes the next turn,
    /// runs under it. The save comes first: it also records that the provider may hold
    /// an override, and nothing reaches the provider unless that is saved.
    fn set_approval_mode(
        &mut self,
        id: u64,
        mode: ApprovalMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.store.set_approval_mode(id, mode) {
            Ok(applied) => {
                self.error = durability_warning(applied.durability);
                if let Some((provider, approval)) = self.approval(id)
                    && let Some(connection) = self.started(provider)
                {
                    connection.set_approval(id, approval);
                }
            }
            Err(error) => self.error = Some(format!("Approval mode not saved. {error}")),
        }
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// The session's provider and its saved approval state.
    fn approval(&self, session: u64) -> Option<(Provider, ApprovalState)> {
        let session = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == session)?;
        Some((
            session.provider(),
            ApprovalState {
                mode: session.approval_mode(),
                overridden: session.approval_overridden(),
            },
        ))
    }

    /// The session's conversation. The first call starts loading its saved history.
    fn conversation(&mut self, session: u64) -> &mut Conversation {
        let logs = &self.logs;
        self.conversations.entry(session).or_insert_with(|| {
            logs.load(session);
            Conversation::loading()
        })
    }

    /// Queues what the conversation logged and tells its list which rows changed.
    fn commit(&mut self, session: u64) {
        let Some(conversation) = self.conversations.get_mut(&session) else {
            return;
        };
        self.logs.append(session, conversation.take_writes());
        let changed = conversation.take_changes();
        if self.store.workspace().selected() == Some(session) {
            let now = self.clock.now();
            let replies = self.replies.get_mut();
            for index in changed.iter().cloned().flatten() {
                if let Some(Row::Item(item)) = conversation.rows.get(index)
                    && item.kind == ItemKind::AgentMessage
                {
                    replies.observe((session, index), item, now);
                }
            }
        }
        self.invalidate(session, changed, true);
    }

    fn invalidate(&mut self, session: u64, changed: Vec<Range<usize>>, status: bool) {
        if let Some(conversation) = self.conversations.get(&session) {
            self.views
                .invalidate(session, conversation.rows.len(), &changed, status);
        }
    }

    fn reveal(&mut self, cx: &mut Context<Self>) {
        if self.reveal_task.is_some() || !self.replies.get_mut().pacing() {
            return;
        }
        self.reveal_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(reveal::TICK).await;
            let _ = this.update(cx, |this, cx| {
                this.reveal_task = None;
                let conversations = &this.conversations;
                let changed = this
                    .replies
                    .get_mut()
                    .tick(this.clock.now(), |(session, index)| {
                        match conversations.get(&session)?.rows.get(index)? {
                            Row::Item(item) if item.kind == ItemKind::AgentMessage => Some(item),
                            _ => None,
                        }
                    });
                let mut sessions: HashMap<u64, Vec<usize>> = HashMap::new();
                for (session, index) in changed {
                    sessions.entry(session).or_default().push(index);
                }
                for (session, rows) in sessions {
                    this.invalidate(session, ranges(rows), false);
                    cx.notify();
                }
                this.reveal(cx);
            });
        }));
    }

    /// Runs before the transcript list lays out. When the row the user is scrolled
    /// into was remeasured, the kept offset can now fall past its end, such as when a
    /// long reasoning summary is replaced by an empty one, or when the transcript
    /// widens and its text wraps onto fewer lines. The view would then skip the rows
    /// below it, so it moves to the first row after the place it was. It spans the
    /// transcript's width so it can measure the row at the width the list is about to
    /// use.
    fn transcript_top_check(&self, key: ViewKey, cx: &mut Context<Self>) -> impl IntoElement {
        let app = cx.weak_entity();
        canvas(
            move |bounds, window, cx| {
                let _ = app.update(cx, |this, cx| {
                    this.clamp_top(key, bounds.size.width, window, cx)
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .w_full()
        .h_0()
    }

    fn clamp_top(
        &mut self,
        key: ViewKey,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let remeasured = self.views.take_remeasured_top(key);
        let Some(list) = self.views.get(key).cloned() else {
            return;
        };
        // The list remeasures every entry when its width changes, and it has not
        // laid out at this width yet.
        let resized = list.viewport_bounds().size.width != width;
        if !(remeasured || resized) || width <= px(0.) {
            return;
        }
        let space = size(AvailableSpace::Definite(width), AvailableSpace::MinContent);
        let mut top = list.logical_scroll_top();
        let start = top.item_ix;
        while top.item_ix < list.item_count() {
            let height = self
                .transcript_entry(key.session, top.item_ix, cx)
                .layout_as_root(space, window, cx)
                .height;
            if top.offset_in_item < height {
                break;
            }
            // The place was past this entry's end, or it has nothing to show.
            top = ListOffset {
                item_ix: top.item_ix + 1,
                offset_in_item: px(0.),
            };
        }
        if top.item_ix != start {
            list.scroll_to(top);
        }
    }

    /// Applies loaded history, save results, and every provider event that arrived
    /// since the last wake-up.
    fn pump(&mut self, cx: &mut Context<Self>) {
        let reports = self.logs.drain();
        let mut events = self.codex.as_ref().map(|c| c.drain()).unwrap_or_default();
        events.extend(self.claude.as_ref().map(|c| c.drain()).unwrap_or_default());
        if self.receive_customization() {
            self.refresh(cx);
        }
        self.receive(reports, events, cx);
    }

    fn receive(&mut self, reports: Vec<Report>, events: Vec<(u64, Event)>, cx: &mut Context<Self>) {
        if reports.is_empty() && events.is_empty() {
            return;
        }
        let mut sidebar_changed = false;
        for report in reports {
            let session = match report {
                Report::Loaded { session, entries } => {
                    if let Some(conversation) = self.conversations.get_mut(&session) {
                        conversation.restore(entries);
                    }
                    // Events held during the load, such as an approval or a turn's
                    // end, apply only now.
                    sidebar_changed = true;
                    session
                }
                Report::Saved { session, error } => {
                    if let Some(conversation) = self.conversations.get_mut(&session) {
                        conversation.save_error = error
                            .map(|error| format!("This conversation is not being saved. {error}"));
                    }
                    session
                }
            };
            self.commit(session);
        }
        for (session, event) in events {
            let failed = turn_failed(&event);
            let saved = match (&event, failed) {
                (Event::TurnStarted, _) => self.store.record_turn_start(session).map(drop),
                (_, Some(failed)) => self
                    .store
                    .record_turn_end(session, failed, now_ms())
                    .map(drop),
                _ => Ok(()),
            };
            if let Err(error) = saved {
                self.error = Some(format!("Could not save the session. {error}"));
            }
            sidebar_changed |= failed.is_some()
                || matches!(
                    event,
                    Event::ApprovalRequested(_) | Event::ApprovalResolved { .. }
                );
            let thread = self.conversation(session).apply(event);
            self.commit(session);
            if let Some(thread) = thread {
                let known = self
                    .store
                    .workspace()
                    .sessions()
                    .iter()
                    .any(|s| s.id() == session && s.provider_thread() == Some(thread.as_str()));
                if !known && let Err(error) = self.store.set_provider_thread(session, thread) {
                    self.error = Some(format!("Could not save the conversation ID. {error}"));
                }
            }
        }
        self.reveal(cx);
        // Streamed text redraws only this view. The sidebar refreshes when a row changes.
        if sidebar_changed {
            self.refresh(cx);
        } else {
            cx.notify();
        }
    }

    /// The provider's connection, created on first use. It finds and starts its
    /// program on its own thread.
    fn connection(&mut self, provider: Provider) -> &dyn Connection {
        let wake = self.wake.clone();
        match provider {
            Provider::Codex => self.codex.get_or_insert_with(|| {
                CodexConnection::new(
                    std::env::var_os("KEYSTONE_CODEX_PATH").map(PathBuf::from),
                    env!("CARGO_PKG_VERSION"),
                    wake,
                )
            }),
            Provider::Claude => self.claude.get_or_insert_with(|| {
                ClaudeConnection::new(
                    std::env::var_os("KEYSTONE_CLAUDE_PATH").map(PathBuf::from),
                    wake,
                )
            }),
        }
    }

    /// The provider's connection if it has been created.
    fn started(&self, provider: Provider) -> Option<&dyn Connection> {
        match provider {
            Provider::Codex => self.codex.as_ref().map(|c| c as &dyn Connection),
            Provider::Claude => self.claude.as_ref().map(|c| c as &dyn Connection),
        }
    }

    /// The session's provider.
    fn provider(&self, session: u64) -> Option<Provider> {
        self.store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == session)
            .map(|s| s.provider())
    }

    /// Writes everything queued for the session logs. Used before the app closes.
    ///
    /// A conversation still loading its history holds what was sent meanwhile, and
    /// logs it only once the history is applied. So apply every load first, or those
    /// messages would be lost. A read can hang, such as on a stalled network disk, so
    /// the wait for loads is bounded: a conversation whose history has not arrived by
    /// then goes on without it, which still logs what it holds after the history.
    fn flush_logs(&mut self, cx: &mut Context<Self>) {
        let deadline = std::time::Instant::now() + LOAD_WAIT;
        loop {
            let all = self.logs.wait_for_loads(deadline);
            self.pump(cx);
            // Applying events can start loading another session; finish that too.
            let loading: Vec<u64> = self
                .conversations
                .iter()
                .filter(|(_, c)| c.is_loading())
                .map(|(id, _)| *id)
                .collect();
            if loading.is_empty() {
                break;
            }
            if !all || std::time::Instant::now() >= deadline {
                for session in loading {
                    self.conversations
                        .get_mut(&session)
                        .expect("listed above")
                        .restore(Err("it did not load before Keystone closed".into()));
                    self.commit(session);
                }
                break;
            }
        }
        self.logs.flush();
    }

    fn composer_draft(&self, cx: &App) -> Option<ComposerDraft> {
        Some(ComposerDraft {
            session: self.store.workspace().selected()?,
            text: self.composer.read(cx).value().to_owned(),
        })
    }

    fn send_message(&mut self, draft: ComposerDraft, window: &mut Window, cx: &mut Context<Self>) {
        let id = draft.session;
        let Some(session) = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == id)
            .cloned()
        else {
            return;
        };
        let text = draft.text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        // Apply anything that already arrived, such as the end of the last turn, so
        // the message is shown in the right turn.
        self.pump(cx);
        let cwd = self.project_dir.to_string_lossy().into_owned();
        // Read after pumping: the pump may have just saved a newer thread ID.
        let resume = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == id)
            .and_then(|s| s.provider_thread().map(str::to_owned));
        // A mode other than the default can leave an override with the provider, so that
        // is saved before the message can carry one. Sessions saved by an earlier build
        // may have a mode without the record.
        let mode = session.approval_mode();
        if mode != ApprovalMode::Provider
            && !session.approval_overridden()
            && let Err(error) = self.store.set_approval_overridden(id)
        {
            self.conversation(id).error = Some(format!(
                "Not sent: the approval mode could not be saved first. {error}"
            ));
            self.commit(id);
            cx.notify();
            return;
        }
        let Some((provider, approval)) = self.approval(id) else {
            return;
        };
        // A failure to start the provider arrives later as an error in this
        // conversation. A refusal comes now, and the text stays in the composer.
        let sent =
            self.connection(provider)
                .send(id, &cwd, resume.as_deref(), approval, text.clone());
        if let Err(reason) = sent {
            // Shown in the session's transcript, where a draft save cannot clear it.
            self.conversation(id).error = Some(reason.to_owned());
            self.commit(id);
            cx.notify();
            return;
        }
        self.error = None;
        self.conversation(id).push_local(&text);
        self.commit(id);
        if let Err(error) = self.store.record_message(id, &text, now_ms()) {
            self.error = Some(format!("Could not save the session. {error}"));
        }
        self.composer
            .update(cx, |input, cx| input.set_value(String::new(), cx));
        self.save_draft(cx);
        self.views.scroll_to_end(ViewKey {
            view: ViewId::MAIN,
            session: id,
        });
        // Clicking Send moves focus off the composer; put it back for the next message.
        self.composer.focus_handle(cx).focus(window, cx);
        // The session moves to Working and shows its new time.
        self.refresh(cx);
    }

    fn stop_turn(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.focus_handle(cx).focus(window, cx);
        if let Some(connection) = self.provider(id).and_then(|p| self.started(p)) {
            connection.command(Command::Interrupt { session: id });
        }
        cx.notify();
    }

    fn answer(
        &mut self,
        id: u64,
        request: RequestId,
        allow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The buttons take focus from the composer; give it back for the next message.
        self.composer.focus_handle(cx).focus(window, cx);
        let Some(approval) = self.conversation(id).decide(&request, allow) else {
            return;
        };
        self.commit(id);
        if let Some(connection) = self.provider(id).and_then(|p| self.started(p)) {
            connection.command(Command::Answer {
                session: id,
                request: approval.request,
                allow,
            });
        }
        self.refresh(cx);
    }

    /// The session's transcript: a virtualized list, so a long session lays out and
    /// paints only the rows in view. It sticks to the bottom while new rows arrive,
    /// unless the user has scrolled up.
    fn transcript_list(&mut self, key: ViewKey, cx: &mut Context<Self>) -> gpui::List {
        let rows = self
            .conversations
            .get(&key.session)
            .map_or(0, |c| c.rows.len());
        let state = self.views.list(key, rows);
        let app = cx.weak_entity();
        list(state, move |index, _, cx| {
            app.update(cx, |this, cx| this.transcript_entry(key.session, index, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .size_full()
        .py_6()
    }

    fn jump_to_latest(&self, key: ViewKey, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.views.scrolled_up(key).then(|| {
            h_flex()
                .absolute()
                .bottom_3()
                .left_0()
                .w_full()
                .justify_center()
                .child(
                    Button::new("jump-to-latest", "Jump to latest")
                        .style(ButtonStyle::Filled)
                        .icon(IconName::Chevron)
                        .debug_selector(|| "jump-to-latest".into())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.views.scroll_to_end(key);
                            cx.notify();
                        })),
                )
        })
    }

    /// One list entry: a row, or after the last row, the turn's status. Each entry is
    /// laid out on its own, so it carries the transcript's width and text style.
    fn transcript_entry(&self, session: u64, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(conversation) = self.conversations.get(&session) else {
            return div().into_any_element();
        };
        let content = match conversation.rows.get(index) {
            Some(row) if conversation::visible(row) => {
                let reply = match row {
                    Row::Item(item) if item.kind == ItemKind::AgentMessage => {
                        Some(self.replies.borrow_mut().document((session, index), item))
                    }
                    _ => None,
                };
                Some(transcript::row(row, index, reply, cx))
            }
            Some(_) => None,
            None => self.transcript_status(session, conversation, cx),
        };
        let Some(content) = content else {
            // Nothing to show takes no space, gap included.
            return div().into_any_element();
        };
        let name = match conversation.rows.len() {
            count if index < count => format!("transcript-row-{index}"),
            _ => "transcript-status".to_owned(),
        };
        h_flex()
            .automation_id(name)
            .w_full()
            .justify_center()
            .child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_WIDTH))
                    .when(index > 0, |d| d.pt_4())
                    .text_size(TextSize::Body.px())
                    .line_height(px(22.))
                    .child(content),
            )
            .into_any_element()
    }

    /// What follows the last row: an approval request or the working indicator, and any
    /// errors. `None` when there is nothing to say.
    fn transcript_status(
        &self,
        session: u64,
        conversation: &Conversation,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let colors = cx.theme().colors;
        let mono = cx.theme().fonts.mono.clone();
        let mut view = v_flex().gap_4();
        let mut empty = true;
        if let Some(approval) = conversation.approvals.first() {
            empty = false;
            let (allow, deny) = (approval.request.clone(), approval.request.clone());
            let name = self.provider(session).map_or("The provider", provider_name);
            view = view.child(
                v_flex()
                    .id("approval")
                    .automation_id("approval")
                    .surface(Elevation::Raised, cx)
                    .border_color(colors.border_strong)
                    .p_3()
                    .gap_2()
                    .child(
                        Label::new(match conversation.approvals.len() {
                            1 => format!("{name} asks to run"),
                            n => format!("{name} asks to run · {} more waiting", n - 1),
                        })
                        .size(TextSize::Ui)
                        .color(Color::Muted),
                    )
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(TextSize::Ui.px())
                            .child(approval.action.clone()),
                    )
                    .when_some(approval.detail.clone(), |d, detail| {
                        d.child(Label::new(detail).size(TextSize::Sm).color(Color::Muted))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("approve", "Allow")
                                    .style(ButtonStyle::Primary)
                                    .icon(IconName::Check)
                                    .debug_selector(|| "approve".into())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.answer(session, allow.clone(), true, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("deny", "Deny")
                                    .style(ButtonStyle::Subtle)
                                    .icon(IconName::Close)
                                    .debug_selector(|| "deny".into())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.answer(session, deny.clone(), false, window, cx)
                                    })),
                            ),
                    ),
            );
        } else if conversation.running {
            empty = false;
            view = view.child(
                h_flex()
                    .id("turn-status")
                    .automation_id("turn-status")
                    .gap_2()
                    .child(div().size(px(6.)).rounded_full().bg(colors.accent))
                    .child(
                        Label::new("Working…")
                            .size(TextSize::Ui)
                            .color(Color::Muted),
                    ),
            );
        }
        for error in conversation
            .load_error
            .iter()
            .chain(&conversation.error)
            .chain(&conversation.save_error)
        {
            empty = false;
            view = view.child(
                Label::new(error.clone())
                    .size(TextSize::Ui)
                    .color(Color::Danger),
            );
        }
        (!empty).then(|| view.into_any_element())
    }

    fn error_line(&self) -> Option<impl IntoElement> {
        self.error
            .clone()
            .map(|error| Label::new(error).size(TextSize::Ui).color(Color::Danger))
    }
}

/// The session list, rendered as its own cached view. GPUI reuses its last layout and paint
/// until this entity is notified, so composer keystrokes skip every session row.
/// [`Keystone::refresh`] notifies it whenever the sessions, sections, search, or menu change.
struct SessionSidebar {
    app: WeakEntity<Keystone>,
    /// The row under the pointer. Only it and the selected row build a Settle button.
    hovered: Option<u64>,
    /// Whether the last render showed a relative time, which the minute timer must age.
    shows_time: bool,
}

/// One session as the sidebar shows it, built once per sidebar render.
struct SessionRow {
    id: u64,
    title: String,
    excerpt: String,
    provider: Provider,
    settled: bool,
    selected: bool,
    attention: Option<Attention>,
    /// How long ago the session last changed, or `None` for sessions saved before times.
    time: Option<String>,
}

/// The sidebar's sections, top to bottom.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    /// Unsettled sessions waiting on the user. Shown only when it has rows.
    Attention,
    /// Every other unsettled session.
    Working,
    /// Pinned to the bottom and collapsed until opened (see `Keystone::settled_open`).
    Settled,
}

impl Section {
    const ALL: [Section; 3] = [Section::Attention, Section::Working, Section::Settled];

    /// Settling is how the user puts a session away, so it outranks attention: a stale
    /// failure would otherwise pin a settled session to the top until its next message.
    fn of(row: &SessionRow) -> Self {
        match (row.settled, row.attention) {
            (true, _) => Section::Settled,
            (false, Some(_)) => Section::Attention,
            (false, None) => Section::Working,
        }
    }

    fn label(self, count: usize) -> String {
        match self {
            Section::Attention => format!("Needs action ({count})"),
            Section::Working => "Working".into(),
            Section::Settled => format!("Settled ({count})"),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Section::Attention => "attention-section",
            Section::Working => "working-section",
            Section::Settled => "settled-section",
        }
    }

    fn collapsible(self) -> bool {
        self == Section::Settled
    }
}

fn attention_reason(attention: Attention) -> (&'static str, Color) {
    match attention {
        Attention::Approval => ("Waiting for approval", Color::Warning),
        Attention::Failed => ("Turn failed", Color::Warning),
        Attention::Replied => ("New reply", Color::Default),
    }
}

/// The first non-blank line of a saved draft, or the provider's name when there is none.
fn excerpt(draft: &str, provider: Provider) -> String {
    draft
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(provider_name(provider))
        .to_owned()
}

impl SessionSidebar {
    /// The 12 px slot left of the title: a wedge for the session's state, or a dot
    /// for an unseen reply.
    fn state_mark(row: &SessionRow, cx: &App) -> gpui::AnyElement {
        let wedge = |name, color| {
            Icon::new(name)
                .size(px(10.))
                .color(color)
                .into_any_element()
        };
        match (row.attention, row.settled) {
            (_, true) => wedge(IconName::WedgeFilled, Color::Muted),
            (Some(Attention::Replied), false) => div()
                .size(px(10.))
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(6.)).rounded_full().bg(cx.theme().colors.text))
                .into_any_element(),
            (Some(_), false) => wedge(IconName::WedgeFilled, Color::Warning),
            (None, false) => wedge(IconName::Wedge, Color::Accent),
        }
    }

    fn row(
        row: SessionRow,
        hovered: bool,
        app: &Entity<Keystone>,
        sidebar: &WeakEntity<Self>,
        cx: &App,
    ) -> impl IntoElement {
        let mark = Self::state_mark(&row, cx);
        let SessionRow {
            id,
            title,
            excerpt,
            provider,
            settled,
            selected,
            attention,
            time,
        } = row;
        let title = Label::new(title).truncate();
        let title = if selected {
            title.weight(FontWeight::MEDIUM).color(Color::Strong)
        } else if settled {
            title.color(Color::Muted)
        } else {
            title
        };
        let show_settle = selected || hovered;
        let detail = match attention.filter(|_| !settled) {
            Some(attention) => {
                let (reason, color) = attention_reason(attention);
                h_flex().child(Label::new(reason).size(TextSize::Sm).color(color))
            }
            None => h_flex()
                .gap(px(6.))
                .child(
                    Icon::new(IconName::Diamond)
                        .size(px(10.))
                        .color(provider_color(provider, cx)),
                )
                .child(
                    Label::new(excerpt)
                        .size(TextSize::Sm)
                        .color(Color::Muted)
                        .truncate(),
                ),
        };
        let (select, settle, sidebar) = (app.downgrade(), app.downgrade(), sidebar.clone());
        ListItem::new(("session", id))
            .on_hover(move |&hovered, _, cx| {
                let _ = sidebar.update(cx, |this, cx| {
                    if hovered {
                        this.hovered = Some(id);
                    } else if this.hovered == Some(id) {
                        this.hovered = None;
                    }
                    cx.notify();
                });
            })
            .debug_selector(move || format!("session-{id}"))
            .selected(selected)
            .on_click(move |_, _, cx| {
                let _ = select.update(cx, |this, cx| this.select(id, cx));
            })
            .child(
                h_flex()
                    .gap(px(9.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(4.))
                            .child(
                                h_flex()
                                    // A 10 px mark centred in a 12 px state slot.
                                    .pl_px()
                                    .gap(px(10.))
                                    .child(mark)
                                    .child(div().flex_1().min_w_0().child(title))
                                    // The Settle button takes this slot while it shows.
                                    .when_some(time.filter(|_| !show_settle), |d, time| {
                                        d.child(
                                            Label::new(time).size(TextSize::Sm).color(Color::Faint),
                                        )
                                    }),
                            )
                            // Under the title, past the 12 px state slot and its gap.
                            .child(detail.pl(px(21.))),
                    )
                    .when(show_settle, |d| {
                        d.child(
                            Button::new(
                                ("settle", id),
                                if settled { "Un-settle" } else { "Settle" },
                            )
                            .size(ButtonSize::Compact)
                            .style(ButtonStyle::Subtle)
                            .icon(IconName::WedgeFilled)
                            .debug_selector(move || format!("settle-{id}"))
                            .tooltip(Tooltip::text(if settled {
                                "Return to active work"
                            } else {
                                "Move out of active work"
                            }))
                            .on_click(move |_, _, cx| {
                                let _ = settle.update(cx, |this, cx| this.settle(id, !settled, cx));
                            }),
                        )
                    }),
            )
    }

    fn header(
        section: Section,
        count: usize,
        open: bool,
        app: &Entity<Keystone>,
        cx: &App,
    ) -> impl IntoElement {
        let colors = cx.theme().colors;
        let app = app.downgrade();
        h_flex()
            .id(section.id())
            .automation_id(section.id())
            .flex_shrink_0()
            .h(px(26.))
            .pl_2()
            .pr(px(6.))
            .gap_2()
            .rounded(radius::CONTROL)
            .when(section == Section::Settled, |d| {
                d.child(
                    Icon::new(IconName::WedgeFilled)
                        .size(px(10.))
                        .color(Color::Faint),
                )
            })
            .child(
                Label::new(section.label(count))
                    .size(TextSize::Sm)
                    .weight(FontWeight::MEDIUM)
                    .color(Color::Faint),
            )
            .child(div().flex_1().h_px().bg(colors.border))
            .child(
                Icon::new(if open {
                    IconName::Chevron
                } else {
                    IconName::Right
                })
                .size(px(12.))
                .color(Color::Faint),
            )
            .when(section.collapsible(), |d| {
                d.cursor_pointer()
                    .hover(move |s| s.bg(colors.element_hover))
                    .on_click(move |_, _, cx| {
                        let _ = app.update(cx, |this, cx| {
                            this.settled_open = !this.settled_open;
                            this.refresh(cx);
                        });
                    })
            })
    }

    fn sections(
        &mut self,
        app: &Entity<Keystone>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let sidebar = cx.entity().downgrade();
        let this = app.read(cx);
        let workspace = this.store.workspace();
        let query = this.search.read(cx).value().to_lowercase();
        let now = LocalTime::at(now_ms());
        let mut sections: [Vec<SessionRow>; 3] = Default::default();
        for session in workspace.sessions().iter().filter(|s| {
            format!("{} {}", s.title(), s.provider())
                .to_lowercase()
                .contains(&query)
        }) {
            let live = this.conversations.get(&session.id());
            let row = SessionRow {
                id: session.id(),
                title: session.title().to_owned(),
                excerpt: excerpt(session.draft(), session.provider()),
                provider: session.provider(),
                settled: session.settled(),
                selected: Some(session.id()) == workspace.selected(),
                attention: session.attention(
                    live.is_some_and(|c| !c.approvals.is_empty()),
                    live.is_some_and(|c| c.running),
                ),
                time: session
                    .updated_at()
                    .map(|then| relative(now, LocalTime::at(then))),
            };
            sections[Section::of(&row) as usize].push(row);
        }
        let empty = match (
            sections.iter().all(Vec::is_empty),
            workspace.sessions().is_empty(),
        ) {
            (false, _) => None,
            (true, true) => Some("No sessions yet. Start one from the composer or press Ctrl N."),
            (true, false) => Some("No sessions match this search."),
        };
        let settled_open = this.settled_open;
        self.shows_time = false;
        let mut list = v_flex()
            .id("session-list")
            .size_full()
            .overflow_y_scroll()
            .px_2()
            .py(px(6.));
        for (section, rows) in Section::ALL.into_iter().zip(sections) {
            let message = empty.filter(|_| section == Section::Working);
            if rows.is_empty() && message.is_none() {
                continue;
            }
            let open = !section.collapsible() || settled_open;
            if open {
                // The selected row shows Settle where its time would be.
                self.shows_time |= rows.iter().any(|row| row.time.is_some() && !row.selected);
            }
            let header = Self::header(section, rows.len(), open, app, cx);
            list = list
                // Spare height sits above Settled, which keeps it at the bottom.
                .when(section == Section::Settled, |d| {
                    d.child(div().flex_grow(1.))
                })
                .child(
                    v_flex()
                        .gap(px(2.))
                        .child(header)
                        .when_some(message, |d, message| {
                            d.child(
                                div()
                                    .px_2()
                                    .py(px(6.))
                                    .text_size(TextSize::Ui.px())
                                    .line_height(px(18.))
                                    .text_color(cx.theme().colors.text_muted)
                                    .child(message),
                            )
                        })
                        .when(open, |d| {
                            d.children(rows.into_iter().map(|row| {
                                let hovered = self.hovered == Some(row.id);
                                Self::row(row, hovered, app, &sidebar, cx)
                            }))
                        }),
                );
        }
        list
    }
}

impl Render for SessionSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.app.upgrade() {
            Some(app) => self.sections(&app, cx).into_any_element(),
            None => div().into_any_element(),
        }
    }
}

impl Render for Keystone {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, text, ui_font) = (
            theme.colors.background,
            theme.colors.text,
            theme.fonts.ui.clone(),
        );
        let current = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| Some(s.id()) == self.store.workspace().selected());
        let session = current.map(|s| (s.title().to_owned(), s.settled()));
        let selected = current.map(|s| s.id());
        // A session that has reached its provider has history, so while that history
        // loads, the transcript layout is shown rather than the welcome screen.
        let has_thread = current.is_some_and(|s| s.provider_thread().is_some());
        let current = current.map(|s| (s.id(), s.provider()));
        if let Some(id) = selected {
            self.conversation(id);
        }
        let has_conversation = selected
            .and_then(|id| self.conversations.get(&id))
            .is_some_and(|c| {
                if c.is_loading() {
                    has_thread || c.running
                } else {
                    !c.is_empty()
                }
            });
        let composer = self.composer(current, cx);
        let error = self.error_line().map(|error| {
            div()
                .w_full()
                .max_w(px(CONTENT_WIDTH))
                .px_1()
                .pb_2()
                .child(error)
        });
        let main = if let (None, false) = (selected, self.fixture) {
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .items_center()
                .justify_center()
                .gap(px(22.))
                .px(px(24.))
                .pb(px(60.))
                .child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .child(arch_mark(px(40.)))
                        .child(
                            div()
                                .id("welcome-heading")
                                .automation_id("welcome-heading")
                                .child(
                                    Label::new("What should we work on?")
                                        .size(TextSize::Title)
                                        .weight(FontWeight::SEMIBOLD)
                                        .color(Color::Strong),
                                ),
                        ),
                )
                .child(composer)
                .children(error)
        } else {
            let body = match selected.filter(|_| !self.fixture) {
                Some(id) if has_conversation => {
                    let key = ViewKey {
                        view: ViewId::MAIN,
                        session: id,
                    };
                    div()
                        .id("conversation")
                        .automation_id("conversation")
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .child(self.transcript_top_check(key, cx))
                        .child(self.transcript_list(key, cx))
                        .children(self.jump_to_latest(key, cx))
                        .into_any_element()
                }
                None if self.fixture => div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(
                        v_flex()
                            .id("conversation")
                            .automation_id("conversation")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.transcript_scroll)
                            .items_center()
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(CONTENT_WIDTH))
                                    .py_6()
                                    .child(self.transcript()),
                            ),
                    )
                    .into_any_element(),
                _ => {
                    let hint = format!(
                        "Your draft is saved on this machine. Ctrl Enter sends it to {}.",
                        current.map_or("the provider", |(_, provider)| provider_name(provider))
                    );
                    v_flex()
                        .id("empty-conversation")
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .items_center()
                        .justify_center()
                        .gap(px(10.))
                        .child(
                            Icon::new(IconName::Wedge)
                                .size(px(28.))
                                .color(Color::Custom(cx.theme().colors.border_strong)),
                        )
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(FontWeight::MEDIUM)
                                .child("No messages yet"),
                        )
                        .child(
                            div()
                                .w(px(320.))
                                .text_center()
                                .text_size(TextSize::Ui.px())
                                .line_height(px(18.))
                                .text_color(cx.theme().colors.text_muted)
                                .child(hint),
                        )
                        .into_any_element()
                }
            };
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .items_center()
                .px(px(24.))
                .pb(px(18.))
                .child(body)
                .children(error)
                .child(composer)
        };
        let window_width = f32::from(window.viewport_size().width);
        let layout = self.customization.shown(self.saved_layout());
        let customizing = self.customization.is_open();
        let sidebar = self.sidebar.then(|| {
            self.sidebar_column(layout.fit(window_width), layout.side, cx)
                .when(customizing, |d| {
                    d.child(self.resize_handle(layout.side, cx))
                })
        });
        let (left, right) = match layout.side {
            SidebarSide::Left => (sidebar, None),
            SidebarSide::Right => (None, sidebar),
        };
        let root =
            h_flex()
                .id("keystone-root")
                .size_full()
                .items_start()
                .bg(background)
                .font_family(ui_font)
                .text_color(text)
                .text_size(TextSize::Base.px())
                .track_focus(&self.focus)
                .key_context("Keystone")
                .when(automation::enabled(), |d| {
                    d.child(automation::frame_writer())
                })
                // Capture runs before the focused input, so Escape closes an open menu first.
                .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key != "escape"
                        || this.renaming.is_some()
                        || this.confirm_full_access.is_some()
                    {
                        return;
                    }
                    if this.new_menu || this.settings_menu || this.approval_menu {
                        this.new_menu = false;
                        this.settings_menu = false;
                        this.approval_menu = false;
                        this.refresh(cx);
                        cx.stop_propagation();
                    } else if this.customization.is_open() {
                        this.customize(customization::Action::Close, window, cx);
                        cx.stop_propagation();
                    }
                }))
                .on_action(cx.listener(|this, _: &Quit, _, cx| {
                    if this.save_before_exit(cx) {
                        cx.quit();
                    }
                }))
                .on_action(cx.listener(|this, _: &NewClaude, window, cx| {
                    this.start(Provider::Claude, window, cx)
                }))
                .on_action(cx.listener(|this, _: &NewCodex, window, cx| {
                    this.start(Provider::Codex, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ToggleSettle, _, cx| this.toggle_settle(cx)))
                .on_action(cx.listener(|this, _: &FocusComposer, window, cx| {
                    this.composer.focus_handle(cx).focus(window, cx)
                }))
                .on_action(cx.listener(|this, _: &SendMessage, window, cx| {
                    if let Some(draft) = this.composer_draft(cx) {
                        this.send_message(draft, window, cx)
                    }
                }))
                .on_action(
                    cx.listener(|this, _: &RenameSession, window, cx| this.open_rename(window, cx)),
                )
                .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                    this.sidebar = true;
                    this.search.focus_handle(cx).focus(window, cx);
                    this.refresh(cx);
                }))
                .children(left)
                .child(
                    v_flex()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.top_bar(session, cx))
                        .child(main)
                        .when(customizing, |d| {
                            d.child(self.customize_panel(window.viewport_size(), cx))
                        }),
                )
                .children(right)
                .children(self.full_access_dialog(cx))
                .children(self.rename_dialog(cx));
        self.on_sidebar_drag(root, cx)
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod tests {
    use super::*;
    use conversation::Row;
    use keystone_agent::{Event, Item, ItemKind, ItemStatus, TurnStatus};

    const MAIN_1: ViewKey = ViewKey {
        view: ViewId::MAIN,
        session: 1,
    };

    #[gpui::test]
    fn welcome_action_focuses_composer_and_preserves_draft_on_session_switch(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        assert!(cx.debug_bounds("welcome-heading").is_some());
        assert!(cx.debug_bounds("welcome-codex").is_some());

        let bounds = cx.debug_bounds("welcome-claude").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(Store::open(&path).unwrap().workspace().selected(), Some(1));
        cx.simulate_input("Draft from the welcome composer\n第二行 😀\n");
        cx.run_until_parked();

        for selector in ["new-session", "new-codex", "session-1"] {
            let bounds = cx.debug_bounds(selector).expect("session control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        view.read_with(cx, |view, cx| {
            assert_eq!(view.store.workspace().selected(), Some(1));
            assert_eq!(
                view.composer.read(cx).value(),
                "Draft from the welcome composer\n第二行 😀\n"
            );
        });
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            "Draft from the welcome composer\n第二行 😀\n"
        );
    }

    #[gpui::test]
    #[ignore = "benchmark; run through scripts/perf.py"]
    fn perf_sidebar_draw(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        for index in 0..200 {
            let provider = if index % 2 == 0 {
                Provider::Claude
            } else {
                Provider::Codex
            };
            store.create(provider, 1).unwrap();
        }
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        // A composer keystroke notifies only the root view. A session change also
        // invalidates the cached sidebar, so it rebuilds every row.
        for (metric, sidebar_changed) in [("sidebar_draw_ms", false), ("sidebar_refresh_ms", true)]
        {
            let mut total = std::time::Duration::ZERO;
            let frames = 30;
            for _ in 0..frames {
                view.update(cx, |this, cx| {
                    if sidebar_changed {
                        this.refresh(cx);
                    } else {
                        cx.notify();
                    }
                });
                cx.update(|window, cx| {
                    let start = std::time::Instant::now();
                    window.draw(cx).clear(cx);
                    total += start.elapsed();
                });
            }
            eprintln!(
                "PERF {metric}={:.2}",
                total.as_secs_f64() * 1000. / frames as f64
            );
        }
    }

    /// Writes a session log with `turns` completed turns: a question, a command, and a
    /// several-line reply each.
    fn seed_long_session(dir: &Path, session: u64, turns: usize) {
        use keystone_agent::log::{Entry, SessionLog};
        let mut log = SessionLog::open(dir, session).unwrap();
        for turn in 0..turns {
            let item = |id: &str, kind: ItemKind, text: String| Entry::Item {
                item: Item {
                    id: format!("{id}-{turn}"),
                    kind,
                    text,
                    status: ItemStatus::Completed,
                },
                seq: None,
                run: None,
            };
            log.append(&Entry::TurnStarted).unwrap();
            log.append(&item(
                "u",
                ItemKind::UserMessage,
                format!("Question {turn}"),
            ))
            .unwrap();
            log.append(&item(
                "c",
                ItemKind::Command {
                    command: "cargo test".into(),
                    exit_code: Some(0),
                },
                "running 12 tests\ntest result: ok".into(),
            ))
            .unwrap();
            log.append(&item(
                "a",
                ItemKind::AgentMessage,
                format!("Reply {turn}. ").repeat(40),
            ))
            .unwrap();
            log.append(&Entry::TurnEnded {
                status: TurnStatus::Completed,
                error: None,
                seq: None,
                run: None,
            })
            .unwrap();
        }
        log.sync().unwrap();
    }

    #[gpui::test]
    #[ignore = "benchmark; run through scripts/perf.py"]
    fn perf_transcript_draw(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 1000);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        wait_until(cx, &view, |view| {
            view.conversations
                .get(&1)
                .is_some_and(|c| c.rows.len() == 3000)
        });
        // A composer keystroke notifies the root view, which rebuilds the transcript.
        let mut total = std::time::Duration::ZERO;
        let frames = 30;
        for _ in 0..frames {
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| {
                let start = std::time::Instant::now();
                window.draw(cx).clear(cx);
                total += start.elapsed();
            });
        }
        eprintln!(
            "PERF transcript_draw_ms={:.2}",
            total.as_secs_f64() * 1000. / frames as f64
        );

        // A reply streaming at the end of the same transcript: one delta per frame.
        view.update(cx, |this, cx| {
            let conversation = this.conversation(1);
            conversation.apply(Event::TurnStarted);
            conversation.apply(Event::ItemStarted(Item {
                id: "live".into(),
                kind: ItemKind::AgentMessage,
                text: String::new(),
                status: ItemStatus::InProgress,
            }));
            this.commit(1);
            cx.notify();
        });
        let mut total = std::time::Duration::ZERO;
        for _ in 0..frames {
            view.update(cx, |this, cx| {
                this.conversation(1).apply(Event::ItemDelta {
                    item_id: "live".into(),
                    text: "More streamed text. ".into(),
                });
                this.commit(1);
                cx.notify();
            });
            cx.update(|window, cx| {
                let start = std::time::Instant::now();
                window.draw(cx).clear(cx);
                total += start.elapsed();
            });
        }
        eprintln!(
            "PERF transcript_stream_ms={:.2}",
            total.as_secs_f64() * 1000. / frames as f64
        );
    }

    #[gpui::test]
    fn a_long_transcript_draws_only_rows_in_view_and_follows_new_rows(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        wait_until(cx, &view, |view| {
            view.conversations
                .get(&1)
                .is_some_and(|c| c.rows.len() == 600)
        });
        cx.run_until_parked();
        // Opened at the latest row. Rows far above were never laid out.
        assert!(cx.debug_bounds("transcript-row-599").is_some());
        assert!(cx.debug_bounds("transcript-row-0").is_none());
        assert!(cx.debug_bounds("transcript-row-300").is_none());

        // A row that arrives while at the bottom is followed.
        let reply = |id: &str| {
            Event::ItemCompleted(Item {
                id: id.into(),
                kind: ItemKind::AgentMessage,
                text: format!("Reply {id}"),
                status: ItemStatus::Completed,
            })
        };
        view.update(cx, |this, cx| {
            this.conversation(1).apply(reply("new"));
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        let composer = cx.debug_bounds("composer-field").unwrap();
        let newest = cx
            .debug_bounds("transcript-row-600")
            .expect("new row drawn");
        assert!(newest.bottom() <= composer.top(), "the new row is in view");

        // The mouse wheel scrolls up through the history.
        let last = cx.debug_bounds("transcript-row-600").unwrap();
        let over = cx.debug_bounds("conversation").unwrap().center();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(30.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let moved = cx.debug_bounds("transcript-row-600").unwrap();
        assert_eq!(moved.top() - last.top(), px(30.), "scrolled by the wheel");

        // Scrolled to the top, the first rows draw and a new row does not pull the
        // view back down.
        view.update(cx, |this, cx| {
            this.views
                .get(MAIN_1)
                .unwrap()
                .scroll_to(ListOffset::default());
            cx.notify();
        });
        cx.run_until_parked();
        let first = cx
            .debug_bounds("transcript-row-0")
            .expect("first row drawn");
        view.update(cx, |this, cx| {
            this.conversation(1).apply(reply("later"));
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(cx.debug_bounds("transcript-row-0").unwrap(), first);
        assert!(cx.debug_bounds("transcript-row-601").is_none());
        // Sending, or selecting the session, returns to the latest row.
        view.update(cx, |this, cx| {
            this.views.scroll_to_end(MAIN_1);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("transcript-row-601").is_some());
    }

    #[gpui::test]
    fn streaming_keeps_the_place_of_a_reader_who_scrolled_up(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        wait_until(cx, &view, |view| {
            view.conversations
                .get(&1)
                .is_some_and(|c| c.rows.len() == 600)
        });
        let message = |id: &str, text: String| Item {
            id: id.into(),
            kind: ItemKind::AgentMessage,
            text,
            status: ItemStatus::InProgress,
        };
        // A short item, then a reply taller than the view, both still streaming.
        view.update(cx, |this, cx| {
            let conversation = this.conversation(1);
            conversation.apply(Event::TurnStarted);
            conversation.apply(Event::ItemStarted(message("early", "Thinking".into())));
            conversation.apply(Event::ItemStarted(message(
                "long",
                "Streamed words. ".repeat(600),
            )));
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        // Scroll up partway into the long reply.
        let over = cx.debug_bounds("conversation").unwrap().center();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(200.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let view_top = cx.debug_bounds("conversation").unwrap().top();
        let read = cx.debug_bounds("transcript-row-601").unwrap();
        assert!(read.top() < view_top, "the reply starts above the view");
        assert!(cx.debug_bounds("transcript-row-600").is_none());

        // More of the reply streams in: the text being read stays where it is.
        view.update(cx, |this, cx| {
            this.conversation(1).apply(Event::ItemDelta {
                item_id: "long".into(),
                text: "More. ".repeat(50),
            });
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("transcript-row-601").unwrap().top(),
            read.top()
        );

        // An item above the view completes: still no jump.
        view.update(cx, |this, cx| {
            this.conversation(1).apply(Event::ItemCompleted(Item {
                status: ItemStatus::Completed,
                ..message("early", "Thought for a while. ".repeat(20))
            }));
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("transcript-row-601").unwrap().top(),
            read.top()
        );
        assert!(cx.debug_bounds("transcript-row-600").is_none());
    }

    #[gpui::test]
    fn each_view_of_a_conversation_keeps_its_own_place_while_it_streams(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        store.create(Provider::Codex, 2).unwrap();
        store.select(1, 3).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        wait_until(cx, &view, |view| {
            view.conversations
                .get(&1)
                .is_some_and(|c| c.rows.len() == 600)
        });
        cx.run_until_parked();
        let other = ViewKey {
            view: ViewId(1),
            session: 1,
        };
        let reader = (100, px(40.));
        view.update(cx, |this, _| {
            this.views.list(other, 600).scroll_to(at(100, 40.));
        });
        let stream = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, id: &str| {
            view.update(cx, |this, cx| {
                this.conversation(1).apply(Event::ItemCompleted(Item {
                    id: id.into(),
                    kind: ItemKind::AgentMessage,
                    text: format!("Reply {id}"),
                    status: ItemStatus::Completed,
                }));
                this.commit(1);
                cx.notify();
            });
            cx.run_until_parked();
        };
        let tops = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.update(cx, |this, _| {
                let main = this.views.get(MAIN_1).unwrap();
                let other = this.views.get(other).unwrap();
                assert_eq!(main.item_count(), other.item_count());
                let top = other.logical_scroll_top();
                (
                    main.logical_scroll_top().item_ix == main.item_count(),
                    (top.item_ix, top.offset_in_item),
                )
            })
        };

        stream(&view, cx, "a");
        assert!(cx.debug_bounds("transcript-row-600").is_some());
        assert_eq!(tops(&view, cx), (true, reader));

        let row = cx.debug_bounds("session-2").unwrap().center();
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("transcript-row-600").is_none());
        stream(&view, cx, "b");
        assert_eq!(tops(&view, cx), (true, reader));

        let row = cx.debug_bounds("session-1").unwrap().center();
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("transcript-row-601").is_some());
        assert_eq!(tops(&view, cx), (true, reader));
        view.update(cx, |this, _| {
            assert_eq!(this.views.get(other).unwrap().item_count(), 603);
            assert!(!this.views.take_remeasured_top(other));
            this.views.get(other).unwrap().scroll_to(at(600, 40.));
        });

        view.update(cx, |this, cx| {
            this.conversation(1).apply(Event::ItemCompleted(Item {
                id: "a".into(),
                kind: ItemKind::AgentMessage,
                text: "Short".into(),
                status: ItemStatus::Completed,
            }));
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(tops(&view, cx), (true, (600, px(40.))));
        view.update(cx, |this, _| {
            assert_eq!(this.views.get(other).unwrap().item_count(), 603);
            assert!(this.views.take_remeasured_top(other));
            assert!(!this.views.take_remeasured_top(MAIN_1));
        });
    }

    #[gpui::test]
    fn a_streamed_burst_reveals_over_frames_while_its_turn_state_shows_at_once(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx, _dir) = long_session(cx);
        cx.run_until_parked();
        let receive = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, events| {
            view.update(cx, |this, cx| this.receive(Vec::new(), events, cx));
            cx.run_until_parked();
        };
        let height = |cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds("transcript-row-600").unwrap().size.height
        };
        let tick = |cx: &mut gpui::VisualTestContext| {
            cx.executor().advance_clock(reveal::TICK);
            cx.run_until_parked();
        };
        let reply = "Streamed words. ".repeat(100);
        receive(
            &view,
            cx,
            vec![
                (1, Event::TurnStarted),
                (
                    1,
                    Event::ItemStarted(Item {
                        id: "reply".into(),
                        kind: ItemKind::AgentMessage,
                        text: String::new(),
                        status: ItemStatus::InProgress,
                    }),
                ),
                (
                    1,
                    Event::ItemDelta {
                        item_id: "reply".into(),
                        text: reply.clone(),
                    },
                ),
                (
                    1,
                    Event::ApprovalRequested(keystone_agent::Approval {
                        request: keystone_agent::RequestId("1".into()),
                        item_id: None,
                        action: "cargo test".into(),
                        detail: None,
                    }),
                ),
            ],
        );
        assert!(cx.debug_bounds("approve").is_some());
        view.read_with(cx, |this, _| {
            assert!(
                matches!(&this.conversations[&1].rows[600], Row::Item(item) if item.text == reply)
            );
        });
        let first = height(cx);
        tick(cx);
        let second = height(cx);
        assert!(second > first, "the reply grows on the next frame");
        for _ in 0..10 {
            tick(cx);
        }
        let full = height(cx);
        assert!(full > second);
        view.read_with(cx, |this, _| {
            assert!(this.reveal_task.is_none(), "no timer once caught up")
        });

        let over = cx.debug_bounds("conversation").unwrap().center();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(120.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let read = cx.debug_bounds("transcript-row-600").unwrap().top();
        receive(
            &view,
            cx,
            vec![(
                1,
                Event::ItemDelta {
                    item_id: "reply".into(),
                    text: "More words. ".repeat(100),
                },
            )],
        );
        for _ in 0..3 {
            tick(cx);
            assert_eq!(cx.debug_bounds("transcript-row-600").unwrap().top(), read);
        }

        receive(
            &view,
            cx,
            vec![(
                1,
                Event::TurnCompleted {
                    status: TurnStatus::Completed,
                    error: None,
                    stopped: false,
                },
            )],
        );
        let done = height(cx);
        tick(cx);
        assert_eq!(height(cx), done, "nothing left to reveal");

        click(cx, "jump-to-latest");
        assert!(cx.debug_bounds("jump-to-latest").is_none());
        let composer = cx.debug_bounds("composer-field").unwrap();
        let row = cx.debug_bounds("transcript-row-600").unwrap();
        assert!(row.bottom() <= composer.top());
        assert!(cx.debug_bounds("copy-reply-600").is_some());
    }

    #[gpui::test]
    fn a_session_selected_again_shows_its_backlog_at_once(cx: &mut gpui::TestAppContext) {
        let (view, cx, _dir) = long_session(cx);
        let delta = |text: &str| {
            (
                1,
                Event::ItemDelta {
                    item_id: "reply".into(),
                    text: text.into(),
                },
            )
        };
        view.update(cx, |this, cx| {
            let reply = Item {
                id: "reply".into(),
                kind: ItemKind::AgentMessage,
                text: "Shown. ".into(),
                status: ItemStatus::InProgress,
            };
            let events = vec![(1, Event::TurnStarted), (1, Event::ItemStarted(reply))];
            this.receive(Vec::new(), events, cx);
        });
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            this.receive(Vec::new(), vec![delta(&"Visible burst. ".repeat(100))], cx);
            assert!(this.replies.get_mut().pacing());
            this.create(Provider::Codex, cx);
            assert!(
                !this.replies.get_mut().pacing(),
                "new sessions settle the reply being left"
            );
        });
        cx.run_until_parked();

        view.update(cx, |this, cx| {
            this.receive(Vec::new(), vec![delta(&"Backlog. ".repeat(100))], cx);
            this.select(1, cx);
            this.receive(Vec::new(), vec![delta("More. ")], cx);
            assert!(
                !this.replies.get_mut().pacing(),
                "the backlog is not replayed"
            );
        });
    }

    /// Opens a long session and returns its view once the history is shown.
    fn long_session(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<Keystone>,
        &mut gpui::VisualTestContext,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        wait_until(cx, &view, |view| {
            view.conversations
                .get(&1)
                .is_some_and(|c| c.rows.len() == 600)
        });
        (view, cx, dir)
    }

    fn at(item_ix: usize, offset: f32) -> ListOffset {
        ListOffset {
            item_ix,
            offset_in_item: px(offset),
        }
    }

    fn scroll_to(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, top: ListOffset) {
        view.update(cx, |this, cx| {
            this.views.get(MAIN_1).unwrap().scroll_to(top);
            cx.notify();
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_reader_scrolled_into_a_row_that_shrinks_sees_the_next_row(cx: &mut gpui::TestAppContext) {
        let (view, cx, _dir) = long_session(cx);
        let item = |id: &str, kind: ItemKind, text: String| Item {
            id: id.into(),
            kind,
            text,
            status: ItemStatus::InProgress,
        };
        let long = || "Streamed words. ".repeat(600);
        let update = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, event| {
            view.update(cx, |this, cx| {
                this.conversation(1).apply(event);
                this.commit(1);
                cx.notify();
            });
            cx.run_until_parked();
        };
        view.update(cx, |this, cx| {
            let conversation = this.conversation(1);
            conversation.apply(Event::TurnStarted);
            conversation.apply(Event::ItemStarted(item(
                "a",
                ItemKind::AgentMessage,
                long(),
            )));
            conversation.apply(Event::ItemStarted(item("r", ItemKind::Reasoning, long())));
            conversation.apply(Event::ItemStarted(item(
                "b",
                ItemKind::AgentMessage,
                long(),
            )));
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        scroll_to(&view, cx, at(600, 0.));
        let content_top = cx.debug_bounds("transcript-row-600").unwrap().top();

        // Reading 1000px into a reply that is then replaced by a short one.
        scroll_to(&view, cx, at(600, 1000.));
        update(
            &view,
            cx,
            Event::ItemCompleted(Item {
                status: ItemStatus::Completed,
                ..item("a", ItemKind::AgentMessage, "Short.".into())
            }),
        );
        assert_eq!(
            cx.debug_bounds("transcript-row-601").unwrap().top(),
            content_top,
            "the next row starts at the top, not above it"
        );

        // Reading inside a reasoning summary that completes empty, so it is not shown.
        scroll_to(&view, cx, at(601, 500.));
        update(
            &view,
            cx,
            Event::ItemCompleted(Item {
                status: ItemStatus::Completed,
                ..item("r", ItemKind::Reasoning, String::new())
            }),
        );
        assert_eq!(
            cx.debug_bounds("transcript-row-602").unwrap().top(),
            content_top
        );
    }

    /// Opens a long session in a window of the given width, with three long replies
    /// streaming at its end, scrolled to the start of the first.
    fn long_replies(
        cx: &mut gpui::TestAppContext,
        width: f32,
    ) -> (
        Entity<Keystone>,
        &mut gpui::VisualTestContext,
        tempfile::TempDir,
        Pixels,
    ) {
        let (view, cx, dir) = long_session(cx);
        cx.simulate_resize(size(px(width), px(700.)));
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            let conversation = this.conversation(1);
            conversation.apply(Event::TurnStarted);
            for id in ["a", "b", "c"] {
                conversation.apply(Event::ItemStarted(Item {
                    id: id.into(),
                    kind: ItemKind::AgentMessage,
                    text: "Streamed words. ".repeat(600),
                    status: ItemStatus::InProgress,
                }));
            }
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        scroll_to(&view, cx, at(600, 0.));
        let content_top = cx.debug_bounds("transcript-row-600").unwrap().top();
        (view, cx, dir, content_top)
    }

    #[gpui::test]
    fn a_reader_scrolled_into_a_row_that_a_wider_window_shortens_sees_the_next_row(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx, _dir, content_top) = long_replies(cx, 700.);
        let narrow = cx.debug_bounds("transcript-row-600").unwrap().size.height;
        let offset = narrow * 0.8;
        scroll_to(&view, cx, at(600, offset.into()));

        // The reply wraps onto fewer lines, ending before the place being read.
        cx.simulate_resize(size(px(1500.), px(700.)));
        cx.run_until_parked();
        let next = cx.debug_bounds("transcript-row-601").unwrap();
        assert!(
            next.size.height < offset,
            "the replies are as long as each other"
        );
        assert_eq!(
            next.top(),
            content_top,
            "the next row starts at the top, not above it"
        );

        // Following the bottom is unchanged by a resize.
        view.update(cx, |this, cx| {
            this.views.scroll_to_end(MAIN_1);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_resize(size(px(900.), px(700.)));
        cx.run_until_parked();
        view.update(cx, |this, _| {
            let list = this.views.get(MAIN_1).unwrap();
            assert_eq!(list.logical_scroll_top().item_ix, list.item_count());
        });
    }

    #[gpui::test]
    fn a_reader_scrolled_into_a_row_that_a_narrower_window_lengthens_keeps_their_place(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx, _dir, content_top) = long_replies(cx, 1500.);
        scroll_to(&view, cx, at(600, 1000.));

        // The reply wraps onto more lines; the place being read is still inside it.
        cx.simulate_resize(size(px(700.), px(700.)));
        cx.run_until_parked();
        let row = cx.debug_bounds("transcript-row-600").unwrap();
        assert!(row.size.height > px(2000.));
        assert_eq!(row.top(), content_top - px(1000.));
    }

    #[gpui::test]
    fn a_reader_scrolled_into_the_status_keeps_their_place(cx: &mut gpui::TestAppContext) {
        let (view, cx, _dir) = long_session(cx);
        // A tall approval card in the status entry.
        view.update(cx, |this, cx| {
            let conversation = this.conversation(1);
            conversation.apply(Event::TurnStarted);
            conversation.apply(Event::ApprovalRequested(keystone_agent::Approval {
                request: keystone_agent::RequestId("1".into()),
                item_id: None,
                action: "cargo test".into(),
                detail: Some("A long reason to run it. ".repeat(400)),
            }));
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        scroll_to(&view, cx, at(600, 300.));
        let read = cx.debug_bounds("transcript-status").unwrap();
        let view_top = cx.debug_bounds("conversation").unwrap().top();
        assert!(read.top() < view_top, "scrolled into the card");

        // A row arrives before the status entry: the card stays where it was read.
        view.update(cx, |this, cx| {
            this.conversation(1).apply(Event::ItemStarted(Item {
                id: "m".into(),
                kind: ItemKind::AgentMessage,
                text: "Starting.".into(),
                status: ItemStatus::InProgress,
            }));
            this.commit(1);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("transcript-status").unwrap().top(),
            read.top()
        );
        assert!(cx.debug_bounds("transcript-row-600").is_none());
    }

    #[gpui::test]
    fn a_message_sent_while_history_loads_is_saved_on_quit(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        // Selected, so session 1's history is not loaded until it is used below.
        store.create(Provider::Codex, 1).unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        // Sent and quit before the loaded history is applied.
        view.update(cx, |this, cx| {
            assert!(!this.conversations.contains_key(&1));
            assert!(this.conversation(1).is_loading());
            this.conversation(1).push_local("Sent while loading");
            this.commit(1);
            this.flush_logs(cx);
        });
        let entries = keystone_agent::log::read(&sessions, 1).unwrap();
        assert!(
            matches!(entries.last(), Some(keystone_agent::log::Entry::Item { item, .. })
                if item.text == "Sent while loading"),
            "the message was saved: {:?}",
            entries.last()
        );
    }

    #[gpui::test]
    fn closing_does_not_wait_for_a_history_read_that_never_returns(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        // Session 1's history read never returns, as on a stalled network disk.
        view.update(cx, |this, _| {
            let read: keystone_agent::log::Reader = Arc::new(|dir, session| {
                if session == 1 {
                    loop {
                        std::thread::park();
                    }
                }
                keystone_agent::log::read(dir, session)
            });
            this.logs = Logs::with_reader(sessions.clone(), this.wake.clone(), read);
            this.conversations.clear();
            this.conversation(2);
        });
        wait_until(cx, &view, |view| !view.conversations[&2].is_loading());
        let started = std::time::Instant::now();
        view.update(cx, |this, cx| {
            this.conversation(2).push_local("In another session");
            this.commit(2);
            assert!(this.conversation(1).is_loading());
            this.conversation(1).push_local("Sent while loading");
            this.commit(1);
            this.flush_logs(cx);
        });
        assert!(
            started.elapsed() < LOAD_WAIT + std::time::Duration::from_secs(3),
            "closing waited {:?}",
            started.elapsed()
        );
        let texts = |session| {
            let mut restored = Conversation::loading();
            restored.restore(Ok(keystone_agent::log::read(&sessions, session).unwrap()));
            restored
                .rows
                .iter()
                .filter_map(|row| match row {
                    Row::Item(item) => Some(item.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(2), ["In another session"]);
        // The held message was saved, and still restores after the history.
        let held = texts(1);
        assert_eq!(held.first().map(String::as_str), Some("Question 0"));
        assert_eq!(held.last().map(String::as_str), Some("Sent while loading"));
        assert_eq!(held.len(), 3 * 3 + 1);
    }

    #[gpui::test]
    fn a_save_failure_shows_in_the_conversation_until_saving_works(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        // A file where the sessions directory belongs makes every write fail.
        let sessions = dir.path().join("sessions");
        std::fs::write(&sessions, "").unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let turn = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.update(cx, |this, cx| {
                let conversation = this.conversation(1);
                conversation.push_local("hello");
                conversation.apply(Event::TurnStarted);
                this.commit(1);
                cx.notify();
            });
        };
        wait_until(cx, &view, |view| !view.conversations[&1].is_loading());
        turn(&view, cx);
        wait_until(cx, &view, |view| {
            view.conversations[&1].save_error.is_some()
        });
        assert!(cx.debug_bounds("transcript-status").is_some());
        std::fs::remove_file(&sessions).unwrap();
        turn(&view, cx);
        wait_until(cx, &view, |view| {
            view.conversations[&1].save_error.is_none()
        });
        view.update(cx, |view, cx| view.flush_logs(cx));
        assert!(
            !keystone_agent::log::read(&sessions, 1).unwrap().is_empty(),
            "written once saving works"
        );
    }

    #[gpui::test]
    fn cached_sidebar_follows_search_and_settlement(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Claude, 1).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        // GPUI 0.2.2 keeps debug bounds for elements that are no longer drawn, so check
        // which row sits at the top of the list instead of which rows exist.
        let top = |cx: &mut gpui::VisualTestContext| cx.debug_bounds("session-1").unwrap().origin;
        let second =
            |cx: &mut gpui::VisualTestContext| cx.debug_bounds("session-2").unwrap().origin;
        let (first_row, second_row) = (top(cx), second(cx));
        assert!(second_row.y > first_row.y);

        // Typing in the composer reuses the cached sidebar.
        view.update_in(cx, |this, window, cx| {
            this.composer.focus_handle(cx).focus(window, cx)
        });
        cx.simulate_input("Draft text");
        cx.run_until_parked();
        assert_eq!(second(cx), second_row);

        view.update_in(cx, |this, window, cx| {
            this.search.focus_handle(cx).focus(window, cx)
        });
        cx.simulate_input("codex");
        cx.run_until_parked();
        assert_eq!(second(cx), first_row);

        cx.update(|_, cx| TextInput::bind_keys(cx));
        cx.simulate_keystrokes("ctrl-a backspace");
        cx.run_until_parked();
        assert_eq!(second(cx), second_row);

        // Settling the selected row moves it into the collapsed Settled section,
        // which sits below the Working rows.
        let bounds = cx
            .debug_bounds("settle-2")
            .expect("selected row shows Settle");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("settled-section").unwrap().origin.y > first_row.y);
        assert_eq!(top(cx), first_row);
    }

    #[gpui::test]
    fn shortcut_settlement_invalidates_cached_sidebar(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            let id = store.create(provider, 1).unwrap().value;
            store.set_settled(id, true).unwrap();
        }
        store.select(1, 1).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        view.update_in(cx, |this, window, cx| {
            this.focus.focus(window, cx);
            this.settled_open = true;
            this.refresh(cx);
        });
        cx.run_until_parked();
        let header = cx.debug_bounds("settled-section").unwrap().origin;
        let first_row = cx.debug_bounds("session-1").unwrap().origin;
        assert!(first_row.y > header.y);

        // No focus change or pointer event re-renders the sidebar here, only the refresh.
        // Un-settling session 1 moves it above the Settled header.
        cx.dispatch_action(ToggleSettle);
        cx.run_until_parked();
        let active_row = cx.debug_bounds("session-1").unwrap().origin.y;
        assert!(cx.debug_bounds("settled-section").unwrap().origin.y > active_row);
    }

    #[gpui::test]
    fn settings_menu_applies_and_persists_appearance(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let settings_height =
            |cx: &mut gpui::VisualTestContext| cx.debug_bounds("settings").unwrap().size.height;
        assert_eq!(settings_height(cx), px(32.));

        for selector in ["settings", "theme-dark", "density-compact"] {
            let bounds = cx
                .debug_bounds(selector)
                .expect("settings control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        cx.update(|_, cx| {
            assert_eq!(cx.theme().appearance, keystone_ui::Appearance::Dark);
            assert_eq!(cx.theme().density, keystone_ui::Density::Compact);
        });
        assert_eq!(settings_height(cx), px(28.));

        let bounds = cx.debug_bounds("theme-light").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(cx.theme().appearance, keystone_ui::Appearance::Light));
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |view, _| assert!(!view.settings_menu));
        let saved = Store::open(&path)
            .unwrap()
            .workspace()
            .preferences()
            .appearance()
            .clone();
        assert_eq!(
            (saved.theme, saved.density),
            (ThemeMode::Light, Density::Compact)
        );
    }

    /// Runs the app's tasks, with real time passing, until `done` holds. Provider
    /// output arrives from a real child process, so simulated time alone cannot reach it.
    fn wait_until(
        cx: &mut gpui::VisualTestContext,
        view: &Entity<Keystone>,
        done: impl Fn(&Keystone) -> bool,
    ) {
        cx.executor().allow_parking();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            if view.read_with(cx, |view, _| done(view)) {
                return;
            }
            if std::time::Instant::now() > deadline {
                view.read_with(cx, |view, _| {
                    let state: Vec<_> = view
                        .conversations
                        .iter()
                        .map(|(id, c)| {
                            (
                                id,
                                c.running,
                                c.rows.clone(),
                                c.error.clone(),
                                c.approvals.clone(),
                            )
                        })
                        .collect();
                    panic!(
                        "timed out; error {:?}; conversations {state:#?}",
                        view.error
                    );
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} not rendered"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    /// Chooses an approval mode the way a user does: the shield button, then the mode,
    /// then Confirm when the mode asks for it.
    fn choose_mode(cx: &mut gpui::VisualTestContext, mode: ApprovalMode) {
        pick_mode(cx, mode);
        if mode == ApprovalMode::FullAccess {
            click(cx, "full-access-dialog-confirm");
        }
    }

    /// Picks a mode from the menu and stops there.
    fn pick_mode(cx: &mut gpui::VisualTestContext, mode: ApprovalMode) {
        click(cx, "approval-mode");
        click(cx, format!("approval-{}", mode.slug()).leak());
    }

    fn asking(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> bool {
        view.read_with(cx, |view, _| view.confirm_full_access.is_some())
    }

    fn composer_focused(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> bool {
        cx.update(|window, cx| view.read(cx).composer.focus_handle(cx).is_focused(window))
    }

    /// What the window's store and the saved file say about session 1, and how many
    /// times the store has written.
    fn saved_mode(
        view: &Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
    ) -> (ApprovalMode, ApprovalMode, u64) {
        view.read_with(cx, |view, _| {
            let mode = |store: &Store| store.workspace().sessions()[0].approval_mode();
            let on_disk = Store::open(view.store.path()).unwrap();
            (mode(&view.store), mode(&on_disk), view.store.writes())
        })
    }

    fn send_text(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, text: &str) {
        view.update_in(cx, |view, window, cx| {
            view.composer
                .update(cx, |input, cx| input.set_value(text.into(), cx));
            let draft = view.composer_draft(cx).unwrap();
            view.send_message(draft, window, cx);
        });
        cx.run_until_parked();
    }

    /// The text of the last completed reply that contains `text`, once the turn ended.
    fn reply_with(view: &Keystone, text: &str) -> Option<String> {
        let c = view.conversations.get(&1)?;
        if c.running {
            return None;
        }
        c.rows.iter().rev().find_map(|row| match row {
            Row::Item(item)
                if item.kind == ItemKind::AgentMessage
                    && item.status == ItemStatus::Completed
                    && item.text.contains(text) =>
            {
                Some(item.text.clone())
            }
            _ => None,
        })
    }

    /// Waits for a completed reply containing `text` and returns the setting the scripted
    /// provider reports at its end: Codex's policy and sandbox, or Claude's mode.
    fn setting_of_reply(
        view: &Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
        text: &'static str,
    ) -> String {
        wait_until(cx, view, move |view| reply_with(view, text).is_some());
        let reply = view.read_with(cx, |view, _| reply_with(view, text).unwrap());
        reply.rsplit('`').nth(1).unwrap().to_owned()
    }

    /// A wrapper that runs a scripted provider with extra environment, so tests sharing
    /// this process see no change.
    fn wrapper(dir: &Path, name: &str, env: &str) -> PathBuf {
        let script = dir.join(format!("{name}-wrapper"));
        std::fs::write(
            &script,
            format!("#!/bin/sh\n{env} exec '{}' \"$@\"\n", fake(name).display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script
    }

    fn open_window<'a>(
        path: &Path,
        cx: &'a mut gpui::TestAppContext,
    ) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
        let store = Store::open(path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx))
    }

    /// 2b in review: the record that the provider may hold an override is saved before
    /// the override can be sent. Choosing a mode saves it with the mode. A session saved
    /// with a mode but no record is not sent while the record cannot be saved.
    #[gpui::test]
    fn an_override_is_recorded_before_it_can_reach_the_provider(cx: &mut gpui::TestAppContext) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            "KEYSTONE_FAKE_CODEX_DELAY_MS=2",
        );
        std::fs::write(
            &path,
            r#"{"version":1,"next_id":3,"workspace":{"sessions":[
                {"id":1,"title":"Old","provider":"Codex","settled":false,"draft":"","approval_mode":"FullAccess"},
                {"id":2,"title":"New","provider":"Codex","settled":false,"draft":""}],"selected":2}}"#,
        )
        .unwrap();
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, _| {
            view.codex = Some(CodexConnection::new(Some(codex), "test", view.wake.clone()))
        });
        let saved = |id: u64| {
            Store::open(&path)
                .unwrap()
                .workspace()
                .sessions()
                .iter()
                .find(|s| s.id() == id)
                .unwrap()
                .clone()
        };
        choose_mode(cx, ApprovalMode::Auto);
        assert!(
            saved(2).approval_overridden(),
            "saved with the mode, before any turn"
        );

        view.update(cx, |view, cx| view.select(1, cx));
        cx.run_until_parked();
        let readonly = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(dir.path(), readonly).unwrap();
        send_text(&view, cx, "hello");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let error = view.conversations[&1].error.clone().unwrap();
            assert!(error.starts_with("Not sent"), "{error}");
            assert_eq!(view.composer.read(cx).value(), "hello");
            assert!(
                !view.codex.as_ref().unwrap().is_open(1),
                "nothing reached Codex"
            );
        });
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: hello"),
            "never / dangerFullAccess"
        );
        assert!(saved(1).approval_overridden());
    }

    /// 2c in review: a mode chosen from the menu reaches work already handed to Codex.
    /// A steer sent during a Full access turn reaches Codex as the turn ends; Codex
    /// rejects it and the driver sends it as the next turn, which must use Read only.
    #[gpui::test]
    fn a_mode_chosen_from_the_menu_reaches_a_steer_that_becomes_the_next_turn(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            "KEYSTONE_FAKE_CODEX_DELAY_MS=20",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, _| {
            view.codex = Some(CodexConnection::new(Some(codex), "test", view.wake.clone()))
        });
        click(cx, "welcome-codex");
        choose_mode(cx, ApprovalMode::FullAccess);
        send_text(&view, cx, "end on steer");
        wait_until(cx, &view, |view| {
            view.conversations[&1].rows.iter().any(|row| {
                matches!(row, Row::Item(item)
                    if item.kind == ItemKind::AgentMessage && !item.text.is_empty())
            })
        });
        send_text(&view, cx, "late");
        // Codex holds the steer for 300 ms; choose while it waits there.
        std::thread::sleep(std::time::Duration::from_millis(100));
        choose_mode(cx, ApprovalMode::ReadOnly);
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: late"),
            "on-request / readOnly"
        );
    }

    /// 2a in review: Codex exits after the user chose Read only on a thread that ran with
    /// Full access. The next message starts a new app-server, which must still get Read
    /// only; the resumed thread would otherwise keep Full access. After Keystone
    /// restarts, Provider settings restore the user's own settings.
    #[gpui::test]
    fn a_mode_survives_a_codex_restart_and_provider_settings_restore_after_one(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            &format!(
                "KEYSTONE_FAKE_CODEX_DELAY_MS=2 KEYSTONE_FAKE_CODEX_STATE='{}'",
                dir.path().join("codex-state.json").display()
            ),
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, _| {
            view.codex = Some(CodexConnection::new(
                Some(codex.clone()),
                "test",
                view.wake.clone(),
            ))
        });
        click(cx, "welcome-codex");
        choose_mode(cx, ApprovalMode::FullAccess);
        send_text(&view, cx, "first");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: first"),
            "never / dangerFullAccess"
        );

        choose_mode(cx, ApprovalMode::ReadOnly);
        let pid = view.read_with(cx, |view, _| view.codex.as_ref().unwrap().pid().unwrap());
        std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()
            .unwrap();
        wait_until(cx, &view, |view| {
            view.codex.as_ref().unwrap().pid().is_none()
        });
        send_text(&view, cx, "second");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: second"),
            "on-request / readOnly"
        );

        // Keystone restarts. The thread still holds Read only, so Provider settings must
        // send the user's own.
        view.update(cx, |view, cx| view.flush_logs(cx));
        let store = Store::open(&path).unwrap();
        cx.executor().allow_parking();
        let (restored, cx) =
            cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        restored.update(cx, |view, _| {
            view.codex = Some(CodexConnection::new(Some(codex), "test", view.wake.clone()))
        });
        wait_until(cx, &restored, |view| {
            view.conversations.get(&1).is_some_and(|c| !c.is_loading())
        });
        choose_mode(cx, ApprovalMode::Provider);
        send_text(&restored, cx, "third");
        assert_eq!(
            setting_of_reply(&restored, cx, "You wrote: third"),
            "on-request / workspaceWrite"
        );
    }

    /// Full access asks first. Cancel, Escape, Enter (Cancel has focus), and a press
    /// outside the dialog each leave the mode as it was: nothing is saved, the
    /// connection keeps its state, and the next turn runs under the old mode.
    #[gpui::test]
    fn full_access_changes_nothing_until_confirmed(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            "KEYSTONE_FAKE_CODEX_DELAY_MS=2",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, _| {
            view.codex = Some(CodexConnection::new(Some(codex), "test", view.wake.clone()))
        });
        click(cx, "welcome-codex");
        choose_mode(cx, ApprovalMode::Auto);
        send_text(&view, cx, "first");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: first"),
            "on-request / workspaceWrite"
        );
        let connection = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, _| view.codex.as_ref().unwrap().approval(1))
        };
        let auto = Some(ApprovalState {
            mode: ApprovalMode::Auto,
            overridden: true,
        });
        assert_eq!(connection(&view, cx), auto);
        let before = saved_mode(&view, cx);
        let file = std::fs::read(&path).unwrap();
        for how in ["cancel", "escape", "enter", "outside"] {
            pick_mode(cx, ApprovalMode::FullAccess);
            assert!(asking(&view, cx), "{how}: the dialog shows");
            assert!(cx.debug_bounds("full-access-dialog").is_some(), "{how}");
            assert!(!composer_focused(&view, cx), "{how}: the dialog has focus");
            match how {
                "cancel" => click(cx, "full-access-dialog-cancel"),
                "escape" => cx.simulate_keystrokes("escape"),
                "enter" => {
                    cx.simulate_keystrokes("enter");
                    cx.simulate_event(gpui::KeyUpEvent {
                        keystroke: gpui::Keystroke::parse("enter").unwrap(),
                    });
                }
                _ => {
                    cx.simulate_click(gpui::point(px(4.), px(4.)), gpui::Modifiers::none());
                    cx.run_until_parked();
                }
            }
            assert!(!asking(&view, cx), "{how}: the dialog closed");
            assert!(composer_focused(&view, cx), "{how}: focus is back");
            assert_eq!(saved_mode(&view, cx), before, "{how}: nothing saved");
            assert_eq!(std::fs::read(&path).unwrap(), file, "{how}");
            assert_eq!(connection(&view, cx), auto, "{how}: nothing sent");
        }
        send_text(&view, cx, "second");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: second"),
            "on-request / workspaceWrite"
        );
    }

    /// Work already handed to Codex keeps the old mode while the dialog is open: a
    /// steer Codex rejects becomes the next turn, under Read only. While the dialog is
    /// open the shortcut sends nothing. Confirm saves Full access once, gives it to the
    /// connection, and the next turn runs in it.
    #[gpui::test]
    fn pending_work_never_runs_in_full_access_before_confirm(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            "KEYSTONE_FAKE_CODEX_DELAY_MS=20",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, _| {
            view.codex = Some(CodexConnection::new(Some(codex), "test", view.wake.clone()))
        });
        click(cx, "welcome-codex");
        choose_mode(cx, ApprovalMode::ReadOnly);
        send_text(&view, cx, "end on steer");
        wait_until(cx, &view, |view| {
            view.conversations[&1].rows.iter().any(|row| {
                matches!(row, Row::Item(item)
                    if item.kind == ItemKind::AgentMessage && !item.text.is_empty())
            })
        });
        send_text(&view, cx, "late");
        // Codex holds the steer for 300 ms; pick Full access while it waits there.
        pick_mode(cx, ApprovalMode::FullAccess);
        assert!(asking(&view, cx));
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: late"),
            "on-request / readOnly"
        );
        view.update(cx, |view, cx| {
            view.composer
                .update(cx, |input, cx| input.set_value("blocked".into(), cx))
        });
        cx.simulate_keystrokes("ctrl-enter");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "blocked", "not sent");
        });
        let (_, _, writes) = saved_mode(&view, cx);
        click(cx, "full-access-dialog-confirm");
        assert!(!asking(&view, cx));
        assert!(composer_focused(&view, cx));
        let full = ApprovalMode::FullAccess;
        assert_eq!(
            saved_mode(&view, cx),
            (full, full, writes + 1),
            "saved once"
        );
        view.read_with(cx, |view, _| {
            assert!(view.store.workspace().sessions()[0].approval_overridden());
            assert_eq!(
                view.codex.as_ref().unwrap().approval(1),
                Some(ApprovalState {
                    mode: full,
                    overridden: true
                })
            );
        });
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: blocked"),
            "never / dangerFullAccess"
        );
    }

    /// Only entering Full access from the menu asks. A saved Full access session opens
    /// without the dialog, choosing it again changes nothing, and leaving it applies at
    /// once.
    #[gpui::test]
    fn only_entering_full_access_from_the_menu_asks(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        std::fs::write(
            &path,
            r#"{"version":1,"next_id":2,"workspace":{"sessions":[
                {"id":1,"title":"Full","provider":"Codex","settled":false,"draft":"",
                 "approval_mode":"FullAccess","approval_overridden":true}],"selected":1}}"#,
        )
        .unwrap();
        let (view, cx) = open_window(&path, cx);
        cx.run_until_parked();
        assert!(!asking(&view, cx), "restoring a saved mode asks nothing");
        assert!(cx.debug_bounds("full-access-dialog").is_none());
        let full = ApprovalMode::FullAccess;
        assert_eq!(saved_mode(&view, cx), (full, full, 0));

        pick_mode(cx, full);
        assert!(!asking(&view, cx), "already in Full access");
        assert_eq!(saved_mode(&view, cx), (full, full, 0), "nothing saved");

        let read_only = ApprovalMode::ReadOnly;
        pick_mode(cx, read_only);
        assert!(!asking(&view, cx), "leaving Full access asks nothing");
        assert_eq!(saved_mode(&view, cx), (read_only, read_only, 1));
    }

    /// For Claude, Cancel keeps the running process and its mode; only Confirm replaces
    /// it with one in Full access.
    #[gpui::test]
    fn claude_keeps_its_process_until_full_access_is_confirmed(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let claude = wrapper(
            dir.path(),
            "keystone-fake-claude",
            "KEYSTONE_FAKE_CLAUDE_DELAY_MS=5",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, _| {
            view.claude = Some(ClaudeConnection::new(Some(claude), view.wake.clone()))
        });
        let pid = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, _| view.claude.as_ref().unwrap().pid(1))
        };
        click(cx, "welcome-claude");
        choose_mode(cx, ApprovalMode::Auto);
        send_text(&view, cx, "hello");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: hello"),
            "acceptEdits"
        );
        let first = pid(&view, cx);

        pick_mode(cx, ApprovalMode::FullAccess);
        click(cx, "full-access-dialog-cancel");
        send_text(&view, cx, "again");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: again"),
            "acceptEdits"
        );
        assert_eq!(pid(&view, cx), first, "Cancel kept the process");

        choose_mode(cx, ApprovalMode::FullAccess);
        send_text(&view, cx, "full");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: full"),
            "bypassPermissions"
        );
        assert_ne!(pid(&view, cx), first, "Confirm replaced it");
    }

    /// Claude sessions apply each mode from the menu. Full access and a return to
    /// Provider settings need a new `claude` process, which resumes the conversation. A
    /// change during a turn applies from the next turn.
    #[gpui::test]
    fn claude_sessions_apply_approval_modes_from_the_menu(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let claude = wrapper(
            dir.path(),
            "keystone-fake-claude",
            "KEYSTONE_FAKE_CLAUDE_DELAY_MS=5",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, _| {
            view.claude = Some(ClaudeConnection::new(Some(claude), view.wake.clone()))
        });
        let pid = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, _| view.claude.as_ref().unwrap().pid(1))
        };
        click(cx, "welcome-claude");
        choose_mode(cx, ApprovalMode::Auto);
        send_text(&view, cx, "hello");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: hello"),
            "acceptEdits"
        );
        let first = pid(&view, cx);

        // A change while a turn runs waits for the next turn.
        send_text(&view, cx, "long");
        wait_until(cx, &view, |view| view.conversations[&1].running);
        choose_mode(cx, ApprovalMode::ReadOnly);
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: long"),
            "acceptEdits"
        );
        send_text(&view, cx, "next");
        assert_eq!(setting_of_reply(&view, cx, "You wrote: next"), "plan");
        assert_eq!(pid(&view, cx), first, "changed in place");

        // Back to the user's own settings, in a process launched without a mode.
        choose_mode(cx, ApprovalMode::Provider);
        send_text(&view, cx, "back");
        assert_eq!(setting_of_reply(&view, cx, "You wrote: back"), "default");
        let own = pid(&view, cx);
        assert_ne!(own, first);

        // Full access runs the command without asking, in a new process.
        choose_mode(cx, ApprovalMode::FullAccess);
        send_text(&view, cx, "please run it");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: please run it"),
            "bypassPermissions"
        );
        view.read_with(cx, |view, _| {
            assert!(view.conversations[&1].rows.iter().any(|row| matches!(row,
                Row::Item(item) if matches!(item.kind, ItemKind::Command { exit_code: Some(0), .. }))));
            assert!(!view.conversations[&1].rows.iter().any(|row| matches!(row, Row::Decision { .. })));
        });
        let full = pid(&view, cx);
        assert_ne!(full, own);

        // Leaving Full access needs a new process too.
        choose_mode(cx, ApprovalMode::Ask);
        send_text(&view, cx, "last");
        assert_eq!(setting_of_reply(&view, cx, "You wrote: last"), "default");
        assert_ne!(pid(&view, cx), full);
        let thread = Store::open(&path).unwrap().workspace().sessions()[0]
            .provider_thread()
            .map(str::to_owned);
        assert!(thread.is_some_and(|t| t.starts_with("fake-claude-session-")));
    }

    /// A scripted provider built by `keystone-agent`. Test binaries live in
    /// target/<profile>/deps; the scripts are one level up. `scripts/check.sh ui` builds
    /// them first.
    fn fake(name: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let path = exe.parent().unwrap().parent().unwrap().join(name);
        assert!(
            path.exists(),
            "build it with `cargo build -p keystone-agent --bins`"
        );
        path
    }

    #[gpui::test]
    fn codex_session_streams_asks_approval_and_restores_from_its_log(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        // SAFETY: only this test reads these variables, and it sets them before any
        // thread that could read them starts.
        unsafe {
            std::env::set_var("KEYSTONE_CODEX_PATH", fake("keystone-fake-codex"));
            std::env::set_var("KEYSTONE_FAKE_CODEX_DELAY_MS", "2");
        }
        let store = Store::open(&path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let bounds = cx.debug_bounds("welcome-codex").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("hello");
        let bounds = cx.debug_bounds("send").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());

        let finished = |view: &Keystone| {
            view.conversations.get(&1).is_some_and(|c| {
                !c.running
                    && c.rows.iter().any(|row| matches!(row, Row::Item(item)
                        if item.kind == ItemKind::AgentMessage && item.status == ItemStatus::Completed))
            })
        };
        wait_until(cx, &view, finished);
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "");
            let rows = &view.conversations[&1].rows;
            assert!(matches!(&rows[0], Row::Item(item) if item.kind == ItemKind::UserMessage && item.text == "hello"));
            // No mode chosen: Keystone sent no override, so Codex used its own settings.
            assert!(rows.iter().any(|row| matches!(row, Row::Item(item)
                if item.text.contains("You wrote: hello") && item.text.contains("`on-request / workspaceWrite`"))));
        });
        // The reply renders as Markdown, with a copy action once it is complete.
        let reply = view.read_with(cx, |view, _| {
            view.conversations[&1]
                .rows
                .iter()
                .position(
                    |row| matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage),
                )
                .unwrap()
        });
        view.read_with(cx, |view, _| {
            let Row::Item(item) = &view.conversations[&1].rows[reply] else {
                panic!("a reply row");
            };
            assert!(
                !view
                    .replies
                    .borrow_mut()
                    .document((1, reply), item)
                    .is_empty()
            );
        });
        click(cx, format!("copy-reply-{reply}").leak());
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        assert!(copied.unwrap().contains("**Approvals:**"));
        hover(cx, "transcript-row-0");
        click(cx, "copy-message-0");
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        assert_eq!(copied.as_deref(), Some("hello"));
        view.update_in(cx, |view, window, cx| {
            view.composer.focus_handle(cx).focus(window, cx)
        });
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].provider_thread(),
            Some("fake-thread-1")
        );

        // The shortcut sends too. A command waits for approval.
        cx.simulate_input("please run it");
        cx.simulate_keystrokes("ctrl-enter");
        wait_until(cx, &view, |view| {
            !view.conversations[&1].approvals.is_empty()
        });
        let bounds = cx.debug_bounds("approve").expect("approval card rendered");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        // Answering hands focus back to the composer.
        cx.simulate_input("next");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "next")
        });
        wait_until(cx, &view, |view| {
            let c = &view.conversations[&1];
            !c.running
                && c.rows.iter().any(|row| {
                    matches!(row, Row::Item(item)
                if matches!(item.kind, ItemKind::Command { exit_code: Some(0), .. }))
                })
        });

        // A fresh window reads the conversation back from the session's log.
        view.update(cx, |view, cx| view.flush_logs(cx));
        let store = Store::open(&path).unwrap();
        cx.executor().allow_parking();
        let (restored, cx) =
            cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        wait_until(cx, &restored, |view| {
            view.conversations.get(&1).is_some_and(|c| !c.is_loading())
        });
        restored.read_with(cx, |view, _| {
            let rows = &view.conversations[&1].rows;
            assert!(rows.contains(&Row::Decision { action: "echo scripted".into(), allowed: true }));
            assert!(rows.iter().any(|row| matches!(row, Row::Item(item) if item.text.contains("You wrote: please run it"))));
        });
    }

    #[gpui::test]
    fn claude_session_streams_asks_approval_stops_and_restores_from_its_log(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        // SAFETY: only this test reads these variables, and it sets them before any
        // thread that could read them starts.
        unsafe {
            std::env::set_var("KEYSTONE_CLAUDE_PATH", fake("keystone-fake-claude"));
            std::env::set_var("KEYSTONE_FAKE_CLAUDE_DELAY_MS", "2");
        }
        let store = Store::open(&path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let bounds = cx.debug_bounds("welcome-claude").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("hello");
        let bounds = cx.debug_bounds("send").expect("Claude sessions can send");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());

        let replied = |text: &'static str| {
            move |view: &Keystone| {
                view.conversations.get(&1).is_some_and(|c| {
                    !c.running
                        && c.rows.iter().any(|row| {
                            matches!(row, Row::Item(item)
                            if item.kind == ItemKind::AgentMessage && item.text.contains(text))
                        })
                })
            }
        };
        wait_until(cx, &view, replied("You wrote: hello"));
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "");
            let rows = &view.conversations[&1].rows;
            // The local copy was replaced by Claude's echo, not shown twice.
            let users = rows
                .iter()
                .filter(|row| {
                    matches!(row, Row::Item(item)
                if item.kind == ItemKind::UserMessage)
                })
                .count();
            assert_eq!(users, 1);
        });
        let thread = Store::open(&path).unwrap().workspace().sessions()[0]
            .provider_thread()
            .map(str::to_owned);
        assert!(thread.is_some_and(|t| t.starts_with("fake-claude-session-")));

        // A command waits for approval.
        cx.simulate_input("please run it");
        cx.simulate_keystrokes("ctrl-enter");
        wait_until(cx, &view, |view| {
            !view.conversations[&1].approvals.is_empty()
        });
        let bounds = cx.debug_bounds("approve").expect("approval card rendered");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        wait_until(cx, &view, |view| {
            let c = &view.conversations[&1];
            !c.running
                && c.rows.iter().any(|row| {
                    matches!(row, Row::Item(item)
                if matches!(item.kind, ItemKind::Command { exit_code: Some(0), .. }))
                })
        });

        // Stop ends a long reply and keeps what streamed.
        cx.simulate_input("a long one");
        cx.simulate_keystrokes("ctrl-enter");
        wait_until(cx, &view, |view| {
            view.conversations[&1].rows.iter().any(|row| {
                matches!(row, Row::Item(item)
                if item.status == ItemStatus::InProgress && item.text.contains("word1"))
            })
        });
        let bounds = cx.debug_bounds("stop").expect("stop shown while running");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        wait_until(cx, &view, |view| {
            let c = &view.conversations[&1];
            !c.running
                && matches!(
                    c.rows.last(),
                    Some(Row::TurnEnded {
                        status: TurnStatus::Interrupted,
                        ..
                    })
                )
        });

        // A fresh window reads the conversation back from the session's log.
        view.update(cx, |view, cx| view.flush_logs(cx));
        let store = Store::open(&path).unwrap();
        cx.executor().allow_parking();
        let (restored, cx) =
            cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        wait_until(cx, &restored, |view| {
            view.conversations.get(&1).is_some_and(|c| !c.is_loading())
        });
        restored.read_with(cx, |view, _| {
            let rows = &view.conversations[&1].rows;
            assert!(rows.contains(&Row::Decision {
                action: "echo scripted".into(),
                allowed: true
            }));
            assert!(rows.iter().any(
                |row| matches!(row, Row::Item(item) if item.text.contains("You wrote: hello"))
            ));
            // Claude completes the cut-off block itself, so the partial reply is kept.
            assert!(rows.iter().any(|row| matches!(row, Row::Item(item)
                if item.kind == ItemKind::AgentMessage && item.text.contains("word1"))));
        });
    }

    #[gpui::test]
    fn a_provider_that_cannot_start_shows_an_error_that_stays(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("workspace.json")).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        view.update(cx, |view, _| {
            view.claude = Some(ClaudeConnection::new(
                Some(PathBuf::from("/nonexistent/claude")),
                view.wake.clone(),
            ))
        });
        let bounds = cx.debug_bounds("welcome-claude").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("hello");
        cx.simulate_keystrokes("ctrl-enter");
        // The failure arrives from the connection's worker, after the send returned.
        wait_until(cx, &view, |view| {
            view.conversations
                .get(&1)
                .is_some_and(|c| c.error.is_some())
        });
        // The draft save that follows typing must not hide the failure.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let conversation = &view.conversations[&1];
            let error = conversation.error.clone().unwrap_or_default();
            assert!(
                error.starts_with("Could not start Claude Code at /nonexistent/claude."),
                "{error}"
            );
            assert!(!conversation.running);
            // The message stays in the transcript, and the error is shown once.
            assert!(
                conversation
                    .rows
                    .iter()
                    .any(|row| matches!(row, Row::Item(item)
                if item.kind == ItemKind::UserMessage && item.text == "hello"))
            );
            assert_eq!(view.error, None);
        });
    }

    /// A Claude without lifecycle frames cannot be steered. The refusal shows its own
    /// reason, not a start failure, and the text stays in the composer.
    #[gpui::test]
    fn a_refused_steer_shows_why_and_keeps_the_text(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("workspace.json")).unwrap();
        // The setting goes in a wrapper, so tests sharing this process see no change.
        let script = dir.path().join("old-claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nKEYSTONE_FAKE_CLAUDE_NO_LIFECYCLE=1 KEYSTONE_FAKE_CLAUDE_DELAY_MS=5 exec '{}' \"$@\"\n",
                fake("keystone-fake-claude").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        view.update(cx, |view, _| {
            view.claude = Some(ClaudeConnection::new(Some(script), view.wake.clone()))
        });
        let bounds = cx.debug_bounds("welcome-claude").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("a long one");
        cx.simulate_keystrokes("ctrl-enter");
        wait_until(cx, &view, |view| {
            view.conversations[&1].rows.iter().any(|row| {
                matches!(row, Row::Item(item)
                if item.status == ItemStatus::InProgress && item.text.contains("word1"))
            })
        });
        cx.simulate_input("change course");
        cx.simulate_keystrokes("ctrl-enter");
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let conversation = &view.conversations[&1];
            let error = conversation.error.clone().unwrap_or_default();
            assert!(
                error.starts_with("This Claude Code version does not report message progress"),
                "{error}"
            );
            assert!(conversation.running, "the turn goes on");
            assert_eq!(view.composer.read(cx).value(), "change course");
            assert!(
                !conversation
                    .rows
                    .iter()
                    .any(|row| matches!(row, Row::Item(item)
                if item.text == "change course"))
            );
        });
        wait_until(cx, &view, |view| !view.conversations[&1].running);
    }

    #[gpui::test]
    fn menu_created_session_receives_typed_draft(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        for selector in ["new-session", "new-claude"] {
            let bounds = cx.debug_bounds(selector).expect("control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        cx.simulate_input("Menu draft");
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            "Menu draft"
        );
    }

    /// Row actions show on hover, so point at the row before clicking one.
    fn hover(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let bounds = cx.debug_bounds(selector).expect("row rendered");
        cx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn settled_section_starts_collapsed_and_opens_below_working(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Claude, 1).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        store.set_settled(1, true).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        assert!(cx.debug_bounds("session-1").is_none());
        assert!(cx.debug_bounds("session-2").is_some());

        click(cx, "settled-section");
        let y = |cx: &mut gpui::VisualTestContext, selector| {
            cx.debug_bounds(selector).unwrap().origin.y
        };
        let (active, header, settled) = (
            y(cx, "session-2"),
            y(cx, "settled-section"),
            y(cx, "session-1"),
        );
        assert!(active < header && header < settled);
    }

    fn y(cx: &mut gpui::VisualTestContext, selector: &'static str) -> Pixels {
        cx.debug_bounds(selector).expect("rendered").origin.y
    }

    #[gpui::test]
    fn needs_action_holds_approval_failure_and_reply_above_working(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        for now in 1..=5 {
            store.create(Provider::Codex, now).unwrap();
        }
        store.record_turn_end(1, true, 10).unwrap();
        store.record_turn_end(2, false, 11).unwrap();
        store.set_settled(5, true).unwrap();
        store.record_turn_end(5, true, 12).unwrap();
        store.select(4, 13).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        view.update(cx, |this, cx| {
            let mut live = Conversation::default();
            live.running = true;
            live.approvals.push(keystone_agent::Approval {
                request: keystone_agent::RequestId("1".into()),
                item_id: None,
                action: "cargo test".into(),
                detail: None,
            });
            this.conversations.insert(3, live);
            this.refresh(cx);
        });
        cx.run_until_parked();

        let header = y(cx, "attention-section");
        let rows = [y(cx, "session-1"), y(cx, "session-2"), y(cx, "session-3")];
        let working = y(cx, "working-section");
        assert!(rows.iter().all(|&row| header < row && row < working));
        assert!(working < y(cx, "session-4"));
        assert!(y(cx, "session-4") < y(cx, "settled-section"));
        // A settled session stays in collapsed Settled even with a failed turn.
        assert!(cx.debug_bounds("session-5").is_none());
        view.read_with(cx, |this, cx| {
            let sidebar = this.session_sidebar.read(cx);
            assert!(sidebar.shows_time);
        });
    }

    #[gpui::test]
    fn a_codex_that_fails_to_start_moves_the_session_to_needs_action(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        store.create(Provider::Codex, 2).unwrap();
        store.select(1, 3).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let missing = dir.path().join("no-such-codex");
        view.update_in(cx, |this, window, cx| {
            this.codex = Some(CodexConnection::new(Some(missing), "0", this.wake.clone()));
            this.composer
                .update(cx, |input, cx| input.set_value("hello".into(), cx));
            let draft = this.composer_draft(cx).unwrap();
            this.send_message(draft, window, cx);
        });
        cx.run_until_parked();
        // The start fails on the connection's worker, after the send returned. Until
        // then the turn is running.
        wait_until(cx, &view, |view| {
            view.conversations[&1].error.is_some() && !view.conversations[&1].running
        });
        cx.run_until_parked();
        let header = y(cx, "attention-section");
        assert!(header < y(cx, "session-1") && y(cx, "session-1") < y(cx, "working-section"));
        assert!(y(cx, "working-section") < y(cx, "session-2"));
        view.read_with(cx, |this, _| {
            let session = &this.store.workspace().sessions()[0];
            assert_eq!(session.last_turn(), Some(keystone_core::LastTurn::Failed));
            assert_eq!(session.attention(false, false), Some(Attention::Failed));
        });
    }

    #[gpui::test]
    fn an_approval_held_while_history_loads_needs_action_once_it_loads(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        // Selected, so session 1 loads only when the test asks for it.
        store.create(Provider::Codex, 2).unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        // Session 1's history arrives only when the test releases it.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        view.update(cx, |this, cx| {
            let read: keystone_agent::log::Reader = Arc::new(move |dir, session| {
                if session == 1 {
                    let _ = released.lock().unwrap().recv();
                }
                keystone_agent::log::read(dir, session)
            });
            this.logs = Logs::with_reader(sessions.clone(), this.wake.clone(), read);
            this.conversations.clear();
            this.conversation(2);
            assert!(this.conversation(1).is_loading());
            this.conversation(1)
                .apply(Event::ApprovalRequested(keystone_agent::Approval {
                    request: keystone_agent::RequestId("1".into()),
                    item_id: None,
                    action: "cargo test".into(),
                    detail: None,
                }));
            this.refresh(cx);
        });
        cx.run_until_parked();
        // Held with the history, as the approval card is.
        assert!(y(cx, "working-section") < y(cx, "session-1"));

        release.send(()).unwrap();
        wait_until(cx, &view, |view| !view.conversations[&1].is_loading());
        cx.run_until_parked();
        view.read_with(cx, |this, _| {
            assert_eq!(this.conversations[&1].approvals.len(), 1)
        });
        let header = y(cx, "attention-section");
        assert!(header < y(cx, "session-1") && y(cx, "session-1") < y(cx, "working-section"));
    }

    #[gpui::test]
    fn an_unread_row_stays_in_needs_action_until_the_user_leaves_it(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        store.create(Provider::Codex, 2).unwrap();
        store.record_turn_end(1, false, 3).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        assert!(y(cx, "session-1") < y(cx, "working-section"));

        click(cx, "session-1");
        assert!(y(cx, "session-1") < y(cx, "working-section"));

        click(cx, "session-2");
        assert!(y(cx, "session-1") > y(cx, "working-section"));
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].attention(false, false),
            None
        );
    }

    /// Types a long single line into a Codex composer, clicks `row` (0-based) at its
    /// left edge, presses `keys`, and types `mark`. Returns the composer's text.
    fn mark_visual_row(
        row: usize,
        keys: &str,
        mark: &str,
        cx: &mut gpui::TestAppContext,
    ) -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let line = (0..60)
            .map(|n| format!("w{n:02}"))
            .collect::<Vec<_>>()
            .join(" ");
        click(cx, "composer-field");
        cx.simulate_input(&line);
        let field = cx.debug_bounds("composer-field").unwrap();
        let point = field.origin + gpui::point(px(1.), px(20.) * row + px(10.));
        cx.simulate_click(point, gpui::Modifiers::none());
        if !keys.is_empty() {
            cx.simulate_keystrokes(keys);
        }
        cx.simulate_input(mark);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| view.composer.read(cx).value().to_owned())
    }

    #[gpui::test]
    fn a_long_draft_wraps_and_the_composer_grows_to_its_cap(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let field = |cx: &mut gpui::VisualTestContext| cx.debug_bounds("composer-field").unwrap();
        let send = |cx: &mut gpui::VisualTestContext| cx.debug_bounds("send").unwrap();
        let (empty, toolbar) = (field(cx), send(cx));
        assert_eq!(empty.size.height, px(44.));

        let set = |text: String, cx: &mut gpui::VisualTestContext| {
            view.update(cx, |this, cx| {
                this.composer
                    .update(cx, |input, cx| input.set_value(text, cx))
            });
            cx.run_until_parked();
        };
        set("word ".repeat(60), cx);
        let wrapped = field(cx);
        assert!(wrapped.size.height > px(44.) && wrapped.size.height < px(160.));
        // The composer grows upward; its toolbar stays docked.
        assert_eq!(wrapped.bottom(), empty.bottom());
        assert_eq!(send(cx), toolbar);
        assert_eq!(wrapped.size.width, empty.size.width);

        // Eight rows at most, then the draft scrolls inside the field.
        set("word ".repeat(600), cx);
        assert_eq!(field(cx).size.height, px(160.));
        assert_eq!(send(cx), toolbar);
    }

    #[gpui::test]
    fn clicks_and_arrows_follow_wrapped_rows(cx: &mut gpui::TestAppContext) {
        let words = |text: &str, mark: &str| {
            let index = text.find(mark).unwrap();
            // A wrapped row starts at a word, after the space the row above ended on.
            assert!(index > 0, "{text}");
            assert_eq!(&text[index - 1..index], " ", "{text}");
            assert!(text[index + mark.len()..].starts_with('w'), "{text}");
            text[..index].split(' ').count() - 1
        };
        let second = words(&mark_visual_row(1, "", "|", cx), "|");
        let third = words(&mark_visual_row(1, "down", "|", cx), "|");
        let back = words(&mark_visual_row(2, "up", "|", cx), "|");
        assert!(second > 0 && third > second, "{second} {third}");
        assert_eq!(back, second);
    }

    #[gpui::test]
    fn row_settle_moves_that_session_and_keeps_the_selection(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Claude, 1).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let saved = |path: &Path| {
            let store = Store::open(path).unwrap();
            let settled = store
                .workspace()
                .sessions()
                .iter()
                .map(|s| s.settled())
                .collect::<Vec<_>>();
            (store.workspace().selected(), settled)
        };

        hover(cx, "session-1");
        click(cx, "settle-1");
        assert_eq!(saved(&path), (Some(2), vec![true, false]));

        click(cx, "settled-section");
        hover(cx, "session-1");
        click(cx, "settle-1");
        assert_eq!(saved(&path), (Some(2), vec![false, false]));
    }

    #[gpui::test]
    fn sidebar_toggle_hides_the_column_and_ctrl_f_restores_it(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("workspace.json")).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let toggle_x =
            |cx: &mut gpui::VisualTestContext| cx.debug_bounds("sidebar-toggle").unwrap().origin.x;
        assert_eq!(toggle_x(cx), px(308.));

        click(cx, "sidebar-toggle");
        assert_eq!(toggle_x(cx), px(6.));

        cx.simulate_keystrokes("ctrl-f");
        assert_eq!(toggle_x(cx), px(308.));
        view.update_in(cx, |this, window, cx| {
            assert!(this.search.focus_handle(cx).is_focused(window))
        });
    }

    #[test]
    fn only_a_turn_the_user_ended_is_interrupted_without_failing() {
        let ended = |status, stopped| Event::TurnCompleted {
            status,
            error: None,
            stopped,
        };
        assert_eq!(
            turn_failed(&ended(TurnStatus::Completed, false)),
            Some(false)
        );
        assert_eq!(
            turn_failed(&ended(TurnStatus::Interrupted, true)),
            Some(false)
        );
        assert_eq!(
            turn_failed(&ended(TurnStatus::Interrupted, false)),
            Some(true)
        );
        assert_eq!(turn_failed(&ended(TurnStatus::Failed, true)), Some(true));
        let failed = Event::Failed {
            message: "Codex exited".into(),
        };
        assert_eq!(turn_failed(&failed), Some(true));
        assert_eq!(turn_failed(&Event::TurnStarted), None);
    }

    /// A Codex driver with session 1's thread open, standing in for the app-server.
    struct Codex {
        driver: keystone_agent::codex::CodexDriver,
        events: Vec<(u64, Event)>,
    }

    impl Codex {
        const THREAD: &str = "thread";

        fn open() -> Self {
            use keystone_agent::{Outputs, codex::CodexDriver};
            let mut out = Outputs::default();
            let mut driver = CodexDriver::new("0", &mut out);
            driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
            driver.command(
                Command::Open {
                    session: 1,
                    cwd: "/tmp".into(),
                    resume: None,
                },
                &mut out,
            );
            let thread = format!(
                r#"{{"id":2,"result":{{"thread":{{"id":"{}"}}}}}}"#,
                Self::THREAD
            );
            driver.on_line(thread.as_bytes(), &mut out);
            Self {
                driver,
                events: Vec::new(),
            }
        }

        fn command(&mut self, command: Command) {
            let mut out = keystone_agent::Outputs::default();
            self.driver.command(command, &mut out);
            self.events.extend(out.events);
        }

        fn line(&mut self, line: String) {
            let mut out = keystone_agent::Outputs::default();
            self.driver.on_line(line.as_bytes(), &mut out);
            self.events.extend(out.events);
        }

        fn send(&mut self, turn: &str) {
            self.command(Command::Send {
                session: 1,
                text: turn.into(),
            });
            self.line(format!(
                r#"{{"method":"turn/started","params":{{"threadId":"{}","turn":{{"id":"{turn}"}}}}}}"#,
                Self::THREAD
            ));
        }

        fn end(&mut self, turn: &str, status: &str) {
            self.line(format!(
                r#"{{"method":"turn/completed","params":{{"threadId":"{}","turn":{{"id":"{turn}","status":"{status}"}}}}}}"#,
                Self::THREAD
            ));
        }

        fn stop(&mut self) {
            self.command(Command::Interrupt { session: 1 });
        }

        /// Asks to run a command, offering `decisions`, and answers it.
        fn ask(&mut self, request: u64, decisions: &[&str], allow: bool) {
            self.line(format!(
                r#"{{"id":{request},"method":"item/commandExecution/requestApproval","params":{{"threadId":"{}","itemId":"exec","command":"touch x","availableDecisions":{decisions:?}}}}}"#,
                Self::THREAD
            ));
            self.command(Command::Answer {
                session: 1,
                request: keystone_agent::RequestId(request.to_string()),
                allow,
            });
        }
    }

    /// Session 1 with three saved turns whose history loads only when `release` is
    /// sent, and session 2 selected.
    fn held_history(
        cx: &mut gpui::TestAppContext,
    ) -> (
        tempfile::TempDir,
        gpui::Entity<Keystone>,
        &mut gpui::VisualTestContext,
        std::sync::mpsc::Sender<()>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        store.create(Provider::Codex, 2).unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        view.update(cx, |this, _| {
            let read: keystone_agent::log::Reader = Arc::new(move |dir, session| {
                if session == 1 {
                    let _ = released.lock().unwrap().recv();
                }
                keystone_agent::log::read(dir, session)
            });
            this.logs = Logs::with_reader(sessions.clone(), this.wake.clone(), read);
            this.conversations.clear();
            this.conversation(2);
            assert!(this.conversation(1).is_loading());
        });
        (dir, view, cx, release)
    }

    fn deliver(view: &gpui::Entity<Keystone>, cx: &mut gpui::VisualTestContext, codex: &mut Codex) {
        let events = std::mem::take(&mut codex.events);
        view.update(cx, |this, cx| this.receive(Vec::new(), events, cx));
    }

    fn load(
        view: &gpui::Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
        release: &std::sync::mpsc::Sender<()>,
    ) {
        release.send(()).unwrap();
        wait_until(cx, view, |view| !view.conversations[&1].is_loading());
    }

    fn last_turn(
        view: &gpui::Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
    ) -> Option<keystone_core::LastTurn> {
        view.read_with(cx, |this, _| {
            this.store.workspace().sessions()[0].last_turn()
        })
    }

    #[gpui::test]
    fn a_stop_while_history_loads_does_not_excuse_the_next_turn(cx: &mut gpui::TestAppContext) {
        let (_dir, view, cx, release) = held_history(cx);
        let mut codex = Codex::open();
        codex.send("a");
        codex.stop();
        codex.end("a", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
        // A second turn is sent before the history arrives, and is cut off by Codex.
        codex.send("b");
        codex.end("b", "interrupted");
        deliver(&view, cx, &mut codex);
        load(&view, cx, &release);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
    }

    #[gpui::test]
    fn a_stop_survives_history_replaying_an_earlier_turn(cx: &mut gpui::TestAppContext) {
        let (_dir, view, cx, release) = held_history(cx);
        let mut codex = Codex::open();
        codex.send("a");
        codex.end("a", "completed");
        deliver(&view, cx, &mut codex);
        codex.send("b");
        codex.stop();
        deliver(&view, cx, &mut codex);
        // The history arrives and replays turn a's end before turn b's arrives.
        load(&view, cx, &release);
        codex.end("b", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
    }

    #[gpui::test]
    fn a_denial_excuses_an_interruption_only_when_it_ends_the_turn(cx: &mut gpui::TestAppContext) {
        let (_dir, view, cx, release) = held_history(cx);
        load(&view, cx, &release);
        let mut codex = Codex::open();
        // Denying with "decline" lets the turn go on; a later interruption is a failure,
        // even after another request was allowed.
        codex.send("a");
        codex.ask(10, &["accept", "decline", "cancel"], false);
        codex.ask(11, &["accept", "decline", "cancel"], true);
        codex.end("a", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
        // With no "decline" offered, denying cancels the turn.
        codex.send("b");
        codex.ask(12, &["accept", "cancel"], false);
        codex.end("b", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
        // A Stop of one turn and a denial in another stay with their own turns.
        codex.send("c");
        codex.stop();
        codex.end("c", "interrupted");
        codex.send("d");
        codex.ask(13, &["accept", "decline"], false);
        codex.end("d", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
    }

    #[gpui::test]
    fn a_late_stop_keeps_the_failure_it_arrived_after(cx: &mut gpui::TestAppContext) {
        let (_dir, view, cx, release) = held_history(cx);
        load(&view, cx, &release);
        let mut codex = Codex::open();
        codex.send("a");
        codex.end("a", "failed");
        deliver(&view, cx, &mut codex);
        let rows = view.read_with(cx, |this, _| this.conversations[&1].rows.len());
        // The user pressed Stop before the failure reached them.
        codex.stop();
        deliver(&view, cx, &mut codex);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
        assert_eq!(
            view.read_with(cx, |this, _| this.conversations[&1].rows.len()),
            rows
        );
    }

    #[gpui::test]
    fn a_second_press_on_an_answered_approval_leaves_the_next_one_waiting(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        Store::open(&path)
            .unwrap()
            .create(Provider::Codex, 1)
            .unwrap();
        let (view, cx) = open_window(&path, cx);
        wait_until(cx, &view, |view| {
            view.conversations.get(&1).is_some_and(|c| !c.is_loading())
        });
        let mut codex = Codex::open();
        codex.send("a");
        for (request, command) in [(10, "touch first"), (11, "touch second")] {
            codex.line(format!(
                r#"{{"id":{request},"method":"item/commandExecution/requestApproval","params":{{"threadId":"{}","itemId":"exec-{request}","command":"{command}","availableDecisions":["accept","decline"]}}}}"#,
                Codex::THREAD
            ));
        }
        deliver(&view, cx, &mut codex);
        let allow = cx
            .debug_bounds("approve")
            .expect("approval card rendered")
            .center();
        cx.update(|window, cx| {
            for _ in 0..2 {
                let down = gpui::MouseDownEvent {
                    position: allow,
                    modifiers: gpui::Modifiers::none(),
                    button: gpui::MouseButton::Left,
                    click_count: 1,
                    first_mouse: false,
                };
                let up = gpui::MouseUpEvent {
                    position: allow,
                    modifiers: gpui::Modifiers::none(),
                    button: gpui::MouseButton::Left,
                    click_count: 1,
                };
                window.dispatch_event(gpui::InputEvent::to_platform_input(down), cx);
                window.dispatch_event(gpui::InputEvent::to_platform_input(up), cx);
            }
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let conversation = &view.conversations[&1];
            let waiting: Vec<_> = conversation
                .approvals
                .iter()
                .map(|a| a.action.as_str())
                .collect();
            assert_eq!(waiting, ["touch second"]);
            let decisions: Vec<_> = conversation
                .rows
                .iter()
                .filter(|row| matches!(row, Row::Decision { .. }))
                .collect();
            assert_eq!(
                decisions,
                [&Row::Decision {
                    action: "touch first".into(),
                    allowed: true
                }]
            );
        });
        assert!(
            cx.debug_bounds("approve").is_some(),
            "the next request stays on screen"
        );
    }

    /// A Claude driver for session 1, standing in for the `claude` process.
    struct Claude {
        driver: keystone_agent::claude::ClaudeDriver,
        sent: u64,
        events: Vec<(u64, Event)>,
    }

    impl Claude {
        const SEED: u64 = 7;

        fn open() -> Self {
            let mut out = keystone_agent::Outputs::default();
            let driver = keystone_agent::claude::ClaudeDriver::new(
                1,
                false,
                Self::SEED,
                ApprovalMode::Provider,
                &mut out,
            );
            let mut claude = Self {
                driver,
                sent: 0,
                events: Vec::new(),
            };
            // Claude confirms the mode it launched in, so messages are written.
            claude.line(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{"current_permission_mode":"default"}}}"#
                    .into(),
            );
            claude.line(
                r#"{"type":"system","subtype":"init","session_id":"s","capabilities":["msg_lifecycle_v1"]}"#
                    .into(),
            );
            claude
        }

        fn line(&mut self, line: String) {
            let mut out = keystone_agent::Outputs::default();
            self.driver.on_line(line.as_bytes(), &mut out);
            self.events.extend(out.events);
        }

        fn lifecycle(&mut self, state: &str) {
            let uuid = keystone_agent::claude::message_uuid(Self::SEED, self.sent);
            self.line(format!(
                r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"{state}"}}"#
            ));
        }

        fn send(&mut self) {
            let mut out = keystone_agent::Outputs::default();
            self.driver.command(
                Command::Send {
                    session: 1,
                    text: "hello".into(),
                },
                &mut out,
            );
            self.events.extend(out.events);
            self.sent += 1;
            self.lifecycle("queued");
            self.lifecycle("started");
        }

        fn stop(&mut self) {
            let mut out = keystone_agent::Outputs::default();
            self.driver
                .command(Command::Interrupt { session: 1 }, &mut out);
            self.events.extend(out.events);
        }

        /// Ends the message with a result, failed or not.
        fn end(&mut self, failed: bool) {
            self.line(if failed {
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["overloaded"]}"#.into()
            } else {
                r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#.into()
            });
            self.lifecycle("completed");
        }
    }

    fn deliver_claude(
        view: &gpui::Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
        claude: &mut Claude,
    ) {
        let events = std::mem::take(&mut claude.events);
        view.update(cx, |this, cx| this.receive(Vec::new(), events, cx));
    }

    #[gpui::test]
    fn a_claude_stop_or_failure_classifies_its_own_turn_while_history_loads(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_dir, view, cx, release) = held_history(cx);
        let mut claude = Claude::open();
        claude.send();
        claude.stop();
        claude.end(true);
        deliver_claude(&view, cx, &mut claude);
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
        // The next turn fails on its own before the history arrives.
        claude.send();
        claude.end(true);
        deliver_claude(&view, cx, &mut claude);
        load(&view, cx, &release);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
        // A Stop that reaches Claude after the failure changes nothing.
        let rows = view.read_with(cx, |this, _| this.conversations[&1].rows.len());
        claude.stop();
        deliver_claude(&view, cx, &mut claude);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
        assert_eq!(
            view.read_with(cx, |this, _| this.conversations[&1].rows.len()),
            rows
        );
    }

    /// A wrapper that runs the scripted Claude with `env` set, so tests sharing this
    /// process see no change.
    fn claude_script(dir: &std::path::Path, env: &str) -> PathBuf {
        let script = dir.join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n{env} KEYSTONE_FAKE_CLAUDE_DELAY_MS=5 exec '{}' \"$@\"\n",
                fake("keystone-fake-claude").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script
    }

    fn in_section(cx: &mut gpui::VisualTestContext, row: &'static str, section: Section) -> bool {
        let row = y(cx, row);
        let mut after = |header| cx.debug_bounds(header).map(|b| b.origin.y);
        let (start, end) = match section {
            Section::Attention => (after("attention-section"), after("working-section")),
            Section::Working => (after("working-section"), after("settled-section")),
            Section::Settled => (after("settled-section"), None),
        };
        start.is_some_and(|start| start < row) && end.is_none_or(|end| row < end)
    }

    #[gpui::test]
    fn claude_sessions_move_between_working_and_needs_action(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Claude, 1).unwrap();
        store.create(Provider::Claude, 2).unwrap();
        store.select(1, 3).unwrap();
        let script = claude_script(dir.path(), "");
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        view.update(cx, |this, _| {
            this.claude = Some(ClaudeConnection::new(Some(script), this.wake.clone()))
        });
        let running = |view: &Keystone| view.conversations[&1].running;

        send_text(&view, cx, "a long one");
        assert!(in_section(cx, "session-1", Section::Working));
        wait_until(cx, &view, |view| {
            view.conversations[&1].rows.iter().any(|row| {
                matches!(row, Row::Item(item)
                if item.status == ItemStatus::InProgress && item.text.contains("word1"))
            })
        });
        view.update_in(cx, |this, window, cx| this.stop_turn(1, window, cx));
        wait_until(cx, &view, |view| !running(view));
        // Stopped by the user, and seen as it ended: nothing to act on.
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
        assert!(in_section(cx, "session-1", Section::Working));

        send_text(&view, cx, "please run it");
        wait_until(cx, &view, |view| {
            !view.conversations[&1].approvals.is_empty()
        });
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));
        click(cx, "approve");
        wait_until(cx, &view, |view| !running(view));
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Working));

        // A reply that ends while the user is elsewhere is unread.
        send_text(&view, cx, "hello");
        view.update(cx, |this, cx| this.select(2, cx));
        wait_until(cx, &view, |view| !running(view));
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));
        view.read_with(cx, |this, _| {
            let session = &this.store.workspace().sessions()[0];
            assert_eq!(session.attention(false, false), Some(Attention::Replied));
        });
    }

    #[gpui::test]
    fn a_failed_claude_turn_moves_the_session_to_needs_action(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Claude, 1).unwrap();
        store.create(Provider::Claude, 2).unwrap();
        store.select(1, 3).unwrap();
        let script = claude_script(dir.path(), "KEYSTONE_FAKE_CLAUDE_SIGNED_OUT=1");
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        view.update(cx, |this, _| {
            this.claude = Some(ClaudeConnection::new(Some(script), this.wake.clone()))
        });
        send_text(&view, cx, "hello");
        wait_until(cx, &view, |view| !view.conversations[&1].running);
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));
        assert!(in_section(cx, "session-2", Section::Working));
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
    }

    #[gpui::test]
    fn a_claude_that_fails_to_start_moves_the_session_to_needs_action(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Claude, 1).unwrap();
        store.create(Provider::Claude, 2).unwrap();
        store.select(1, 3).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        let missing = dir.path().join("no-such-claude");
        view.update(cx, |this, _| {
            this.claude = Some(ClaudeConnection::new(Some(missing), this.wake.clone()))
        });
        send_text(&view, cx, "hello");
        wait_until(cx, &view, |view| {
            view.conversations[&1].error.is_some() && !view.conversations[&1].running
        });
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));
        assert!(in_section(cx, "session-2", Section::Working));
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
    }

    #[test]
    fn excerpt_is_the_first_written_line_or_the_provider() {
        assert_eq!(
            excerpt(
                "\n  Fix persistence durability first \nthen the rest",
                Provider::Claude
            ),
            "Fix persistence durability first"
        );
        assert_eq!(excerpt(" \n\n", Provider::Codex), "Codex");
        assert_eq!(excerpt("", Provider::Claude), "Claude Code");
    }

    #[gpui::test]
    fn clicking_create_and_settle_persists_sessions(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        for selector in [
            "new-session",
            "new-claude",
            "new-session",
            "new-codex",
            "settle-2",
        ] {
            let bounds = cx.debug_bounds(selector).expect("control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        let restored = Store::open(&path).unwrap();
        assert_eq!(restored.workspace().selected(), Some(2));
        assert_eq!(
            restored
                .workspace()
                .sessions()
                .iter()
                .map(|s| (s.provider(), s.settled()))
                .collect::<Vec<_>>(),
            vec![(Provider::Claude, false), (Provider::Codex, true)]
        );
        for selector in ["settled-section", "settle-2"] {
            let bounds = cx.debug_bounds(selector).expect("control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        let restored = Store::open(path).unwrap();
        assert_eq!(restored.workspace().selected(), Some(2));
        assert!(!restored.workspace().sessions()[1].settled());
    }

    #[gpui::test]
    fn a_rename_saves_on_enter_and_escape_keeps_the_title(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open_window(&path, cx);
        click(cx, "new-session");
        click(cx, "new-codex");
        let saved_title = || {
            Store::open(&path).unwrap().workspace().sessions()[0]
                .title()
                .to_owned()
        };

        click(cx, "rename-session");
        view.update(cx, |view, cx| {
            assert!(view.renaming.is_some(), "the title opens the rename dialog");
            view.rename_input.update(cx, |input, cx| {
                input.set_value(" Release  notes ".into(), cx)
            });
        });
        cx.simulate_keystrokes("enter");
        assert!(view.read_with(cx, |view, _| view.renaming.is_none()));
        assert_eq!(saved_title(), "Release notes");

        cx.simulate_keystrokes("f2");
        view.update(cx, |view, cx| {
            assert!(view.renaming.is_some(), "F2 opens the rename dialog");
            view.rename_input
                .update(cx, |input, cx| input.set_value("Discarded".into(), cx));
        });
        cx.simulate_keystrokes("escape");
        assert!(view.read_with(cx, |view, _| view.renaming.is_none()));
        assert_eq!(saved_title(), "Release notes");
    }

    #[gpui::test]
    fn a_rename_tabs_through_its_field_and_retries_a_failed_save(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let path = state_dir.join("workspace.json");
        let (view, cx) = open_window(&path, cx);
        click(cx, "new-session");
        click(cx, "new-codex");
        let value = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| view.rename_input.read(cx).value().to_owned())
        };

        cx.simulate_keystrokes("f2");
        cx.simulate_input("New");
        assert_eq!(
            value(cx),
            "New",
            "the field opens focused with its title selected"
        );
        cx.simulate_keystrokes("tab tab tab");
        cx.simulate_input("er");
        assert_eq!(
            value(cx),
            "Newer",
            "Tab cycles Cancel, Rename, and back to the field"
        );

        std::fs::rename(&state_dir, dir.path().join("backup")).unwrap();
        cx.simulate_keystrokes("shift-tab enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.renaming.is_some()));
        assert!(
            cx.debug_bounds("rename-error").is_some(),
            "the dialog shows the error"
        );
        cx.simulate_input("!");
        assert_eq!(
            value(cx),
            "Newer!",
            "the typed title stays, with the field focused"
        );

        std::fs::rename(dir.path().join("backup"), &state_dir).unwrap();
        cx.simulate_keystrokes("enter");
        assert!(view.read_with(cx, |view, _| view.renaming.is_none()));
        let saved = Store::open(&path).unwrap().workspace().sessions()[0]
            .title()
            .to_owned();
        assert_eq!(saved, "Newer!");
    }

    #[gpui::test]
    fn unsaved_draft_survives_failed_navigation_and_retries(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let path = state_dir.join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Claude, 1).unwrap();
        store.create(Provider::Codex, 1).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        std::fs::rename(&state_dir, dir.path().join("backup")).unwrap();
        let bounds = cx.debug_bounds("composer-field").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.simulate_input("Keep my unsaved draft");
        cx.run_until_parked();
        let bounds = cx.debug_bounds("session-1").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.store.workspace().selected(), Some(2));
            assert_eq!(view.composer.read(cx).value(), "Keep my unsaved draft");
            assert!(view.error.as_deref().unwrap().contains("Draft not saved"));
        });
        std::fs::rename(dir.path().join("backup"), &state_dir).unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let restored = Store::open(&path).unwrap();
        assert_eq!(restored.workspace().selected(), Some(1));
        assert_eq!(
            restored.workspace().sessions()[1].draft(),
            "Keep my unsaved draft"
        );
        let bounds = cx.debug_bounds("composer-field").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.update(|_, cx| {
            TextInput::bind_keys(cx);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                "first line\nsecond line".into(),
            ));
        });
        cx.simulate_keystrokes("ctrl-v");
        cx.run_until_parked();
        let bounds = cx.debug_bounds("session-2").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            "first line\nsecond line"
        );
    }
}
/// The app's keyboard shortcuts, shared by `main` and the GPUI tests.
fn bind_keys(cx: &mut App) {
    TextInput::bind_keys(cx);
    customization::bind_keys(cx);
    cx.bind_keys([
        gpui::KeyBinding::new("ctrl-q", Quit, None),
        gpui::KeyBinding::new("ctrl-n", NewClaude, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-shift-n", NewCodex, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-shift-s", ToggleSettle, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-l", FocusComposer, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-f", FocusSearch, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-enter", SendMessage, Some("!Dialog")),
        gpui::KeyBinding::new("f2", RenameSession, Some("!Dialog")),
    ]);
}

fn state_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("KEYSTONE_STATE_PATH") {
        return Ok(path.into());
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or("Set KEYSTONE_STATE_PATH or HOME to select a state location")?;
    Ok(base.join("keystone/workspace.json"))
}

fn main() {
    let path = state_path().unwrap_or_else(|error| {
        eprintln!("Cannot locate Keystone workspace: {error}");
        std::process::exit(1);
    });
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent).expect("create Keystone data directory");
    let state_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))
        .expect("open workspace lock");
    if let Err(error) = state_lock.try_lock() {
        eprintln!("Cannot lock workspace. Another Keystone process may be using it: {error}");
        std::process::exit(1);
    }
    let store = Store::open(path).map_err(|e| e.to_string());
    let store = match store {
        Ok(store) => store,
        Err(error) => {
            eprintln!("Cannot open Keystone workspace: {error}");
            std::process::exit(1);
        }
    };
    gpui_platform::application()
        .with_assets(keystone_ui::Icons)
        .run(move |cx: &mut App| {
            keystone_ui::init(cx);
            bind_keys(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: None,
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| {
                        let view = Keystone::new(
                            store,
                            std::env::var_os("KEYSTONE_VISUAL_FIXTURE").is_some(),
                            window,
                            cx,
                        );
                        view.focus.focus(window, cx);
                        view
                    });
                    let weak = view.downgrade();
                    window.on_window_should_close(cx, move |_, cx| {
                        weak.update(cx, |view, cx| view.save_before_exit(cx))
                            .unwrap_or(true)
                    });
                    view
                },
            )
            .expect("open Keystone window");
            cx.activate(true);
        });
}
