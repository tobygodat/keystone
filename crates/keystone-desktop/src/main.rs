mod text_input;

use gpui::{
    App, Application, Bounds, Context, Corner, Entity, FocusHandle, Focusable, FontWeight,
    IntoElement, Render, Subscription, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    size,
};
use keystone_core::{Durability, Provider, Store};
use keystone_ui::prelude::*;
use std::path::PathBuf;
use text_input::{Changed, InputMode, TextInput};

gpui::actions!(
    keystone,
    [
        Quit,
        NewClaude,
        NewCodex,
        ToggleSettle,
        FocusComposer,
        FocusSearch
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
    _subscriptions: Vec<Subscription>,
    draft_save_task: Option<gpui::Task<()>>,
    focus: FocusHandle,
    new_menu: bool,
    settled_only: bool,
    sidebar: bool,
    fixture: bool,
}

impl Keystone {
    fn new(store: Store, fixture: bool, cx: &mut Context<Self>) -> Self {
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
            cx.subscribe(&search, |_, _, _: &Changed, cx| cx.notify()),
        ];
        let mut result = Self {
            store,
            error: None,
            composer,
            search,
            _subscriptions: subscriptions,
            draft_save_task: None,
            focus: cx.focus_handle(),
            new_menu: false,
            settled_only: false,
            sidebar: true,
            fixture,
        };
        result.sync_draft(cx);
        result
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
        cx.notify();
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
        cx.notify();
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
        cx.notify();
    }

    fn start(&mut self, provider: Provider, window: &mut Window, cx: &mut Context<Self>) {
        self.create(provider, cx);
        if self.error.is_none() && self.store.workspace().selected().is_some() {
            self.composer.focus_handle(cx).focus(window);
        }
    }

    fn titlebar(&self, title: String, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_bg = cx.theme().colors.element_selected;
        h_flex()
            .h(px(38.))
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
                    .size(ButtonSize::Large)
                    .tooltip(Tooltip::text(if self.sidebar {
                        "Hide sidebar"
                    } else {
                        "Show sidebar"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar = !this.sidebar;
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("conversations", IconName::Chat)
                    .size(ButtonSize::Large)
                    .selected(!self.settled_only)
                    .tooltip(Tooltip::text("Conversations"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settled_only = false;
                        this.sidebar = true;
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("search", IconName::Search)
                    .size(ButtonSize::Large)
                    .tooltip(Tooltip::with_key("Search", "Ctrl+F"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.sidebar = true;
                        this.search.focus_handle(cx).focus(window);
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("settled", IconName::Inbox)
                    .size(ButtonSize::Large)
                    .selected(self.settled_only)
                    .tooltip(Tooltip::text("Settled sessions"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settled_only = true;
                        this.sidebar = true;
                        cx.notify();
                    })),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.store.workspace().selected();
        let query = self.search.read(cx).value().to_lowercase();
        let sessions = self
            .store
            .workspace()
            .sessions()
            .iter()
            .filter(|s| {
                (!self.settled_only || s.settled())
                    && format!("{} {}", s.title(), s.provider())
                        .to_lowercase()
                        .contains(&query)
            })
            .map(|session| {
                let id = session.id();
                let state = if session.settled() {
                    "Settled"
                } else {
                    "Draft"
                };
                ListItem::new(("session", id))
                    .debug_selector(move || format!("session-{id}"))
                    .selected(Some(id) == selected)
                    .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
                    .child(Label::new(session.title().to_owned()).truncate())
                    .child(
                        Label::new(format!("{} · {state}", session.provider()))
                            .size(TextSize::Sm)
                            .color(Color::Muted),
                    )
            })
            .collect::<Vec<_>>();
        let empty = match (
            sessions.is_empty(),
            self.store.workspace().sessions().is_empty(),
        ) {
            (false, _) => None,
            (true, true) => Some("No sessions yet. Press + to start one."),
            (true, false) => Some("No sessions match these filters"),
        };
        v_flex()
            .w(px(256.))
            .flex_shrink_0()
            .h_full()
            .hairline_r(cx)
            .child(
                h_flex()
                    .h(px(40.))
                    .pl_3()
                    .pr_1p5()
                    .hairline_b(cx)
                    .child(
                        div().flex_1().child(
                            Label::new(if self.settled_only {
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
                                .selected(self.new_menu)
                                .tooltip(Tooltip::text("New session")),
                        )
                        .anchor(Corner::TopRight)
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
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.start(Provider::Claude, window, cx)
                                })),
                        )
                        .child(
                            MenuItem::new("new-codex", "New Codex session")
                                .icon(IconName::Chat)
                                .kbd("Ctrl+Shift+N")
                                .debug_selector(|| "new-codex".into())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.start(Provider::Codex, window, cx)
                                })),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .h(px(36.))
                    .pl_3()
                    .pr_1p5()
                    .gap_2()
                    .child(Icon::new(IconName::Search).size(px(14.)))
                    .child(div().flex_1().min_w_0().child(self.search.clone()))
                    .child(
                        IconButton::new("filter", IconName::Filter)
                            .size(ButtonSize::Compact)
                            .selected(self.settled_only)
                            .tooltip(Tooltip::text("Show settled only"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.settled_only = !this.settled_only;
                                cx.notify();
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

    fn composer(&self, current: Option<(String, bool)>, cx: &mut Context<Self>) -> gpui::Div {
        let colors = cx.theme().colors;
        let has_session = current.is_some();
        let meta = |text: &'static str| Label::new(text).size(TextSize::Ui).color(Color::Muted);
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
                        .child(meta("Current checkout"))
                        .child(Icon::new(IconName::Branch).size(px(14.)))
                        .child(meta("No project selected"))
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
                        .debug_selector(|| "composer-field".into())
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
                                    .child(Label::new(provider.clone()).size(TextSize::Sm)),
                            )
                        })
                        .child(
                            Label::new("Provider not connected · Enter for a new line")
                                .size(TextSize::Sm)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1())
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

    fn error_line(&self) -> Option<impl IntoElement> {
        self.error
            .clone()
            .map(|error| Label::new(error).size(TextSize::Ui).color(Color::Danger))
    }
}

impl Render for Keystone {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let title = current
            .map(|s| s.title().to_owned())
            .unwrap_or_else(|| "Workspace".into());
        let current = current.map(|s| (s.provider().to_string(), s.settled()));
        let composer = self.composer(current, cx);
        let main = if self.fixture {
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .items_center()
                .child(
                    v_flex()
                        .id("conversation")
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_y_scroll()
                        .items_center()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(CONTENT_WIDTH))
                                .px_3()
                                .py_6()
                                .child(self.transcript(cx)),
                        ),
                )
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
                                .debug_selector(|| "welcome-heading".into())
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
            .on_action(cx.listener(|this, _: &Quit, _, cx| {
                if this.save_draft(cx) {
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
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.sidebar = true;
                this.search.focus_handle(cx).focus(window);
                cx.notify();
            }))
            .child(self.titlebar(title, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.rail(cx))
                    .when(self.sidebar, |d| d.child(self.sidebar(cx)))
                    .child(main),
            )
            .child(
                h_flex()
                    .h(px(26.))
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

    #[gpui::test]
    fn welcome_action_focuses_composer_and_preserves_draft_on_session_switch(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        let (view, cx) = cx.add_window_view(|_, cx| Keystone::new(store, false, cx));
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
        let (view, cx) = cx.add_window_view(|_, cx| Keystone::new(store, false, cx));
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
            "PERF sidebar_draw_ms={:.2}",
            total.as_secs_f64() * 1000. / frames as f64
        );
    }

    #[gpui::test]
    fn menu_created_session_receives_typed_draft(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        let (_view, cx) = cx.add_window_view(|_, cx| Keystone::new(store, false, cx));
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
        let (_view, cx) = cx.add_window_view(|_, cx| Keystone::new(store, false, cx));
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
        let (view, cx) = cx.add_window_view(|_, cx| Keystone::new(store, false, cx));
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
            TextInput::bind_keys(cx);
            cx.bind_keys([
                gpui::KeyBinding::new("ctrl-q", Quit, None),
                gpui::KeyBinding::new("ctrl-n", NewClaude, None),
                gpui::KeyBinding::new("ctrl-shift-n", NewCodex, None),
                gpui::KeyBinding::new("ctrl-shift-s", ToggleSettle, None),
                gpui::KeyBinding::new("ctrl-l", FocusComposer, None),
                gpui::KeyBinding::new("ctrl-f", FocusSearch, None),
            ]);
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
                            cx,
                        );
                        view.focus.focus(window);
                        view
                    });
                    let weak = view.downgrade();
                    window.on_window_should_close(cx, move |_, cx| {
                        weak.update(cx, |view, cx| view.save_draft(cx))
                            .unwrap_or(true)
                    });
                    view
                },
            )
            .expect("open Keystone window");
            cx.activate(true);
        });
}
