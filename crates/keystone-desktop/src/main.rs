mod icons;
mod text_input;

use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, Render,
    Subscription, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size, svg,
};
use keystone_core::{Provider, Store};
use std::path::PathBuf;
use text_input::{Changed, RejectedMultiline, TextInput};

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

const BG: u32 = 0x171717;
const BORDER: u32 = 0x2b2b2b;
const MUTED: u32 = 0x858585;

fn icon(name: &'static str) -> gpui::Svg {
    svg().path(name).size(px(16.)).text_color(rgb(MUTED))
}

fn icon_button(id: &'static str, name: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(30.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0x2b2b2b)))
        .child(icon(name))
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
        let composer = cx.new(|cx| TextInput::new("Write a single-line draft…", cx));
        let search = cx.new(|cx| TextInput::new("Search conversations…", cx));
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
            cx.subscribe(&composer, |this, _, _: &RejectedMultiline, cx| {
                this.error = Some(
                    "This preview supports single-line drafts. Multiline text was not inserted."
                        .into(),
                );
                cx.notify();
            }),
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
        let Some(session) = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| Some(s.id()) == self.store.workspace().selected())
        else {
            return true;
        };
        let draft = self.composer.read(cx).value().to_owned();
        if session.draft() == draft {
            return true;
        }
        match self.store.set_draft(session.id(), draft) {
            Ok(()) => {
                self.error = None;
                cx.notify();
                true
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
        self.error = self.store.create(provider).err().map(|e| e.to_string());
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
            self.error = self
                .store
                .set_settled(session.id(), !session.settled())
                .err()
                .map(|e| e.to_string());
        }
        cx.notify();
    }

    fn transcript(&self) -> gpui::Div {
        let mut table = div()
            .rounded_lg()
            .border_1()
            .border_color(rgb(0x353535))
            .bg(rgb(0x232323))
            .overflow_hidden();
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
                div()
                    .flex()
                    .py_2()
                    .px_3()
                    .when(index > 0, |d| d.border_t_1().border_color(rgb(0x353535)))
                    .children(cells.into_iter().enumerate().map(|(i, cell)| {
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .when(i == 1 && index > 0, |d| {
                                d.font_family("monospace").text_size(px(11.))
                            })
                            .child(cell)
                    })),
            );
        }
        div().flex().flex_col().gap_5().text_size(px(14.)).line_height(px(24.))
            .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child("Interface preview · sample transcript"))
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("A local workspace for Claude Code and Codex."))
            .child("This sample shows the conversation layout. The current preview saves sessions and drafts locally. Agent execution is not connected yet.")
            .child("The testing loop is ready.")
            .child(table)
            .child("Use behavior-focused integration tests for rapid iteration, plus a small native E2E suite. Keep live provider tests separate from deterministic UI checks.")
            .child("The workspace stays focused on conversations. Session selection, draft editing, search, and settle controls are available in this preview.")
            .child("Run the preview with the command below.")
            .child(div().rounded_lg().border_1().border_color(rgb(0x353535)).bg(rgb(0x232323)).overflow_hidden()
                .child(div().px_3().py_2().border_b_1().border_color(rgb(0x353535)).text_color(rgb(MUTED)).text_size(px(12.)).child("⌘  bash"))
                .child(div().p_4().font_family("monospace").text_size(px(12.)).text_color(rgb(0x82a9ce)).child("cargo run -p keystone-desktop --locked")))
            .child("This is a visual fixture, not an agent response. No model request is made.")
    }
}

impl Render for Keystone {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.store.workspace().selected();
        let current = self
            .store
            .workspace()
            .sessions()
            .iter()
            .find(|s| Some(s.id()) == selected);
        let title = current
            .map(|s| s.title().to_owned())
            .unwrap_or_else(|| "Workspace".into());
        let provider = current
            .map(|s| s.provider().to_string())
            .unwrap_or_else(|| "Choose a session".into());
        let settled = current.is_some_and(|s| s.settled());
        let has_session = current.is_some();
        let has_transcript = self.fixture;
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
                div()
                    .id(("session", id))
                    .debug_selector(move || format!("session-{id}").into())
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(if Some(id) == selected { 0x292929 } else { BG }))
                    .hover(|s| s.bg(rgb(0x242424)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.save_draft(cx) {
                            return;
                        }
                        this.error = this.store.select(id).err().map(|e| e.to_string());
                        this.sync_draft(cx);
                        cx.notify();
                    }))
                    .child(div().text_size(px(13.)).child(session.title().to_owned()))
                    .child(
                        div()
                            .mt_1()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(format!(
                                "{} · {}",
                                session.provider(),
                                if session.settled() {
                                    "Settled"
                                } else {
                                    "Draft"
                                }
                            )),
                    )
            })
            .collect::<Vec<_>>();
        let no_matches = sessions.is_empty();
        let composer = div().w_full().max_w(px(884.)).px_3().child(
            div()
                .rounded_lg()
                .border_1()
                .border_color(rgb(0x393939))
                .bg(rgb(0x1e1e1e))
                .p_3()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(icon("folder"))
                        .child("Current checkout")
                        .child(icon("branch"))
                        .child("No project selected")
                        .child(div().flex_1())
                        .when(has_session, |d| {
                            d.child(
                                div()
                                    .id("settle")
                                    .debug_selector(|| "settle".into())
                                    .px_2()
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(rgb(0xdddddd)))
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_settle(cx)))
                                    .child(if settled { "Un-settle" } else { "Settle" }),
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
                            d.text_color(rgb(MUTED))
                                .child("Choose a provider to write a draft…")
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .text_size(px(11.))
                        .when(has_session, |d| {
                            d.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(0x2c2c2c))
                                    .child(provider),
                            )
                        })
                        .child(
                            div()
                                .text_color(rgb(MUTED))
                                .child("Provider not connected · single-line draft"),
                        )
                        .child(div().flex_1())
                        .when(!has_session, |d| {
                            d.child(
                                div()
                                    .id("welcome-claude")
                                    .debug_selector(|| "welcome-claude".into())
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .bg(rgb(0x343434))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(0x434343)))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.create(Provider::Claude, cx);
                                        if this.error.is_none()
                                            && this.store.workspace().selected().is_some()
                                        {
                                            this.composer.focus_handle(cx).focus(window);
                                        }
                                    }))
                                    .child("Start with Claude Code"),
                            )
                            .child(
                                div()
                                    .id("welcome-codex")
                                    .debug_selector(|| "welcome-codex".into())
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .bg(rgb(0x343434))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(0x434343)))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.create(Provider::Codex, cx);
                                        if this.error.is_none()
                                            && this.store.workspace().selected().is_some()
                                        {
                                            this.composer.focus_handle(cx).focus(window);
                                        }
                                    }))
                                    .child("Start with Codex"),
                            )
                        }),
                ),
        );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .font_family("Inter")
            .text_color(rgb(0xc9c9c9))
            .text_size(px(13.))
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Quit, _, cx| {
                if this.save_draft(cx) {
                    cx.quit();
                }
            }))
            .on_action(cx.listener(|this, _: &NewClaude, _, cx| this.create(Provider::Claude, cx)))
            .on_action(cx.listener(|this, _: &NewCodex, _, cx| this.create(Provider::Codex, cx)))
            .on_action(cx.listener(|this, _: &ToggleSettle, _, cx| this.toggle_settle(cx)))
            .on_action(cx.listener(|this, _: &FocusComposer, window, cx| {
                this.composer.focus_handle(cx).focus(window)
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.sidebar = true;
                this.search.focus_handle(cx).focus(window);
                cx.notify();
            }))
            .child(
                div()
                    .h(px(38.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .w(px(if self.sidebar { 302. } else { 46. }))
                            .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .h(px(30.))
                            .w(px(220.))
                            .px_3()
                            .rounded_md()
                            .bg(rgb(0x2c2c2c))
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(icon("chat"))
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_center()
                            .text_size(px(11.))
                            .text_color(rgb(0x626262))
                            .child("keystone"),
                    )
                    .child(
                        icon_button("minimize", "minus")
                            .on_click(|_, window, _| window.minimize_window()),
                    )
                    .child(
                        icon_button("maximize", "square")
                            .on_click(|_, window, _| window.zoom_window()),
                    )
                    .child(icon_button("close", "close").on_click(cx.listener(
                        |this, _, window, cx| {
                            if this.save_draft(cx) {
                                window.remove_window();
                            }
                        },
                    ))),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .w(px(46.))
                            .flex_shrink_0()
                            .h_full()
                            .border_r_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(0x141414))
                            .flex()
                            .flex_col()
                            .items_center()
                            .py_2()
                            .gap_2()
                            .child(icon_button("sidebar", "panel").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.sidebar = !this.sidebar;
                                    cx.notify();
                                },
                            )))
                            .child(
                                icon_button("conversations", "chat")
                                    .bg(rgb(if self.settled_only {
                                        0x141414
                                    } else {
                                        0x2b2b2b
                                    }))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settled_only = false;
                                        this.sidebar = true;
                                        cx.notify();
                                    })),
                            )
                            .child(icon_button("search", "search").on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.sidebar = true;
                                    this.search.focus_handle(cx).focus(window);
                                    cx.notify();
                                },
                            )))
                            .child(
                                icon_button("settled", "inbox")
                                    .bg(rgb(if self.settled_only {
                                        0x2b2b2b
                                    } else {
                                        0x141414
                                    }))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settled_only = true;
                                        this.sidebar = true;
                                        cx.notify();
                                    })),
                            )
                            .child(div().flex_1()),
                    )
                    .when(self.sidebar, |d| {
                        d.child(
                            div()
                                .w(px(256.))
                                .flex_shrink_0()
                                .h_full()
                                .border_r_1()
                                .border_color(rgb(BORDER))
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .h(px(40.))
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .border_b_1()
                                        .border_color(rgb(BORDER))
                                        .child(div().flex_1().child(if self.settled_only {
                                            "Settled sessions"
                                        } else {
                                            "Workspace"
                                        }))
                                        .child(
                                            icon_button("new-session", "plus")
                                                .debug_selector(|| "new-session".into())
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.new_menu = !this.new_menu;
                                                    cx.notify();
                                                })),
                                        ),
                                )
                                .when(self.new_menu, |d| {
                                    d.child(
                                        div()
                                            .mx_2()
                                            .mt_2()
                                            .p_1()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(BORDER))
                                            .bg(rgb(0x232323))
                                            .child(
                                                div()
                                                    .id("new-claude")
                                                    .debug_selector(|| "new-claude".into())
                                                    .p_2()
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(rgb(0x333333)))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.create(Provider::Claude, cx)
                                                    }))
                                                    .child("New Claude session"),
                                            )
                                            .child(
                                                div()
                                                    .id("new-codex")
                                                    .debug_selector(|| "new-codex".into())
                                                    .p_2()
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(rgb(0x333333)))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.create(Provider::Codex, cx)
                                                    }))
                                                    .child("New Codex session"),
                                            ),
                                    )
                                })
                                .child(
                                    div()
                                        .h(px(36.))
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(icon("search"))
                                        .child(div().flex_1().min_w_0().child(self.search.clone()))
                                        .child(icon_button("filter", "filter").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.settled_only = !this.settled_only;
                                                cx.notify();
                                            }),
                                        )),
                                )
                                .child(
                                    div()
                                        .id("session-list")
                                        .flex_1()
                                        .min_h_0()
                                        .overflow_y_scroll()
                                        .px_2()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .when(no_matches, |d| {
                                            d.child(
                                                div()
                                                    .px_1()
                                                    .py_2()
                                                    .text_size(px(12.))
                                                    .text_color(rgb(MUTED))
                                                    .child("No sessions match these filters"),
                                            )
                                        })
                                        .children(sessions),
                                ),
                        )
                    })
                    .child(if has_transcript {
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .child(
                                div()
                                    .id("conversation")
                                    .flex_1()
                                    .min_h_0()
                                    .w_full()
                                    .overflow_y_scroll()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .child(
                                        div()
                                            .w_full()
                                            .max_w(px(884.))
                                            .px_3()
                                            .pt_6()
                                            .pb_6()
                                            .child(self.transcript()),
                                    ),
                            )
                            .when_some(self.error.clone(), |d, error| {
                                d.child(
                                    div()
                                        .w_full()
                                        .max_w(px(884.))
                                        .px_4()
                                        .pb_2()
                                        .text_color(rgb(0xea9990))
                                        .child(error),
                                )
                            })
                            .child(composer.pb_2())
                    } else {
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(884.))
                                    .flex()
                                    .flex_col()
                                    .gap_5()
                                    .child(
                                        div()
                                            .id("welcome-heading")
                                            .debug_selector(|| "welcome-heading".into())
                                            .px_3()
                                            .text_size(px(20.))
                                            .child("What should we work on?"),
                                    )
                                    .child(composer)
                                    .when_some(self.error.clone(), |d, error| {
                                        d.child(div().px_4().text_color(rgb(0xea9990)).child(error))
                                    }),
                            )
                    }),
            )
            .child(
                div()
                    .h(px(26.))
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .px_3()
                    .flex()
                    .items_center()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child("Local workspace")
                    .child(div().flex_1())
                    .child("Claude Code + Codex · preview"),
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
        let (view, cx) = cx.add_window_view(|_, cx| Keystone::new(store, false, cx));
        assert!(cx.debug_bounds("welcome-heading").is_some());
        assert!(cx.debug_bounds("welcome-codex").is_some());

        let bounds = cx.debug_bounds("welcome-claude").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(Store::open(&path).unwrap().workspace().selected(), Some(1));
        cx.simulate_input("Draft from the welcome composer");
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
                "Draft from the welcome composer"
            );
        });
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            "Draft from the welcome composer"
        );
    }

    #[gpui::test]
    fn clicking_create_and_settle_persists_sessions(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
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
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            ""
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
        .with_assets(icons::Icons)
        .run(move |cx: &mut App| {
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
