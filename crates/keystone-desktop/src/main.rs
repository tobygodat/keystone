mod conversation;
mod settings;
mod text_input;
mod transcript;

use conversation::{Conversation, Row};
use futures::StreamExt;
use gpui::{
    AnyElement, AnyView, App, Application, AvailableSpace, Bounds, Context, Corner, Entity,
    FocusHandle, Focusable, FontWeight, IntoElement, ListAlignment, ListOffset, ListState, Pixels,
    Render, ScrollHandle, StyleRefinement, Subscription, WeakEntity, Window, WindowBounds,
    WindowOptions, canvas, div, list, prelude::*, px, size,
};
use keystone_agent::log::{Logs, Report};
use keystone_agent::{
    ApprovalState, ClaudeConnection, CodexConnection, Command, Connection, ItemKind, Wake,
};
use keystone_core::{ApprovalMode, Density, Durability, Preferences, Provider, Store, ThemeMode};
use keystone_ui::markdown::Document;
use keystone_ui::{automation, prelude::*};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use text_input::{Changed, InputMode, TextInput};

gpui::actions!(
    keystone,
    [
        Quit,
        NewClaude,
        NewCodex,
        ToggleSettle,
        FocusComposer,
        FocusSearch,
        SendMessage
    ]
);

const CONTENT_WIDTH: f32 = 884.;

/// How long closing waits for saved histories to finish loading.
const LOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The provider's name as people know it.
fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude Code",
        Provider::Codex => "Codex",
    }
}

fn durability_warning(durability: Durability) -> Option<String> {
    match durability {
        Durability::Synced => None,
        Durability::DirectorySyncPending(error) => Some(format!(
            "Change saved, but crash durability is unconfirmed. Retry saving before leaving. {error}"
        )),
    }
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
    settled_only: bool,
    sidebar: bool,
    fixture: bool,
    /// The shared Codex app-server connection, created on the first message.
    codex: Option<CodexConnection>,
    /// Claude Code processes, one per session, created on the first message.
    claude: Option<ClaudeConnection>,
    conversations: HashMap<u64, Conversation>,
    /// Each session's log of completed items, read and written off the UI thread.
    logs: Logs,
    /// The virtualized transcript list for each session shown so far. It holds one
    /// entry per conversation row plus a status entry after the last row.
    transcripts: HashMap<u64, ListState>,
    /// Sessions whose list top row was remeasured while the user was scrolled into it,
    /// to check against the row's new height before the list lays out.
    remeasured_top: HashSet<u64>,
    /// The directory agents work in.
    project_dir: PathBuf,
    wake: Wake,
    /// Scrolls the sample transcript, which is not a list.
    transcript_scroll: ScrollHandle,
    /// Each reply row's parsed Markdown and the hash of the text it was parsed from, so
    /// a row is parsed again only when its text changes, such as while it streams.
    markdown: RefCell<HashMap<(u64, usize), (u64, Arc<Document>)>>,
    /// The sample transcript, parsed once.
    fixture_document: Arc<Document>,
}

impl Keystone {
    fn new(store: Store, fixture: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| TextInput::new("Write a draft…", InputMode::Multiline, cx));
        let search =
            cx.new(|cx| TextInput::new("Search conversations…", InputMode::SingleLine, cx));
        let subscriptions = vec![
            cx.subscribe(&composer, |this, _, _: &Changed, cx| {
                this.draft_save_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        this.save_draft(cx);
                    });
                }));
                cx.notify();
            }),
            cx.subscribe(&search, |this, _, _: &Changed, cx| this.refresh(cx)),
            cx.observe_window_appearance(window, |this, window, cx| this.apply_theme(window, cx)),
        ];
        let app = cx.weak_entity();
        let session_sidebar = cx.new(|_| SessionSidebar { app });
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
            settled_only: false,
            sidebar: true,
            fixture,
            codex: None,
            claude: None,
            conversations: HashMap::new(),
            logs: Logs::new(sessions_dir, wake.clone()),
            transcripts: HashMap::new(),
            remeasured_top: HashSet::new(),
            project_dir,
            wake,
            transcript_scroll: ScrollHandle::new(),
            markdown: RefCell::default(),
            fixture_document: Arc::new(Document::parse(if fixture {
                include_str!("fixture.md")
            } else {
                ""
            })),
        };
        result.sync_draft(cx);
        result.apply_theme(window, cx);
        result
    }

    fn apply_theme(&self, window: &Window, cx: &mut App) {
        keystone_ui::set_theme(
            settings::theme_for(self.store.workspace().preferences(), window, cx),
            cx,
        );
    }

    fn update_preferences(
        &mut self,
        change: impl FnOnce(&mut Preferences),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut preferences = self.store.workspace().preferences().clone();
        change(&mut preferences);
        self.error = match self.store.set_preferences(preferences) {
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

    fn create(&mut self, provider: Provider, cx: &mut Context<Self>) {
        if !self.save_draft(cx) {
            return;
        }
        self.error = match self.store.create(provider) {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(error.to_string()),
        };
        self.new_menu = false;
        self.settled_only = false;
        self.sync_draft(cx);
        self.refresh(cx);
    }

    fn toggle_settle(&mut self, cx: &mut Context<Self>) {
        if !self.save_draft(cx) {
            return;
        }
        if let Some(session) = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| Some(s.id()) == self.store.workspace().selected())
        {
            self.error = match self.store.set_settled(session.id(), !session.settled()) {
                Ok(applied) => durability_warning(applied.durability),
                Err(error) => Some(error.to_string()),
            };
        }
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

    /// The parsed Markdown of reply row `index`, reused while its text is unchanged.
    fn parsed(&self, session: u64, index: usize, text: &str) -> Arc<Document> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();
        let mut cache = self.markdown.borrow_mut();
        match cache.get(&(session, index)) {
            Some((cached, document)) if *cached == hash => document.clone(),
            _ => {
                let document = Arc::new(Document::parse(text));
                cache.insert((session, index), (hash, document.clone()));
                document
            }
        }
    }

    fn select(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.save_draft(cx) {
            return;
        }
        self.error = match self.store.select(id) {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(error.to_string()),
        };
        self.sync_draft(cx);
        self.scroll_to_end(id);
        self.refresh(cx);
    }

    fn start(&mut self, provider: Provider, window: &mut Window, cx: &mut Context<Self>) {
        self.create(provider, cx);
        if self.error.is_none() && self.store.workspace().selected().is_some() {
            self.composer.focus_handle(cx).focus(window);
        }
    }

    fn titlebar(&self, title: String, cx: &mut Context<Self>) -> impl IntoElement {
        let (tab_bg, density) = (cx.theme().colors.element_selected, cx.theme().density);
        h_flex()
            .h(density.control(px(38.)))
            .flex_shrink_0()
            .pr_1()
            .hairline_b(cx)
            .child(
                div()
                    .w(px(if self.sidebar { 302. } else { 46. }))
                    .flex_shrink_0(),
            )
            .child(
                h_flex()
                    .h(px(28.))
                    .w(px(220.))
                    .px_2p5()
                    .gap_2()
                    .rounded(radius::CONTROL)
                    .bg(tab_bg)
                    .child(Icon::new(IconName::Chat).size(px(14.)))
                    .child(Label::new(title).truncate()),
            )
            .child(
                div().flex_1().flex().justify_center().child(
                    Label::new("keystone")
                        .size(TextSize::Sm)
                        .color(Color::Faint),
                ),
            )
            .child(
                IconButton::new("minimize", IconName::Minus)
                    .tooltip(Tooltip::text("Minimize"))
                    .on_click(|_, window, _| window.minimize_window()),
            )
            .child(
                IconButton::new("maximize", IconName::Square)
                    .tooltip(Tooltip::text("Maximize"))
                    .on_click(|_, window, _| window.zoom_window()),
            )
            .child(
                IconButton::new("close", IconName::Close)
                    .tooltip(Tooltip::with_key("Close window", "Ctrl+Q"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.save_draft(cx) {
                            window.remove_window();
                        }
                    })),
            )
    }

    fn rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rail_bg = cx.theme().colors.rail;
        v_flex()
            .w(px(46.))
            .flex_shrink_0()
            .h_full()
            .items_center()
            .py_2()
            .gap_1()
            .bg(rail_bg)
            .hairline_r(cx)
            .child(
                IconButton::new("sidebar", IconName::Panel)
                    .debug_selector(|| "rail-sidebar".into())
                    .size(ButtonSize::Large)
                    .tooltip(Tooltip::text(if self.sidebar {
                        "Hide sidebar"
                    } else {
                        "Show sidebar"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar = !this.sidebar;
                        this.refresh(cx);
                    })),
            )
            .child(
                IconButton::new("conversations", IconName::Chat)
                    .debug_selector(|| "rail-conversations".into())
                    .size(ButtonSize::Large)
                    .selected(!self.settled_only)
                    .tooltip(Tooltip::text("Conversations"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settled_only = false;
                        this.sidebar = true;
                        this.refresh(cx);
                    })),
            )
            .child(
                IconButton::new("search", IconName::Search)
                    .debug_selector(|| "rail-search".into())
                    .size(ButtonSize::Large)
                    .tooltip(Tooltip::with_key("Search", "Ctrl+F"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.sidebar = true;
                        this.search.focus_handle(cx).focus(window);
                        this.refresh(cx);
                    })),
            )
            .child(
                IconButton::new("settled", IconName::Inbox)
                    .debug_selector(|| "rail-settled".into())
                    .size(ButtonSize::Large)
                    .selected(self.settled_only)
                    .tooltip(Tooltip::text("Settled sessions"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settled_only = true;
                        this.sidebar = true;
                        this.refresh(cx);
                    })),
            )
            .child(div().flex_1())
            .child(self.settings_menu(cx))
    }

    fn settings_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let preferences = self.store.workspace().preferences().clone();
        let fonts = if self.settings_menu {
            settings::font_choices(&preferences, cx)
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
                let checked = font == preferences.ui_font;
                let label = font.clone().unwrap_or_else(|| "Default".into());
                MenuItem::new(("font", index), label)
                    .checked(checked)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let font = font.clone();
                        this.update_preferences(|p| p.ui_font = font, window, cx)
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
        .anchor(Corner::BottomLeft)
        .open(self.settings_menu)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.settings_menu = *open;
            cx.notify();
        }))
        .child(MenuHeader::new("Theme"))
        .children(themes.map(|(mode, label, selector)| {
            MenuItem::new(selector, label)
                .checked(preferences.theme == mode)
                .debug_selector(|| selector.into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_preferences(|p| p.theme = mode, window, cx)
                }))
        }))
        .child(MenuSeparator)
        .child(MenuHeader::new("Density"))
        .children(densities.map(|(density, label, selector)| {
            MenuItem::new(selector, label)
                .checked(preferences.density == density)
                .debug_selector(|| selector.into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_preferences(|p| p.density = density, window, cx)
                }))
        }))
        .child(MenuSeparator)
        .child(MenuHeader::new("Font"))
        .children(font_items)
    }

    fn composer(&self, current: Option<(Provider, bool)>, cx: &mut Context<Self>) -> gpui::Div {
        let colors = cx.theme().colors;
        let has_session = current.is_some();
        let conversation = self
            .store
            .workspace()
            .selected()
            .and_then(|id| self.conversations.get(&id));
        let running = conversation.is_some_and(|c| c.running);
        let waiting = conversation.is_some_and(|c| !c.approvals.is_empty());
        let mode = self.selected_session().map(|s| s.approval_mode());
        let status = match current {
            None => "Choose a provider to start",
            Some(_) if waiting => "Waiting for your approval",
            Some(_) if running => "Working · Ctrl+Enter steers this turn",
            Some(_) => "Ctrl+Enter to send · Enter for a new line",
        };
        let project = self.project_dir.file_name().map_or_else(
            || self.project_dir.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        div().w_full().max_w(px(CONTENT_WIDTH)).px_3().child(
            v_flex()
                .surface(Elevation::Surface, cx)
                .border_color(colors.border_strong)
                .p_3()
                .gap_3()
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(IconName::Folder).size(px(14.)))
                        .child(
                            div()
                                .id("project")
                                .automation_id("project")
                                .child(Label::new(project).size(TextSize::Ui).color(Color::Muted))
                                .tooltip(
                                    Tooltip::text(self.project_dir.display().to_string()).builder(),
                                ),
                        )
                        .child(div().flex_1())
                        .when_some(current.as_ref(), |d, (_, settled)| {
                            d.child(
                                Button::new(
                                    "settle",
                                    if *settled { "Un-settle" } else { "Settle" },
                                )
                                .size(ButtonSize::Compact)
                                .icon(IconName::Check)
                                .selected(*settled)
                                .debug_selector(|| "settle".into())
                                .tooltip(Tooltip::with_key(
                                    if *settled {
                                        "Return to active work"
                                    } else {
                                        "Move out of active work"
                                    },
                                    "Ctrl+Shift+S",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_settle(cx))),
                            )
                        }),
                )
                .child(
                    div()
                        .id("composer-field")
                        .automation_id("composer-field")
                        .min_h(px(28.))
                        .when(has_session, |d| d.child(self.composer.clone()))
                        .when(!has_session, |d| {
                            d.text_color(colors.text_placeholder)
                                .child("Choose a provider to write a draft…")
                        }),
                )
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .when_some(current.as_ref(), |d, (provider, _)| {
                            d.child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(radius::CONTROL)
                                    .bg(colors.element_hover)
                                    .child(Label::new(provider.to_string()).size(TextSize::Sm)),
                            )
                        })
                        .when_some(mode, |d, mode| {
                            d.child(self.approval_menu(mode, running, cx))
                        })
                        .child(Label::new(status).size(TextSize::Sm).color(Color::Muted))
                        .child(div().flex_1())
                        .when(has_session && running, |d| {
                            d.child(
                                Button::new("stop", "Stop")
                                    .style(ButtonStyle::Subtle)
                                    .icon(IconName::Square)
                                    .debug_selector(|| "stop".into())
                                    .tooltip(Tooltip::text("Stop this turn"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.stop_turn(window, cx)
                                    })),
                            )
                        })
                        .when_some(current.as_ref(), |d, (provider, _)| {
                            let send_tip = format!("Send to {}", provider_name(*provider));
                            d.child(
                                Button::new("send", if running { "Steer" } else { "Send" })
                                    .style(ButtonStyle::Filled)
                                    .icon(IconName::Up)
                                    .debug_selector(|| "send".into())
                                    .tooltip(Tooltip::with_key(
                                        if running {
                                            "Send to the running turn".to_owned()
                                        } else {
                                            send_tip
                                        },
                                        "Ctrl+Enter",
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send_message(window, cx)
                                    })),
                            )
                        })
                        .when(!has_session, |d| {
                            d.child(
                                Button::new("welcome-claude", "Start with Claude Code")
                                    .style(ButtonStyle::Filled)
                                    .icon(IconName::Plus)
                                    .debug_selector(|| "welcome-claude".into())
                                    .tooltip(Tooltip::with_key("New Claude session", "Ctrl+N"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start(Provider::Claude, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("welcome-codex", "Start with Codex")
                                    .style(ButtonStyle::Filled)
                                    .icon(IconName::Plus)
                                    .debug_selector(|| "welcome-codex".into())
                                    .tooltip(Tooltip::with_key("New Codex session", "Ctrl+Shift+N"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start(Provider::Codex, window, cx)
                                    })),
                            )
                        }),
                ),
        )
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
        .anchor(Corner::BottomLeft)
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
            self.composer.focus_handle(cx).focus(window);
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
            this.composer.focus_handle(cx).focus(window);
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
        self.composer.focus_handle(cx).focus(window);
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
        let count = conversation.rows.len() + 1;
        if let Some(list) = self.transcripts.get(&session) {
            // Rows are only appended or replaced in place, so every row the list
            // already shows keeps its index; the status entry is always last.
            let old = list.item_count();
            let status = old.saturating_sub(1).min(count - 1);
            // A splice moves the view to the start of a remeasured entry. Unless it
            // is following the bottom, which reads as the end of the list, keep it on
            // the same entry at the same offset.
            let top = list.logical_scroll_top();
            let scrolled = top.item_ix < old;
            let mut remeasured = top.item_ix >= status;
            for range in changed {
                remeasured |= range.contains(&top.item_ix);
                let range = range.start..range.end.min(status);
                if !range.is_empty() {
                    list.splice(range.clone(), range.len());
                }
            }
            // The status entry changes with almost anything, so it is always remeasured.
            list.splice(status..old, count - status);
            if scrolled {
                // Rows added before the status entry move it down; follow it.
                let item_ix = if top.item_ix == status {
                    count - 1
                } else {
                    top.item_ix
                };
                list.scroll_to(ListOffset { item_ix, ..top });
                if remeasured && top.offset_in_item > px(0.) {
                    self.remeasured_top.insert(session);
                }
            }
        }
    }

    /// Runs before the transcript list lays out. When the row the user is scrolled
    /// into was remeasured, the kept offset can now fall past its end, such as when a
    /// long reasoning summary is replaced by an empty one, or when the transcript
    /// widens and its text wraps onto fewer lines. The view would then skip the rows
    /// below it, so it moves to the first row after the place it was. It spans the
    /// transcript's width so it can measure the row at the width the list is about to
    /// use.
    fn transcript_top_check(&self, session: u64, cx: &mut Context<Self>) -> impl IntoElement {
        let app = cx.weak_entity();
        canvas(
            move |bounds, window, cx| {
                let _ = app.update(cx, |this, cx| {
                    this.clamp_top(session, bounds.size.width, window, cx)
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
        session: u64,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let remeasured = self.remeasured_top.remove(&session);
        let Some(list) = self.transcripts.get(&session).cloned() else {
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
                .transcript_entry(session, top.item_ix, cx)
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

    /// Scrolls the session's transcript to its latest row. It then follows new rows
    /// until the user scrolls up.
    fn scroll_to_end(&self, session: u64) {
        if let Some(list) = self.transcripts.get(&session) {
            list.scroll_to(ListOffset {
                item_ix: list.item_count(),
                offset_in_item: px(0.),
            });
        }
    }

    /// Applies loaded history, save results, and every provider event that arrived
    /// since the last wake-up.
    fn pump(&mut self, cx: &mut Context<Self>) {
        let reports = self.logs.drain();
        let mut events = self.codex.as_ref().map(|c| c.drain()).unwrap_or_default();
        events.extend(self.claude.as_ref().map(|c| c.drain()).unwrap_or_default());
        if reports.is_empty() && events.is_empty() {
            return;
        }
        for report in reports {
            let session = match report {
                Report::Loaded { session, entries } => {
                    if let Some(conversation) = self.conversations.get_mut(&session) {
                        conversation.restore(entries);
                    }
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
        cx.notify();
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

    /// Sends the composer text: a new turn, or a steer for the running one.
    fn send_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.store.workspace().selected().and_then(|id| {
            self.store
                .workspace()
                .sessions()
                .iter()
                .find(|s| s.id() == id)
                .cloned()
        }) else {
            return;
        };
        let text = self.composer.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let id = session.id();
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
        self.composer
            .update(cx, |input, cx| input.set_value(String::new(), cx));
        self.save_draft(cx);
        self.scroll_to_end(id);
        // Clicking Send moves focus off the composer; put it back for the next message.
        self.composer.focus_handle(cx).focus(window);
        cx.notify();
    }

    fn stop_turn(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.focus_handle(cx).focus(window);
        let Some(id) = self.store.workspace().selected() else {
            return;
        };
        if let Some(connection) = self.provider(id).and_then(|p| self.started(p)) {
            connection.command(Command::Interrupt { session: id });
        }
        cx.notify();
    }

    fn answer(&mut self, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        // The buttons take focus from the composer; give it back for the next message.
        self.composer.focus_handle(cx).focus(window);
        let Some(id) = self.store.workspace().selected() else {
            return;
        };
        let Some(approval) = self.conversation(id).decide(allow) else {
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
        cx.notify();
    }

    /// The session's transcript: a virtualized list, so a long session lays out and
    /// paints only the rows in view. It sticks to the bottom while new rows arrive,
    /// unless the user has scrolled up.
    fn transcript_list(&mut self, session: u64, cx: &mut Context<Self>) -> gpui::List {
        let count = self.conversations.get(&session).map_or(0, |c| c.rows.len()) + 1;
        let state = self
            .transcripts
            .entry(session)
            .or_insert_with(|| ListState::new(count, ListAlignment::Bottom, px(600.)))
            .clone();
        let app = cx.weak_entity();
        list(state, move |index, _, cx| {
            app.update(cx, |this, cx| this.transcript_entry(session, index, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .size_full()
        .py_6()
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
                        Some(self.parsed(session, index, &item.text))
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
                    .px_3()
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
            let name = self.provider(session).map_or("The provider", provider_name);
            view = view.child(
                v_flex()
                    .id("approval")
                    .automation_id("approval")
                    .surface(Elevation::Raised, cx)
                    .border_color(colors.border_focused)
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
                                    .style(ButtonStyle::Filled)
                                    .icon(IconName::Check)
                                    .debug_selector(|| "approve".into())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.answer(true, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("deny", "Deny")
                                    .style(ButtonStyle::Subtle)
                                    .icon(IconName::Close)
                                    .debug_selector(|| "deny".into())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.answer(false, window, cx)
                                    })),
                            ),
                    ),
            );
        } else if conversation.running {
            empty = false;
            view = view.child(
                div().id("turn-status").automation_id("turn-status").child(
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
/// [`Keystone::refresh`] notifies it whenever the sessions, filters, or menu change.
struct SessionSidebar {
    app: WeakEntity<Keystone>,
}

struct SessionRow {
    id: u64,
    title: String,
    detail: String,
}

impl Render for SessionSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else {
            return div();
        };
        let this = app.read(cx);
        let workspace = this.store.workspace();
        let selected = workspace.selected();
        let query = this.search.read(cx).value().to_lowercase();
        let rows = workspace
            .sessions()
            .iter()
            .filter(|s| {
                (!this.settled_only || s.settled())
                    && format!("{} {}", s.title(), s.provider())
                        .to_lowercase()
                        .contains(&query)
            })
            .map(|session| SessionRow {
                id: session.id(),
                title: session.title().to_owned(),
                detail: format!(
                    "{} · {}",
                    session.provider(),
                    if session.settled() {
                        "Settled"
                    } else {
                        "Draft"
                    }
                ),
            })
            .collect::<Vec<_>>();
        let empty = match (rows.is_empty(), workspace.sessions().is_empty()) {
            (false, _) => None,
            (true, true) => Some("No sessions yet. Press + to start one."),
            (true, false) => Some("No sessions match these filters"),
        };
        let (settled_only, new_menu, search) =
            (this.settled_only, this.new_menu, this.search.clone());
        let density = cx.theme().density;
        let listener = |f: fn(&mut Keystone, &mut Window, &mut Context<Keystone>)| {
            let app = app.downgrade();
            move |_: &_, window: &mut Window, cx: &mut App| {
                let _ = app.update(cx, |this, cx| f(this, window, cx));
            }
        };
        let sessions = rows.into_iter().map(|row| {
            let SessionRow { id, title, detail } = row;
            let app = app.downgrade();
            ListItem::new(("session", id))
                .debug_selector(move || format!("session-{id}"))
                .selected(Some(id) == selected)
                .on_click(move |_, _, cx| {
                    let _ = app.update(cx, |this, cx| this.select(id, cx));
                })
                .child(Label::new(title).truncate())
                .child(Label::new(detail).size(TextSize::Sm).color(Color::Muted))
        });
        let set_menu = {
            let app = app.downgrade();
            move |open: &bool, _: &mut Window, cx: &mut App| {
                let open = *open;
                let _ = app.update(cx, |this, cx| {
                    this.new_menu = open;
                    this.refresh(cx);
                });
            }
        };
        v_flex()
            .size_full()
            .hairline_r(cx)
            .child(
                h_flex()
                    .h(density.control(px(40.)))
                    .pl_3()
                    .pr_1p5()
                    .hairline_b(cx)
                    .child(
                        div().flex_1().child(
                            Label::new(if settled_only {
                                "Settled sessions"
                            } else {
                                "Workspace"
                            })
                            .weight(FontWeight::MEDIUM),
                        ),
                    )
                    .child(
                        PopoverMenu::new(
                            "new-session-menu",
                            IconButton::new("new-session", IconName::Plus)
                                .debug_selector(|| "new-session".into())
                                .selected(new_menu)
                                .tooltip(Tooltip::text("New session")),
                        )
                        .anchor(Corner::TopRight)
                        .open(new_menu)
                        .on_open_change(set_menu)
                        .child(
                            MenuItem::new("new-claude", "New Claude session")
                                .icon(IconName::Chat)
                                .kbd("Ctrl+N")
                                .debug_selector(|| "new-claude".into())
                                .on_click(listener(|this, window, cx| {
                                    this.start(Provider::Claude, window, cx)
                                })),
                        )
                        .child(
                            MenuItem::new("new-codex", "New Codex session")
                                .icon(IconName::Chat)
                                .kbd("Ctrl+Shift+N")
                                .debug_selector(|| "new-codex".into())
                                .on_click(listener(|this, window, cx| {
                                    this.start(Provider::Codex, window, cx)
                                })),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .h(density.control(px(36.)))
                    .pl_3()
                    .pr_1p5()
                    .gap_2()
                    .child(Icon::new(IconName::Search).size(px(14.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .automation_id("search-field")
                            .child(search),
                    )
                    .child(
                        IconButton::new("filter", IconName::Filter)
                            .debug_selector(|| "filter".into())
                            .size(ButtonSize::Compact)
                            .selected(settled_only)
                            .tooltip(Tooltip::text("Show settled only"))
                            .on_click(listener(|this, _, cx| {
                                this.settled_only = !this.settled_only;
                                this.refresh(cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("session-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_1p5()
                    .pt_1()
                    .gap(px(2.))
                    .when_some(empty, |d, message| {
                        d.child(
                            div()
                                .px_2()
                                .py_2()
                                .child(Label::new(message).size(TextSize::Ui).color(Color::Muted)),
                        )
                    })
                    .children(sessions),
            )
    }
}

impl Render for Keystone {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, text, ui_font, density) = (
            theme.colors.background,
            theme.colors.text,
            theme.fonts.ui.clone(),
            theme.density,
        );
        let current = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| Some(s.id()) == self.store.workspace().selected());
        let title = current
            .map(|s| s.title().to_owned())
            .unwrap_or_else(|| "Workspace".into());
        let selected = current.map(|s| s.id());
        // A session that has reached its provider has history, so while that history
        // loads, the transcript layout is shown rather than the welcome screen.
        let has_thread = current.is_some_and(|s| s.provider_thread().is_some());
        let current = current.map(|s| (s.provider(), s.settled()));
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
        let main = if self.fixture || has_conversation {
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .items_center()
                .child(match selected.filter(|_| !self.fixture) {
                    Some(id) => div()
                        .id("conversation")
                        .automation_id("conversation")
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .child(self.transcript_top_check(id, cx))
                        .child(self.transcript_list(id, cx)),
                    None => v_flex()
                        .id("conversation")
                        .automation_id("conversation")
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.transcript_scroll)
                        .items_center()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(CONTENT_WIDTH))
                                .px_3()
                                .py_6()
                                .child(self.transcript()),
                        ),
                })
                .when_some(self.error_line(), |d, error| {
                    d.child(
                        div()
                            .w_full()
                            .max_w(px(CONTENT_WIDTH))
                            .px_4()
                            .pb_2()
                            .child(error),
                    )
                })
                .child(composer.pb_2())
        } else {
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .items_center()
                .justify_center()
                .child(
                    v_flex()
                        .w_full()
                        .max_w(px(CONTENT_WIDTH))
                        .gap_5()
                        .child(
                            div()
                                .id("welcome-heading")
                                .automation_id("welcome-heading")
                                .px_3()
                                .child(Label::new("What should we work on?").size(TextSize::Title)),
                        )
                        .child(composer)
                        .when_some(self.error_line(), |d, error| {
                            d.child(div().px_4().child(error))
                        }),
                )
        };
        v_flex()
            .size_full()
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
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && (this.new_menu || this.settings_menu || this.approval_menu)
                {
                    this.new_menu = false;
                    this.settings_menu = false;
                    this.approval_menu = false;
                    this.refresh(cx);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &Quit, _, cx| {
                if this.save_draft(cx) {
                    this.flush_logs(cx);
                    cx.quit();
                }
            }))
            .on_action(cx.listener(|this, _: &NewClaude, window, cx| {
                this.start(Provider::Claude, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &NewCodex, window, cx| {
                    this.start(Provider::Codex, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleSettle, _, cx| this.toggle_settle(cx)))
            .on_action(cx.listener(|this, _: &FocusComposer, window, cx| {
                this.composer.focus_handle(cx).focus(window)
            }))
            .on_action(
                cx.listener(|this, _: &SendMessage, window, cx| this.send_message(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.sidebar = true;
                this.search.focus_handle(cx).focus(window);
                this.refresh(cx);
            }))
            .child(self.titlebar(title, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.rail(cx))
                    .when(self.sidebar, |d| {
                        let sidebar = AnyView::from(self.session_sidebar.clone());
                        let style = StyleRefinement::default()
                            .w(px(256.))
                            .flex_shrink_0()
                            .h_full();
                        // A reused cached view skips prepaint, so it would report no
                        // element geometry. Verification trades that speed for names.
                        if automation::enabled() {
                            d.child(div().w(px(256.)).flex_shrink_0().h_full().child(sidebar))
                        } else {
                            d.child(sidebar.cached(style))
                        }
                    })
                    .child(main),
            )
            .child(
                h_flex()
                    .h(density.control(px(26.)))
                    .flex_shrink_0()
                    .px_3()
                    .hairline_t(cx)
                    .child(
                        Label::new("Local workspace")
                            .size(TextSize::Xs)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new("Claude Code + Codex · preview")
                            .size(TextSize::Xs)
                            .color(Color::Muted),
                    ),
            )
            .children(self.full_access_dialog(cx))
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod tests {
    use super::*;
    use conversation::Row;
    use keystone_agent::{Event, Item, ItemKind, ItemStatus, TurnStatus};

    #[gpui::test]
    fn welcome_action_focuses_composer_and_preserves_draft_on_session_switch(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
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
            store.create(provider).unwrap();
        }
        cx.update(keystone_ui::init);
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
                    window.draw(cx).clear();
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
        store.create(Provider::Codex).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 1000);
        cx.update(keystone_ui::init);
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
                window.draw(cx).clear();
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
                window.draw(cx).clear();
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
        store.create(Provider::Codex).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
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
            this.transcripts[&1].scroll_to(ListOffset::default());
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
            this.scroll_to_end(1);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("transcript-row-601").is_some());
    }

    #[gpui::test]
    fn streaming_keeps_the_place_of_a_reader_who_scrolled_up(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Codex).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
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
        store.create(Provider::Codex).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
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
            this.transcripts[&1].scroll_to(top);
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
            this.scroll_to_end(1);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_resize(size(px(900.), px(700.)));
        cx.run_until_parked();
        view.update(cx, |this, _| {
            let list = &this.transcripts[&1];
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
        store.create(Provider::Codex).unwrap();
        // Selected, so session 1's history is not loaded until it is used below.
        store.create(Provider::Codex).unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
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
        store.create(Provider::Codex).unwrap();
        store.create(Provider::Codex).unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
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
        store.create(Provider::Codex).unwrap();
        // A file where the sessions directory belongs makes every write fail.
        let sessions = dir.path().join("sessions");
        std::fs::write(&sessions, "").unwrap();
        cx.update(keystone_ui::init);
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
    fn cached_sidebar_follows_search_settlement_and_filters(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(Provider::Claude).unwrap();
        store.create(Provider::Codex).unwrap();
        cx.update(keystone_ui::init);
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
            this.composer.focus_handle(cx).focus(window)
        });
        cx.simulate_input("Draft text");
        cx.run_until_parked();
        assert_eq!(second(cx), second_row);

        view.update_in(cx, |this, window, cx| {
            this.search.focus_handle(cx).focus(window)
        });
        cx.simulate_input("codex");
        cx.run_until_parked();
        assert_eq!(second(cx), first_row);

        view.update_in(cx, |this, _, cx| {
            this.search
                .update(cx, |input, cx| input.set_value(String::new(), cx))
        });
        cx.run_until_parked();
        assert_eq!(second(cx), second_row);

        for selector in ["settle", "filter"] {
            let bounds = cx.debug_bounds(selector).expect("control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        assert_eq!(second(cx), first_row);
    }

    #[gpui::test]
    fn shortcut_settlement_invalidates_cached_sidebar(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            let id = store.create(provider).unwrap().value;
            store.set_settled(id, true).unwrap();
        }
        store.select(1).unwrap();
        cx.update(keystone_ui::init);
        let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        view.update_in(cx, |this, window, cx| {
            this.focus.focus(window);
            this.settled_only = true;
            this.refresh(cx);
        });
        cx.run_until_parked();
        let first_row = cx.debug_bounds("session-1").unwrap().origin;

        // No focus change or pointer event re-renders the sidebar here, only the refresh.
        cx.dispatch_action(ToggleSettle);
        cx.run_until_parked();
        assert_eq!(cx.debug_bounds("session-2").unwrap().origin, first_row);
    }

    #[gpui::test]
    fn settings_menu_applies_and_persists_appearance(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
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
        view.update(cx, |view, cx| {
            view.composer
                .update(cx, |input, cx| input.set_value(text.into(), cx))
        });
        cx.simulate_keystrokes("ctrl-enter");
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
            let cache = view.markdown.borrow();
            let (_, document) = &cache[&(1, reply)];
            assert!(!document.is_empty());
        });
        click(cx, format!("copy-reply-{reply}").leak());
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        assert!(copied.unwrap().contains("**Approvals:**"));
        view.update_in(cx, |view, window, cx| {
            view.composer.focus_handle(cx).focus(window)
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

    #[gpui::test]
    fn clicking_create_and_settle_persists_sessions(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        let (_view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
        for selector in [
            "new-session",
            "new-claude",
            "new-session",
            "new-codex",
            "settle",
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
        let bounds = cx.debug_bounds("settle").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(!Store::open(path).unwrap().workspace().sessions()[1].settled());
    }

    #[gpui::test]
    fn unsaved_draft_survives_failed_navigation_and_retries(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let path = state_dir.join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store.create(Provider::Claude).unwrap();
        store.create(Provider::Codex).unwrap();
        cx.update(keystone_ui::init);
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
    cx.bind_keys([
        gpui::KeyBinding::new("ctrl-q", Quit, None),
        gpui::KeyBinding::new("ctrl-n", NewClaude, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-shift-n", NewCodex, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-shift-s", ToggleSettle, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-l", FocusComposer, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-f", FocusSearch, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-enter", SendMessage, Some("!Dialog")),
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
    Application::new()
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
                        view.focus.focus(window);
                        view
                    });
                    let weak = view.downgrade();
                    window.on_window_should_close(cx, move |_, cx| {
                        weak.update(cx, |view, cx| {
                            let saved = view.save_draft(cx);
                            view.flush_logs(cx);
                            saved
                        })
                        .unwrap_or(true)
                    });
                    view
                },
            )
            .expect("open Keystone window");
            cx.activate(true);
        });
}
