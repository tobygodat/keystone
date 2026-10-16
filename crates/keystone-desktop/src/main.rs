mod conversation;
mod settings;
mod text_input;
mod transcript;

use conversation::Conversation;
use futures::StreamExt;
use gpui::{
    AnyElement, AnyView, App, Application, Bounds, Context, Corner, Entity, FocusHandle, Focusable,
    FontWeight, IntoElement, ListAlignment, ListOffset, ListState, Render, ScrollHandle,
    StyleRefinement, Subscription, WeakEntity, Window, WindowBounds, WindowOptions, div, list,
    prelude::*, px, size,
};
use keystone_agent::codex::Command;
use keystone_agent::log::{Logs, Report};
use keystone_agent::{CodexConnection, Wake};
use keystone_core::{Density, Durability, Preferences, Provider, Store, ThemeMode};
use keystone_ui::{automation, prelude::*};
use std::collections::HashMap;
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
    settled_only: bool,
    sidebar: bool,
    fixture: bool,
    /// The shared Codex app-server connection, created on the first message.
    codex: Option<CodexConnection>,
    conversations: HashMap<u64, Conversation>,
    /// Each session's log of completed items, read and written off the UI thread.
    logs: Logs,
    /// The virtualized transcript list for each session shown so far. It holds one
    /// entry per conversation row plus a status entry after the last row.
    transcripts: HashMap<u64, ListState>,
    /// The directory agents work in.
    project_dir: PathBuf,
    wake: Wake,
    /// Scrolls the sample transcript, which is not a list.
    transcript_scroll: ScrollHandle,
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
            settled_only: false,
            sidebar: true,
            fixture,
            codex: None,
            conversations: HashMap::new(),
            logs: Logs::new(sessions_dir, wake.clone()),
            transcripts: HashMap::new(),
            project_dir,
            wake,
            transcript_scroll: ScrollHandle::new(),
        };
        result.sync_draft(cx);
        result.apply_theme(window, cx);
        result
    }

    fn apply_theme(&self, window: &Window, cx: &mut App) {
        keystone_ui::set_theme(
            settings::theme_for(self.store.workspace().preferences(), window),
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

    fn transcript(&self, cx: &App) -> gpui::Div {
        let mut table = v_flex().surface(Elevation::Raised, cx).overflow_hidden();
        for (index, cells) in [
            ["Check", "Command", "When"],
            [
                "Core behavior",
                "bash scripts/check.sh core",
                "After persistence changes",
            ],
            [
                "GPUI interactions",
                "bash scripts/check.sh ui",
                "After UI behavior changes",
            ],
            [
                "Native build",
                "bash scripts/check.sh build",
                "Before desktop verification",
            ],
            [
                "Native capture",
                "python scripts/native-smoke.py",
                "Check the real window",
            ],
        ]
        .into_iter()
        .enumerate()
        {
            table = table.child(
                h_flex()
                    .py_2()
                    .px_3()
                    .when(index > 0, |d| d.hairline_t(cx))
                    .children(cells.into_iter().enumerate().map(|(i, cell)| {
                        let label = if index == 0 {
                            Label::new(cell).size(TextSize::Ui).color(Color::Muted)
                        } else if i == 1 {
                            Label::new(cell).size(TextSize::Sm).mono()
                        } else {
                            Label::new(cell).size(TextSize::Ui)
                        };
                        div().flex_1().child(label)
                    })),
            );
        }
        let code = v_flex()
            .surface(Elevation::Raised, cx)
            .overflow_hidden()
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .hairline_b(cx)
                    .child(Icon::new(IconName::File).size(px(14.)))
                    .child(Label::new("bash").size(TextSize::Ui).color(Color::Muted)),
            )
            .child(
                div().p_4().child(
                    Label::new("cargo run -p keystone-desktop --locked")
                        .size(TextSize::Ui)
                        .color(Color::Custom(cx.theme().colors.code_text))
                        .mono(),
                ),
            );
        v_flex().gap_5().text_size(TextSize::Body.px()).line_height(px(24.))
            .child(Label::new("Interface preview · sample transcript").size(TextSize::Ui).color(Color::Muted))
            .child(Label::new("A local workspace for Claude Code and Codex.").size(TextSize::Body).weight(FontWeight::SEMIBOLD))
            .child("This sample shows the conversation layout. The current preview saves sessions and drafts locally. Agent execution is not connected yet.")
            .child("The testing loop is ready.")
            .child(table)
            .child("Use behavior-focused integration tests for rapid iteration, plus a small native E2E suite. Keep live provider tests separate from deterministic UI checks.")
            .child("The workspace stays focused on conversations. Session selection, draft editing, search, and settle controls are available in this preview.")
            .child("Run the preview with the command below.")
            .child(code)
            .child("This is a visual fixture, not an agent response. No model request is made.")
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
        let codex = matches!(current, Some((Provider::Codex, _)));
        let status = match current {
            None => "Choose a provider to start",
            Some((Provider::Claude, _)) => {
                "Claude Code is not connected yet · Enter for a new line"
            }
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
                        .child(Label::new(status).size(TextSize::Sm).color(Color::Muted))
                        .child(div().flex_1())
                        .when(codex && running, |d| {
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
                        .when(codex, |d| {
                            d.child(
                                Button::new("send", if running { "Steer" } else { "Send" })
                                    .style(ButtonStyle::Filled)
                                    .icon(IconName::Up)
                                    .debug_selector(|| "send".into())
                                    .tooltip(Tooltip::with_key(
                                        if running {
                                            "Send to the running turn"
                                        } else {
                                            "Send to Codex"
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
            // is following the bottom, keep it on the same row at the same offset.
            let top = list.logical_scroll_top();
            for range in changed {
                let range = range.start..range.end.min(status);
                if !range.is_empty() {
                    list.splice(range.clone(), range.len());
                }
            }
            // The status entry changes with almost anything, so it is always remeasured.
            list.splice(status..old, count - status);
            if top.item_ix < status {
                list.scroll_to(top);
            }
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
        let events = self.codex.as_ref().map(|c| c.drain()).unwrap_or_default();
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
                    self.error = Some(format!("Could not save the Codex thread. {error}"));
                }
            }
        }
        cx.notify();
    }

    /// The shared Codex connection. It finds and starts `codex` on its own thread.
    fn codex(&mut self) -> &CodexConnection {
        let wake = self.wake.clone();
        self.codex.get_or_insert_with(|| {
            CodexConnection::new(
                std::env::var_os("KEYSTONE_CODEX_PATH").map(PathBuf::from),
                env!("CARGO_PKG_VERSION"),
                wake,
            )
        })
    }

    /// Writes everything queued for the session logs. Used before the app closes.
    ///
    /// A conversation still loading its history holds what was sent meanwhile, and
    /// logs it only once the history is applied. So finish every load and apply it
    /// first, or those messages would be lost.
    fn flush_logs(&mut self, cx: &mut Context<Self>) {
        loop {
            self.logs.flush();
            self.pump(cx);
            // Applying events can start loading another session; finish that too.
            if !self.conversations.values().any(Conversation::is_loading) {
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
        if session.provider() != Provider::Codex {
            self.error = Some("Claude Code is not connected yet. Codex sessions can send.".into());
            cx.notify();
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
        // A failure to start Codex arrives later as an error in this conversation.
        self.codex().send(id, &cwd, resume.as_deref(), text.clone());
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
        let (Some(id), Some(codex)) = (self.store.workspace().selected(), &self.codex) else {
            return;
        };
        codex.command(Command::Interrupt { session: id });
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
        if let Some(codex) = &self.codex {
            codex.command(Command::Answer {
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
            Some(row) if conversation::visible(row) => Some(transcript::row(row, cx)),
            Some(_) => None,
            None => self.transcript_status(conversation, cx),
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
        conversation: &Conversation,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let colors = cx.theme().colors;
        let mono = cx.theme().fonts.mono.clone();
        let mut view = v_flex().gap_4();
        let mut empty = true;
        if let Some(approval) = conversation.approvals.first() {
            empty = false;
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
                            1 => "Codex wants to run".to_owned(),
                            n => format!("Codex wants to run · {} more waiting", n - 1),
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
        for error in conversation.error.iter().chain(&conversation.save_error) {
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
                                .child(self.transcript(cx)),
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
            .when(automation::enabled(), |d| {
                d.child(automation::frame_writer())
            })
            // Capture runs before the focused input, so Escape closes an open menu first.
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && (this.new_menu || this.settings_menu) {
                    this.new_menu = false;
                    this.settings_menu = false;
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

    fn fake_codex() -> PathBuf {
        // Test binaries live in target/<profile>/deps; the scripted app-server is one
        // level up. `scripts/check.sh ui` builds it first.
        let exe = std::env::current_exe().unwrap();
        let path = exe
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("keystone-fake-codex");
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
            std::env::set_var("KEYSTONE_CODEX_PATH", fake_codex());
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
            assert!(rows.iter().any(|row| matches!(row, Row::Item(item) if item.text.contains("You wrote: hello"))));
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
        gpui::KeyBinding::new("ctrl-n", NewClaude, None),
        gpui::KeyBinding::new("ctrl-shift-n", NewCodex, None),
        gpui::KeyBinding::new("ctrl-shift-s", ToggleSettle, None),
        gpui::KeyBinding::new("ctrl-l", FocusComposer, None),
        gpui::KeyBinding::new("ctrl-f", FocusSearch, None),
        gpui::KeyBinding::new("ctrl-enter", SendMessage, None),
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
